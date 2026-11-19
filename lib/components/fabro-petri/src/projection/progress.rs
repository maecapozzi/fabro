//! A step's progress records folded into its stage: a command's log lines,
//! the payloads the Attractor steps emit (the prompt and its completion,
//! the fallback plan, the tools a session was offered, a parallel branch's
//! start), Pebble's coding-agent envelope, and a gate's question (VIEWS.md
//! "Agent activity", "Questions").

use chrono::{DateTime, Utc};
use fabro_types::{
    BlockedReason, CodingAgentEvent, CodingEvent, InterviewOption, InterviewQuestionRecord,
    PendingInterviewRecord, ReviewTarget, ReviewTargetKind, RunStatus, StageInferenceProjection,
    StageModelUsage, StageProjection, ToolCategory, ToolSource, ToolSummary, timing,
};
use lithos_llm::types::{ReasoningEffort, Speed, Usage};
use petri_execution::ExecutionId;
use petri_execution::events::{Parsed, RunEvent};
use petri_runtime::ir::StepEvent;
use petri_runtime::steps::QuestionReference;
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use super::model::{model_ref, split_model};
use super::{FiringKey, RunView, apply_status};
use crate::interview::question_type;

impl RunView {
    pub(super) fn fold_progress(
        &mut self,
        execution: ExecutionId,
        event: &RunEvent,
        ev: &StepEvent,
        at: DateTime<Utc>,
    ) {
        match ev {
            StepEvent::Log { line, .. } => {
                if let Some(stage) = self.stage_of(execution, event.subject.as_ref()) {
                    let output = stage.output.get_or_insert_default();
                    output.push_str(line);
                    output.push('\n');
                    stage.output_bytes = Some(output.len() as u64);
                    stage.live_streaming = Some(true);
                }
            }
            StepEvent::Artifact { .. } => {}
            StepEvent::Custom(payload) => {
                if let Some(parsed) = event.parsed() {
                    self.fold_parsed(execution, event, parsed, at);
                    return;
                }
                let progress = match Progress::deserialize(payload) {
                    Ok(progress) => progress,
                    Err(error) => {
                        debug!(error = %error, "a progress payload did not decode; skipped");
                        return;
                    }
                };
                match progress {
                    Progress::Pebble { event: envelope } => {
                        self.fold_pebble(execution, event, &envelope, at);
                    }
                    Progress::Prompt { prompt, model } => {
                        if let Some(stage) = self.stage_of(execution, event.subject.as_ref()) {
                            stage.prompt = prompt;
                            if let Some(model) = model.as_deref() {
                                let (provider, model_id) = split_model(model);
                                stage.provider_used = Some(StageModelUsage::new(
                                    StageModelUsage::MODE_PROMPT,
                                    provider.map(str::to_string),
                                    Some(model_id.to_string()),
                                ));
                                stage.model = model_ref(provider, model_id);
                            }
                        }
                    }
                    Progress::PromptCompleted { response, usage } => {
                        if let Some(stage) = self.stage_of(execution, event.subject.as_ref()) {
                            stage.response = response;
                            if let Some(usage) = usage {
                                stage.usage = usage;
                            }
                        }
                    }
                    // `routes[0]` is the original route: what the stage was
                    // asked to run on, with its request controls.
                    Progress::FallbackPlan { routes } => {
                        if let Some(stage) = self.stage_of(execution, event.subject.as_ref()) {
                            if let Some(route) = routes.into_iter().next() {
                                stage.model = model_ref(Some(&route.provider), &route.model);
                                stage.provider_used = Some(route.usage());
                            }
                        }
                    }
                    // The tools a native session was offered, once per
                    // session (VIEWS.md "Agent activity", tools available):
                    // the stage's list is the union over its sessions, by
                    // name, in the order the sessions listed them.
                    Progress::Tools { tools } => {
                        if let Some(stage) = self.stage_of(execution, event.subject.as_ref()) {
                            for tool in tools {
                                if !stage
                                    .agent_tools
                                    .iter()
                                    .any(|known| known.name == tool.name)
                                {
                                    stage.agent_tools.push(tool.summary());
                                }
                            }
                        }
                    }
                    Progress::BranchStarted {
                        invocation,
                        index,
                        occurrence,
                    } => {
                        let group = self
                            .state
                            .stages
                            .get(&FiringKey::new(execution.raw(), occurrence.firing))
                            .map(|stage| stage.stage_id.clone());
                        if let Some(group) = group {
                            self.set_invocation_branch(invocation, &group, index);
                        }
                    }
                    Progress::Other => {}
                }
            }
        }
    }

