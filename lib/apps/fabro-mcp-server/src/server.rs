use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fabro_manifest::SuppliedWorkflowVersionPackager;
use fabro_tool::fabro_client::ClientBackend;
use fabro_tool::{self as run_tools, FabroToolBackend};
use fabro_util::version::FABRO_VERSION;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, serve_server, tool, tool_handler, tool_router};
use serde::Serialize;
use tokio::sync::OnceCell;
use tokio::time;
use tracing::warn;

use crate::executable_monitor::ExecutableMonitor;
use crate::{FabroMcpServerSettings, SERVER_NAME};

#[derive(Clone)]
pub(crate) struct FabroMcpServer {
    settings:    Arc<FabroMcpServerSettings>,
    backend:     Arc<OnceCell<Arc<dyn FabroToolBackend>>>,
    tool_router: ToolRouter<Self>,
}

/// How long to wait for the MCP service to stop after an upgrade is detected.
/// The wait is bounded because the transport closes by writing to stdout, which
/// blocks if the host has stopped reading.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn start(settings: FabroMcpServerSettings) -> Result<()> {
    let monitor = match ExecutableMonitor::current() {
        Ok(monitor) => Some(monitor),
        Err(error) => {
            warn!(
                %error,
                "Upgrade detection is unavailable; this MCP server will keep running after an \
                 upgrade replaces it"
            );
            None
        }
    };
    let service = serve_server(FabroMcpServer::new(Arc::new(settings)), stdio()).await?;
    let Some(monitor) = monitor else {
        service.waiting().await?;
        return Ok(());
    };

    let cancellation = service.cancellation_token();
    let mut service_wait = Box::pin(service.waiting());
    tokio::select! {
        result = &mut service_wait => {
            result?;
        }
        () = monitor.wait_until_replaced() => {
            // An upgrade replaced the executable, so stop serving and let the
            // host reconnect to the new one. The CLI exits the process rather
            // than returning, because Tokio's stdin worker stays blocked on a
            // read that only the host can end.
            cancellation.cancel();
            let _ = time::timeout(SHUTDOWN_TIMEOUT, service_wait).await;
        }
    }
    Ok(())
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FabroMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, FABRO_VERSION).with_title("Fabro"))
            .with_instructions("Use these tools to register workflow versions and create, inspect, control, wait for, and read events from Fabro workflow runs.")
    }
}

#[tool_router(router = tool_router)]
impl FabroMcpServer {
    pub(crate) fn new(settings: Arc<FabroMcpServerSettings>) -> Self {
        Self {
            settings,
            backend: Arc::new(OnceCell::new()),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "fabro_workflow_version_create",
        description = "Register supplied workflow file contents and all local dependencies as a reusable immutable workflow version ID. Obtain files with shell/read tools first; this does not create or start a run."
    )]
    async fn fabro_workflow_version_create(
        &self,
        params: Parameters<run_tools::FabroWorkflowVersionCreateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let source = match run_tools::ValidatedWorkflowVersionCreate::try_from(params.0) {
            Ok(source) => source,
            Err(err) => return Ok(error_result(&err)),
        };
        let backend = match self.backend().await {
            Ok(backend) => backend,
            Err(err) => return Ok(error_result(&err)),
        };
        match run_tools::create_workflow_version(backend, source).await {
            Ok(result) => success_result(&result, run_tools::workflow_version_create_text(&result)),
            Err(err) => Ok(error_result(&err)),
        }
    }

