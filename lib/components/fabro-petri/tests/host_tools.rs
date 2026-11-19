//! Fabro's run tools on a Petri run from this crate: `RuntimeSpec::run_tools`
//! installs the adapter as Petri's host tool capability, a workflow with one
//! agent stage runs on the real step registry against a scripted model, and the
//! stage's session gets the tools the legacy worker registers, bound to the
//! run: the model is advertised every run tool, its `fabro_run_create` call
//! reaches Fabro's API with the Petri run as the child's parent, the API's
//! answer comes back to the model, and the call is in the run's record under
//! the stage.
//!
//! Built-in Host scopes run in process without a plugin executable.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fabro_petri::host_tools::recorded::{self, ExecutionId, InvocationId};
use fabro_petri::projection::{Item, RunView};
use fabro_petri::runtime::RuntimeSpec;
use fabro_store::platform_records::{PlatformRecord, RunCreatedRecord, StoredPlatformRecord};
use fabro_tool::fabro_client::ClientBackend;
use fabro_types::{
    BlobHash, RunId, StageId, ToolCategory, ToolSource, WorkflowVersionId,
    test_support as types_support,
};
use fabro_workflow::run_tools::register_fabro_run_tools;
use fabro_workflow::services::FabroRunToolServices;
use httpmock::{Method, MockServer};
use lithos_llm::types::Request;
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedProvider, scripted_client, text_response, tool_call_response,
};
use petri_execution::events::replay_run;
use petri_execution::host::{self, HostRun};
use petri_runtime::executor::Retention;
use petri_runtime::frontend::CompileInputs;
use petri_runtime::ir::RunStatus;
use petri_runtime::{RunOptions, Runtime};
use petri_store::{Access, MemoryRunStore, RunKey, RunStore};
use serde_json::json;
use tokio::fs;

/// One agent stage on the native backend, pinned to the scripted model.
const AGENT_WORKFLOW: &str = r#"digraph Agent {
    graph [goal="Start a child run", backend="api", default_max_retries=0]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=box, prompt="Start the child run", model="test/model", max_retries=0]
    start -> work -> exit
}"#;

const AGENT_SETTINGS: &str = "_version = 1\n\n[workflow]\ngraph = \"workflow.fabro\"\n";

/// Write the agent bundle into `<root>/.fabro/workflows/agent`; the
/// workflow file.
async fn install_bundle(root: &Path) -> PathBuf {
    let bundle = root.join(".fabro").join("workflows").join("agent");
    fs::create_dir_all(&bundle)
        .await
        .expect("the bundle directory is creatable");
    fs::write(bundle.join("workflow.fabro"), AGENT_WORKFLOW)
        .await
        .expect("the workflow is writable");
    fs::write(bundle.join("workflow.toml"), AGENT_SETTINGS)
        .await
        .expect("the settings are writable");
    bundle.join("workflow.fabro")
}

/// The run tools' services over `server`, as the worker binds them: the
/// client backend and the run the tools serve.
fn services(server: &MockServer, run_id: RunId) -> FabroRunToolServices {
    let client = fabro_client::Client::new_no_proxy(&server.url("")).expect("the client builds");
    FabroRunToolServices {
        backend:        Arc::new(ClientBackend::new(Arc::new(client))),
        current_run_id: run_id,
    }
}

/// The scripted model: one `fabro_run_create` call, then a closing line.
fn scripted_model(version_id: &str) -> (lithos_llm::Client, Arc<ScriptedProvider>) {
    scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "fabro_run_create",
            "create",
            json!({
                "runs": [{
                    "workflow_version_id": version_id,
                    "target": {"kind": "none"},
                    "args": {"auto_approve": false},
                }],
            }),
        )),
        ScriptedCall::response(text_response("Asked for the child run.")),
    ])
}

/// The scripted model saves one bounded event page, then closes the stage.
fn scripted_events_model(run_id: RunId) -> (lithos_llm::Client, Arc<ScriptedProvider>) {
    scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "fabro_run_events",
            "events",
            json!({
                "action": "list",
                "run_id": run_id,
                "direction": "asc",
                "first": 200,
                "max_content_length": 262144,
                "output_path": ".fabro-observe/events-001.json",
            }),
        )),
        ScriptedCall::response(text_response("Saved the event page.")),
    ])
}

