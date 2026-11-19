//! Fabro installs Petri's graph-wide inactivity policy on the real adapter.

#[path = "support/slow_stream.rs"]
mod slow_stream;
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fabro_petri::check::Launch;
use fabro_petri::controls::SchedulerAdmissionEvent;
use fabro_petri::engine::{self, Execution, RunRequest, RunStatus};
use fabro_petri::runtime::RuntimeSpec;
use pebble_coding_agent::test_support::{self, ScriptedCompletion, ScriptedProvider};
use petri_execution::events::{self, ViewEvent};
use petri_execution::{
    CancelReason, CoordinatorEvent, CoordinatorRecord, CoordinatorState, ExecutionId,
    ExecutionObserver, InterviewReply, InterviewRequest, Interviewer, host, inspect,
};
use petri_runtime::engine::{EngineState, Event, EventRecord};
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
            None => Some(Duration::from_mins(90)),
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
    async fn reply(&self, _: InterviewRequest, cancel: CancellationToken) -> InterviewReply {
        self.asked.notify_one();
        tokio::select! {
            () = self.answer.notified() => InterviewReply::Answered(Answer::choice("Y")),
            () = cancel.cancelled() => InterviewReply::Cancelled,
        }
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
async fn pending_question_parks_the_maximum_active_duration() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let mut request = request_with_max_active_duration(
        root.path(),
        store.clone(),
        r#"
        gate [shape=hexagon, label="Go?", question_type="yes_no"]
        work [shape=parallelogram, script="sleep 3", timeout="10s"]
        start -> gate; gate -> work [label="[Y] Yes"]; work -> exit
    "#,
        "0s",
        Some("1s"),
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
    assert!(
        !task.is_finished(),
        "a human wait must not consume active runtime"
    );
    answer.notify_one();
    time::sleep(Duration::from_millis(600)).await;
    assert!(
        !task.is_finished(),
        "answering gives the active work its remaining budget"
    );
    assert_eq!(task.await.unwrap().unwrap().status, RunStatus::Cancelled);
    assert_maximum_active_duration(&store).await;
    assert_no_tasks().await;
}

#[tokio::test]
async fn explicit_pause_parks_the_maximum_active_duration() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let request = request_with_max_active_duration(
        root.path(),
        store.clone(),
        QUIET,
        "0s",
        Some("1s"),
        RuntimeSpec::default(),
    );
    let controls = request.controls.clone();
    controls.pause();
    let task = tokio::spawn(engine::run(request));
    time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !task.is_finished(),
        "an explicit pause must not consume active runtime"
    );
    controls.unpause().await;
    time::sleep(Duration::from_millis(600)).await;
    assert!(
        !task.is_finished(),
        "unpause gives active work its remaining budget"
    );
    assert_eq!(task.await.unwrap().unwrap().status, RunStatus::Cancelled);
    assert_maximum_active_duration(&store).await;
    assert_no_tasks().await;
}

