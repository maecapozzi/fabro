//! Fabro's run tools inside a Petri run: a
//! workflow that enables `[run.agent] fabro_tools` runs on Petri in the
//! worker the server launched, and the agent stage's model, the twin, calls
//! the run tools the worker registered through Petri's host tool
//! capability. The agent creates a child run from inside the Petri run; a
//! `[[run.hooks]]` hook blocks a run tool; a sub-agent calls an inherited
//! run tool. Each call is read back from Petri's record of the run, under
//! the stage it served.
//!
//! The harness is `petri.rs`'s: a foreground server on disk storage with
//! the `openai` provider repointed at the twin, its key in the vault, and
//! the run started with `fabro run --detach`. Host scopes run in process.

#![expect(
    clippy::disallowed_methods,
    reason = "these scenarios stage workspaces with sync std::fs and start a real server subprocess"
)]
#![expect(
    clippy::print_stderr,
    reason = "a scenario says where it is, and why it skipped"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fabro_petri::engine::{self, RunStatus};
use fabro_petri::host_tools::recorded::{self, ExecutionId, InvocationId, ToolCall};
use fabro_static::EnvVars;
use fabro_test::{TwinScenario, TwinScenarios, TwinToolCall, test_context, twin_openai};
use fabro_types::{WorkflowPath, WorkflowVersion};
use serde_json::{Value, json};

use super::petri::{RunningServer, run_detached_with, run_json, run_stream, wait_for_status};
use crate::support::TEST_DEV_TOKEN;

const MODEL: &str = "gpt-5.4";
/// How every scenario starts its run: approved up front, on the twin's
/// model.
const RUN_ARGS: &[&str] = &["--auto-approve", "--provider", "openai", "--model", MODEL];
const POLL: Duration = Duration::from_millis(50);
const RUN_TIMEOUT: Duration = Duration::from_mins(1);
const TIMER_DURATION_MS: u64 = 4_000;

/// What the stage asks of its agent; every request of the stage's own
/// session carries it, and no request of a sub-agent does.
const PROMPT: &str = "Work with the Fabro run tools as instructed.";
/// The task the stage hands a sub-agent; every request of the child's
/// session carries it.
const TASK: &str = "Helper: search the Fabro runs and report what you find.";

/// The child workflow the agent starts: one command stage, on Petri.
const CHILD_DOT: &str = r#"digraph Child {
    graph [goal="Run one command", default_max_retries=0]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    say [shape=parallelogram, script="echo hello from the child", max_retries=0]
    start -> say -> exit
}"#;
const CHILD_SETTINGS: &str =
    "_version = 1\n\n[workflow]\nname = \"Scheduler Child\"\ngraph = \"workflow.fabro\"\n";

/// A `[[run.hooks]]` entry that blocks every `fabro_run_search` call.
const BLOCKING_HOOK: &str = r#"
[[run.hooks]]
name = "no-run-search"
event = "pre_tool_use"
script = '''if grep -q 'fabro_run_search' "$FABRO_HOOK_CONTEXT"; then echo '{"decision":"block","reason":"run tools are not allowed here"}'; exit 2; fi'''
"#;

/// A server whose `openai` provider is the twin, keyed by `namespace`.
async fn server_on_twin(twin_base_url: &str, namespace: &str) -> RunningServer {
    RunningServer::start_with(
        &format!("\n[llm.providers.openai]\nbase_url = \"{twin_base_url}\"\n"),
        &[(EnvVars::OPENAI_API_KEY, namespace)],
    )
    .await
}