/// The runtime the worker would build for the run: the scripted model as
/// the model client and `run_tools` as the run tools.
fn runtime(
    run_dir: &Path,
    run_id: &str,
    model_client: lithos_llm::Client,
    run_tools: Option<FabroRunToolServices>,
    store: &Arc<MemoryRunStore>,
    retention: Retention,
) -> Runtime {
    let mut options = RunOptions::new(run_dir);
    options.grace = Duration::from_secs(2);
    options.retention = retention;
    options.echo = false;
    options.run_key = Some(RunKey::new(run_id));
    RuntimeSpec {
        model_client: Some(model_client),
        run_tools,
        ..RuntimeSpec::default()
    }
    .runtime(true)
    .store(Arc::clone(store) as Arc<dyn RunStore>)
    .options(options)
}

/// Lower the bundle and run it to its end.
async fn run(rt: &Runtime, workflow: &Path) {
    let lowered = rt
        .check(workflow, None, None, &CompileInputs::new())
        .expect("the workflow file loads");
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("the workflow lowers: {:?}", lowered.diagnostics));
    let report = host::run_configured(rt, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "errors: {:?}; history: {:#?}",
        report.state.errors(),
        report.state.history()
    );
}

/// The single host scope's workspace after a retained test run.
async fn workspace(run_dir: &Path) -> PathBuf {
    let mut entries = fs::read_dir(run_dir.join("scopes"))
        .await
        .expect("the run has scopes");
    let scope = entries
        .next_entry()
        .await
        .expect("the scopes directory reads")
        .expect("the run has one scope");
    assert!(
        entries
            .next_entry()
            .await
            .expect("the scopes directory reads")
            .is_none(),
        "the test run has one scope"
    );
    scope.path().join("work")
}

fn resolved_run_json(run_id: RunId) -> serde_json::Value {
    json!({
        "id": run_id,
        "title": "native result test",
        "goal": "save a native result",
        "workflow": {
            "slug": "native-result-test",
            "name": "Native result test",
            "graph_name": null
        },
        "repository": {
            "name": "repo",
            "origin_url": null,
            "provider": "unknown"
        },
        "created_by": serde_json::to_value(types_support::test_principal())
            .expect("the test principal serializes"),
        "origin": {"kind": "api"},
        "labels": {},
        "lifecycle": {
            "status": {"kind": "succeeded", "reason": "completed"},
            "pending_control": null,
            "queue_position": null,
            "error": null,
            "archived": false,
            "archived_at": null
        },
        "models": [],
        "source_directory": "/srv/repo",
        "timestamps": {
            "created_at": "2026-11-19T12:00:00Z",
            "started_at": "2026-11-19T12:00:00Z",
            "last_event_at": null,
            "completed_at": null
        },
        "timing": null,
        "usage": {"tokens": {"input": 0, "output": 0, "reasoning": 0, "cache_read": 0, "cache_write": 0}},
        "diff": null,
        "pull_request": null,
        "current_question": null,
        "superseded_by": null,
        "links": {"web": null}
    })
}

/// The tools a request advertised, as `(name, description)`.
fn advertised(request: &Request) -> Vec<(String, String)> {
    request
        .tools()
        .iter()
        .map(|tool| (tool.name.clone(), tool.description.clone()))
        .collect()
}