    fn fold_parsed(
        &mut self,
        execution: ExecutionId,
        event: &RunEvent,
        parsed: &Parsed,
        at: DateTime<Utc>,
    ) {
        match parsed {
            Parsed::Question { question } => {
                let Some(subject) = event.subject.as_ref() else {
                    return;
                };
                let Some(firing) = subject.firing else {
                    return;
                };
                let key = FiringKey::new(execution.raw(), firing.raw());
                let label = self.state.stages.get(&key).map_or_else(
                    || subject.node.name.to_string(),
                    |stage| stage.stage_id.to_string(),
                );
                self.state.questions.insert(question.id.clone(), key);
                let Some(projection) = self.projection.as_mut() else {
                    return;
                };
                projection
                    .pending_interviews
                    .insert(question.id.clone(), PendingInterviewRecord {
                        question:   InterviewQuestionRecord {
                            id:              question.id.clone(),
                            text:            question.text.clone(),
                            stage:           label,
                            question_type:   question_type(question),
                            options:         question
                                .options
                                .iter()
                                .map(|option| InterviewOption {
                                    key:         option.key.clone(),
                                    label:       option.label.clone(),
                                    description: option.description.clone(),
                                    preview:     option.preview.clone(),
                                })
                                .collect(),
                            allow_freeform:  question.freeform,
                            timeout_seconds: question
                                .timeout_ms
                                .map(|timeout| timeout as f64 / 1000.0),
                            context_display: question.context.clone(),
                            review_target:   question.reference.as_ref().and_then(review_target),
                        },
                        started_at: at,
                    });
                apply_status(
                    projection,
                    RunStatus::Blocked {
                        blocked_reason: BlockedReason::HumanInputRequired,
                    },
                    at,
                );
            }
            Parsed::QuestionExpired { expired } => {
                self.close_questions(Some(expired.question.as_str()), None, at);
            }
            Parsed::Note { .. } => {}
        }
    }

    /// Close one question by id, or every question of a firing, and unblock
    /// Close one question by id, or every question of a firing, and unblock
    /// the run when none is left.
    pub(super) fn close_questions(
        &mut self,
        question: Option<&str>,
        firing: Option<FiringKey>,
        at: DateTime<Utc>,
    ) {
        let closed: Vec<String> = match (question, firing) {
            (Some(question), _) => vec![question.to_string()],
            (None, Some(firing)) => self
                .state
                .questions
                .iter()
                .filter(|(_, asked_by)| **asked_by == firing)
                .map(|(id, _)| id.clone())
                .collect(),
            (None, None) => Vec::new(),
        };
        for id in &closed {
            self.state.questions.remove(id);
        }
        let Some(projection) = self.projection.as_mut() else {
            return;
        };
        for id in &closed {
            projection.pending_interviews.remove(id);
        }
        if projection.pending_interviews.is_empty()
            && matches!(projection.status, RunStatus::Blocked { .. })
        {
            apply_status(projection, RunStatus::Running, at);
        }
    }

    fn fold_pebble(
        &mut self,
        execution: ExecutionId,
        event: &RunEvent,
        envelope: &CodingAgentEvent,
        at: DateTime<Utc>,
    ) {
        let Some(stage) = self.stage_of(execution, event.subject.as_ref()) else {
            return;
        };
        let agent = stage.agent.get_or_insert_default();
        agent.apply(envelope);
        if stage.completion.is_none() {
            stage.usage = agent
                .totals
                .usage
                .saturating_add(agent.totals.descendant_usage());
        }
        // A tool the stage's list names was called, by any of its sessions.
        if let CodingEvent::ToolCallStarted { tool_name, .. } = &envelope.event {
            if let Some(tool) = stage
                .agent_tools
                .iter_mut()
                .find(|tool| tool.name == *tool_name)
            {
                tool.invoked = true;
            }
        }
        let is_root = envelope.parent_session_id.is_none();
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "pebble's event vocabulary is non-exhaustive and only some events project"
        )]
        match &envelope.event {
            CodingEvent::SessionStarted {
                provider, model, ..
            } if is_root => {
                stage.provider_used = Some(StageModelUsage::new(
                    StageModelUsage::MODE_AGENT,
                    provider.clone(),
                    model.clone(),
                ));
                if let Some(model) = model.as_deref() {
                    stage.model = model_ref(provider.as_deref(), model);
                }
            }
            CodingEvent::LlmRequestStarted { requested_model } if is_root => {
                stage.inference = Some(StageInferenceProjection {
                    session_id:        envelope.session_id.clone(),
                    started_at:        at,
                    requested_model:   requested_model.clone(),
                    first_output_at:   None,
                    first_output_kind: None,
                    retries:           0,
                });
            }
            CodingEvent::LlmFirstOutput { kind } => {
                if let Some(inference) = stage.inference.as_mut() {
                    if inference.session_id == envelope.session_id {
                        inference.first_output_at = Some(at);
                        inference.first_output_kind = Some(*kind);
                    }
                }
            }
            CodingEvent::LlmRetry { .. } => {
                if let Some(inference) = stage.inference.as_mut() {
                    if inference.session_id == envelope.session_id {
                        inference.retries = inference.retries.saturating_add(1);
                        inference.first_output_at = None;
                        inference.first_output_kind = None;
                    }
                }
            }
            CodingEvent::AssistantMessage { model, .. } => {
                if is_root {
                    if let Some(provider) = stage
                        .provider_used
                        .as_ref()
                        .and_then(|used| used.provider.as_deref())
                    {
                        stage.model = model_ref(Some(provider), model);
                    }
                }
                close_inference(stage, &envelope.session_id, at);
            }
            CodingEvent::Error { .. } | CodingEvent::RoundInterrupted { .. } => {
                close_inference(stage, &envelope.session_id, at);
            }
            CodingEvent::SessionEnded => {
                close_inference(stage, &envelope.session_id, at);
                stage.close_tool_batch_for_session(&envelope.session_id, at);
            }
            CodingEvent::ToolCallStarted { tool_call_id, .. } if is_root => {
                stage.open_tool_call(envelope.session_id.clone(), tool_call_id.clone(), at);
            }
            CodingEvent::ToolCallCompleted { tool_call_id, .. } if is_root => {
                stage.close_tool_call(&envelope.session_id, tool_call_id, at);
            }
            _ => {}
        }
    }
}