    #[tool(
        name = "fabro_run_create",
        description = "Create runs from registered workflow_version_id values and canonical RunIntent settings. Register contents with fabro_workflow_version_create first. Standalone calls require an explicit target; native workers may inherit the parent target. Starts runs by default."
    )]
    async fn fabro_run_create(
        &self,
        params: Parameters<run_tools::FabroRunCreateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let params = params.0;
        if let Err(err) = params.validate(run_tools::CreateRunOptions::default()) {
            return Ok(error_result(&err));
        }
        let backend = match self.backend().await {
            Ok(backend) => backend,
            Err(err) => return Ok(error_result(&err)),
        };
        match run_tools::create_runs(backend, params).await {
            Ok(result) => success_result(&result, run_tools::create_runs_text(&result)),
            Err(err) => Ok(error_result(&err)),
        }
    }

    #[tool(
        name = "fabro_run_search",
        description = "Search Fabro workflow runs by id, parent, workflow, labels, status, archival state, and creation time."
    )]
    async fn fabro_run_search(
        &self,
        params: Parameters<run_tools::FabroRunSearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let params = match run_tools::ValidatedSearchRuns::try_from(params.0) {
            Ok(params) => params,
            Err(err) => return Ok(error_result(&err)),
        };
        let backend = match self.backend().await {
            Ok(backend) => backend,
            Err(err) => return Ok(error_result(&err)),
        };
        match run_tools::search_runs(backend, params).await {
            Ok(result) => success_result(&result, run_tools::search_runs_text(&result)),
            Err(err) => Ok(error_result(&err)),
        }
    }

    #[tool(
        name = "fabro_run_get",
        description = "Read-only inspection of a Fabro run: returns its summary, projection, and pending questions without mutating state."
    )]
    async fn fabro_run_get(
        &self,
        params: Parameters<run_tools::FabroRunGetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let params = match run_tools::ValidatedRunGet::try_from(params.0) {
            Ok(params) => params,
            Err(err) => return Ok(error_result(&err)),
        };
        let backend = match self.backend().await {
            Ok(backend) => backend,
            Err(err) => return Ok(error_result(&err)),
        };
        match run_tools::run_get(backend, params).await {
            Ok(result) => success_result(&result, run_tools::run_get_text(&result)),
            Err(err) => Ok(error_result(&err)),
        }
    }

    #[tool(
        name = "fabro_run_interact",
        description = "Control a Fabro run: start, approve, deny, message, interrupt, cancel, archive, unarchive, link or unlink a parent, inspect or answer questions. Use fabro_run_get for read-only inspection."
    )]
    async fn fabro_run_interact(
        &self,
        params: Parameters<run_tools::FabroRunInteractParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let params = match run_tools::ValidatedInteractRun::try_from(params.0) {
            Ok(params) => params,
            Err(err) => return Ok(error_result(&err)),
        };
        let backend = match self.backend().await {
            Ok(backend) => backend,
            Err(err) => return Ok(error_result(&err)),
        };
        match run_tools::interact_run(backend, params).await {
            Ok(result) => success_result(&result, run_tools::interact_run_text(&result)),
            Err(err) => Ok(error_result(&err)),
        }
    }

    #[tool(
        name = "fabro_run_gather",
        description = "Wait for Fabro runs to reach terminal states or operator attention. Bounded waits return current state on timeout; wait_until_terminal_or_attention=true observes until meaningful state or caller cancellation."
    )]
    async fn fabro_run_gather(
        &self,
        params: Parameters<run_tools::FabroRunGatherParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let params = match run_tools::ValidatedGatherRuns::try_from(params.0) {
            Ok(params) => params,
            Err(err) => return Ok(error_result(&err)),
        };
        let backend = match self.backend().await {
            Ok(backend) => backend,
            Err(err) => return Ok(error_result(&err)),
        };
        match run_tools::gather_runs(backend, params).await {
            Ok(result) => success_result(&result, run_tools::gather_runs_text(&result)),
            Err(err) => Ok(error_result(&err)),
        }
    }

    #[tool(
        name = "fabro_run_pair",
        description = "Inspect, start, message, end, or read transcript for a live Fabro run pairing session."
    )]
    async fn fabro_run_pair(
        &self,
        params: Parameters<run_tools::FabroRunPairParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let params = match run_tools::ValidatedPairRun::try_from(params.0) {
            Ok(params) => params,
            Err(err) => return Ok(error_result(&err)),
        };
        let backend = match self.backend().await {
            Ok(backend) => backend,
            Err(err) => return Ok(error_result(&err)),
        };
        match run_tools::pair_run(backend, params).await {
            Ok(result) => success_result(&result, run_tools::pair_run_text(&result)),
            Err(err) => Ok(error_result(&err)),
        }
    }

    #[tool(
        name = "fabro_run_events",
        description = "List, inspect, or search stored events for a Fabro workflow run."
    )]
    async fn fabro_run_events(
        &self,
        params: Parameters<run_tools::FabroRunEventsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let params = match run_tools::ValidatedRunEvents::try_from(params.0) {
            Ok(params) => params,
            Err(err) => return Ok(error_result(&err)),
        };
        let backend = match self.backend().await {
            Ok(backend) => backend,
            Err(err) => return Ok(error_result(&err)),
        };
        match run_tools::run_events(backend, params).await {
            Ok(result) => success_result(&result, run_tools::run_events_text(&result)),
            Err(err) => Ok(error_result(&err)),
        }
    }

    async fn backend(&self) -> Result<Arc<dyn FabroToolBackend>, run_tools::ToolError> {
        self.backend
            .get_or_try_init(|| async {
                (self.settings.client_factory)()
                    .await
                    .map(|client| {
                        Arc::new(
                            ClientBackend::new(Arc::new(client)).with_workflow_version_packager(
                                Arc::new(SuppliedWorkflowVersionPackager),
                            ),
                        ) as Arc<dyn FabroToolBackend>
                    })
                    .map_err(|err| run_tools::ToolError::from_anyhow(&err))
            })
            .await
            .map(Arc::clone)
    }
}

fn success_result<T: Serialize>(
    value: &T,
    text: impl Into<String>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let structured_content = serde_json::to_value(value).map_err(|err| {
        rmcp::ErrorData::internal_error(
            format!("failed to serialize Fabro MCP tool result: {err}"),
            None,
        )
    })?;
    let mut result = CallToolResult::structured(structured_content);
    result.content = vec![Content::text(text.into())];
    Ok(result)
}