/// The stage's session is given every run tool the legacy worker
/// registers, by the same names and descriptions; its `fabro_run_create`
/// call reaches Fabro's API with the Petri run as the parent; the API's
/// answer reaches the model; the call is in the record under the stage.
#[tokio::test]
async fn a_petri_stage_calls_a_run_tool_bound_to_the_run() {
    let root = tempfile::tempdir().expect("a temp dir");
    let workflow = install_bundle(root.path()).await;
    let run_id = RunId::new();
    let version_id: WorkflowVersionId = BlobHash::new(b"child workflow").into();
    let server = MockServer::start_async().await;
    // Admission rejection proves the tool reached the canonical API with
    // the Petri run as the child's parent, and nothing was created.
    let create = server
        .mock_async(|when, then| {
            when.method(Method::POST)
                .path("/api/v1/runs")
                .json_body(json!({
                    "workflow_version_id": version_id,
                    "target": {"kind": "none"},
                    "parent_id": run_id,
                    "args": {"auto_approve": false},
                }));
            then.status(422).body("native admission rejection");
        })
        .await;
    let services = services(&server, run_id);
    let legacy: Vec<(String, String)> = register_fabro_run_tools(&services)
        .iter()
        .map(|tool| {
            (
                tool.definition().name.clone(),
                tool.definition().description.clone(),
            )
        })
        .collect();
    let (client, provider) = scripted_model(&version_id.to_string());
    let store = Arc::new(MemoryRunStore::new());
    let rt = runtime(
        &root.path().join("run"),
        &run_id.to_string(),
        client,
        Some(services),
        &store,
        Retention::Never,
    );

    run(&rt, &workflow).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one tool call, one closing turn");
    let tools = advertised(&requests[0]);
    assert!(!legacy.is_empty());
    for tool in &legacy {
        assert!(
            tools.contains(tool),
            "the model was advertised {tool:?} as the legacy worker registers it: {tools:?}"
        );
    }
    assert!(
        tools.iter().any(|(name, _)| name == "shell"),
        "Pebble's own tools stay: {tools:?}"
    );
    let answer = serde_json::to_string(&requests[1]).expect("the request serializes");
    assert!(
        answer.contains("native admission rejection"),
        "the model read the API's answer: {answer}"
    );
    create.assert_calls_async(1).await;

    let calls = recorded::tool_calls(store.as_ref(), &run_id.to_string(), "fabro_run_create")
        .await
        .expect("the record replays");
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].node, "work", "recorded under the stage");
    assert_eq!(calls[0].invocation, Some(InvocationId::ROOT));
    assert_eq!(calls[0].execution, Some(ExecutionId::new(0)));
    assert!(calls[0].parent_session.is_none());
    assert_eq!(calls[0].payload["is_error"], true, "{:?}", calls[0].payload);
}

/// The stage's projection lists the tools its session was offered, host
/// tools included, as the `attractor.tools` payload records them: the
/// same names the model was advertised, each with its description and
/// Pebble's source, the sub-agent tools under Pebble's category, and the
/// one tool the model called marked invoked.
#[tokio::test]
async fn the_projection_lists_the_stages_tools_and_marks_the_one_called() {
    let root = tempfile::tempdir().expect("a temp dir");
    let workflow = install_bundle(root.path()).await;
    let run_id = RunId::new();
    let version_id: WorkflowVersionId = BlobHash::new(b"child workflow").into();
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/api/v1/runs");
            then.status(422).body("native admission rejection");
        })
        .await;
    let (client, provider) = scripted_model(&version_id.to_string());
    let store = Arc::new(MemoryRunStore::new());
    let rt = runtime(
        &root.path().join("run"),
        &run_id.to_string(),
        client,
        Some(services(&server, run_id)),
        &store,
        Retention::Never,
    );

    run(&rt, &workflow).await;

    // The run's events, folded as the projector folds them: the run's
    // `run.created` record first, then Petri's events in record order.
    let logs = store
        .open(&RunKey::new(run_id.to_string()), Access::Read)
        .await
        .expect("the run opens");
    let events = replay_run(&*logs).await.expect("the record replays");
    let mut spec = types_support::test_run_spec();
    spec.run_id = run_id;
    let created = StoredPlatformRecord {
        seq:         1,
        recorded_at: 0,
        record:      PlatformRecord::RunCreated(RunCreatedRecord {
            spec,
            title: Some("Start a child run".to_string()),
            parent_id: None,
            retried_from: None,
            web_url: None,
        }),
        position:    None,
    };
    let mut view = RunView::new();
    view.fold(&Item::Platform(&created), 1);
    for (index, event) in events.iter().enumerate() {
        view.fold(&Item::Petri(event), index as u64 + 2);
    }
    let projection = view.projection().expect("the run has a projection");
    let stage = projection
        .stage(&StageId::new("work", 1))
        .expect("the agent stage is projected");

    let mut listed: Vec<(String, String)> = stage
        .agent_tools
        .iter()
        .map(|tool| (tool.name.clone(), tool.description.clone()))
        .collect();
    listed.sort();
    let requests = provider.requests();
    let mut offered = advertised(&requests[0]);
    offered.sort();
    assert_eq!(
        listed, offered,
        "the list is what the model was offered, by name and description"
    );
    let create = stage
        .agent_tools
        .iter()
        .find(|tool| tool.name == "fabro_run_create")
        .expect("the host tool is listed");
    assert!(create.invoked, "the model called it");
    assert_eq!(create.source, ToolSource::Application);
    assert_eq!(create.category, ToolCategory::Other);
    let shell = stage
        .agent_tools
        .iter()
        .find(|tool| tool.name == "shell")
        .expect("Pebble's own tool is listed");
    assert!(!shell.invoked, "the model never called it");
    assert_eq!(shell.source, ToolSource::Native);
    assert!(
        stage
            .agent_tools
            .iter()
            .any(|tool| tool.category == ToolCategory::Subagent),
        "Pebble's sub-agent tools keep their category: {:?}",
        stage
            .agent_tools
            .iter()
            .map(|tool| (&tool.name, tool.category))
            .collect::<Vec<_>>()
    );
}