/// The `StepEvent::Custom` payloads the fold reads, by their `kind`. The
/// kinds are Petri's, and a test holds each literal to the constant the
/// Attractor steps export, so a rename there fails here rather than
/// projecting nothing. A payload of another kind, or of no kind, is
/// `Other`.
#[derive(Deserialize)]
#[serde(tag = "kind")]
enum Progress {
    /// Pebble's coding-agent envelope, forwarded by the agent step.
    #[serde(rename = "pebble")]
    Pebble { event: Box<CodingAgentEvent> },
    /// The prompt step before its first model call: the prompt, and the
    /// `provider/model` selector it runs on.
    #[serde(rename = "attractor.prompt")]
    Prompt {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        model:  Option<String>,
    },
    /// The prompt step after its last model call.
    #[serde(rename = "attractor.prompt.completed")]
    PromptCompleted {
        #[serde(default)]
        response: Option<String>,
        #[serde(default)]
        usage:    Option<Usage>,
    },
    /// A stage's fallback plan, once per stage: `routes[0]` is the
    /// original route.
    #[serde(rename = "attractor.fallback.plan")]
    FallbackPlan {
        #[serde(default)]
        routes: Vec<PlannedRoute>,
    },
    /// The tools a native session was offered, once per session.
    #[serde(rename = "attractor.tools")]
    Tools {
        #[serde(default)]
        tools: Vec<OfferedTool>,
    },
    /// A parallel branch's child started: which fork visit it belongs to,
    /// its index, and the child invocation.
    #[serde(rename = "attractor.parallel.branch.started")]
    BranchStarted {
        invocation: u64,
        index:      u32,
        occurrence: ForkOccurrence,
    },
    #[serde(other)]
    Other,
}

/// One route of a fallback plan: the provider and model, with the request
/// controls the route carries.
#[derive(Deserialize)]
struct PlannedRoute {
    provider:         String,
    model:            String,
    #[serde(default)]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    speed:            Option<Speed>,
}

impl PlannedRoute {
    /// The route as the stage's model usage: an agent route with its
    /// controls.
    fn usage(self) -> StageModelUsage {
        StageModelUsage {
            reasoning_effort: self.reasoning_effort,
            speed: self.speed,
            ..StageModelUsage::new(
                StageModelUsage::MODE_AGENT,
                Some(self.provider),
                Some(self.model),
            )
        }
    }
}

/// The fork visit a branch belongs to: the fork step's firing in the
/// branch's execution.
#[derive(Deserialize)]
struct ForkOccurrence {
    firing: u64,
}

/// One tool of an `attractor.tools` payload: the name and description as
/// recorded, Pebble's `source` as it is, and Petri's origin category
/// (`builtin`, `mcp`, `host`, `question`, `subagent`).
#[derive(Deserialize)]
struct OfferedTool {
    name:        String,
    #[serde(default)]
    description: String,
    /// Left as recorded: a source Pebble adds later still lists the tool,
    /// under the default source.
    #[serde(default)]
    source:      Value,
    #[serde(default)]
    category:    Option<String>,
}