fn error_result(err: &run_tools::ToolError) -> CallToolResult {
    CallToolResult::error(vec![Content::text(err.to_string())])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use serde_json::Value;

    use super::*;
    use crate::FabroMcpServerSettings;

    #[tokio::test]
    async fn workflow_version_mcp_surface_matches_catalog_and_returns_minimal_result() {
        let mock = httpmock::MockServer::start_async().await;
        let version = fabro_types::WorkflowVersion::new(
            "workflow".parse().unwrap(),
            BTreeMap::from([("workflow".parse().unwrap(), "digraph W {}".to_string())]),
            BTreeMap::new(),
        )
        .unwrap();
        let id = version.id().unwrap();
        let upload = mock
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v1/workflow-versions")
                    .json_body_obj(&version);
                then.status(201)
                    .json_body(serde_json::json!({"workflow_version_id":id}));
            })
            .await;
        let url = mock.url("");
        let server = FabroMcpServer::new(Arc::new(FabroMcpServerSettings {
            client_factory: Arc::new(move || {
                let url = url.clone();
                Box::pin(async move { fabro_client::Client::new_no_proxy(&url) })
            }),
        }));
        let tools = server.tool_router.list_all();
        let tool = tools
            .iter()
            .find(|tool| tool.name == "fabro_workflow_version_create")
            .unwrap();
        let definition = run_tools::tool_definitions()
            .iter()
            .find(|tool| tool.name == "fabro_workflow_version_create")
            .unwrap();
        let mut expected = definition.parameters.clone();
        expected.as_object_mut().unwrap().remove("$schema");
        let mut actual = Value::Object(tool.input_schema.as_ref().clone());
        actual.as_object_mut().unwrap().remove("$schema");
        assert_eq!(actual, expected);
        assert_eq!(tool.description.as_deref(), Some(definition.description));
        let params =
            serde_json::json!({"entrypoint":"workflow","files":{"workflow":"digraph W {}"}});
        let result = server
            .fabro_workflow_version_create(Parameters(serde_json::from_value(params).unwrap()))
            .await
            .unwrap();
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"workflow_version_id":id}))
        );
        assert_ne!(result.is_error, Some(true));
        upload.assert_calls_async(1).await;
    }

    #[test]
    fn server_info_reports_fabro_version() {
        let settings = FabroMcpServerSettings {
            client_factory: Arc::new(|| {
                Box::pin(async { panic!("client should not be constructed while reading info") })
            }),
        };
        let info = FabroMcpServer::new(Arc::new(settings)).get_info();

        assert_eq!(info.server_info.name, "fabro");
        assert_eq!(info.server_info.title.as_deref(), Some("Fabro"));
        assert_eq!(info.server_info.version, FABRO_VERSION);
    }

    #[test]
    fn fabro_run_pair_tool_is_registered_with_stage_based_schema() {
        let settings = FabroMcpServerSettings {
            client_factory: Arc::new(|| {
                Box::pin(async { panic!("client should not be constructed while listing tools") })
            }),
        };
        let server = FabroMcpServer::new(Arc::new(settings));
        let tools = server.tool_router.list_all();
        let tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "fabro_run_pair")
            .expect("fabro_run_pair should be registered");
        let schema = Value::Object(tool.input_schema.as_ref().clone());
        let schema_text = schema.to_string();

        assert!(schema_text.contains("stage_id"));
        assert!(!schema_text.contains("agent_session_id"));
        assert!(!schema_text.contains("session_id"));
        assert!(!schema_text.contains("PairTargetSelector"));
        assert!(!schema_text.contains("\"target\""));
        assert!(!schema_text.contains("provider"));
        assert!(!schema_text.contains("\"model\""));
        assert!(!schema_text.contains("\"node_id\""));
        assert!(!schema_text.contains("\"visit\""));
    }

    #[test]
    fn fabro_run_create_tool_advertises_canonical_version_contract() {
        let settings = FabroMcpServerSettings {
            client_factory: Arc::new(|| {
                Box::pin(async { panic!("client should not be constructed while listing tools") })
            }),
        };
        let server = FabroMcpServer::new(Arc::new(settings));
        let tools = server.tool_router.list_all();
        let tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "fabro_run_create")
            .expect("fabro_run_create should be registered");
        let schema = Value::Object(tool.input_schema.as_ref().clone());
        let definition = run_tools::tool_definitions()
            .iter()
            .find(|definition| definition.name == "fabro_run_create")
            .unwrap();
        let mut expected = definition.parameters.clone();
        expected.as_object_mut().unwrap().remove("$schema");
        let mut actual = schema;
        actual.as_object_mut().unwrap().remove("$schema");
        assert_eq!(actual, expected);
        assert_eq!(tool.description.as_deref(), Some(definition.description));
    }
}
