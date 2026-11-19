//! The Fabro workflow and run tools as application tools a pebble coding
//! agent can call.

use std::path::{Component, Path};
use std::sync::Arc;

use async_trait::async_trait;
use pebble_agent::ToolScheduling;
use pebble_coding_agent::environment::Environment;
use pebble_coding_agent::tools::{RegisteredTool, ToolError, ToolSource};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::services::FabroRunToolServices;

/// A scheduler wait opened around `fabro_run_gather`.
///
/// The gather result is withheld from the agent until `readmit` has
/// reacquired execution capacity. Dropping a wait cancels the wait without
/// allowing more work to run.
#[async_trait]
pub trait GatherWait: Send {
    async fn readmit(self: Box<Self>);
}

/// Native execution admission for one agent stage.
pub trait GatherAdmission: Send + Sync {
    fn wait(&self) -> Box<dyn GatherWait>;
}

/// Every Fabro run tool, bound to `services`.
#[must_use]
pub fn register_fabro_run_tools(services: &FabroRunToolServices) -> Vec<RegisteredTool> {
    register_fabro_run_tools_with_gather_admission(services, None)
}

/// Every Fabro run tool, with an optional native admission wait around gather.
///
/// A scheduler-aware gather is root-session-only. Other run tools keep their
/// ordinary subagent inheritance.
#[must_use]
pub fn register_fabro_run_tools_with_gather_admission(
    services: &FabroRunToolServices,
    gather_admission: Option<&Arc<dyn GatherAdmission>>,
) -> Vec<RegisteredTool> {
    fabro_tool::tool_definitions()
        .iter()
        .map(|definition| {
            fabro_run_tool(
                &native_tool_definition(definition),
                services.clone(),
                gather_admission.map(Arc::clone),
            )
        })
        .collect()
}

/// Only the Fabro run tools whose names appear in `names`.
///
/// Unknown names are silently ignored so callers can list every tool they
/// care about without depending on the current `fabro_tool` catalog.
#[must_use]
pub fn register_named_fabro_run_tools(
    services: &FabroRunToolServices,
    names: &[&str],
) -> Vec<RegisteredTool> {
    fabro_tool::tool_definitions()
        .iter()
        .filter(|definition| names.contains(&definition.name))
        .map(|definition| {
            fabro_run_tool(&native_tool_definition(definition), services.clone(), None)
        })
        .collect()
}

fn fabro_run_tool(
    definition: &fabro_tool::ToolDefinition,
    services: FabroRunToolServices,
    gather_admission: Option<Arc<dyn GatherAdmission>>,
) -> RegisteredTool {
    let name = definition.name.to_string();
    let is_gather = name == fabro_tool::FABRO_RUN_GATHER_TOOL_NAME;
    let waits_for_admission = is_gather && gather_admission.is_some();
    let gather_admission = admission_for_tool(&name, gather_admission);
    let services = Arc::new(services);
    let tool = RegisteredTool::function(
        name.clone(),
        definition.description.to_string(),
        definition.parameters.clone(),
        move |context, arguments| {
            let name = name.clone();
            let services = Arc::clone(&services);
            let gather_admission = gather_admission.as_ref().map(Arc::clone);
            async move {
                invoke_fabro_run_tool(
                    &name,
                    arguments,
                    &services,
                    gather_admission,
                    Some(context.env().as_ref()),
                )
                .await
                .map_err(|error| ToolError::execution(error.to_string()))
            }
        },
    )
    .with_source(ToolSource::Application);
    // A gather carrying scheduler admission must be the only call in its
    // tool round: only then is the parent genuinely quiescent while it waits.
    let tool = if waits_for_admission {
        tool.with_scheduling(ToolScheduling::ExclusiveRound)
    } else {
        tool
    };
    if waits_for_admission {
        // Admission belongs to the whole Petri firing, while Pebble schedules
        // subagents independently. Keep the releasing gather on the root
        // session, whose active descendants the firing admission observes.
        tool
    } else {
        // A subagent spawned by a workflow stage does the same work under the
        // same run, so it keeps the same view of the run tree.
        tool.allow_in_subagents()
    }
}

fn admission_for_tool(
    name: &str,
    gather_admission: Option<Arc<dyn GatherAdmission>>,
) -> Option<Arc<dyn GatherAdmission>> {
    (name == fabro_tool::FABRO_RUN_GATHER_TOOL_NAME)
        .then_some(gather_admission)
        .flatten()
}

async fn invoke_fabro_run_tool(
    name: &str,
    args: serde_json::Value,
    services: &FabroRunToolServices,
    gather_admission: Option<Arc<dyn GatherAdmission>>,
    environment: Option<&dyn Environment>,
) -> fabro_tool::ToolResult<String> {
    let wait = gather_admission.map(|admission| admission.wait());
    let result = execute_fabro_run_tool_with_environment(name, args, services, environment).await;
    if let Some(wait) = wait {
        wait.readmit().await;
    }
    result
}