impl OfferedTool {
    /// The tool as the stage's list carries it. Pebble's behavioural
    /// category is kept where Petri's says which (a sub-agent tool); every
    /// other tool is `other`, because the payload carries Petri's origin
    /// category, not Pebble's permission class. `invoked` starts false and
    /// flips on the session's `ToolCallStarted`.
    fn summary(self) -> ToolSummary {
        let category = match self.category.as_deref() {
            Some("subagent") => ToolCategory::Subagent,
            _ => ToolCategory::Other,
        };
        ToolSummary {
            name: self.name,
            description: self.description,
            source: serde_json::from_value::<ToolSource>(self.source).unwrap_or_default(),
            category,
            invoked: false,
        }
    }
}

/// The question's `reference` as Fabro's review target, when it is one
/// Fabro's validation admits (a `document`, or a reference without a kind,
/// with a label and an absolute HTTP URL within Fabro's limits).
fn review_target(reference: &QuestionReference) -> Option<ReviewTarget> {
    let kind = match reference.kind.as_deref() {
        Some("document") | None => ReviewTargetKind::Document,
        Some(_) => return None,
    };
    ReviewTarget::new(reference.label.clone(), reference.url.clone(), kind).ok()
}

fn close_inference(stage: &mut StageProjection, session_id: &str, at: DateTime<Utc>) {
    let open = stage
        .inference
        .as_ref()
        .is_some_and(|inference| inference.session_id == session_id);
    if !open {
        return;
    }
    if let Some(inference) = stage.inference.take() {
        stage.accumulate_inference_ms(timing::elapsed_ms(inference.started_at, at));
    }
}

#[cfg(test)]
mod tests {
    use petri_attractor_steps::{fallback, parallel, pebble, prompt};
    use serde_json::json;

    use super::*;

    /// The kinds the fold matches are the ones the Attractor steps emit.
    #[test]
    fn the_progress_kinds_are_petris() {
        let kind_of = |value: Value| -> &'static str {
            match Progress::deserialize(&value).expect("a known kind decodes") {
                Progress::Pebble { .. } => "pebble",
                Progress::Prompt { .. } => "prompt",
                Progress::PromptCompleted { .. } => "prompt.completed",
                Progress::FallbackPlan { .. } => "fallback.plan",
                Progress::Tools { .. } => "tools",
                Progress::BranchStarted { .. } => "branch.started",
                Progress::Other => "other",
            }
        };
        assert_eq!(kind_of(json!({ "kind": prompt::PROMPT_EVENT })), "prompt");
        assert_eq!(
            kind_of(json!({ "kind": prompt::COMPLETED_EVENT })),
            "prompt.completed"
        );
        assert_eq!(
            kind_of(json!({ "kind": fallback::PLAN_EVENT })),
            "fallback.plan"
        );
        assert_eq!(kind_of(json!({ "kind": pebble::tools::EVENT })), "tools");
        assert_eq!(
            kind_of(json!({
                "kind": parallel::BRANCH_STARTED_EVENT,
                "invocation": 3,
                "index": 1,
                "occurrence": { "fork": "fan", "firing": 7 },
            })),
            "branch.started"
        );
        assert_eq!(
            kind_of(json!({ "kind": parallel::BRANCH_COMPLETED_EVENT })),
            "other"
        );
    }

    /// A plan's original route carries its request controls onto the
    /// stage's model usage.
    #[test]
    fn a_fallback_plan_route_keeps_its_controls() {
        let progress = Progress::deserialize(&json!({
            "kind": fallback::PLAN_EVENT,
            "routes": [
                { "position": 0, "provider": "openai", "model": "gpt-5.4",
                  "reasoning_effort": "high", "speed": null },
                { "position": 1, "provider": "anthropic", "model": "claude" },
            ],
        }))
        .expect("the plan decodes");
        let Progress::FallbackPlan { routes } = progress else {
            panic!("not a plan");
        };
        let usage = routes.into_iter().next().expect("a route").usage();
        assert_eq!(usage.mode, StageModelUsage::MODE_AGENT);
        assert_eq!(usage.provider.as_deref(), Some("openai"));
        assert_eq!(usage.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(usage.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(usage.speed, None);
    }

    /// A tool lists under Petri's category, with its source as recorded.
    #[test]
    fn an_offered_tool_maps_to_its_summary() {
        let progress = Progress::deserialize(&json!({
            "kind": pebble::tools::EVENT,
            "tools": [
                { "name": "spawn_agent", "description": "a child", "source": "native",
                  "category": "subagent" },
                { "name": "Read", "category": "builtin" },
            ],
        }))
        .expect("the tools decode");
        let Progress::Tools { tools } = progress else {
            panic!("not a tool list");
        };
        let summaries: Vec<ToolSummary> = tools.into_iter().map(OfferedTool::summary).collect();
        assert_eq!(summaries[0].category, ToolCategory::Subagent);
        assert_eq!(summaries[1].category, ToolCategory::Other);
        assert_eq!(summaries[1].description, "");
        assert!(!summaries[0].invoked);
    }
}