/// A workspace holding a one-stage agent workflow on Petri with the run
/// tools enabled, and `extra_settings` appended to its `workflow.toml`.
fn write_agent_workspace(context: &fabro_test::TestContext, extra_settings: &str) -> PathBuf {
    let workspace = context.temp_dir.join("tools-workspace");
    std::fs::create_dir_all(&workspace).expect("the workspace creates");
    std::fs::write(
        workspace.join("workflow.fabro"),
        format!(
            "digraph Tools {{\n  graph [goal=\"Use the run tools\", default_max_retries=0]\n  \
             start [shape=Mdiamond]\n  exit [shape=Msquare]\n  work [shape=box, \
             prompt=\"{PROMPT}\", max_retries=0]\n  start -> work -> exit\n}}\n"
        ),
    )
    .expect("the workflow writes");
    std::fs::write(
        workspace.join("workflow.toml"),
        format!(
            "_version = 1\n\n[workflow]\ngraph = \"workflow.fabro\"\n\n[run]\n\
             goal = \"Use the run tools\"\n\n[run.agent]\nfabro_tools = true\n{extra_settings}"
        ),
    )
    .expect("the settings write");
    workspace
}

fn write_timer_parent_workspace(context: &fabro_test::TestContext) -> PathBuf {
    let workspace = context.temp_dir.join("timer-parent-workspace");
    std::fs::create_dir_all(&workspace).expect("the workspace creates");
    std::fs::write(
        workspace.join("workflow.fabro"),
        format!(
            r#"digraph TimerParent {{
    graph [goal="Start an approved child while a native timer is pending", default_max_retries=0]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=box, prompt="{PROMPT}", max_retries=0]
    timer [shape=insulator, duration="{TIMER_DURATION_MS}ms"]
    done [shape=parallelogram, script="echo parent resumed", max_retries=0]
    start -> work -> timer -> done -> exit
}}
"#
        ),
    )
    .expect("the workflow writes");
    std::fs::write(
        workspace.join("workflow.toml"),
        format!(
            "_version = 1\n\n[workflow]\ngraph = \"workflow.fabro\"\n\n[run]\n\
             goal = \"Start an approved child while a native timer is pending\"\n\n\
             [run.agent]\nfabro_tools = true\n"
        ),
    )
    .expect("the settings write");
    workspace
}

/// Register the child workflow as a version through the server's API; its
/// id, for the agent's `fabro_run_create` call.
async fn register_child_version(server: &RunningServer) -> String {
    let path = |name: &str| WorkflowPath::new(name).expect("the fixture path is valid");
    let version = WorkflowVersion::new(
        path("workflow.fabro"),
        BTreeMap::from([
            (path("workflow.fabro"), CHILD_DOT.to_string()),
            (path("workflow.toml"), CHILD_SETTINGS.to_string()),
        ]),
        BTreeMap::new(),
    )
    .expect("the child version is valid");
    let response = fabro_test::test_http_client()
        .post(format!("{}/api/v1/workflow-versions", server.api_base_url))
        .bearer_auth(TEST_DEV_TOKEN)
        .json(&version)
        .send()
        .await
        .expect("the registration sends");
    let body = fabro_test::expect_reqwest_json(
        response,
        fabro_http::StatusCode::CREATED,
        "POST /api/v1/workflow-versions",
    )
    .await;
    body["workflow_version_id"]
        .as_str()
        .expect("the registration names the version")
        .to_string()
}

fn run_stage_scenario() -> TwinScenario {
    TwinScenario::responses(MODEL).input_contains(PROMPT)
}

fn child_scenario() -> TwinScenario {
    TwinScenario::responses(MODEL).input_contains(TASK)
}

/// The run's Petri outcome: succeeded and whole, or the test says why not.
async fn assert_petri_succeeded(server: &RunningServer, run_id: &str) {
    let store = server.petri_store().await;
    let outcome = engine::outcome_of(&store, run_id)
        .await
        .expect("the run's Petri record inspects");
    assert_eq!(outcome.status, RunStatus::Success, "{outcome:?}");
    assert!(outcome.complete, "{:?}", outcome.incomplete);
}

/// Every completed call of `tool` in the run's Petri record.
async fn recorded_calls(server: &RunningServer, run_id: &str, tool: &str) -> Vec<ToolCall> {
    let store = server.petri_store().await;
    recorded::tool_calls(&store, run_id, tool)
        .await
        .expect("the run's Petri record replays")
}