/// The live Petri HostTools capability invokes the native adapter and saves
/// its lossless envelope in that run's sandbox workspace.
#[tokio::test]
async fn a_petri_stage_saves_native_run_events_in_its_sandbox() {
    let root = tempfile::tempdir().expect("a temp dir");
    let workflow = install_bundle(root.path()).await;
    let run_id = RunId::new();
    let server = MockServer::start_async().await;
    let resolve = server
        .mock_async(|when, then| {
            when.method(Method::GET)
                .path("/api/v1/runs/resolve")
                .query_param("selector", run_id.to_string());
            then.status(200).json_body(resolved_run_json(run_id));
        })
        .await;
    let events = server
        .mock_async(|when, then| {
            when.method(Method::GET)
                .path(format!("/api/v1/runs/{run_id}/events"));
            then.status(200).json_body(json!({
                "data": [{
                    "run_id": run_id,
                    "stream_seq": 1,
                    "kind": "petri",
                    "id": "coordinator/1/0",
                    "recorded_at": 1_795_088_400_001_u64,
                    "item": {
                        "record": {
                            "body": {
                                "event": "stage.output",
                                "content": "exact native event"
                            }
                        }
                    }
                }],
                "meta": {"has_more": false, "total": null},
                "event_contract_version": 3
            }));
        })
        .await;
    let (client, provider) = scripted_events_model(run_id);
    let store = Arc::new(MemoryRunStore::new());
    let run_dir = root.path().join("run");
    let rt = runtime(
        &run_dir,
        &run_id.to_string(),
        client,
        Some(services(&server, run_id)),
        &store,
        Retention::Always,
    );

    run(&rt, &workflow).await;

    events.assert_calls_async(1).await;
    resolve.assert_calls_async(1).await;
    let saved = fs::read_to_string(
        workspace(&run_dir)
            .await
            .join(".fabro-observe/events-001.json"),
    )
    .await
    .expect("the native result envelope is in the run sandbox");
    let saved: serde_json::Value =
        serde_json::from_str(&saved).expect("the native result envelope is JSON");
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["tool_name"], "fabro_run_events");
    assert_eq!(
        saved["arguments"],
        json!({
            "action": "list",
            "run_id": run_id,
            "direction": "asc",
            "first": 200,
            "max_content_length": 262144
        })
    );
    assert_eq!(saved["response"]["run_id"], run_id.to_string());
    assert_eq!(
        saved["response"]["events"][0]["event"]["item"]["record"]["body"]["content"],
        "exact native event"
    );

    let calls = recorded::tool_calls(store.as_ref(), &run_id.to_string(), "fabro_run_events")
        .await
        .expect("the record replays");
    assert_eq!(calls.len(), 1, "{calls:?}");
    let tool_output = calls[0].payload["output"]
        .as_str()
        .expect("the tool completion has output");
    assert!(tool_output.starts_with("saved Fabro run events\n"));
    assert!(!tool_output.contains("exact native event"));
    assert_eq!(
        provider.requests().len(),
        2,
        "one tool call, one closing turn"
    );
}

/// Services bound to another run give the stage no run tools: the model
/// is not advertised them, and its call is refused as an unknown tool
/// rather than parenting a child run to the wrong run.
#[tokio::test]
async fn services_for_another_run_give_the_stage_no_run_tools() {
    let root = tempfile::tempdir().expect("a temp dir");
    let workflow = install_bundle(root.path()).await;
    let run_id = RunId::new();
    let server = MockServer::start_async().await;
    let create = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/api/v1/runs");
            then.status(500);
        })
        .await;
    let services = services(&server, RunId::new());
    let (client, provider) = scripted_model("0000");
    let store = Arc::new(MemoryRunStore::new());
    let rt = runtime(
        &root.path().join("run"),
        &run_id.to_string(),
        client,
        Some(services),
        &store,
        Retention::Never,
    );

    run(&rt, &workflow).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let tools = advertised(&requests[0]);
    assert!(
        !tools.iter().any(|(name, _)| name.starts_with("fabro_")),
        "no run tool was advertised: {tools:?}"
    );
    create.assert_calls_async(0).await;
}
