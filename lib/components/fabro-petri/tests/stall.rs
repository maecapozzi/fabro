//! Fabro installs Petri's graph-wide inactivity policy on the real adapter.

#[path = "support/slow_stream.rs"]
mod slow_stream;
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use fabro_petri::check::Launch;
use fabro_petri::engine::{self, Execution, RunRequest, RunStatus};
use fabro_petri::runtime::RuntimeSpec;
use pebble_coding_agent::test_support::{self, ScriptedCompletion, ScriptedProvider};
use petri_execution::events::{self, ViewEvent};
use petri_execution::{CancelReason, InterviewReply, InterviewRequest, Interviewer, host, inspect};
use petri_runtime::steps::Answer;
use petri_store::{Access, MemoryRunStore, OwnerId, RunKey, RunStore};
use support::{SETTINGS, Silent, admit, no_questions, run_request, wait_until};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::time;
use tokio_util::sync::CancellationToken;

const QUIET: &str = r#"work [shape=parallelogram, script="sleep 3; echo done", timeout="10s"] start -> work -> exit"#;
const ACTIVE: &str = r#"work [shape=parallelogram, script="for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do echo tick; sleep 0.2; done", timeout="10s"] start -> work -> exit"#;

fn request(
    root: &std::path::Path,
    store: Arc<MemoryRunStore>,
    body: &str,
    budget: &str,
    runtime: RuntimeSpec,
) -> RunRequest {
    request_with_max_active_duration(root, store, body, budget, None, runtime)
}

fn request_with_max_active_duration(
    root: &std::path::Path,
    store: Arc<MemoryRunStore>,
    body: &str,
    budget: &str,
    max_active_duration: Option<&str>,
    runtime: RuntimeSpec,
) -> RunRequest {
    let max_active_attribute = max_active_duration.map_or_else(String::new, |duration| {
        format!(r#", max_active_duration="{duration}""#)
    });
    let graph = format!(
        r#"digraph Stall {{
        graph [stall_timeout="{budget}"{max_active_attribute}, default_model="test/model"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        {body}
    }}"#
    );
    let graphs = admit(
        &[("workflow.fabro", &graph), ("workflow.toml", SETTINGS)],
        Launch::default(),
        &runtime,
    );
    assert_eq!(
        graphs.graph.policy.stall_timeout,
        if budget == "0s" {
            None
        } else {
            Some(Duration::from_secs(1))
        }
    );
    assert_eq!(
        graphs.graph.policy.max_active_duration,
        match max_active_duration {
            Some("0s") => None,
            Some(duration) => Some(
                duration
                    .strip_suffix('s')
                    .and_then(|seconds| seconds.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .expect("the test uses whole-second durations"),
            ),
            None => Some(Duration::from_secs(90 * 60)),
        },
        "the native policy preserves an override or supplies the public active-runtime default"
    );
    run_request(
        "stall",
        root,
        graphs,
        store,
        runtime,
        no_questions(Arc::new(Silent)),
    )
}

async fn assert_maximum_active_duration(store: &MemoryRunStore) {
    assert!(matches!(
        stall_reason(store).await,
        Some(CancelReason::MaximumActiveDuration {
            maximum_active_duration_ms: 1000,
            active_duration_ms,
        }) if active_duration_ms >= 1000
    ));
    let logs = store
        .open(&RunKey::new("stall"), Access::Read)
        .await
        .expect("the run opens for inspection");
    let events = events::replay_run(&*logs)
        .await
        .expect("the run events replay");
    assert!(events.iter().any(|event| matches!(
        event.view(),
        Some(ViewEvent::RunMaximumActiveDurationExceeded {
            maximum_active_duration_ms: 1000,
            ..
        })
    )));
}

async fn stall_reason(store: &MemoryRunStore) -> Option<CancelReason> {
    let logs = store
        .open(&RunKey::new("stall"), Access::Read)
        .await
        .expect("the run opens for inspection");
    let inspection = inspect::inspect_run(&*logs)
        .await
        .expect("the run inspects");
    inspection
        .invocations
        .iter()
        .find(|invocation| invocation.invocation == inspection.root.invocation)
        .expect("the root invocation exists")
        .cancel_reason
        .clone()
}

async fn assert_stalled(store: &MemoryRunStore) {
    assert!(
        matches!(stall_reason(store).await, Some(CancelReason::StallTimeout { stall_timeout_ms: 1000, idle_ms }) if idle_ms >= 1000)
    );
    let logs = store
        .open(&RunKey::new("stall"), Access::Read)
        .await
        .expect("the run opens for inspection");
    let events = events::replay_run(&*logs)
        .await
        .expect("the run events replay");
    assert!(events.iter().any(|event| matches!(
        event.view(),
        Some(ViewEvent::RunStalled {
            stall_timeout_ms: 1000,
            ..
        })
    )));
}

async fn assert_no_tasks() {
    let metrics = Handle::current().metrics();
    wait_until("all execution tasks to stop", || {
        metrics.num_alive_tasks() == 0
    })
    .await;
}

async fn case(body: &str, budget: &str, runtime: RuntimeSpec, expected: RunStatus) {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let request = request(root.path(), store.clone(), body, budget, runtime);
    let started = Instant::now();
    let outcome = engine::run(request)
        .await
        .expect("the adapter returns an outcome");
    assert_eq!(outcome.status, expected, "{outcome:?}");
    assert!(outcome.complete, "{outcome:?}");
    if expected == RunStatus::Cancelled {
        assert_stalled(&store).await;
        let engine::Conclusion::Failed { message, .. } = engine::conclusion(&Ok(outcome)) else {
            panic!("stall must fail");
        };
        assert!(message.contains("stall timeout: 1000 ms"), "{message}");
    } else {
        assert!(started.elapsed() > Duration::from_secs(1));
        assert_eq!(stall_reason(&store).await, None);
    }
    assert_no_tasks().await;
}

#[tokio::test]
async fn quiet_command_exhausts_the_admitted_stall_budget() {
    case(QUIET, "1s", RuntimeSpec::default(), RunStatus::Cancelled).await;
}

#[tokio::test]
async fn zero_disables_the_watchdog() {
    case(QUIET, "0s", RuntimeSpec::default(), RunStatus::Success).await;
}

#[tokio::test]
async fn command_output_refreshes_the_budget() {
    case(ACTIVE, "1s", RuntimeSpec::default(), RunStatus::Success).await;
}

fn parallel(active: bool) -> String {
    let script = if active {
        "for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do echo branch; sleep 0.2; done"
    } else {
        "sleep 3"
    };
    format!(
        r#"
        fan [shape=component]
        a [shape=parallelogram, script="{script}", timeout="10s"]
        b [shape=parallelogram, script="sleep 3", timeout="10s"]
        join [shape=tripleoctagon]
        start -> fan; fan -> a; fan -> b; a -> join; b -> join; join -> exit
    "#
    )
}

#[tokio::test]
async fn one_active_child_keeps_the_whole_parallel_run_alive() {
    case(
        &parallel(true),
        "1s",
        RuntimeSpec::default(),
        RunStatus::Success,
    )
    .await;
}

#[tokio::test]
async fn continuous_output_refreshes_inactivity_but_not_the_active_runtime_backstop() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let request = request_with_max_active_duration(
        root.path(),
        store.clone(),
        ACTIVE,
        "1s",
        Some("1s"),
        RuntimeSpec::default(),
    );
    let outcome = engine::run(request)
        .await
        .expect("the adapter returns an outcome");
    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_maximum_active_duration(&store).await;
    let engine::Conclusion::Failed { message, .. } = engine::conclusion(&Ok(outcome)) else {
        panic!("maximum active duration must fail");
    };
    assert!(
        message.contains("maximum active duration: 1000 ms"),
        "{message}"
    );
    assert_no_tasks().await;
}

