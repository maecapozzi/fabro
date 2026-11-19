use std::sync::Arc;
use std::time::{Duration, Instant};

use fabro_types::{BlockedReason, PendingReason, Run, RunStatus};
use futures::future::try_join_all;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::time;

use super::common;
use super::common::{FabroToolBackend, RunSummaryResult, ToolError, ToolResult};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FabroRunGatherParams {
    pub run_ids:               Vec<String>,
    pub timeout_seconds:       Option<u64>,
    pub poll_interval_seconds: Option<u64>,
}

#[derive(Debug)]
pub struct ValidatedGatherRuns {
    pub run_ids:               Vec<String>,
    pub timeout_seconds:       u64,
    pub poll_interval_seconds: u64,
}

impl TryFrom<FabroRunGatherParams> for ValidatedGatherRuns {
    type Error = ToolError;

    fn try_from(params: FabroRunGatherParams) -> Result<Self, Self::Error> {
        common::validate_len("run_ids", params.run_ids.len(), 1, 50)?;
        if params.timeout_seconds.is_some_and(|timeout| timeout > 600) {
            return Err(ToolError::message("timeout_seconds must be <= 600"));
        }
        if params
            .poll_interval_seconds
            .is_some_and(|interval| interval < 5)
        {
            return Err(ToolError::message("poll_interval_seconds must be >= 5"));
        }
        Ok(Self {
            run_ids:               params.run_ids,
            timeout_seconds:       params.timeout_seconds.unwrap_or(300),
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
    let deadline = start + Duration::from_secs(params.timeout_seconds);
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
        let now = Instant::now();
        if now >= deadline {
            return Ok(GatherRunsResult {
                runs:               summaries.iter().map(common::run_summary_result).collect(),
                attention_required: Vec::new(),
                timed_out:          true,
                elapsed_seconds:    start.elapsed().as_secs(),
            });
        }
        let sleep_for = Duration::from_secs(params.poll_interval_seconds).min(deadline - now);
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
    use std::collections::HashMap;

    use chrono::{TimeZone, Utc};
    use fabro_api::types::Usage;
    use fabro_types::{
        AskFabro, FailureReason, RunId, RunLifecycle, RunLinks, RunOrigin, RunSize, RunStatus,
        RunTimestamps, WorkflowRef, test_support,
    };

    use super::{GatherAttentionKind, attention_required};

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
}