#[tokio::test]
async fn a_human_wait_does_not_exempt_an_active_parallel_sibling() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let mut request = request_with_max_active_duration(
        root.path(),
        store.clone(),
        r#"
        fan [shape=component]
        gate [shape=hexagon, label="Go?", question_type="yes_no"]
        active [shape=parallelogram, script="while true; do echo tick; sleep 0.2; done", timeout="10s"]
        join [shape=tripleoctagon]
        start -> fan; fan -> gate; fan -> active
        gate -> join [label="[Y] Yes"]; active -> join; join -> exit
    "#,
        "1s",
        Some("1s"),
        RuntimeSpec::default(),
    );
    let asked = Arc::new(Notify::new());
    request.interviewer = Arc::new(HeldQuestion {
        asked:  asked.clone(),
        answer: Arc::new(Notify::new()),
    });
    let task = tokio::spawn(engine::run(request));
    asked.notified().await;
    let outcome = time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the active sibling remains bounded")
        .expect("the run task joins")
        .expect("the adapter returns an outcome");
    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_maximum_active_duration(&store).await;
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
async fn restart_preserves_accumulated_active_runtime() {
    let root = tempfile::tempdir().expect("a temporary run directory");
    let store = Arc::new(MemoryRunStore::new());
    let first = request_with_max_active_duration(
        root.path(),
        store.clone(),
        ACTIVE,
        "0s",
        Some("2s"),
        RuntimeSpec::default(),
    );
    let task = tokio::spawn(engine::run(first));
    time::sleep(Duration::from_millis(1300)).await;
    assert!(
        !task.is_finished(),
        "the first lifetime should still be active"
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_no_tasks().await;

    let mut next = request_with_max_active_duration(
        root.path(),
        store.clone(),
        ACTIVE,
        "0s",
        Some("0s"),
        RuntimeSpec::default(),
    );
    let Execution::Start(graphs) = next.execution else {
        unreachable!()
    };
    next.execution = Execution::Resume(graphs);
    let resumed_at = Instant::now();
    let outcome = time::timeout(Duration::from_secs(4), engine::run(next))
        .await
        .expect("resume and timeout cleanup are bounded")
        .expect("the adapter returns an outcome");
    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert!(
        resumed_at.elapsed() < Duration::from_millis(1900),
        "resume reset accumulated active runtime: {:?}",
        resumed_at.elapsed()
    );
    assert!(
        matches!(
            stall_reason(&store).await,
            Some(CancelReason::MaximumActiveDuration {
                maximum_active_duration_ms: 2000,
                active_duration_ms,
            }) if active_duration_ms >= 2000
        ),
        "the stored policy and accounting should survive restart"
    );
    assert_no_tasks().await;
}

#[derive(Clone, Default, Debug)]
struct ResumeProbe {
    coordinator_resumed: Arc<AtomicUsize>,
    execution_resumed:   Arc<AtomicUsize>,
    resumed_live:        Arc<AtomicUsize>,
    resumed_started:     Arc<AtomicUsize>,
    step_started:        Arc<AtomicUsize>,
    run_unpaused:        Arc<AtomicUsize>,
}

impl ExecutionObserver for ResumeProbe {
    fn on_engine_record(&self, _: ExecutionId, record: &EventRecord, _: u64, _: &EngineState) {
        if matches!(record.event, Event::StepStarted { .. }) {
            self.step_started.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        if matches!(record.body, CoordinatorEvent::RunUnpaused) {
            self.run_unpaused.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn on_resumed(&self, _: &CoordinatorState) {
        self.coordinator_resumed.fetch_add(1, Ordering::SeqCst);
    }

    fn on_execution_resumed(&self, _: ExecutionId, state: &EngineState) {
        self.execution_resumed.fetch_add(1, Ordering::SeqCst);
        self.resumed_live
            .fetch_add(state.live_firings().count(), Ordering::SeqCst);
        self.resumed_started.fetch_add(
            state.live_firings().filter(|firing| firing.started).count(),
            Ordering::SeqCst,
        );
    }
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
    time::timeout(Duration::from_secs(5), assert_no_tasks())
        .await
        .expect("the dropped execution should stop all owned tasks");
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
    let admission = controls.scheduler_admission();
    let mut admission_events = controls
        .take_scheduler_admission_events()
        .expect("the test owns scheduler admission events");
    let probe = ResumeProbe::default();
    next.observers.push(Arc::new(probe.clone()));
    let mut task = tokio::spawn(engine::run(next));
    wait_until("restored pause", || controls.is_paused()).await;
    assert_eq!(
        time::timeout(Duration::from_secs(5), admission_events.recv())
            .await
            .expect("the resumed idle pause should release scheduler admission"),
        Some(SchedulerAdmissionEvent::Released)
    );
    time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !task.is_finished(),
        "a resumed pause must park the watchdog"
    );
    assert_eq!(probe.coordinator_resumed.load(Ordering::SeqCst), 1);
    assert_eq!(probe.execution_resumed.load(Ordering::SeqCst), 1);
    assert_eq!(probe.resumed_live.load(Ordering::SeqCst), 1);
    assert_eq!(probe.resumed_started.load(Ordering::SeqCst), 0);

    // Match the worker's production handshake: clear the scheduler's pause
    // state before recording the unpause, then wait for server readmission.
    admission.unpaused();
    time::timeout(Duration::from_secs(5), controls.unpause())
        .await
        .unwrap_or_else(|_| panic!("recording the resumed unpause timed out: probe={probe:?}"));
    assert_eq!(
        time::timeout(Duration::from_secs(5), admission_events.recv())
            .await
            .expect("unpause should request scheduler readmission"),
        Some(SchedulerAdmissionEvent::ReadmissionRequested)
    );
    time::sleep(Duration::from_millis(600)).await;
    assert!(
        !task.is_finished(),
        "work must stay blocked before scheduler admission"
    );
    assert_eq!(
        probe.step_started.load(Ordering::SeqCst),
        0,
        "a pending firing must not start before scheduler admission"
    );

    admission.grant();
    time::timeout(
        Duration::from_secs(5),
        wait_until("resumed step dispatch", || {
            probe.step_started.load(Ordering::SeqCst) >= 1
        }),
    )
    .await
    .unwrap_or_else(|_| panic!("scheduler grant did not dispatch resumed work: probe={probe:?}"));
    time::sleep(Duration::from_millis(600)).await;
    assert!(
        !task.is_finished(),
        "actual dispatch should receive a full inactivity budget"
    );
    let joined = time::timeout(Duration::from_secs(5), &mut task).await;
    let outcome = if let Ok(joined) = joined {
        joined.unwrap().unwrap()
    } else {
        task.abort();
        let _ = task.await;
        let logs = store
            .open(&RunKey::new("stall"), Access::Read)
            .await
            .expect("the timed-out run opens");
        let state = host::stored_state(&*logs)
            .await
            .expect("the timed-out state replays");
        panic!(
            "resumed watchdog stayed parked after actual dispatch: probe={probe:?}, \
             paused={}, timeout={:?}",
            state.paused, state.timeout
        );
    };
    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_eq!(probe.run_unpaused.load(Ordering::SeqCst), 1);
    assert_stalled(&store).await;
    time::timeout(Duration::from_secs(5), assert_no_tasks())
        .await
        .expect("timeout cleanup should stop all owned tasks");
}