#[tokio::test]
async fn zero_disables_the_active_runtime_backstop() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let request = request_with_max_active_duration(
        root.path(),
        store,
        ACTIVE,
        "0s",
        Some("0s"),
        RuntimeSpec::default(),
    );
    let outcome = engine::run(request)
        .await
        .expect("the adapter returns an outcome");
    assert_eq!(outcome.status, RunStatus::Success);
    assert_no_tasks().await;
}

#[tokio::test]
async fn quiet_parallel_children_stall() {
    case(
        &parallel(false),
        "1s",
        RuntimeSpec::default(),
        RunStatus::Cancelled,
    )
    .await;
}

#[tokio::test]
async fn a_quiet_in_flight_one_shot_prompt_can_stall() {
    let (client, provider) = test_support::client_from(
        ScriptedProvider::new(Vec::new()).completing(vec![ScriptedCompletion::Pending]),
    );
    case(r#"p [shape=tab, prompt="Answer", model="test/model", timeout="10s", project_memory=false] start -> p -> exit"#,
        "1s", RuntimeSpec { model_client: Some(client), ..RuntimeSpec::default() }, RunStatus::Cancelled).await;
    assert_eq!(provider.completion_requests().len(), 1);
}

#[tokio::test]
async fn resume_before_root_declaration_uses_the_fallback_policy() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    drop(
        store
            .open(&RunKey::new("stall"), Access::Create {
                owner: OwnerId::new("crashed"),
            })
            .await
            .unwrap(),
    );
    let mut request = request(
        root.path(),
        store.clone(),
        QUIET,
        "1s",
        RuntimeSpec::default(),
    );
    let Execution::Start(graphs) = request.execution else {
        unreachable!()
    };
    request.execution = Execution::Resume(graphs);
    let outcome = engine::run(request)
        .await
        .expect("the adapter returns an outcome");
    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_stalled(&store).await;
    assert_no_tasks().await;
}