async fn execute_fabro_run_tool_with_environment(
    name: &str,
    mut args: serde_json::Value,
    services: &FabroRunToolServices,
    environment: Option<&dyn Environment>,
) -> fabro_tool::ToolResult<String> {
    match name {
        fabro_tool::FABRO_WORKFLOW_VERSION_CREATE_TOOL_NAME => {
            let params =
                parse_fabro_tool_args::<fabro_tool::FabroWorkflowVersionCreateParams>(name, args)?;
            let source = fabro_tool::ValidatedWorkflowVersionCreate::try_from(params)?;
            let result =
                fabro_tool::create_workflow_version(Arc::clone(&services.backend), source).await?;
            let summary = fabro_tool::workflow_version_create_text(&result);
            render_fabro_tool_result(&summary, &result)
        }
        fabro_tool::FABRO_RUN_CREATE_TOOL_NAME => {
            let params = parse_fabro_tool_args::<fabro_tool::FabroRunCreateParams>(name, args)?;
            let result = fabro_tool::create_runs_with_options(
                Arc::clone(&services.backend),
                params,
                fabro_tool::CreateRunOptions {
                    forced_parent_id: Some(services.current_run_id),
                },
            )
            .await?;
            let summary = fabro_tool::create_runs_text(&result);
            render_fabro_tool_result(&summary, &result)
        }
        fabro_tool::FABRO_RUN_SEARCH_TOOL_NAME => {
            let output_path = take_output_path(&mut args, environment)?;
            let params = parse_fabro_tool_args::<fabro_tool::FabroRunSearchParams>(name, args)?;
            let result = fabro_tool::search_runs(
                Arc::clone(&services.backend),
                fabro_tool::ValidatedSearchRuns::try_from(params)?,
            )
            .await?;
            if let Some(output_path) = output_path {
                let environment = environment.ok_or_else(|| {
                    fabro_tool::ToolError::message(
                        "output_path is available only to native workflow sandbox tools",
                    )
                })?;
                let receipt = save_result(environment, &output_path, &result).await?;
                return render_fabro_tool_result("saved Fabro run search results", &receipt);
            }
            let summary = fabro_tool::search_runs_text(&result);
            render_fabro_tool_result(&summary, &result)
        }
        fabro_tool::FABRO_RUN_GET_TOOL_NAME => {
            let params = parse_fabro_tool_args::<fabro_tool::FabroRunGetParams>(name, args)?;
            let result = fabro_tool::run_get(
                Arc::clone(&services.backend),
                fabro_tool::ValidatedRunGet::try_from(params)?,
            )
            .await?;
            let summary = fabro_tool::run_get_text(&result);
            render_fabro_tool_result(&summary, &result)
        }
        fabro_tool::FABRO_RUN_INTERACT_TOOL_NAME => {
            let params = parse_fabro_tool_args::<fabro_tool::FabroRunInteractParams>(name, args)?;
            let validated = fabro_tool::ValidatedInteractRun::try_from(params)?;
            if validated.action.requires_user() {
                return Err(fabro_tool::ToolError::message(
                    "Run approval must be performed by a user through the API, CLI, web UI, or human MCP server.",
                ));
            }
            let result = fabro_tool::interact_run(Arc::clone(&services.backend), validated).await?;
            let summary = fabro_tool::interact_run_text(&result);
            render_fabro_tool_result(&summary, &result)
        }
        fabro_tool::FABRO_RUN_GATHER_TOOL_NAME => {
            let params = parse_fabro_tool_args::<fabro_tool::FabroRunGatherParams>(name, args)?;
            let result = fabro_tool::gather_runs(
                Arc::clone(&services.backend),
                fabro_tool::ValidatedGatherRuns::try_from(params)?,
            )
            .await?;
            let summary = fabro_tool::gather_runs_text(&result);
            render_fabro_tool_result(&summary, &result)
        }
        fabro_tool::FABRO_RUN_EVENTS_TOOL_NAME => {
            let output_path = take_output_path(&mut args, environment)?;
            let params = parse_fabro_tool_args::<fabro_tool::FabroRunEventsParams>(name, args)?;
            let result = fabro_tool::run_events(
                Arc::clone(&services.backend),
                fabro_tool::ValidatedRunEvents::try_from(params)?,
            )
            .await?;
            if let Some(output_path) = output_path {
                let environment = environment.ok_or_else(|| {
                    fabro_tool::ToolError::message(
                        "output_path is available only to native workflow sandbox tools",
                    )
                })?;
                let receipt = save_result(environment, &output_path, &result).await?;
                return render_fabro_tool_result("saved Fabro run events", &receipt);
            }
            let summary = fabro_tool::run_events_text(&result);
            render_fabro_tool_result(&summary, &result)
        }
        fabro_tool::FABRO_RUN_PAIR_TOOL_NAME => {
            let params = parse_fabro_tool_args::<fabro_tool::FabroRunPairParams>(name, args)?;
            let result = fabro_tool::pair_run(
                Arc::clone(&services.backend),
                fabro_tool::ValidatedPairRun::try_from(params)?,
            )
            .await?;
            let summary = fabro_tool::pair_run_text(&result);
            render_fabro_tool_result(&summary, &result)
        }
        _ => Err(fabro_tool::ToolError::message(format!(
            "unknown Fabro run tool `{name}`"
        ))),
    }
}