/// The twin's request log for `namespace`: the input text of each request
/// the stage's agent or its sub-agents made, in order. The server's own
/// request for a run title goes to the same twin and is left out.
async fn request_inputs(twin: &fabro_test::TwinOpenAi, namespace: &str) -> Vec<String> {
    let logs = twin.request_logs(namespace).await;
    logs["requests"]
        .as_array()
        .expect("the twin request log is an array")
        .iter()
        .map(|request| {
            request["input_text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .filter(|input| input.contains(PROMPT) || input.contains(TASK))
        .collect()
}

/// Approve `run_id` as a user, through the server's API.
async fn approve_run(server: &RunningServer, run_id: &str) {
    let response = fabro_test::test_http_client()
        .post(format!(
            "{}/api/v1/runs/{run_id}/approve",
            server.api_base_url
        ))
        .bearer_auth(TEST_DEV_TOKEN)
        .send()
        .await
        .expect("the approval sends");
    fabro_test::expect_reqwest_json(
        response,
        fabro_http::StatusCode::OK,
        format!("POST /api/v1/runs/{run_id}/approve"),
    )
    .await;
}

async fn scheduler_slots_used(server: &RunningServer) -> u64 {
    run_json(server, "system/info").await["runs"]["scheduler_slots_used"]
        .as_u64()
        .expect("system info includes scheduler occupancy")
}

async fn wait_for_step_started(server: &RunningServer, run_id: &str, node: &str) -> u64 {
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        let stream = run_stream(server, run_id).await;
        if let Some(recorded_at) = stream.iter().find_map(|line| {
            (line["item"]["record"]["body"]["event"] == "step.started"
                && line["item"]["subject"]["node"]["name"] == node)
                .then(|| line["recorded_at"].as_u64())
                .flatten()
        }) {
            return recorded_at;
        }
        assert!(
            Instant::now() < deadline,
            "run {run_id} never started node {node}"
        );
        tokio::time::sleep(POLL).await;
    }
}

fn lifecycle_recorded_at(stream: &[Value], transition: &str) -> u64 {
    stream
        .iter()
        .find_map(|line| {
            (line["item"]["record"]["kind"] == "run.lifecycle"
                && line["item"]["record"]["transition"] == transition)
                .then(|| line["recorded_at"].as_u64())
                .flatten()
        })
        .unwrap_or_else(|| panic!("run stream has no {transition} lifecycle record"))
}

fn step_recorded_at(stream: &[Value], node: &str, event: &str) -> u64 {
    stream
        .iter()
        .find_map(|line| {
            (line["item"]["record"]["body"]["event"] == event
                && line["item"]["subject"]["node"]["name"] == node)
                .then(|| line["recorded_at"].as_u64())
                .flatten()
        })
        .unwrap_or_else(|| panic!("run stream has no {event} record for {node}"))
}

fn sandbox_startup_ms(stream: &[Value]) -> u64 {
    stream
        .iter()
        .find_map(|line| {
            (line["item"]["record"]["body"]["event"] == "scope.acquired")
                .then(|| line["item"]["record"]["body"]["duration_ms"].as_u64())
                .flatten()
        })
        .expect("child stream includes sandbox startup duration")
}

#[derive(Debug)]
struct TimerChildMeasurement {
    approval_wait_ms:                    u64,
    approval_request_ms:                 u64,
    runnable_admission_queue_ms:         u64,
    worker_startup_ms:                   u64,
    sandbox_startup_ms:                  u64,
    approved_to_running_ms:              u64,
    occupancy_while_parent_timer_waits:  u64,
    child_running_before_timer_finished: bool,
}

async fn measure_approved_child_during_native_timer(
    context: &fabro_test::TestContext,
    twin: &fabro_test::TwinOpenAi,
    namespace: &str,
    fabro_bin: Option<&Path>,
) -> TimerChildMeasurement {
    let settings = format!(
        "\n[llm.providers.openai]\nbase_url = \"{}\"\n\n\
         [server.scheduler]\nmax_concurrent_runs = 1\n",
        twin.base_url
    );
    let server = match fabro_bin {
        Some(fabro_bin) => {
            RunningServer::start_with_binary_and_env(
                fabro_bin,
                &settings,
                &[(EnvVars::OPENAI_API_KEY, namespace)],
                &[("FABRO_SERVER_MAX_CONCURRENT_RUNS", "1")],
            )
            .await
        }
        None => {
            RunningServer::start_with_env(&settings, &[(EnvVars::OPENAI_API_KEY, namespace)], &[(
                "FABRO_SERVER_MAX_CONCURRENT_RUNS",
                "1",
            )])
            .await
        }
    };
    let child_version = register_child_version(&server).await;
    let child_workspace = context.temp_dir.join("timer-child-workspace");
    std::fs::create_dir_all(&child_workspace).expect("the child workspace creates");
    TwinScenarios::new(namespace.to_string())
        .scenario(run_stage_scenario().tool_call(TwinToolCall::new(
            "fabro_run_create",
            json!({
                "runs": [{
                    "workflow_version_id": child_version,
                    "target": {"kind": "folder", "path": child_workspace},
                    "environment_id": "local",
                    "args": {"auto_approve": true},
                }],
            }),
        )))
        .scenario(run_stage_scenario().text("The child is waiting for approval."))
        .load(twin)
        .await;
    let workspace = write_timer_parent_workspace(context);
    let parent_id = run_detached_with(context, &server, &workspace, RUN_ARGS);

    let children = wait_for_children(&server, &parent_id).await;
    assert_eq!(children.len(), 1, "one child run: {children:?}");
    let child_id = children[0]["id"]
        .as_str()
        .expect("the child run has an id")
        .to_string();
    assert_eq!(
        children[0]["lifecycle"]["status"]["reason"], "approval_required",
        "the child must not run before explicit approval"
    );
    let _timer_started_at = wait_for_step_started(&server, &parent_id, "timer").await;
    let occupancy_while_parent_timer_waits = scheduler_slots_used(&server).await;

    let approval_started = Instant::now();
    approve_run(&server, &child_id).await;
    let approval_request_ms = u64::try_from(approval_started.elapsed().as_millis())
        .expect("approval request duration fits u64");

    assert_eq!(
        wait_for_status(&server, &child_id, &["succeeded", "failed"]).await,
        "succeeded",
        "child: {}\nserver stderr:\n{}",
        run_json(&server, &format!("runs/{child_id}")).await,
        server.stderr_text()
    );
    assert_eq!(
        wait_for_status(&server, &parent_id, &["succeeded", "failed"]).await,
        "succeeded",
        "parent: {}\nserver stderr:\n{}",
        run_json(&server, &format!("runs/{parent_id}")).await,
        server.stderr_text()
    );

    let child_stream = run_stream(&server, &child_id).await;
    let parent_stream = run_stream(&server, &parent_id).await;
    let pending_at = lifecycle_recorded_at(&child_stream, "pending");
    let approved_at = lifecycle_recorded_at(&child_stream, "approved");
    let runnable_at = lifecycle_recorded_at(&child_stream, "runnable");
    let starting_at = lifecycle_recorded_at(&child_stream, "starting");
    let running_at = lifecycle_recorded_at(&child_stream, "running");
    let timer_finished_at = step_recorded_at(&parent_stream, "timer", "step.finished");
    let measurement = TimerChildMeasurement {
        approval_wait_ms: approved_at.saturating_sub(pending_at),
        approval_request_ms,
        runnable_admission_queue_ms: starting_at.saturating_sub(runnable_at),
        worker_startup_ms: running_at.saturating_sub(starting_at),
        sandbox_startup_ms: sandbox_startup_ms(&child_stream),
        approved_to_running_ms: running_at.saturating_sub(approved_at),
        occupancy_while_parent_timer_waits,
        child_running_before_timer_finished: running_at < timer_finished_at,
    };
    server.shutdown();
    measurement
}

/// The runs whose parent is `parent_id`.
async fn children_of(server: &RunningServer, parent_id: &str) -> Vec<Value> {
    run_json(server, &format!("runs?parent_id={parent_id}")).await["data"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

async fn wait_for_children(server: &RunningServer, parent_id: &str) -> Vec<Value> {
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        let children = children_of(server, parent_id).await;
        if !children.is_empty() {
            return children;
        }
        assert!(
            Instant::now() < deadline,
            "no child run of {parent_id} appeared"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// The agent calls `fabro_run_create` from inside the Petri run: the child
/// run is created under the Petri run as its parent and runs to its end,
/// the model reads the tool's answer, the run succeeds, and the call is in
/// Petri's record under the stage.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_starts_a_child_run_with_a_run_tool_inside_a_petri_run() {
    let context = test_context!();
    let twin = twin_openai().await;
    let namespace = format!("{}::{}", module_path!(), line!());
    let server = server_on_twin(&twin.base_url, &namespace).await;
    let child_version = register_child_version(&server).await;
    // The child runs in the local environment, which serves a folder
    // target and not a `none` one.
    let child_workspace = context.temp_dir.join("child-workspace");
    std::fs::create_dir_all(&child_workspace).expect("the child workspace creates");
    TwinScenarios::new(namespace.clone())
        .scenario(run_stage_scenario().tool_call(TwinToolCall::new(
            "fabro_run_create",
            json!({
                "runs": [{
                    "workflow_version_id": child_version,
                    "target": {"kind": "folder", "path": child_workspace},
                    "environment_id": "local",
                    "args": {"auto_approve": true},
                }],
            }),
        )))
        .scenario(run_stage_scenario().text("The child run is on its way."))
        .load(twin)
        .await;
    let workspace = write_agent_workspace(&context, "");
    let run_id = run_detached_with(&context, &server, &workspace, RUN_ARGS);

    eprintln!("run {run_id} started");
    let status = wait_for_status(&server, &run_id, &["succeeded", "failed"]).await;
    eprintln!("run {run_id} is {status}");
    let run = run_json(&server, &format!("runs/{run_id}")).await;
    assert_eq!(
        status,
        "succeeded",
        "run: {run}\nserver stderr:\n{}",
        server.stderr_text()
    );
    assert_petri_succeeded(&server, &run_id).await;

    let calls = recorded_calls(&server, &run_id, "fabro_run_create").await;
    assert_eq!(calls.len(), 1, "one call in the record: {calls:?}");
    let call = &calls[0];
    assert_eq!(call.node, "work", "recorded under the stage");
    assert_eq!(call.invocation, Some(InvocationId::ROOT));
    assert_eq!(call.execution, Some(ExecutionId::new(0)));
    assert!(
        call.parent_session.is_none(),
        "the stage's own session called"
    );
    assert_eq!(call.payload["is_error"], false, "{:?}", call.payload);

    let children = wait_for_children(&server, &run_id).await;
    assert_eq!(children.len(), 1, "one child run: {children:?}");
    let child_id = children[0]["id"]
        .as_str()
        .expect("the child run has an id")
        .to_string();
    eprintln!("child run {child_id} found");
    assert_eq!(children[0]["parent_id"], run_id, "{:?}", children[0]);
    // A run a worker creates waits for a person's approval, as it does
    // when the legacy worker's agent creates one; the test is that person.
    assert_eq!(
        children[0]["lifecycle"]["status"]["reason"], "approval_required",
        "{:?}",
        children[0]["lifecycle"]
    );
    approve_run(&server, &child_id).await;
    let child_status = wait_for_status(&server, &child_id, &["succeeded", "failed"]).await;
    eprintln!("child run {child_id} is {child_status}");
    assert_eq!(
        child_status,
        "succeeded",
        "child: {}",
        run_json(&server, &format!("runs/{child_id}")).await
    );

    let inputs = request_inputs(twin, &namespace).await;
    assert_eq!(inputs.len(), 2, "{inputs:?}");
    assert!(
        inputs[1].contains(&child_id),
        "the model read the tool's answer naming the child run: {}",
        inputs[1]
    );
    server.shutdown();
}

/// With one shared slot, a parent gathering its approved child releases the
/// slot, then waits for readmission before its agent consumes the result.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_gathering_its_child_makes_progress_with_one_scheduler_slot() {
    let context = test_context!();
    let twin = twin_openai().await;
    let namespace = format!("{}::{}", module_path!(), line!());
    let server = RunningServer::start_with_env(
        &format!(
            "\n[llm.providers.openai]\nbase_url = \"{}\"\n\n\
             [server.scheduler]\nmax_concurrent_runs = 1\n",
            twin.base_url
        ),
        &[(EnvVars::OPENAI_API_KEY, namespace.as_str())],
        &[("FABRO_SERVER_MAX_CONCURRENT_RUNS", "1")],
    )
    .await;
    let child_version = register_child_version(&server).await;
    let child_workspace = context.temp_dir.join("gather-child-workspace");
    std::fs::create_dir_all(&child_workspace).expect("the child workspace creates");
    TwinScenarios::new(namespace.clone())
        .scenario(run_stage_scenario().tool_call(TwinToolCall::new(
            "fabro_run_create",
            json!({
                "runs": [{
                    "workflow_version_id": child_version,
                    "target": {"kind": "folder", "path": child_workspace},
                    "environment_id": "local",
                    "args": {"auto_approve": true},
                }],
            }),
        )))
        .scenario(run_stage_scenario().tool_call(TwinToolCall::new(
            "fabro_run_gather",
            json!({
                "run_ids": ["Scheduler Child"],
                "timeout_seconds": 60,
                "poll_interval_seconds": 5,
            }),
        )))
        .scenario(run_stage_scenario().text("The child finished."))
        .load(twin)
        .await;
    let workspace = write_agent_workspace(&context, "");
    let parent_id = run_detached_with(&context, &server, &workspace, RUN_ARGS);

    let children = wait_for_children(&server, &parent_id).await;
    assert_eq!(children.len(), 1, "one child run: {children:?}");
    let child_id = children[0]["id"]
        .as_str()
        .expect("the child run has an id")
        .to_string();
    assert_eq!(
        children[0]["lifecycle"]["status"]["reason"], "approval_required",
        "the scheduler fix must preserve the child's approval boundary"
    );
    approve_run(&server, &child_id).await;

    let child_status = wait_for_status(&server, &child_id, &["succeeded", "failed"]).await;
    assert_eq!(
        child_status,
        "succeeded",
        "child: {}\nserver stderr:\n{}",
        run_json(&server, &format!("runs/{child_id}")).await,
        server.stderr_text()
    );
    let parent_status = wait_for_status(&server, &parent_id, &["succeeded", "failed"]).await;
    assert_eq!(
        parent_status,
        "succeeded",
        "parent: {}\nserver stderr:\n{}",
        run_json(&server, &format!("runs/{parent_id}")).await,
        server.stderr_text()
    );

    let gather_calls = recorded_calls(&server, &parent_id, "fabro_run_gather").await;
    assert_eq!(gather_calls.len(), 1, "{gather_calls:?}");
    assert_eq!(
        gather_calls[0].payload["is_error"], false,
        "{:?}",
        gather_calls[0].payload
    );
    let inputs = request_inputs(twin, &namespace).await;
    assert_eq!(inputs.len(), 3, "{inputs:?}");
    assert!(
        inputs[2].contains(&child_id),
        "the parent was readmitted and received the gathered child: {}",
        inputs[2]
    );
    server.shutdown();
}

fn measurement_json(label: &str, measurement: &TimerChildMeasurement) -> Value {
    json!({
        "label": label,
        "timer_duration_ms": TIMER_DURATION_MS,
        "approval_wait_ms": measurement.approval_wait_ms,
        "approval_request_ms": measurement.approval_request_ms,
        "runnable_admission_queue_ms": measurement.runnable_admission_queue_ms,
        "worker_startup_ms": measurement.worker_startup_ms,
        "sandbox_startup_ms": measurement.sandbox_startup_ms,
        "approved_to_running_ms": measurement.approved_to_running_ms,
        "occupancy_while_parent_timer_waits":
            measurement.occupancy_while_parent_timer_waits,
        "child_running_before_timer_finished":
            measurement.child_running_before_timer_finished,
    })
}

/// An ordinary native timer is inactive scheduler work: with the sole slot
/// released, an explicitly approved child reaches `running`, and the parent
/// later reacquires capacity for its downstream command.
#[tokio::test(flavor = "multi_thread")]
async fn approved_child_runs_during_parent_native_timer_with_one_scheduler_slot() {
    let context = test_context!();
    let twin = twin_openai().await;
    let namespace = format!("{}::{}", module_path!(), line!());

    let measurement =
        measure_approved_child_during_native_timer(&context, twin, &namespace, None).await;

    assert_eq!(
        measurement.occupancy_while_parent_timer_waits, 0,
        "a native timer must release the only scheduler slot"
    );
    assert!(
        measurement.child_running_before_timer_finished,
        "the approved child must reach running before the parent timer finishes"
    );
    eprintln!(
        "DAAP-4008 candidate measurement: {}",
        measurement_json("candidate", &measurement)
    );
}

/// Runs the same bounded native workload with an explicit Fabro binary.
///
/// Invoke this once with the exact-tree baseline and once with the candidate;
/// the emitted JSON keeps approval, scheduler queue, worker startup, sandbox
/// startup, and actual permit occupancy separate.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual identical-workload baseline/candidate measurement"]
async fn measure_approved_child_during_parent_native_timer() {
    let fabro_bin = std::env::var_os("FABRO_DAAP_4008_MEASURE_BIN")
        .map(PathBuf::from)
        .expect("FABRO_DAAP_4008_MEASURE_BIN names the isolated server binary");
    let context = test_context!();
    let twin = twin_openai().await;
    let namespace = format!("{}::{}", module_path!(), line!());

    let measurement =
        measure_approved_child_during_native_timer(&context, twin, &namespace, Some(&fabro_bin))
            .await;

    eprintln!(
        "DAAP-4008 isolated measurement: {}",
        measurement_json(&fabro_bin.display().to_string(), &measurement)
    );
}

/// A `pre_tool_use` hook from `[[run.hooks]]` blocks a run tool as it
/// blocks Pebble's: the tool never runs, the model reads the reason, the
/// run goes on, and Petri's record holds the hook's report and the denied
/// call.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_hook_blocks_a_run_tool_inside_a_petri_run() {
    let context = test_context!();
    let twin = twin_openai().await;
    let namespace = format!("{}::{}", module_path!(), line!());
    let server = server_on_twin(&twin.base_url, &namespace).await;
    TwinScenarios::new(namespace.clone())
        .scenario(
            run_stage_scenario()
                .tool_call(TwinToolCall::new("fabro_run_search", json!({ "first": 5 }))),
        )
        .scenario(run_stage_scenario().text("The search was refused."))
        .load(twin)
        .await;
    let workspace = write_agent_workspace(&context, BLOCKING_HOOK);
    let run_id = run_detached_with(&context, &server, &workspace, RUN_ARGS);

    let status = wait_for_status(&server, &run_id, &["succeeded", "failed"]).await;
    let run = run_json(&server, &format!("runs/{run_id}")).await;
    assert_eq!(
        status,
        "succeeded",
        "run: {run}\nserver stderr:\n{}",
        server.stderr_text()
    );
    assert_petri_succeeded(&server, &run_id).await;

    let calls = recorded_calls(&server, &run_id, "fabro_run_search").await;
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].node, "work");
    assert_eq!(calls[0].payload["is_error"], true, "{:?}", calls[0].payload);
    assert_eq!(
        calls[0].payload["error_kind"], "denied",
        "{:?}",
        calls[0].payload
    );
    assert!(
        children_of(&server, &run_id).await.is_empty(),
        "the blocked tool created nothing"
    );

    let store = server.petri_store().await;
    let reports = recorded::hook_reports(&store, &run_id, "pre_tool_use")
        .await
        .expect("the record replays");
    assert_eq!(reports.len(), 1, "one pre_tool_use report: {reports:?}");
    assert_eq!(reports[0]["node"], "work");
    let report = serde_json::to_string(&reports[0]["report"]).expect("the report serializes");
    assert!(
        report.contains("run tools are not allowed here"),
        "the report carries the block: {report}"
    );

    let inputs = request_inputs(twin, &namespace).await;
    assert_eq!(inputs.len(), 2, "{inputs:?}");
    assert!(
        inputs[1].contains("run tools are not allowed here"),
        "the model saw the block reason: {}",
        inputs[1]
    );
    server.shutdown();
}

/// A sub-agent the stage spawns inherits the run tools: the child's call
/// runs under the stage, and Petri records it under the stage naming the
/// parent session.
#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_calls_an_inherited_run_tool_inside_a_petri_run() {
    let context = test_context!();
    let twin = twin_openai().await;
    let namespace = format!("{}::{}", module_path!(), line!());
    let server = server_on_twin(&twin.base_url, &namespace).await;
    // The queue answers each request with its first unspent match: the
    // stage's requests carry `PROMPT` and the child's carry `TASK`, so the
    // stage spawns, then waits, then finishes, while the child searches
    // and reports, whichever order the two sessions ask in.
    TwinScenarios::new(namespace.clone())
        .scenario(
            run_stage_scenario()
                .tool_call(TwinToolCall::new("spawn_agent", json!({ "task": TASK }))),
        )
        .scenario(
            child_scenario()
                .tool_call(TwinToolCall::new("fabro_run_search", json!({ "first": 5 }))),
        )
        .scenario(child_scenario().text("Found the runs."))
        .scenario(run_stage_scenario().tool_call(TwinToolCall::new("wait", json!({}))))
        .scenario(run_stage_scenario().text("The helper searched."))
        .load(twin)
        .await;
    let workspace = write_agent_workspace(&context, "");
    let run_id = run_detached_with(&context, &server, &workspace, RUN_ARGS);

    let status = wait_for_status(&server, &run_id, &["succeeded", "failed"]).await;
    let run = run_json(&server, &format!("runs/{run_id}")).await;
    assert_eq!(
        status,
        "succeeded",
        "run: {run}\nserver stderr:\n{}",
        server.stderr_text()
    );
    assert_petri_succeeded(&server, &run_id).await;

    let calls = recorded_calls(&server, &run_id, "fabro_run_search").await;
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = &calls[0];
    assert_eq!(call.node, "work", "recorded under the parent stage");
    assert_eq!(call.invocation, Some(InvocationId::ROOT));
    assert!(
        call.parent_session.is_some(),
        "the child's call names its parent session: {call:?}"
    );
    assert_eq!(call.payload["is_error"], false, "{:?}", call.payload);

    let inputs = request_inputs(twin, &namespace).await;
    let child_inputs: Vec<&String> = inputs.iter().filter(|input| input.contains(TASK)).collect();
    assert_eq!(child_inputs.len(), 2, "the child asked twice: {inputs:?}");
    assert!(
        child_inputs[1].contains(&run_id),
        "the child read the search answer naming this run: {}",
        child_inputs[1]
    );
    server.shutdown();
}