#[tokio::test]
async fn explicit_cancellation_keeps_its_reason_and_stops_the_monitor() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let request = request(
        root.path(),
        store.clone(),
        QUIET,
        "1s",
        RuntimeSpec::default(),
    );
    request.cancel.cancel();
    let outcome = engine::run(request)
        .await
        .expect("the adapter returns an outcome");
    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_eq!(stall_reason(&store).await, Some(CancelReason::Control));
    assert_no_tasks().await;
}

#[tokio::test]
async fn pause_parks_the_clock_and_unpause_gets_a_full_budget() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let request = request(
        root.path(),
        store.clone(),
        QUIET,
        "1s",
        RuntimeSpec::default(),
    );
    let controls = request.controls.clone();
    controls.pause();
    let task = tokio::spawn(engine::run(request));
    time::sleep(Duration::from_millis(1500)).await;
    assert!(!task.is_finished(), "paused execution must not stall");
    controls.unpause().await;
    time::sleep(Duration::from_millis(600)).await;
    assert!(!task.is_finished(), "unpause restarts the whole budget");
    assert_eq!(task.await.unwrap().unwrap().status, RunStatus::Cancelled);
    assert_stalled(&store).await;
    assert_no_tasks().await;
}

struct HeldQuestion {
    asked:  Arc<Notify>,
    answer: Arc<Notify>,
}

#[async_trait::async_trait]
impl Interviewer for HeldQuestion {
    async fn reply(&self, _: InterviewRequest, _: CancellationToken) -> InterviewReply {
        self.asked.notify_one();
        self.answer.notified().await;
        InterviewReply::Answered(Answer::choice("Y"))
    }
}

#[tokio::test]
async fn pending_question_parks_the_clock_and_answer_gets_a_full_budget() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let mut request = request(
        root.path(),
        store.clone(),
        r#"
        gate [shape=hexagon, label="Go?", question_type="yes_no"]
        work [shape=parallelogram, script="sleep 3", timeout="10s"]
        start -> gate; gate -> work [label="[Y] Yes"]; work -> exit
    "#,
        "1s",
        RuntimeSpec::default(),
    );
    let asked = Arc::new(Notify::new());
    let answer = Arc::new(Notify::new());
    request.interviewer = Arc::new(HeldQuestion {
        asked:  asked.clone(),
        answer: answer.clone(),
    });
    let task = tokio::spawn(engine::run(request));
    asked.notified().await;
    time::sleep(Duration::from_millis(1500)).await;
    assert!(!task.is_finished(), "pending questions must not stall");
    answer.notify_one();
    time::sleep(Duration::from_millis(600)).await;
    assert!(!task.is_finished(), "answer restarts the whole budget");
    assert_eq!(task.await.unwrap().unwrap().status, RunStatus::Cancelled);
    assert_stalled(&store).await;
    assert_no_tasks().await;
}

#[tokio::test]
async fn streaming_model_activity_in_child_branches_refreshes_the_budget() {
    case(r#"
        fan [shape=component]
        a [shape=box, prompt="Reply briefly", model="test/model", timeout="10s", project_memory=false]
        b [shape=box, prompt="Reply briefly", model="test/model", timeout="10s", project_memory=false]
        join [shape=tripleoctagon]
        start -> fan; fan -> a; fan -> b; a -> join; b -> join; join -> exit
    "#, "1s", RuntimeSpec { model_client: Some(slow_stream::client()), ..RuntimeSpec::default() }, RunStatus::Success).await;
}

#[tokio::test]
async fn a_dropped_paused_execution_stops_its_monitor_and_resumes_parked() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let first = request(
        root.path(),
        store.clone(),
        QUIET,
        "1s",
        RuntimeSpec::default(),
    );
    first.controls.pause();
    let task = tokio::spawn(engine::run(first));
    // Wait for the coordinator to persist the pause before simulating a crash.
    time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(logs) = store.open(&RunKey::new("stall"), Access::Read).await {
                if host::stored_state(&*logs)
                    .await
                    .is_ok_and(|state| state.paused)
                {
                    break;
                }
            }
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_no_tasks().await;
    let mut next = request(
        root.path(),
        store.clone(),
        QUIET,
        "0s",
        RuntimeSpec::default(),
    );
    let Execution::Start(graphs) = next.execution else {
        unreachable!()
    };
    next.execution = Execution::Resume(graphs);
    let controls = next.controls.clone();
    let task = tokio::spawn(engine::run(next));
    wait_until("restored pause", || controls.is_paused()).await;
    time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !task.is_finished(),
        "a resumed pause must park the watchdog"
    );
    controls.unpause().await;
    time::sleep(Duration::from_millis(600)).await;
    assert!(!task.is_finished(), "resumed unpause gets a full budget");
    assert_eq!(task.await.unwrap().unwrap().status, RunStatus::Cancelled);
    assert_stalled(&store).await;
    assert_no_tasks().await;
}