fn parse_fabro_tool_args<T>(name: &str, args: serde_json::Value) -> fabro_tool::ToolResult<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(args)
        .map_err(|err| fabro_tool::ToolError::message(format!("invalid {name} arguments: {err}")))
}

fn render_fabro_tool_result<T>(summary: &str, result: &T) -> fabro_tool::ToolResult<String>
where
    T: Serialize,
{
    let json = serde_json::to_string_pretty(result).map_err(|err| {
        fabro_tool::ToolError::message(format!("failed to serialize tool result: {err}"))
    })?;
    Ok(format!("{summary}\n{json}"))
}

fn native_tool_definition(definition: &fabro_tool::ToolDefinition) -> fabro_tool::ToolDefinition {
    let mut definition = definition.clone();
    if matches!(
        definition.name,
        fabro_tool::FABRO_RUN_SEARCH_TOOL_NAME | fabro_tool::FABRO_RUN_EVENTS_TOOL_NAME
    ) {
        definition.description = match definition.name {
            fabro_tool::FABRO_RUN_SEARCH_TOOL_NAME => {
                "Search Fabro workflow runs by id, parent, workflow, labels, status, archival state, and creation time. In a workflow sandbox, output_path saves the exact JSON result to a new workspace-relative file and returns only a receipt."
            }
            fabro_tool::FABRO_RUN_EVENTS_TOOL_NAME => {
                "List, inspect, or search stored events for a Fabro workflow run. In a workflow sandbox, output_path saves the exact JSON result to a new workspace-relative file and returns only a receipt."
            }
            _ => unreachable!("only search and events receive native output_path"),
        };
        definition.parameters["properties"]["output_path"] = serde_json::json!({
            "type": "string",
            "description": "Optional new workspace-relative file that receives the exact JSON result. Native workflow sandboxes only; absolute paths, traversal, symlinks, and existing files are refused."
        });
    }
    definition
}

fn take_output_path(
    args: &mut Value,
    environment: Option<&dyn Environment>,
) -> fabro_tool::ToolResult<Option<String>> {
    let object = args.as_object_mut().ok_or_else(|| {
        fabro_tool::ToolError::message("native Fabro run tool arguments must be a JSON object")
    })?;
    let Some(output_path) = object.remove("output_path") else {
        return Ok(None);
    };
    let output_path = output_path
        .as_str()
        .ok_or_else(|| fabro_tool::ToolError::message("output_path must be a non-empty string"))?;
    if environment.is_none() {
        return Err(fabro_tool::ToolError::message(
            "output_path is available only to native workflow sandbox tools",
        ));
    }
    if output_path.trim().is_empty() {
        return Err(fabro_tool::ToolError::message(
            "output_path must be a non-empty string",
        ));
    }
    validate_output_path(output_path)?;
    Ok(Some(output_path.to_string()))
}

#[derive(Serialize)]
struct SavedResultReceipt {
    output_path:  String,
    result_count: usize,
    next_cursor:  Option<Value>,
    byte_length:  usize,
    sha256:       String,
}

