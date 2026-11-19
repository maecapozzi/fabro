use std::sync::Arc;
use std::time::Duration;

use fabro_types::{BlockedReason, PendingReason, Run, RunStatus};
use futures::future::try_join_all;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::time::{self, Instant};

use super::common;
use super::common::{FabroToolBackend, RunSummaryResult, ToolError, ToolResult};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FabroRunGatherParams {
    pub run_ids: Vec<String>,
    /// Keep observing through ordinary active states until every run is
    /// terminal or one needs operator attention. The enclosing tool/run
    /// cancellation and execution budget still apply.
    pub wait_until_terminal_or_attention: Option<bool>,
    pub timeout_seconds: Option<u64>,
    pub poll_interval_seconds: Option<u64>,
}

#[derive(Debug)]
pub struct ValidatedGatherRuns {
    pub run_ids:               Vec<String>,
    /// `None` selects terminal-or-attention observation. `Some` preserves
    /// bounded gather behavior.
    pub timeout_seconds:       Option<u64>,
    pub poll_interval_seconds: u64,
}

impl TryFrom<FabroRunGatherParams> for ValidatedGatherRuns {
    type Error = ToolError;

    fn try_from(params: FabroRunGatherParams) -> Result<Self, Self::Error> {
        common::validate_len("run_ids", params.run_ids.len(), 1, 50)?;
        if params.timeout_seconds.is_some_and(|timeout| timeout > 600) {
            return Err(ToolError::message("timeout_seconds must be <= 600"));
        }
        let wait_until_terminal_or_attention =
            params.wait_until_terminal_or_attention.unwrap_or(false);
        if wait_until_terminal_or_attention && params.timeout_seconds.is_some() {
            return Err(ToolError::message(
                "timeout_seconds cannot be set when wait_until_terminal_or_attention is true",
            ));
        }
        if params
            .poll_interval_seconds
            .is_some_and(|interval| interval < 5)
        {
            return Err(ToolError::message("poll_interval_seconds must be >= 5"));
        }
        Ok(Self {
            run_ids:               params.run_ids,
            timeout_seconds:       (!wait_until_terminal_or_attention)
                .then(|| params.timeout_seconds.unwrap_or(300)),
            poll_interval_seconds: params.poll_interval_seconds.unwrap_or(15),
        })
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GatherRunsResult {
    pub runs:               Vec<RunSummaryResult>,
    pub attention_required: Vec<GatherAttention>,
    pub timed_out:          bool,
    pub elapsed_seconds:    u64,
}

/// A child state that requires an operator before a gather can make further
/// progress. A gather returns this promptly instead of consuming its whole
/// timeout while a person must act.
#[derive(Debug, Serialize, JsonSchema)]
pub struct GatherAttention {
    pub run_id: String,
    pub kind:   GatherAttentionKind,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GatherAttentionKind {
    ApprovalRequired,
    QuestionRequired,
}

pub async fn gather_runs(
    backend: Arc<dyn FabroToolBackend>,
    params: ValidatedGatherRuns,
) -> ToolResult<GatherRunsResult> {
    let start = Instant::now();
    let deadline = params
        .timeout_seconds
        .map(|timeout| start + Duration::from_secs(timeout));
    let run_ids = try_join_all(params.run_ids.into_iter().map(|selector| {
        let backend = Arc::clone(&backend);
        async move {
            backend
                .resolve_run(&selector)
                .await
                .map(|run| run.id)
                .map_err(|err| ToolError::from_anyhow(&err))
        }
    }))
    .await?;

    loop {
        let summaries = try_join_all(run_ids.iter().map(|run_id| {
            let backend = Arc::clone(&backend);
            async move { common::retrieve_run(backend.as_ref(), run_id).await }
        }))
        .await?;
        let attention_required = attention_required(&summaries);
        if summaries
            .iter()
            .all(|run| run.lifecycle.status.is_terminal())
            || !attention_required.is_empty()
        {
            return Ok(GatherRunsResult {
                runs: summaries.iter().map(common::run_summary_result).collect(),
                attention_required,
                timed_out: false,
                elapsed_seconds: start.elapsed().as_secs(),
            });
        }
        let sleep_for = if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                return Ok(GatherRunsResult {
                    runs:               summaries.iter().map(common::run_summary_result).collect(),
                    attention_required: Vec::new(),
                    timed_out:          true,
                    elapsed_seconds:    start.elapsed().as_secs(),
                });
            }
            Duration::from_secs(params.poll_interval_seconds).min(deadline - now)
        } else {
            Duration::from_secs(params.poll_interval_seconds)
        };
        time::sleep(sleep_for).await;
    }
}

fn attention_required(runs: &[Run]) -> Vec<GatherAttention> {
    runs.iter()
        .filter_map(|run| {
            let kind = match run.lifecycle.status {
                RunStatus::Pending {
                    reason: PendingReason::ApprovalRequired,
                } => GatherAttentionKind::ApprovalRequired,
                RunStatus::Blocked {
                    blocked_reason: BlockedReason::HumanInputRequired,
                }
                | RunStatus::Paused {
                    prior_block: Some(BlockedReason::HumanInputRequired),
                } => GatherAttentionKind::QuestionRequired,
                _ => return None,
            };
            Some(GatherAttention {
                run_id: run.id.to_string(),
                kind,
            })
        })
        .collect()
}

pub fn gather_runs_text(result: &GatherRunsResult) -> String {
    format!(
        "gathered {} Fabro run(s), attention_required={}, timed_out={}",
        result.runs.len(),
        result.attention_required.len(),
        result.timed_out
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use fabro_api::types::{ApiQuestion, SubmitAnswerRequest, Usage};
    use fabro_types::{
        AskFabro, FailureReason, PairId, PairMessageRecord, PairMessageRequest, PairRecord,
        PairTranscriptResponse, RunId, RunIntent, RunLifecycle, RunLinks, RunOrigin,
        RunPairStatusResponse, RunSize, RunStatus, RunTimestamps, StageId, SuccessReason,
        WorkflowRef, test_support,
    };
    use tokio::{task, time};

    use super::{
        FabroRunGatherParams, GatherAttentionKind, ValidatedGatherRuns, attention_required,
        gather_runs,
    };
    use crate::FabroToolBackend;

    struct ScriptedBackend {
        resolved: fabro_types::Run,
        states:   Mutex<VecDeque<fabro_types::Run>>,
        reads:    AtomicUsize,
    }

    impl ScriptedBackend {
        fn new(states: impl IntoIterator<Item = fabro_types::Run>) -> Self {
            let states = states.into_iter().collect::<VecDeque<_>>();
            let resolved = states
                .front()
                .cloned()
                .expect("gather test supplies at least one run state");
            Self {
                resolved,
                states: Mutex::new(states),
                reads: AtomicUsize::new(0),
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::Relaxed)
        }
    }

    fn unexpected() -> anyhow::Error {
        anyhow::anyhow!("unexpected gather test backend call")
    }

    #[async_trait]
    impl FabroToolBackend for ScriptedBackend {
        async fn create_run_from_intent(&self, _: RunIntent) -> anyhow::Result<RunId> {
            Err(unexpected())
        }

        async fn resolve_run(&self, _: &str) -> anyhow::Result<fabro_types::Run> {
            Ok(self.resolved.clone())
        }

        async fn retrieve_run(&self, _: &RunId) -> anyhow::Result<fabro_types::Run> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let mut states = self.states.lock().expect("test state lock is not poisoned");
            if states.len() > 1 {
                Ok(states.pop_front().expect("the queue has a front"))
            } else {
                Ok(states
                    .front()
                    .cloned()
                    .expect("the queue retains its final state"))
            }
        }

        async fn start_run(&self, _: &RunId, _: bool) -> anyhow::Result<fabro_types::Run> {
            Err(unexpected())
        }

        async fn approve_run(&self, _: &RunId) -> anyhow::Result<fabro_types::Run> {
            Err(unexpected())
        }

        async fn deny_run(&self, _: &RunId, _: Option<String>) -> anyhow::Result<fabro_types::Run> {
            Err(unexpected())
        }

        async fn cancel_run(&self, _: &RunId) -> anyhow::Result<fabro_types::Run> {
            Err(unexpected())
        }

        async fn interrupt_run(&self, _: &RunId) -> anyhow::Result<()> {
            Err(unexpected())
        }

        async fn steer_run(&self, _: &RunId, _: String, _: bool) -> anyhow::Result<()> {
            Err(unexpected())
        }

        async fn archive_run(&self, _: &RunId) -> anyhow::Result<fabro_types::Run> {
            Err(unexpected())
        }

        async fn unarchive_run(&self, _: &RunId) -> anyhow::Result<fabro_types::Run> {
            Err(unexpected())
        }

        async fn list_store_runs(&self) -> anyhow::Result<Vec<fabro_types::Run>> {
            Err(unexpected())
        }

        async fn list_store_runs_by_parent(
            &self,
            _: RunId,
        ) -> anyhow::Result<Vec<fabro_types::Run>> {
            Err(unexpected())
        }

        async fn link_run_parent(&self, _: &RunId, _: &RunId) -> anyhow::Result<fabro_types::Run> {
            Err(unexpected())
        }

        async fn unlink_run_parent(&self, _: &RunId) -> anyhow::Result<fabro_types::Run> {
            Err(unexpected())
        }

        async fn get_run_state(&self, _: &RunId) -> anyhow::Result<fabro_types::RunProjection> {
            Err(unexpected())
        }

        async fn list_run_stream(
            &self,
            _: &RunId,
            _: u64,
            _: Option<usize>,
        ) -> anyhow::Result<Vec<fabro_types::RunStreamItem>> {
            Err(unexpected())
        }

        async fn list_run_questions(&self, _: &RunId) -> anyhow::Result<Vec<ApiQuestion>> {
            Err(unexpected())
        }

        async fn submit_run_answer(
            &self,
            _: &RunId,
            _: &str,
            _: SubmitAnswerRequest,
        ) -> anyhow::Result<()> {
            Err(unexpected())
        }

        async fn get_run_pair_status(&self, _: &RunId) -> anyhow::Result<RunPairStatusResponse> {
            Err(unexpected())
        }

        async fn start_run_pair(&self, _: &RunId, _: StageId) -> anyhow::Result<PairRecord> {
            Err(unexpected())
        }

        async fn get_run_pair(&self, _: &RunId, _: &PairId) -> anyhow::Result<PairRecord> {
            Err(unexpected())
        }

        async fn end_run_pair(&self, _: &RunId, _: &PairId) -> anyhow::Result<PairRecord> {
            Err(unexpected())
        }

        async fn send_run_pair_message(
            &self,
            _: &RunId,
            _: &PairId,
            _: PairMessageRequest,
        ) -> anyhow::Result<PairMessageRecord> {
            Err(unexpected())
        }

        async fn get_run_pair_transcript(
            &self,
            _: &RunId,
            _: &PairId,
            _: Option<u32>,
            _: Option<u32>,
        ) -> anyhow::Result<PairTranscriptResponse> {
            Err(unexpected())
        }
    }

    fn gather_params(timeout_seconds: u64, poll_interval_seconds: u64) -> ValidatedGatherRuns {
        ValidatedGatherRuns {
            run_ids: vec!["child".to_string()],
            timeout_seconds: Some(timeout_seconds),
            poll_interval_seconds,
        }
    }

    fn terminal_or_attention_params(poll_interval_seconds: u64) -> ValidatedGatherRuns {
        ValidatedGatherRuns {
            run_ids: vec!["child".to_string()],
            timeout_seconds: None,
            poll_interval_seconds,
        }
    }

    fn run(status: RunStatus) -> fabro_types::Run {
        fabro_types::Run {
            id:               RunId::new(),
            parent_id:        None,
            children_count:   0,
            title:            "Child".to_string(),
            goal:             "Child".to_string(),
            workflow:         WorkflowRef {
                slug:       None,
                name:       None,
                graph_name: None,
                node_count: 0,
                edge_count: 0,
            },
            automation:       None,
            repository:       None,
            created_by:       test_support::test_principal(),
            origin:           RunOrigin::default(),
            labels:           HashMap::new(),
            lifecycle:        RunLifecycle {
                status,
                approval: None,
                pending_control: None,
                queue_position: None,
                error: None,
                archived: false,
                archived_at: None,
            },
            sandbox:          None,
            models:           Vec::new(),
            source_directory: None,
            timestamps:       RunTimestamps {
                created_at:    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                started_at:    None,
                last_event_at: None,
                completed_at:  None,
            },
            timing:           None,
            usage:            Usage::default(),
            size:             RunSize::default(),
            ask_fabro:        AskFabro::default(),
            diff:             None,
            pull_request:     None,
            current_question: None,
            superseded_by:    None,
            retried_from:     None,
            links:            RunLinks { web: None },
        }
    }

    #[test]
    fn attention_returns_only_operator_blocked_children() {
        let approval = run(RunStatus::Pending {
            reason: fabro_types::PendingReason::ApprovalRequired,
        });
        let question = run(RunStatus::Blocked {
            blocked_reason: fabro_types::BlockedReason::HumanInputRequired,
        });
        let active = run(RunStatus::Running);
        let failure = run(RunStatus::Failed {
            reason: FailureReason::WorkflowError,
        });

        let attention = attention_required(&[approval, question, active, failure]);

        assert_eq!(attention.len(), 2);
        assert!(matches!(
            attention[0].kind,
            GatherAttentionKind::ApprovalRequired
        ));
        assert!(matches!(
            attention[1].kind,
            GatherAttentionKind::QuestionRequired
        ));
    }

    #[test]
    fn terminal_or_attention_wait_is_explicit_and_mutually_exclusive_with_timeout() {
        let long_wait = ValidatedGatherRuns::try_from(FabroRunGatherParams {
            run_ids: vec!["child".to_string()],
            wait_until_terminal_or_attention: Some(true),
            timeout_seconds: None,
            poll_interval_seconds: None,
        })
        .expect("the opt-in wait validates");
        assert_eq!(long_wait.timeout_seconds, None);

        let bounded = ValidatedGatherRuns::try_from(FabroRunGatherParams {
            run_ids: vec!["child".to_string()],
            wait_until_terminal_or_attention: None,
            timeout_seconds: None,
            poll_interval_seconds: None,
        })
        .expect("the default bounded wait validates");
        assert_eq!(bounded.timeout_seconds, Some(300));

        let error = ValidatedGatherRuns::try_from(FabroRunGatherParams {
            run_ids: vec!["child".to_string()],
            wait_until_terminal_or_attention: Some(true),
            timeout_seconds: Some(600),
            poll_interval_seconds: None,
        })
        .expect_err("a long wait cannot also declare a bounded timeout");
        assert!(error.as_str().contains("cannot be set"));
    }

    #[tokio::test]
    async fn gather_returns_already_terminal_success_and_failure_without_waiting() {
        for status in [
            RunStatus::Succeeded {
                reason: SuccessReason::Completed,
            },
            RunStatus::Failed {
                reason: FailureReason::WorkflowError,
            },
        ] {
            let backend = Arc::new(ScriptedBackend::new([run(status)]));

            let result = gather_runs(backend.clone(), gather_params(60, 5))
                .await
                .expect("terminal child gather succeeds");

            assert!(!result.timed_out);
            assert!(result.attention_required.is_empty());
            assert_eq!(backend.reads(), 1);
        }
    }

    #[tokio::test]
    async fn gather_returns_operator_attention_without_approving_or_answering() {
        for status in [
            RunStatus::Pending {
                reason: fabro_types::PendingReason::ApprovalRequired,
            },
            RunStatus::Blocked {
                blocked_reason: fabro_types::BlockedReason::HumanInputRequired,
            },
        ] {
            let backend = Arc::new(ScriptedBackend::new([run(status)]));

            let result = gather_runs(backend.clone(), gather_params(60, 5))
                .await
                .expect("attention state gather succeeds");

            assert!(!result.timed_out);
            assert_eq!(result.attention_required.len(), 1);
            assert_eq!(backend.reads(), 1);
        }
    }

    #[tokio::test]
    async fn gather_returns_current_state_at_a_bounded_timeout() {
        let backend = Arc::new(ScriptedBackend::new([run(RunStatus::Running)]));

        let result = gather_runs(backend.clone(), gather_params(0, 5))
            .await
            .expect("bounded gather succeeds");

        assert!(result.timed_out);
        assert!(result.attention_required.is_empty());
        assert_eq!(backend.reads(), 1);
    }

    #[tokio::test]
    async fn gather_observes_completion_that_arrives_while_parent_is_waiting() {
        let backend = Arc::new(ScriptedBackend::new([
            run(RunStatus::Running),
            run(RunStatus::Succeeded {
                reason: SuccessReason::Completed,
            }),
        ]));
        let result = gather_runs(backend.clone(), gather_params(10, 0))
            .await
            .expect("completion is returned");

        assert!(!result.timed_out);
        assert_eq!(result.runs[0].status, "succeeded");
        assert_eq!(backend.reads(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_or_attention_wait_spans_more_than_ten_minutes_without_model_polling() {
        for final_status in [
            RunStatus::Succeeded {
                reason: SuccessReason::Completed,
            },
            RunStatus::Failed {
                reason: FailureReason::WorkflowError,
            },
            RunStatus::Pending {
                reason: fabro_types::PendingReason::ApprovalRequired,
            },
            RunStatus::Blocked {
                blocked_reason: fabro_types::BlockedReason::HumanInputRequired,
            },
        ] {
            let states = std::iter::repeat_with(|| run(RunStatus::Running))
                .take(42)
                .chain(std::iter::once(run(final_status)));
            let backend = Arc::new(ScriptedBackend::new(states));

            let result = gather_runs(backend.clone(), terminal_or_attention_params(15))
                .await
                .expect("the long-lived gather reaches a meaningful state");

            assert!(
                result.elapsed_seconds > 600,
                "ordinary active observations must not end the wait at the legacy cap"
            );
            assert!(!result.timed_out);
            assert_eq!(backend.reads(), 43);
            assert!(
                result.runs[0].status != "running",
                "the result is terminal or needs operator attention"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_or_attention_wait_remains_cancellable_after_ten_minutes() {
        let backend = Arc::new(ScriptedBackend::new([run(RunStatus::Running)]));
        let started = time::Instant::now();
        let gather = task::spawn(gather_runs(
            backend.clone(),
            terminal_or_attention_params(15),
        ));
        task::yield_now().await;
        for _ in 0..41 {
            time::advance(Duration::from_secs(15)).await;
            task::yield_now().await;
        }
        assert!(started.elapsed() > Duration::from_mins(10));
        assert_eq!(backend.reads(), 42);
        assert!(
            !gather.is_finished(),
            "ordinary active state does not complete the long wait"
        );
        gather.abort();
        assert!(
            gather.await.is_err(),
            "cancelling the tool interrupts its long-lived observation"
        );
        let reads_after_cancellation = backend.reads();
        time::advance(Duration::from_secs(30)).await;
        assert_eq!(backend.reads(), reads_after_cancellation);
    }

    #[tokio::test]
    async fn interrupted_gather_stops_waiting_without_another_observation() {
        let backend = Arc::new(ScriptedBackend::new([run(RunStatus::Running)]));
        let gather = task::spawn(gather_runs(backend.clone(), gather_params(60, 5)));

        task::yield_now().await;
        gather.abort();

        assert!(
            gather.await.is_err(),
            "interrupted gather task is cancelled"
        );
        assert_eq!(backend.reads(), 1);
    }
}