async fn save_result<T>(
    environment: &dyn Environment,
    output_path: &str,
    result: &T,
) -> fabro_tool::ToolResult<SavedResultReceipt>
where
    T: Serialize,
{
    ensure_new_workspace_file(environment, output_path).await?;
    let contents = serde_json::to_string_pretty(result).map_err(|error| {
        fabro_tool::ToolError::message(format!("failed to serialize saved tool result: {error}"))
    })?;
    environment
        .write_file(output_path, &contents)
        .await
        .map_err(|error| {
            fabro_tool::ToolError::message(format!(
                "failed to save tool result to {output_path}: {error}"
            ))
        })?;
    let value = serde_json::to_value(result).map_err(|error| {
        fabro_tool::ToolError::message(format!("failed to serialize saved tool receipt: {error}"))
    })?;
    let result_count = value
        .get("runs")
        .or_else(|| value.get("events"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    Ok(SavedResultReceipt {
        output_path: output_path.to_string(),
        result_count,
        next_cursor: value.get("next_cursor").cloned(),
        byte_length: contents.len(),
        sha256: hex::encode(Sha256::digest(contents.as_bytes())),
    })
}

fn validate_output_path(output_path: &str) -> fabro_tool::ToolResult<()> {
    let path = Path::new(output_path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(fabro_tool::ToolError::message(
            "output_path must be a workspace-relative path without traversal",
        ));
    }
    Ok(())
}

async fn ensure_new_workspace_file(
    environment: &dyn Environment,
    output_path: &str,
) -> fabro_tool::ToolResult<()> {
    let components = Path::new(output_path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(component) => component.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut directory = ".".to_string();
    for (index, component) in components.iter().enumerate() {
        let entries = environment
            .list_directory(&directory, Some(1))
            .await
            .map_err(|error| {
                fabro_tool::ToolError::message(format!(
                    "failed to validate output_path {output_path}: {error}"
                ))
            })?;
        let entry = entries.iter().find(|entry| entry.name == *component);
        let is_file = index + 1 == components.len();
        match (is_file, entry) {
            (true, Some(_)) => {
                return Err(fabro_tool::ToolError::message(format!(
                    "output_path must name a new workspace file: {output_path}"
                )));
            }
            (false, Some(entry)) if !entry.is_dir => {
                return Err(fabro_tool::ToolError::message(format!(
                    "output_path traverses a non-directory or symlink: {output_path}"
                )));
            }
            (false, None) => return Ok(()),
            _ => {}
        }
        if !is_file {
            directory.push('/');
            directory.push_str(component);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use chrono::{TimeZone as _, Utc};
    use fabro_api::types;
    use fabro_tool::fabro_client::ClientBackend;
    use fabro_tool::{FabroToolBackend, ValidatedWorkflowVersionCreate, WorkflowVersionPackager};
    use fabro_types::{
        Run, RunId, RunIntent, RunLifecycle, RunLinks, RunOrigin, RunProjection, RunStatus,
        RunStreamItem, RunStreamItemKind, RunTimestamps, WorkflowRef, WorkflowVersion,
        test_support,
    };
    use fabro_workflow_version::{CollectedWorkflowClosure, ValidatedWorkflowVersion};
    use lithos_llm::types::ToolCall;
    use pebble_coding_agent::environment::{Environment as _, LocalEnvironment};
    use pebble_coding_agent::sandbox_driver::SandboxEnvironment;
    use pebble_coding_agent::tools::{CodingToolSet, ToolRunner};
    use sandbox_driver::{SandboxProvider as _, SandboxSource, SandboxSpec};
    use sandbox_driver_host::HostProvider;
    use serde_json::json;
    use tokio::sync::Notify;
    use tokio::task;
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct CountingAdmission {
        waits:   AtomicUsize,
        entered: Arc<Notify>,
        grant:   Arc<Notify>,
    }

    impl CountingAdmission {
        fn new() -> Self {
            Self {
                waits:   AtomicUsize::new(0),
                entered: Arc::new(Notify::new()),
                grant:   Arc::new(Notify::new()),
            }
        }
    }

    impl GatherAdmission for CountingAdmission {
        fn wait(&self) -> Box<dyn GatherWait> {
            self.waits.fetch_add(1, Ordering::Relaxed);
            Box::new(TestReadmission {
                entered: Arc::clone(&self.entered),
                grant:   Arc::clone(&self.grant),
            })
        }
    }

    struct TestReadmission {
        entered: Arc<Notify>,
        grant:   Arc<Notify>,
    }

    #[async_trait]
    impl GatherWait for TestReadmission {
        async fn readmit(self: Box<Self>) {
            self.entered.notify_one();
            self.grant.notified().await;
        }
    }

    fn test_services() -> FabroRunToolServices {
        let client = fabro_client::Client::new_no_proxy("http://127.0.0.1:1")
            .expect("the test URL is valid");
        FabroRunToolServices {
            backend:        Arc::new(ClientBackend::new(Arc::new(client))),
            current_run_id: fabro_types::RunId::new(),
        }
    }

    #[derive(Clone)]
    struct QueryBackend {
        runs:   Vec<Run>,
        events: Vec<RunStreamItem>,
    }

    fn unsupported<T>() -> anyhow::Result<T> {
        anyhow::bail!("operation is not used by native query adapter tests")
    }

    #[async_trait]
    impl FabroToolBackend for QueryBackend {
        async fn create_run_from_intent(&self, _intent: RunIntent) -> anyhow::Result<RunId> {
            unsupported()
        }

        async fn resolve_run(&self, selector: &str) -> anyhow::Result<Run> {
            self.runs
                .iter()
                .find(|run| run.id.to_string() == selector)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("run {selector} not found"))
        }

        async fn retrieve_run(&self, run_id: &RunId) -> anyhow::Result<Run> {
            self.resolve_run(&run_id.to_string()).await
        }

        async fn start_run(&self, _run_id: &RunId, _resume: bool) -> anyhow::Result<Run> {
            unsupported()
        }

        async fn approve_run(&self, _run_id: &RunId) -> anyhow::Result<Run> {
            unsupported()
        }

        async fn deny_run(&self, _run_id: &RunId, _reason: Option<String>) -> anyhow::Result<Run> {
            unsupported()
        }

        async fn cancel_run(&self, _run_id: &RunId) -> anyhow::Result<Run> {
            unsupported()
        }

        async fn interrupt_run(&self, _run_id: &RunId) -> anyhow::Result<()> {
            unsupported()
        }

        async fn steer_run(
            &self,
            _run_id: &RunId,
            _text: String,
            _interrupt: bool,
        ) -> anyhow::Result<()> {
            unsupported()
        }

        async fn archive_run(&self, _run_id: &RunId) -> anyhow::Result<Run> {
            unsupported()
        }

        async fn unarchive_run(&self, _run_id: &RunId) -> anyhow::Result<Run> {
            unsupported()
        }

        async fn list_store_runs(&self) -> anyhow::Result<Vec<Run>> {
            Ok(self.runs.clone())
        }

        async fn list_store_runs_by_parent(&self, _parent_id: RunId) -> anyhow::Result<Vec<Run>> {
            unsupported()
        }

        async fn link_run_parent(
            &self,
            _child_id: &RunId,
            _parent_id: &RunId,
        ) -> anyhow::Result<Run> {
            unsupported()
        }

        async fn unlink_run_parent(&self, _child_id: &RunId) -> anyhow::Result<Run> {
            unsupported()
        }

        async fn get_run_state(&self, _run_id: &RunId) -> anyhow::Result<RunProjection> {
            unsupported()
        }

        async fn list_run_stream(
            &self,
            _run_id: &RunId,
            after: u64,
            limit: Option<usize>,
        ) -> anyhow::Result<Vec<RunStreamItem>> {
            Ok(self
                .events
                .iter()
                .filter(|event| event.stream_seq > after)
                .take(limit.unwrap_or(usize::MAX))
                .cloned()
                .collect())
        }

        async fn list_run_questions(
            &self,
            _run_id: &RunId,
        ) -> anyhow::Result<Vec<types::ApiQuestion>> {
            unsupported()
        }

        async fn submit_run_answer(
            &self,
            _run_id: &RunId,
            _question_id: &str,
            _body: types::SubmitAnswerRequest,
        ) -> anyhow::Result<()> {
            unsupported()
        }
    }

    fn query_run(id: RunId, created_offset: i64) -> Run {
        Run {
            id,
            parent_id: None,
            children_count: 0,
            title: "query test".to_string(),
            goal: "Preserve this exact goal".to_string(),
            workflow: WorkflowRef {
                slug:       Some("query-test".to_string()),
                name:       Some("QueryTest".to_string()),
                graph_name: Some("QueryGraph".to_string()),
                node_count: 1,
                edge_count: 0,
            },
            automation: None,
            repository: None,
            created_by: test_support::test_principal(),
            origin: RunOrigin::default(),
            labels: HashMap::from([("purpose".to_string(), "adapter-test".to_string())]),
            lifecycle: RunLifecycle {
                status:          RunStatus::Submitted,
                approval:        None,
                pending_control: None,
                queue_position:  None,
                error:           None,
                archived:        false,
                archived_at:     None,
            },
            sandbox: None,
            models: Vec::new(),
            source_directory: None,
            timestamps: RunTimestamps {
                created_at:    Utc.with_ymd_and_hms(2026, 11, 19, 12, 0, 0).unwrap()
                    + chrono::Duration::seconds(created_offset),
                started_at:    None,
                last_event_at: None,
                completed_at:  None,
            },
            timing: None,
            usage: types::Usage::default(),
            size: fabro_types::RunSize::default(),
            ask_fabro: fabro_types::AskFabro::default(),
            diff: None,
            pull_request: None,
            current_question: None,
            superseded_by: None,
            retried_from: None,
            links: RunLinks { web: None },
        }
    }

    fn query_services(run_count: usize, event_count: usize) -> FabroRunToolServices {
        let runs = (0..run_count.max(1))
            .map(|index| query_run(RunId::new(), index as i64))
            .collect::<Vec<_>>();
        let run_id = runs[0].id;
        let events = (1..=event_count)
            .map(|sequence| RunStreamItem {
                run_id,
                stream_seq: sequence as u64,
                kind: RunStreamItemKind::Petri,
                id: format!("coordinator/{sequence}/0"),
                recorded_at: 1_795_088_400_000 + sequence as u64,
                item: json!({
                    "record": {
                        "body": {
                            "event": "stage.output",
                            "sequence": sequence,
                            "content": "lossless payload"
                        }
                    }
                }),
            })
            .collect();
        FabroRunToolServices {
            backend:        Arc::new(QueryBackend { runs, events }),
            current_run_id: run_id,
        }
    }

    fn receipt(output: &str) -> Value {
        serde_json::from_str(output.split_once('\n').expect("summary and receipt").1)
            .expect("receipt is JSON")
    }

    #[tokio::test]
    async fn scheduler_admission_is_scoped_to_root_session_gather() {
        let services = test_services();
        let admission = Arc::new(CountingAdmission::new());
        let dyn_admission: Arc<dyn GatherAdmission> = admission.clone();
        let tools = register_fabro_run_tools_with_gather_admission(&services, Some(&dyn_admission));
        let gather = tools
            .iter()
            .find(|tool| tool.definition().name == fabro_tool::FABRO_RUN_GATHER_TOOL_NAME)
            .expect("gather is registered");
        assert_eq!(gather.scheduling(), ToolScheduling::ExclusiveRound);
        assert!(
            !gather.is_inheritable(),
            "a descendant cannot release whole-stage admission"
        );
        for tool in tools
            .iter()
            .filter(|tool| tool.definition().name != fabro_tool::FABRO_RUN_GATHER_TOOL_NAME)
        {
            assert_eq!(tool.scheduling(), ToolScheduling::Concurrent);
            assert!(
                tool.is_inheritable(),
                "{} keeps ordinary subagent inheritance",
                tool.definition().name
            );
        }

        let temp_dir = tempfile::tempdir().expect("the tool runner has a workspace");
        let ordinary = tools
            .iter()
            .find(|tool| tool.definition().name == fabro_tool::FABRO_RUN_GET_TOOL_NAME)
            .expect("run get is registered");
        let ordinary_runner = ToolRunner::new(
            CodingToolSet::empty()
                .with_tool(ordinary.clone())
                .expect("the ordinary tool has a distinct name"),
            Arc::new(LocalEnvironment::new(temp_dir.path())),
        );
        let ordinary_result = ordinary_runner
            .run(
                &ToolCall::function(
                    "ordinary",
                    fabro_tool::FABRO_RUN_GET_TOOL_NAME,
                    json!({"run_id": "missing"}),
                ),
                CancellationToken::default(),
            )
            .await
            .expect("the ordinary registered tool executes");
        assert!(ordinary_result.is_error, "the missing run is refused");
        assert_eq!(
            admission.waits.load(Ordering::Relaxed),
            0,
            "an ordinary registered tool never opens scheduler admission"
        );

        let gather_runner = ToolRunner::new(
            CodingToolSet::empty()
                .with_tool(gather.clone())
                .expect("gather has a distinct name"),
            Arc::new(LocalEnvironment::new(temp_dir.path())),
        );
        let gather_call = task::spawn({
            async move {
                gather_runner
                    .run(
                        &ToolCall::function(
                            "gather",
                            fabro_tool::FABRO_RUN_GATHER_TOOL_NAME,
                            json!({
                                "run_ids": ["missing"],
                                "timeout_seconds": 0,
                                "poll_interval_seconds": 5,
                            }),
                        ),
                        CancellationToken::default(),
                    )
                    .await
            }
        });
        admission.entered.notified().await;
        assert_eq!(admission.waits.load(Ordering::Relaxed), 1);
        assert!(
            !gather_call.is_finished(),
            "the gather result waits for scheduler readmission"
        );
        admission.grant.notify_one();
        let gather_result = gather_call
            .await
            .expect("the gather task joins")
            .expect("the gather registered tool executes after readmission");
        assert!(gather_result.is_error, "the missing run is refused");
        assert_eq!(
            admission.waits.load(Ordering::Relaxed),
            1,
            "gather opens exactly one scheduler wait"
        );
    }

    #[tokio::test]
    async fn native_search_saves_exact_pages_and_keeps_default_output() {
        let temp_dir = tempfile::tempdir().expect("a workspace");
        let environment = LocalEnvironment::new(temp_dir.path());
        let services = query_services(101, 0);
        let name = fabro_tool::FABRO_RUN_SEARCH_TOOL_NAME;

        let first = execute_fabro_run_tool_with_environment(
            name,
            json!({"first": 100, "output_path": "results/search-1.json"}),
            &services,
            Some(&environment),
        )
        .await
        .expect("the first page is saved");
        let first_receipt = receipt(&first);
        assert_eq!(first_receipt["result_count"], 100);
        let first_json = environment
            .read_file_text("results/search-1.json")
            .await
            .expect("saved search page is readable");
        let first_value: Value =
            serde_json::from_str(&first_json).expect("saved search page is exact JSON");
        assert_eq!(first_value["runs"].as_array().unwrap().len(), 100);
        assert_eq!(
            first_receipt["next_cursor"], first_value["next_cursor"],
            "the receipt preserves the page cursor"
        );
        assert_eq!(
            first_receipt["sha256"],
            hex::encode(Sha256::digest(first_json.as_bytes()))
        );

        let cursor = first_value["next_cursor"].as_str().unwrap();
        let second = execute_fabro_run_tool_with_environment(
            name,
            json!({
                "first": 100,
                "after": cursor,
                "output_path": "results/search-2.json"
            }),
            &services,
            Some(&environment),
        )
        .await
        .expect("the second page is saved");
        assert_eq!(receipt(&second)["result_count"], 1);
        let final_run_id: String = serde_json::from_str::<Value>(
            &environment
                .read_file_text("results/search-2.json")
                .await
                .unwrap(),
        )
        .unwrap()["runs"][0]["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let terminal = execute_fabro_run_tool_with_environment(
            name,
            json!({
                "first": 100,
                "after": final_run_id,
                "output_path": "results/search-3.json"
            }),
            &services,
            Some(&environment),
        )
        .await
        .expect("the terminal empty page is saved");
        assert_eq!(receipt(&terminal)["result_count"], 0);
        let terminal_value: Value = serde_json::from_str(
            &environment
                .read_file_text("results/search-3.json")
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(terminal_value, json!({"runs": [], "next_cursor": null}));

        let ordinary = execute_fabro_run_tool_with_environment(
            name,
            json!({"first": 1}),
            &services,
            Some(&environment),
        )
        .await
        .expect("ordinary native calls keep returning their payload");
        assert!(ordinary.starts_with("found 1 Fabro run(s)\n"));
        assert_eq!(
            serde_json::from_str::<Value>(ordinary.split_once('\n').unwrap().1).unwrap()["runs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn native_events_save_bounded_full_wrappers_and_truncation_metadata() {
        let temp_dir = tempfile::tempdir().expect("a workspace");
        let environment = LocalEnvironment::new(temp_dir.path());
        let services = query_services(1, 201);
        let run_id = services.current_run_id;
        let output = execute_fabro_run_tool_with_environment(
            fabro_tool::FABRO_RUN_EVENTS_TOOL_NAME,
            json!({
                "action": "list",
                "run_id": run_id,
                "direction": "asc",
                "first": 200,
                "max_content_length": 262144,
                "output_path": "results/events.json"
            }),
            &services,
            Some(&environment),
        )
        .await
        .expect("the bounded event page is saved");
        let saved = environment
            .read_file_text("results/events.json")
            .await
            .expect("saved event page is readable");
        let value: Value = serde_json::from_str(&saved).expect("saved event page is exact JSON");
        assert_eq!(receipt(&output)["result_count"], 200);
        assert_eq!(value["next_cursor"], 200);
        assert_eq!(value["events"].as_array().unwrap().len(), 200);
        assert_eq!(value["events"][0]["event"]["stream_seq"], 1);
        assert_eq!(
            value["events"][0]["event"]["recorded_at"],
            1_795_088_400_001_u64
        );
        assert_eq!(value["events"][0]["truncated"], false);
        assert_eq!(
            value["events"][199]["event"]["item"]["record"]["body"]["content"],
            "lossless payload"
        );
    }

    #[tokio::test]
    async fn native_persistence_refuses_escapes_symlinks_existing_files_and_write_failures() {
        let temp_dir = tempfile::tempdir().expect("a workspace");
        let provider = HostProvider::new();
        let sandbox = provider
            .create(
                &SandboxSpec::new(SandboxSource::HostDirectory)
                    .working_directory(temp_dir.path().display().to_string()),
                None,
            )
            .await
            .expect("a host sandbox");
        let working_directory = sandbox.working_directory().to_string();
        let environment =
            SandboxEnvironment::with_platform(sandbox, working_directory, "linux", "linux test");
        let services = query_services(1, 0);
        let name = fabro_tool::FABRO_RUN_SEARCH_TOOL_NAME;

        for output_path in ["../outside.json", "/tmp/outside.json"] {
            let error = execute_fabro_run_tool_with_environment(
                name,
                json!({"output_path": output_path}),
                &services,
                Some(&environment),
            )
            .await
            .expect_err("an escaping path is refused");
            assert!(error.to_string().contains("workspace-relative"));
        }

        tokio::fs::write(temp_dir.path().join("existing.json"), "original")
            .await
            .unwrap();
        let error = execute_fabro_run_tool_with_environment(
            name,
            json!({"output_path": "existing.json"}),
            &services,
            Some(&environment),
        )
        .await
        .expect_err("an existing destination is refused");
        assert!(error.to_string().contains("new workspace file"));
        assert_eq!(
            tokio::fs::read_to_string(temp_dir.path().join("existing.json"))
                .await
                .unwrap(),
            "original"
        );

        tokio::fs::write(temp_dir.path().join("not-a-directory"), "original")
            .await
            .unwrap();
        let error = execute_fabro_run_tool_with_environment(
            name,
            json!({"output_path": "not-a-directory/result.json"}),
            &services,
            Some(&environment),
        )
        .await
        .expect_err("a persistence failure is reported");
        assert!(error.to_string().contains("non-directory"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = tempfile::tempdir().expect("an outside directory");
            symlink(outside.path(), temp_dir.path().join("linked")).unwrap();
            let error = execute_fabro_run_tool_with_environment(
                name,
                json!({"output_path": "linked/result.json"}),
                &services,
                Some(&environment),
            )
            .await
            .expect_err("a symlinked parent is refused");
            assert!(error.to_string().contains("symlink"));
            assert!(!outside.path().join("result.json").exists());
        }
    }

    #[test]
    fn output_path_is_advertised_only_by_native_tool_definitions() {
        for definition in fabro_tool::tool_definitions().iter().filter(|definition| {
            matches!(
                definition.name,
                fabro_tool::FABRO_RUN_SEARCH_TOOL_NAME | fabro_tool::FABRO_RUN_EVENTS_TOOL_NAME
            )
        }) {
            assert!(definition.parameters["properties"]["output_path"].is_null());
            let native = native_tool_definition(definition);
            assert_eq!(
                native.parameters["properties"]["output_path"]["type"],
                "string"
            );
        }
    }

    #[tokio::test]
    async fn native_run_create_submits_intent_and_enforces_current_parent() {
        let server = httpmock::MockServer::start_async().await;
        let parent_id = fabro_types::RunId::new();
        let version_id: fabro_types::WorkflowVersionId =
            fabro_types::BlobHash::new(b"registered workflow").into();
        let create = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v1/runs")
                    .json_body(json!({
                        "workflow_version_id": version_id,
                        "target": {"kind":"none"},
                        "parent_id": parent_id,
                        "args": {"auto_approve":false}
                    }));
                // Admission rejection proves the native dispatcher reached the
                // canonical API without registering or looking up a workflow.
                then.status(422).body("native admission rejection");
            })
            .await;
        let state = server
            .mock_async(|when, then| {
                when.path(format!("/api/v1/runs/{parent_id}/state"));
                then.status(500);
            })
            .await;
        let client = fabro_client::Client::new_no_proxy(&server.url("")).unwrap();
        let services = FabroRunToolServices {
            backend:        Arc::new(ClientBackend::new(Arc::new(client))),
            current_run_id: parent_id,
        };
        let name = fabro_tool::FABRO_RUN_CREATE_TOOL_NAME;
        let mut args = json!({"runs":[{
            "workflow_version_id":version_id,
            "target":{"kind":"none"},
            "args":{"auto_approve":false}
        }]});
        let error = execute_fabro_run_tool_with_environment(name, args.clone(), &services, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("native admission rejection"));
        args["runs"][0]["parent_id"] = json!(fabro_types::RunId::new());
        let error = execute_fabro_run_tool_with_environment(name, args, &services, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("match the current run"));
        create.assert_calls_async(1).await;
        state.assert_calls_async(0).await;
    }

    struct SingleGraphPackager;

    #[async_trait]
    impl WorkflowVersionPackager for SingleGraphPackager {
        async fn package(
            &self,
            source: ValidatedWorkflowVersionCreate,
        ) -> anyhow::Result<CollectedWorkflowClosure> {
            let version = WorkflowVersion::new(source.entrypoint, source.files, BTreeMap::new())?;
            let id = version.id()?;
            Ok(CollectedWorkflowClosure::from_dependency_order(id, vec![(
                id,
                ValidatedWorkflowVersion::new(version)?,
            )]))
        }
    }

    #[tokio::test]
    async fn workflow_version_native_dispatch_registers_and_returns_version() {
        let server = httpmock::MockServer::start_async().await;
        let version = WorkflowVersion::new(
            "workflow".parse().unwrap(),
            BTreeMap::from([("workflow".parse().unwrap(), "digraph W {}".into())]),
            BTreeMap::new(),
        )
        .unwrap();
        let id = version.id().unwrap();
        let upload = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/api/v1/workflow-versions")
                    .json_body_obj(&version);
                then.status(201)
                    .json_body(json!({"workflow_version_id": id}));
            })
            .await;
        let client = fabro_client::Client::new_no_proxy(&server.url("")).unwrap();
        let services = FabroRunToolServices {
            backend:        Arc::new(
                ClientBackend::new(Arc::new(client))
                    .with_workflow_version_packager(Arc::new(SingleGraphPackager)),
            ),
            current_run_id: "01KRBZW4DW0000000000000002".parse().unwrap(),
        };
        let name = fabro_tool::FABRO_WORKFLOW_VERSION_CREATE_TOOL_NAME;
        assert_eq!(register_named_fabro_run_tools(&services, &[name]).len(), 1);
        let output = execute_fabro_run_tool_with_environment(
            name,
            json!({"entrypoint":"workflow", "files":{"workflow":"digraph W {}"}}),
            &services,
            None,
        )
        .await
        .unwrap();
        let (summary, body) = output.split_once('\n').unwrap();
        assert_eq!(summary, format!("Registered workflow version {id}"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            json!({"workflow_version_id": id})
        );
        let error = execute_fabro_run_tool_with_environment(
            name,
            json!({"entrypoint":"missing", "files":{"workflow":"digraph W {}"}}),
            &services,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("not present"));
        upload.assert_calls_async(1).await;
    }
}
