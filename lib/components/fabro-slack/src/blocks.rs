use std::fmt::Write as _;

use fabro_interview::Question;
use fabro_types::QuestionType;
use serde_json::{Value, json};

use crate::payload::{SlackActionPayload, SlackApprovalDecision, encode_action_value};

pub(crate) const ANSWER_ACTION_ID_PREFIX: &str = "interview.answer";
const MULTI_SELECT_BLOCK_ID: &str = "interview.checkboxes";
const MULTI_SELECT_ACTION_ID: &str = "interview.select";
const MULTI_SELECT_SUBMIT_ACTION_ID: &str = "interview.submit";

/// Slack section block `text.text` is documented to accept at most 3000
/// characters (Unicode scalars). Both the header section and the context
/// preview are capped against this so a pathological question, stage, URL,
/// or LLM-produced context_display can never produce an `invalid_blocks`
/// response. See https://docs.slack.dev/reference/block-kit/blocks/section-block/.
const SLACK_SECTION_TEXT_LIMIT: usize = 3000;

/// Suffix appended when `context_display` is truncated. Included in the
/// budget arithmetic so the final block is guaranteed to fit under the
/// section limit no matter how long the upstream stage's response was.
const CONTEXT_TRUNCATION_SUFFIX: &str =
    "\n…\n_(truncated; open the run in Fabro for the full context)_";

/// Suffix appended when the header text itself exceeds the section limit
/// (e.g. an extremely long question label combined with a long stage name).
const HEADER_TRUNCATION_SUFFIX: &str = " …";
const SLACK_BUTTON_TEXT_LIMIT: usize = 75;
const SLACK_FALLBACK_TEXT_LIMIT: usize = 500;
const TASK_CONTEXT_LIMIT: usize = 160;
const REPOSITORY_CONTEXT_LIMIT: usize = 300;

/// Human-facing identity supplied by the run projection. These values are
/// presentation metadata; question routing remains in the action payloads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuestionIdentity<'a> {
    pub task:             Option<&'a str>,
    pub repository:       Option<&'a str>,
    pub child_run:        bool,
    /// A child run URL verified by the server to belong to this parent.
    pub child_action_url: Option<&'a str>,
}

/// A complete Slack interview message, including the top-level text Slack
/// uses for push notifications and accessibility clients.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionMessage {
    pub fallback_text: String,
    pub blocks:        Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildNavigation {
    Approval,
    Question,
}

#[derive(Debug, Clone, Copy, Default)]
struct RoutingContext {
    child_navigation: Option<ChildNavigation>,
}

#[derive(Debug, Clone)]
struct QuestionPresentation {
    question_text: String,
    action_url:    Option<String>,
    action_label:  &'static str,
    heading:       &'static str,
    prompt_label:  &'static str,
    navigation:    bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchApprovalIdentity<'a> {
    pub task:       &'a str,
    pub repository: Option<&'a str>,
}

/// Build a Slack-unique `action_id` for an interview button.
///
/// Slack requires `action_id`s to be unique within a single message and caps
/// them at 255 characters. The selected option is carried in the button
/// `value` payload, so the `action_id` only needs to be unique — it doesn't
/// have to encode the selection. Suffixes are short, fixed-shape tokens
/// (`yes`, `no`, or the option index) to avoid any character-set or length
/// concerns when option keys are author-supplied.
fn answer_action_id(suffix: &str) -> String {
    format!("{ANSWER_ACTION_ID_PREFIX}.{suffix}")
}

fn text_block(text: &str) -> Value {
    json!({
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": text
        }
    })
}

fn button(label: &str, value: &str, action_id: &str) -> Value {
    let label = truncate_to_limit(label, SLACK_BUTTON_TEXT_LIMIT, HEADER_TRUNCATION_SUFFIX);
    json!({
        "type": "button",
        "text": {
            "type": "plain_text",
            "text": label
        },
        "value": value,
        "action_id": action_id
    })
}

fn divider() -> Value {
    json!({ "type": "divider" })
}

/// Escape Slack control characters in untrusted text. Slack treats `<…>`
/// as link/mention syntax and `&` as the escape character, so leaving them
/// raw lets an upstream LLM stage post `<!here>`, `<@U…>`, or `<#C…>`
/// payloads that ping people or surface channels. Escaping these does NOT
/// break legitimate markdown like `*bold*`, `_italic_`, `~strike~`, or
/// `` `code` `` — those characters are not escaped here on purpose so
/// formatted text (e.g. a plan summary) still renders.
/// Per https://docs.slack.dev/messaging/formatting-message-text/#escaping.
fn escape_slack_controls(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Truncate a string to at most `limit` Unicode scalars, appending `suffix`
/// when truncation occurs. `suffix` is included in the budget so the result
/// is always `<= limit` characters total.
fn truncate_to_limit(text: &str, limit: usize, suffix: &str) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let suffix_len = suffix.chars().count();
    let keep = limit.saturating_sub(suffix_len);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(suffix);
    out
}

/// Build the leading section block for an interview message: question label,
/// stage hint, and a deep link back to the run when one is available. The
/// final text is bounded by Slack's section-text limit so even pathological
/// inputs cannot produce `invalid_blocks`.
fn compact_task_context(task: &str) -> String {
    let single_line = task.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_to_limit(
        &escape_slack_controls(&single_line),
        TASK_CONTEXT_LIMIT,
        HEADER_TRUNCATION_SUFFIX,
    )
}

fn task_context_section(identity: QuestionIdentity<'_>) -> Option<Value> {
    let task = identity
        .task
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let repository = identity
        .repository
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if task.is_none() && repository.is_none() {
        return None;
    }

    let mut lines = Vec::new();
    if let Some(task) = task {
        lines.push(format!("*Task*\n{}", compact_task_context(task)));
    }
    if let Some(repository) = repository {
        lines.push(format!(
            "*Repository*\n{}",
            truncate_to_limit(
                &escape_slack_controls(repository),
                REPOSITORY_CONTEXT_LIMIT,
                HEADER_TRUNCATION_SUFFIX
            )
        ));
    }
    Some(text_block(&lines.join("\n\n")))
}

fn question_section(question: &Question, presentation: &QuestionPresentation) -> Value {
    let question_text = question.review_target.as_ref().map_or_else(
        || escape_slack_controls(&presentation.question_text),
        |target| {
            let link = slack_link(target.url(), target.label());
            target.question_text_with_link(&link)
        },
    );
    let text = format!("*{}*\n{question_text}", presentation.prompt_label);
    text_block(&truncate_to_limit(
        &text,
        SLACK_SECTION_TEXT_LIMIT,
        CONTEXT_TRUNCATION_SUFFIX,
    ))
}

fn legacy_header_section(
    question: &Question,
    identity: QuestionIdentity<'_>,
    presentation: &QuestionPresentation,
) -> Value {
    let mut lines = vec!["*Mae Factory needs your input.*".to_string()];
    let task = identity
        .task
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let repository = identity
        .repository
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (task, repository) {
        (Some(task), Some(repository)) => lines.push(format!(
            "*Task:* {}  ·  *Repository:* {}",
            escape_slack_controls(task),
            escape_slack_controls(repository)
        )),
        (Some(task), None) => lines.push(format!("*Task:* {}", escape_slack_controls(task))),
        (None, Some(repository)) => lines.push(format!(
            "*Repository:* {}",
            escape_slack_controls(repository)
        )),
        (None, None) => {}
    }
    let question_text = question.review_target.as_ref().map_or_else(
        || escape_slack_controls(&presentation.question_text),
        |target| {
            let link = slack_link(target.url(), target.label());
            target.question_text_with_link(&link)
        },
    );
    lines.push(question_text);
    if let (Some(url), label) = (
        presentation.action_url.as_deref(),
        presentation.action_label,
    ) {
        lines.push(slack_link(url, label));
    }
    text_block(&truncate_to_limit(
        &lines.join("\n\n"),
        SLACK_SECTION_TEXT_LIMIT,
        HEADER_TRUNCATION_SUFFIX,
    ))
}

/// Build a context section showing the upstream stage's response so a Slack
/// reviewer has enough information to act on the buttons without having to
/// open the run in the web UI. Slack control characters are escaped (so
/// LLM-produced content can't trigger unintended pings or channel mentions)
/// while leaving Markdown formatting intact. Truncated to fit Slack's
/// section text limit.
fn context_section(context_display: &str) -> Option<Value> {
    let trimmed = context_display.trim();
    if trimmed.is_empty() {
        return None;
    }
    if looks_like_structured_context(trimmed) {
        return None;
    }
    let neutralized = escape_slack_controls(trimmed);
    let bounded = truncate_to_limit(
        &neutralized,
        SLACK_SECTION_TEXT_LIMIT,
        CONTEXT_TRUNCATION_SUFFIX,
    );
    Some(text_block(&bounded))
}

/// Assemble the leading blocks shared by every question shape: header
/// section + optional context preview + a divider before the buttons.
fn lead_blocks(
    question: &Question,
    identity: QuestionIdentity<'_>,
    presentation: &QuestionPresentation,
) -> Vec<Value> {
    if !identity.child_run && !presentation.navigation {
        let mut blocks = vec![legacy_header_section(question, identity, presentation)];
        if let Some(context_display) = question.context_display.as_deref() {
            if let Some(section) = context_section(context_display) {
                blocks.push(section);
                blocks.push(divider());
            }
        }
        return blocks;
    }

    let mut blocks = vec![text_block(&format!("*{}*", presentation.heading))];
    if let Some(context) = task_context_section(identity) {
        blocks.push(context);
    }
    blocks.push(question_section(question, presentation));
    if let Some(context_display) = question.context_display.as_deref() {
        if let Some(section) = context_section(context_display) {
            blocks.push(section);
        }
    }
    if let Some(url) = presentation.action_url.as_deref() {
        blocks.push(text_block(&slack_link(url, presentation.action_label)));
    }
    blocks.push(divider());
    blocks
}

fn looks_like_structured_context(context: &str) -> bool {
    if context.starts_with('{') {
        return true;
    }
    if !context.starts_with('[') {
        return false;
    }
    if serde_json::from_str::<Value>(context).is_ok() {
        return true;
    }
    matches!(
        context[1..].trim_start().chars().next(),
        Some('{' | '[' | '"')
    )
}

fn routing_context(question: &Question) -> RoutingContext {
    let Some(context) = question.context_display.as_deref().map(str::trim) else {
        return RoutingContext::default();
    };
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(context) else {
        return RoutingContext::default();
    };
    let is_routing = [
        "preferred_next_label",
        "suggested_next_ids",
        "context_updates",
        "child_run_id",
        "child_run_url",
        "child_status",
        "factory_commit",
        "target_base",
    ]
    .iter()
    .any(|key| object.contains_key(*key));
    if !is_routing {
        return RoutingContext::default();
    }

    let context_updates = object
        .get("context_updates")
        .and_then(serde_json::Value::as_object);
    let has_child = ["child_run_id", "child_status"].iter().any(|key| {
        object.contains_key(*key)
            || context_updates.is_some_and(|updates| updates.contains_key(*key))
    });
    if !has_child {
        return RoutingContext::default();
    }
    let preferred_next_label = object
        .get("preferred_next_label")
        .and_then(serde_json::Value::as_str);
    let monitoring = context_updates
        .and_then(|updates| updates.get("monitoring"))
        .and_then(serde_json::Value::as_str);
    let child_navigation =
        if is_native_navigation_gate(
            question,
            "CHILD LAUNCH APPROVAL",
            "approval",
            preferred_next_label,
            monitoring,
        ) || is_legacy_approval_navigation_gate(question, preferred_next_label)
        {
            Some(ChildNavigation::Approval)
        } else if is_native_navigation_gate(
            question,
            "CHILD QUESTION",
            "question",
            preferred_next_label,
            monitoring,
        ) {
            Some(ChildNavigation::Question)
        } else {
            None
        };

    RoutingContext { child_navigation }
}

fn is_native_navigation_gate(
    question: &Question,
    gate_prefix: &str,
    expected_kind: &str,
    preferred_next_label: Option<&str>,
    monitoring: Option<&str>,
) -> bool {
    question
        .text
        .trim_start()
        .to_ascii_uppercase()
        .starts_with(gate_prefix)
        && preferred_next_label == Some(expected_kind)
        && monitoring == Some(expected_kind)
        && question.options.iter().any(|option| {
            option.key.starts_with("recheck")
                || option.label.trim().eq_ignore_ascii_case("check again")
        })
}

fn is_legacy_approval_navigation_gate(
    question: &Question,
    preferred_next_label: Option<&str>,
) -> bool {
    question
        .text
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("CHILD LAUNCH APPROVAL")
        && preferred_next_label == Some("Recheck child launch approval")
        && question
            .options
            .iter()
            .any(|option| option.key == "recheck_child")
}

fn is_parent_navigation_prompt(routing: RoutingContext) -> bool {
    routing.child_navigation.is_some()
}

#[must_use]
pub fn is_launch_navigation_question(question: &Question) -> bool {
    routing_context(question).child_navigation == Some(ChildNavigation::Approval)
}

#[must_use]
pub fn is_child_navigation_question(question: &Question) -> bool {
    routing_context(question).child_navigation.is_some()
}

fn question_presentation(
    question: &Question,
    identity: QuestionIdentity<'_>,
    run_web_url: Option<&str>,
) -> QuestionPresentation {
    let routing = routing_context(question);
    if is_parent_navigation_prompt(routing) {
        let (direct_text, fallback_text, direct_label) = match routing.child_navigation {
            Some(ChildNavigation::Approval) => (
                "Approve or deny this task, then choose Check again below.",
                "Open this task’s Children tab to approve or deny the launch, then choose Check \
                 again below.",
                "Open approval in Fabro",
            ),
            Some(ChildNavigation::Question) => (
                "Answer this task’s question, then choose Check again below.",
                "Open this task’s Children tab to answer the question, then choose Check again \
                 below.",
                "Open question in Fabro",
            ),
            None => unreachable!("parent navigation requires a classified child action"),
        };
        let (question_text, action_url, action_label) = identity.child_action_url.map_or_else(
            || {
                (
                    fallback_text.to_string(),
                    run_web_url.map(ToOwned::to_owned),
                    "Open parent task in Fabro",
                )
            },
            |child_url| {
                (
                    direct_text.to_string(),
                    Some(child_url.to_string()),
                    direct_label,
                )
            },
        );
        return QuestionPresentation {
            question_text,
            action_url,
            action_label,
            heading: match routing.child_navigation {
                Some(ChildNavigation::Approval) => "Approval needed",
                Some(ChildNavigation::Question) => "Question for you",
                None => unreachable!("parent navigation requires a classified child action"),
            },
            prompt_label: "Next step",
            navigation: true,
        };
    }

    QuestionPresentation {
        question_text: question.text.clone(),
        action_url:    run_web_url.map(ToOwned::to_owned),
        action_label:  if identity.child_run {
            "Open question in Fabro"
        } else {
            "Open this task in Fabro"
        },
        heading:       "Question for you",
        prompt_label:  "Question",
        navigation:    false,
    }
}

/// Render a native run launch approval. The run id and URL are supplied by
/// the server from the pending run projection, never from interview context.
pub fn launch_approval_message(
    run_id: &str,
    run_web_url: &str,
    identity: LaunchApprovalIdentity<'_>,
) -> QuestionMessage {
    let task = identity.task.trim();
    let repository = identity
        .repository
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let identity = QuestionIdentity {
        task: Some(task),
        repository,
        child_run: true,
        child_action_url: None,
    };
    let mut blocks = vec![text_block("*Approval needed*")];
    if let Some(context) = task_context_section(identity) {
        blocks.push(context);
    }
    blocks.extend([
        text_block("Approve to start this task, or deny to stop it."),
        json!({
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "style": "primary",
                    "text": { "type": "plain_text", "text": "Approve" },
                    "value": encode_action_value(&SlackActionPayload::Approval {
                        run_id: run_id.to_string(),
                        decision: SlackApprovalDecision::Approve,
                    }),
                    "action_id": "run.approval.approve"
                },
                {
                    "type": "button",
                    "style": "danger",
                    "text": { "type": "plain_text", "text": "Deny" },
                    "value": encode_action_value(&SlackActionPayload::Approval {
                        run_id: run_id.to_string(),
                        decision: SlackApprovalDecision::Deny,
                    }),
                    "action_id": "run.approval.deny"
                }
            ]
        }),
        text_block(&slack_link(run_web_url, "Open approval in Fabro")),
    ]);
    let subject = if task.is_empty() { "this task" } else { task };
    QuestionMessage {
        fallback_text: truncate_to_limit(
            &format!(
                "Approval needed to start {}{} — open Fabro to approve or deny.",
                escape_slack_controls(subject),
                repository.map_or_else(String::new, |repository| {
                    format!(" in {}", escape_slack_controls(repository))
                }),
            ),
            SLACK_FALLBACK_TEXT_LIMIT,
            HEADER_TRUNCATION_SUFFIX,
        ),
        blocks,
    }
}

pub fn launch_approval_decided_message(
    identity: LaunchApprovalIdentity<'_>,
    decision: SlackApprovalDecision,
    actor: Option<&str>,
    run_web_url: &str,
) -> QuestionMessage {
    let outcome = match decision {
        SlackApprovalDecision::Approve => "Approved",
        SlackApprovalDecision::Deny => "Denied",
    };
    let by = actor
        .map(str::trim)
        .filter(|actor| !actor.is_empty())
        .map_or_else(String::new, |actor| {
            format!(" by {}", escape_slack_controls(actor))
        });
    let task = escape_slack_controls(identity.task.trim());
    let context_identity = QuestionIdentity {
        task:             Some(identity.task),
        repository:       identity.repository,
        child_run:        true,
        child_action_url: None,
    };
    let mut blocks = vec![text_block(&format!("*{outcome}*"))];
    if let Some(context) = task_context_section(context_identity) {
        blocks.push(context);
    }
    if !by.is_empty() {
        blocks.push(text_block(&format!("Decision recorded{by}.")));
    }
    blocks.push(text_block(&slack_link(run_web_url, "Open task in Fabro")));
    QuestionMessage {
        fallback_text: truncate_to_limit(
            &format!("{outcome}: {task}"),
            SLACK_FALLBACK_TEXT_LIMIT,
            HEADER_TRUNCATION_SUFFIX,
        ),
        blocks,
    }
}

fn fallback_text(identity: QuestionIdentity<'_>, presentation: &QuestionPresentation) -> String {
    let mut text = String::from("Mae Factory needs your input");
    if let Some(task) = identity
        .task
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        text.push_str(" for ");
        text.push_str(&escape_slack_controls(task));
    }
    text.push_str(": ");
    text.push_str(&escape_slack_controls(&presentation.question_text));
    if presentation.action_url.is_some() {
        text.push_str(" Open in Fabro to respond.");
    }
    truncate_to_limit(&text, SLACK_FALLBACK_TEXT_LIMIT, HEADER_TRUNCATION_SUFFIX)
}

fn navigation_button_label(label: &str) -> &str {
    let normalized = label.trim().to_ascii_lowercase();
    if normalized.contains("recheck")
        || normalized.contains("check again")
        || normalized.contains("refresh")
    {
        "Check again"
    } else if normalized.contains("stop") || normalized.contains("cancel") {
        "Stop"
    } else {
        label
    }
}

fn option_descriptions_section(question: &Question) -> Option<Value> {
    let rows = question
        .options
        .iter()
        .filter_map(|option| {
            let description = option
                .description
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if description.is_none() && option.label.chars().count() <= SLACK_BUTTON_TEXT_LIMIT {
                return None;
            }
            let label = escape_slack_controls(&option.label);
            Some(description.map_or_else(
                || format!("• *{label}*"),
                |description| format!("• *{label}* — {}", escape_slack_controls(description)),
            ))
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return None;
    }
    Some(text_block(&truncate_to_limit(
        &rows.join("\n"),
        SLACK_SECTION_TEXT_LIMIT,
        HEADER_TRUNCATION_SUFFIX,
    )))
}

pub fn answered_message(
    question_text: &str,
    answer_text: &str,
    identity: QuestionIdentity<'_>,
    run_web_url: Option<&str>,
) -> QuestionMessage {
    let mut blocks = vec![text_block("*Answered*")];
    if let Some(context) = task_context_section(identity) {
        blocks.push(context);
    }
    let question = format!("*Question*\n{}", escape_slack_controls(question_text));
    blocks.push(text_block(&truncate_to_limit(
        &question,
        SLACK_SECTION_TEXT_LIMIT,
        CONTEXT_TRUNCATION_SUFFIX,
    )));
    let answer = format!("*Answer*\n{}", escape_slack_controls(answer_text));
    blocks.push(text_block(&truncate_to_limit(
        &answer,
        SLACK_SECTION_TEXT_LIMIT,
        CONTEXT_TRUNCATION_SUFFIX,
    )));
    if let Some(url) = run_web_url {
        blocks.push(text_block(&slack_link(url, "Open task in Fabro")));
    }
    QuestionMessage {
        fallback_text: truncate_to_limit(
            &format!("Answered: {}", compact_task_context(question_text)),
            SLACK_FALLBACK_TEXT_LIMIT,
            HEADER_TRUNCATION_SUFFIX,
        ),
        blocks,
    }
}

pub fn answered_blocks(question_text: &str, answer_text: &str) -> Vec<Value> {
    vec![text_block(&format!(
        "~{}~\n*Answer:* {}",
        escape_slack_controls(question_text),
        escape_slack_controls(answer_text),
    ))]
}

pub fn question_to_blocks(
    run_id: &str,
    question_id: &str,
    question: &Question,
    run_web_url: Option<&str>,
) -> Vec<Value> {
    question_message(
        run_id,
        question_id,
        question,
        run_web_url,
        QuestionIdentity::default(),
    )
    .blocks
}

pub fn question_message(
    run_id: &str,
    question_id: &str,
    question: &Question,
    run_web_url: Option<&str>,
    identity: QuestionIdentity<'_>,
) -> QuestionMessage {
    let presentation = question_presentation(question, identity, run_web_url);
    let fallback_text = fallback_text(identity, &presentation);
    let mut blocks = lead_blocks(question, identity, &presentation);
    if let Some(descriptions) = option_descriptions_section(question) {
        blocks.push(descriptions);
    }

    match question.question_type {
        QuestionType::YesNo | QuestionType::Confirmation => {
            blocks.push(json!({
                "type": "actions",
                "elements": [
                    button("Yes", &encode_action_value(&SlackActionPayload::Yes {
                        run_id: run_id.to_string(),
                        qid: question_id.to_string(),
                    }), &answer_action_id("yes")),
                    button("No", &encode_action_value(&SlackActionPayload::No {
                        run_id: run_id.to_string(),
                        qid: question_id.to_string(),
                    }), &answer_action_id("no")),
                ]
            }));
        }
        QuestionType::MultipleChoice => {
            let elements: Vec<Value> = question
                .options
                .iter()
                .enumerate()
                .map(|(idx, opt)| {
                    let label = if presentation.navigation {
                        navigation_button_label(&opt.label)
                    } else {
                        &opt.label
                    };
                    button(
                        label,
                        &encode_action_value(&SlackActionPayload::Selected {
                            run_id: run_id.to_string(),
                            qid:    question_id.to_string(),
                            key:    opt.key.clone(),
                        }),
                        &answer_action_id(&idx.to_string()),
                    )
                })
                .collect();
            blocks.push(json!({
                "type": "actions",
                "elements": elements,
            }));
        }
        QuestionType::MultiSelect => {
            let options: Vec<Value> = question
                .options
                .iter()
                .map(|opt| {
                    let mut option = json!({
                        "text": {
                            "type": "plain_text",
                            "text": truncate_to_limit(
                                &opt.label,
                                SLACK_BUTTON_TEXT_LIMIT,
                                HEADER_TRUNCATION_SUFFIX
                            )
                        },
                        "value": opt.key
                    });
                    if let Some(description) = opt
                        .description
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    {
                        option["description"] = json!({
                            "type": "plain_text",
                            "text": truncate_to_limit(
                                description,
                                75,
                                HEADER_TRUNCATION_SUFFIX
                            )
                        });
                    }
                    option
                })
                .collect();
            blocks.push(json!({
                "type": "actions",
                "block_id": MULTI_SELECT_BLOCK_ID,
                "elements": [{
                    "type": "checkboxes",
                    "action_id": MULTI_SELECT_ACTION_ID,
                    "options": options
                }]
            }));
            blocks.push(json!({
                "type": "actions",
                "elements": [
                    button("Submit", &encode_action_value(&SlackActionPayload::SubmitMulti {
                        run_id: run_id.to_string(),
                        qid: question_id.to_string(),
                    }), MULTI_SELECT_SUBMIT_ACTION_ID),
                ]
            }));
        }
        QuestionType::Freeform => {
            blocks.push(text_block("_Answer in the Fabro form._"));
        }
    }
    QuestionMessage {
        fallback_text,
        blocks,
    }
}

/// Pull request metadata rendered in run lifecycle notification messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLifecyclePullRequest<'a> {
    pub number: u64,
    pub title:  Option<&'a str>,
    pub url:    Option<&'a str>,
}

/// Data needed to render a run lifecycle notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLifecycleBlocks<'a> {
    pub run_id:         &'a str,
    pub run_url:        Option<&'a str>,
    pub workflow_label: &'a str,
    pub result:         Option<&'a str>,
    pub duration_ms:    Option<u64>,
    pub pull_request:   Option<RunLifecyclePullRequest<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
pub enum RunLifecycleKind {
    #[strum(serialize = "Fabro run started")]
    Started,
    #[strum(serialize = "Fabro run completed")]
    Completed,
    #[strum(serialize = "Fabro run failed")]
    Failed,
}

const LIFECYCLE_FIELD_TEXT_LIMIT: usize = 800;

pub fn run_lifecycle_blocks(
    kind: RunLifecycleKind,
    details: &RunLifecycleBlocks<'_>,
) -> Vec<Value> {
    let title: &'static str = kind.into();
    let mut blocks = vec![
        json!({
            "type": "header",
            "text": {
                "type": "plain_text",
                "text": title,
                "emoji": true
            }
        }),
        text_block(&format!("*{}*", lifecycle_field(details.workflow_label))),
    ];

    let mut fields = vec![
        lifecycle_mrkdwn_field(
            "Workflow",
            &format!("`{}`", lifecycle_field(details.workflow_label)),
        ),
        lifecycle_mrkdwn_field("Run ID", &format!("`{}`", lifecycle_field(details.run_id))),
    ];
    if !matches!(kind, RunLifecycleKind::Failed) {
        if let Some(result) = details.result.filter(|value| !value.trim().is_empty()) {
            fields.push(lifecycle_mrkdwn_field("Result", &lifecycle_field(result)));
        }
    }
    if let Some(duration_ms) = details.duration_ms {
        fields.push(lifecycle_mrkdwn_field(
            "Duration",
            &compact_duration(duration_ms),
        ));
    }
    blocks.push(json!({
        "type": "section",
        "fields": fields
    }));

    if matches!(kind, RunLifecycleKind::Failed) {
        if let Some(result) = details.result.filter(|value| !value.trim().is_empty()) {
            blocks.push(text_block(&format!(
                "*Failure*\n{}",
                lifecycle_field(result)
            )));
        }
    }

    if let Some(pull_request) = details.pull_request {
        blocks.push(text_block(&format!(
            "*Pull request*\n{}",
            lifecycle_pull_request_text(&pull_request)
        )));
    }

    if let Some(url) = details.run_url {
        blocks.push(json!({
            "type": "context",
            "elements": [
                {
                    "type": "mrkdwn",
                    "text": slack_link(url, "Open in Fabro")
                }
            ]
        }));
    }

    blocks
}

fn lifecycle_mrkdwn_field(label: &str, value: &str) -> Value {
    json!({
        "type": "mrkdwn",
        "text": format!("*{label}*\n{value}")
    })
}

fn lifecycle_field(text: &str) -> String {
    truncate_to_limit(
        &escape_slack_controls(text),
        LIFECYCLE_FIELD_TEXT_LIMIT,
        HEADER_TRUNCATION_SUFFIX,
    )
}

fn lifecycle_pull_request_text(pull_request: &RunLifecyclePullRequest<'_>) -> String {
    let label = format!("#{}", pull_request.number);
    let mut text = pull_request
        .url
        .map(|url| slack_link(url, &label))
        .unwrap_or(label);
    if let Some(title) = pull_request.title.filter(|value| !value.trim().is_empty()) {
        let _ = write!(text, " — {}", lifecycle_field(title));
    }
    text
}

/// Render `label` as a link to `url` in Slack's `<url|label>` syntax, falling
/// back to plain text when the URL cannot be embedded safely.
///
/// `escape_slack_controls` covers Slack's documented escapes (`&`, `<`, `>`)
/// but Slack has no escape for `|`, which separates the URL from the label.
/// A `|` in the label would split the markup and can make Slack reject the
/// whole block, so it is replaced inside link labels only. Labels can be
/// model-authored (a review target label, for example), so this is reachable.
fn slack_link(url: &str, label: &str) -> String {
    if is_safe_slack_link_url(url) {
        let label = escape_slack_controls(label).replace('|', "/");
        format!("<{url}|{label}>")
    } else {
        escape_slack_controls(label)
    }
}

fn is_safe_slack_link_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url
            .chars()
            .any(|ch| matches!(ch, '<' | '>' | '|' | '\n' | '\r'))
}

fn compact_duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }

    let seconds = ms / 1000;
    if seconds < 60 {
        if ms.is_multiple_of(1000) {
            return format!("{seconds}s");
        }
        let tenths = (ms.saturating_add(50)) / 100;
        let whole = tenths / 10;
        let fraction = tenths % 10;
        return if fraction == 0 {
            format!("{whole}s")
        } else {
            format!("{whole}.{fraction}s")
        };
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{}m {}s", minutes, seconds % 60);
    }

    let hours = minutes / 60;
    if hours < 24 {
        return format!("{}h {}m", hours, minutes % 60);
    }

    format!("{}d {}h", hours / 24, hours % 24)
}

#[cfg(test)]
mod tests {
    use fabro_types::InterviewOption;

    use super::*;

    fn lifecycle_text(blocks: &[Value]) -> String {
        let mut texts = Vec::new();
        for block in blocks {
            if let Some(text) = block["text"]["text"].as_str() {
                texts.push(text);
            }
            if let Some(fields) = block["fields"].as_array() {
                texts.extend(fields.iter().filter_map(|field| field["text"].as_str()));
            }
            if let Some(elements) = block["elements"].as_array() {
                texts.extend(elements.iter().filter_map(|element| {
                    element["text"]
                        .as_str()
                        .or_else(|| element["text"]["text"].as_str())
                }));
            }
        }
        texts.join("\n")
    }

    #[test]
    fn yes_no_produces_two_buttons() {
        let q = Question::new("Approve this PR?", QuestionType::YesNo);
        let blocks = question_to_blocks("run-1", "q-1", &q, None);
        let blocks_json: Value = serde_json::to_value(&blocks).unwrap();

        let section = &blocks_json[0];
        assert_eq!(section["type"], "section");
        assert!(
            section["text"]["text"]
                .as_str()
                .unwrap()
                .contains("Approve this PR?")
        );

        let actions = &blocks_json[1];
        assert_eq!(actions["type"], "actions");
        let elements = actions["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0]["text"]["text"], "Yes");
        assert_eq!(elements[1]["text"]["text"], "No");
    }

    #[test]
    fn confirmation_produces_two_buttons() {
        let q = Question::new("Continue?", QuestionType::Confirmation);
        let blocks = question_to_blocks("run-1", "q-2", &q, None);
        let blocks_json: Value = serde_json::to_value(&blocks).unwrap();

        let actions = &blocks_json[1];
        let elements = actions["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0]["text"]["text"], "Yes");
        assert_eq!(elements[1]["text"]["text"], "No");
    }

    #[test]
    fn multiple_choice_produces_button_per_option() {
        let mut q = Question::new("Pick a language:", QuestionType::MultipleChoice);
        q.options = vec![
            InterviewOption {
                key:         "rs".to_string(),
                label:       "Rust".to_string(),
                description: None,
                preview:     None,
            },
            InterviewOption {
                key:         "ts".to_string(),
                label:       "TypeScript".to_string(),
                description: None,
                preview:     None,
            },
            InterviewOption {
                key:         "py".to_string(),
                label:       "Python".to_string(),
                description: None,
                preview:     None,
            },
        ];
        let blocks = question_to_blocks("run-1", "q-3", &q, None);
        let blocks_json: Value = serde_json::to_value(&blocks).unwrap();

        let actions = &blocks_json[1];
        let elements = actions["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 3);
        assert_eq!(elements[0]["text"]["text"], "Rust");
        assert_eq!(elements[0]["action_id"], "interview.answer.0");
        assert_eq!(elements[1]["action_id"], "interview.answer.1");
        assert_eq!(elements[2]["action_id"], "interview.answer.2");
        // Slack requires action_id to be unique within a message.
        let ids: std::collections::HashSet<&str> = elements
            .iter()
            .map(|e| e["action_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), elements.len());
        // The option key remains in the button `value` payload so the server
        // can still route the answer regardless of suffix scheme.
        assert!(
            elements[0]["value"]
                .as_str()
                .unwrap()
                .contains("\"key\":\"rs\"")
        );
        assert!(
            elements[0]["value"]
                .as_str()
                .unwrap()
                .contains("\"run_id\":\"run-1\"")
        );
        assert_eq!(elements[1]["text"]["text"], "TypeScript");
        assert_eq!(elements[2]["text"]["text"], "Python");
    }

    #[test]
    fn freeform_produces_section_prompting_thread_reply() {
        let q = Question::new("What's the repo URL?", QuestionType::Freeform);
        let blocks = question_to_blocks("run-1", "q-4", &q, None);
        let blocks_json: Value = serde_json::to_value(&blocks).unwrap();

        let arr = blocks_json.as_array().unwrap();
        assert_eq!(arr.len(), 2, "header section + thread-reply prompt");
        let header_text = arr[0]["text"]["text"].as_str().unwrap();
        assert!(header_text.contains("What's the repo URL?"));
        let prompt_text = arr[1]["text"]["text"].as_str().unwrap();
        assert!(prompt_text.contains("Answer in the Fabro form"));
    }

    #[test]
    fn action_values_include_run_id_and_question_id() {
        let q = Question::new("Approve?", QuestionType::YesNo);
        let blocks = question_to_blocks("run-7", "q-7", &q, None);
        let blocks_json: Value = serde_json::to_value(&blocks).unwrap();

        let actions = &blocks_json[1];
        let elements = actions["elements"].as_array().unwrap();
        assert_eq!(elements[0]["action_id"], "interview.answer.yes");
        assert_eq!(elements[1]["action_id"], "interview.answer.no");
        assert_ne!(elements[0]["action_id"], elements[1]["action_id"]);
        let value = elements[0]["value"].as_str().unwrap();
        assert!(value.contains("\"run_id\":\"run-7\""));
        assert!(value.contains("\"qid\":\"q-7\""));
    }

    #[test]
    fn launch_approval_identifies_task_repository_and_owning_run() {
        let message = launch_approval_message(
            "01JCHILD",
            "https://fabro.example/runs/01JCHILD",
            LaunchApprovalIdentity {
                task:       "Improve approval actions",
                repository: Some("maecapozzi/fabro"),
            },
        );
        let rendered = serde_json::to_string(&message.blocks).unwrap();

        assert!(rendered.contains("Improve approval actions"));
        assert!(rendered.contains("maecapozzi/fabro"));
        assert!(rendered.contains("Open approval in Fabro"));
        assert!(rendered.contains("https://fabro.example/runs/01JCHILD"));
        assert!(rendered.contains("\"text\":\"Approve\""));
        assert!(rendered.contains("\"text\":\"Deny\""));
        assert!(rendered.contains("\\\"kind\\\":\\\"approval\\\""));
        assert!(!rendered.contains("Recheck"));
    }

    #[test]
    fn launch_approval_decision_removes_actions_and_keeps_truthful_outcome() {
        let identity = LaunchApprovalIdentity {
            task:       "Improve approval actions",
            repository: Some("maecapozzi/fabro"),
        };
        let approved = launch_approval_decided_message(
            identity,
            SlackApprovalDecision::Approve,
            Some("ada"),
            "https://fabro.example/runs/01JCHILD",
        );
        let denied = launch_approval_decided_message(
            identity,
            SlackApprovalDecision::Deny,
            None,
            "https://fabro.example/runs/01JCHILD",
        );

        assert_eq!(approved.blocks[0]["text"]["text"], "*Approved*");
        assert!(approved.blocks.iter().any(|block| {
            block["text"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Decision recorded by ada"))
        }));
        assert!(
            denied.blocks[0]["text"]["text"]
                .as_str()
                .unwrap()
                .contains("Denied")
        );
        assert!(
            approved
                .blocks
                .iter()
                .all(|block| block["type"] != "actions")
        );
        assert!(denied.blocks.iter().all(|block| block["type"] != "actions"));
        assert_eq!(approved.fallback_text, "Approved: Improve approval actions");
        assert_eq!(denied.fallback_text, "Denied: Improve approval actions");
    }

    #[test]
    fn header_includes_run_link_when_url_provided() {
        let q = Question::new("Approve Plan", QuestionType::YesNo);
        let blocks = question_to_blocks(
            "run-1",
            "q-1",
            &q,
            Some("http://127.0.0.1:32276/runs/run-1"),
        );
        let header = serde_json::to_value(&blocks).unwrap()[0]["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(header.contains("<http://127.0.0.1:32276/runs/run-1|Open this task in Fabro>"));
    }

    #[test]
    fn header_renders_review_target_as_the_question_link() {
        let mut q = Question::new(
            "Review the Quarry review exercise document, then choose the next action.",
            QuestionType::MultipleChoice,
        );
        q.review_target = Some(
            fabro_types::ReviewTarget::new(
                "Quarry review exercise",
                "https://quarry.lithos.computer/tmp/0123456789abcdef0123456789abcdef",
                fabro_types::ReviewTargetKind::Document,
            )
            .unwrap(),
        );

        let blocks = question_to_blocks("run-1", "q-1", &q, None);
        let header = serde_json::to_value(&blocks).unwrap()[0]["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();

        assert!(header.starts_with("*Mae Factory needs your input.*"));
        assert!(header.contains(
            "Review the \
             <https://quarry.lithos.computer/tmp/0123456789abcdef0123456789abcdef|Quarry review \
             exercise> document, then choose the next action."
        ));
    }

    #[test]
    fn review_target_label_pipe_cannot_split_the_slack_link() {
        let mut q = Question::new("Review", QuestionType::MultipleChoice);
        q.review_target = Some(
            fabro_types::ReviewTarget::new(
                "Draft | v2",
                "https://quarry.lithos.computer/tmp/0123456789abcdef0123456789abcdef",
                fabro_types::ReviewTargetKind::Document,
            )
            .unwrap(),
        );

        let blocks = question_to_blocks("run-1", "q-1", &q, None);
        let header = serde_json::to_value(&blocks).unwrap()[0]["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();

        // Exactly one `|`: the one separating the URL from the label.
        assert_eq!(header.matches('|').count(), 1);
        assert!(header.contains("|Draft / v2>"));
    }

    #[test]
    fn header_omits_link_when_url_missing() {
        let q = Question::new("Approve Plan", QuestionType::YesNo);
        let blocks = question_to_blocks("run-1", "q-1", &q, None);
        let header = serde_json::to_value(&blocks).unwrap()[0]["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!header.contains("Open in Fabro"));
    }

    #[test]
    fn header_hides_internal_stage_identifier() {
        let mut q = Question::new("Approve Plan", QuestionType::YesNo);
        q.stage = "approval_wait@01JABC123".to_string();
        let blocks = question_to_blocks("run-1", "q-1", &q, None);
        let header = serde_json::to_value(&blocks).unwrap()[0]["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!header.contains("approval_wait"));
        assert!(!header.contains("01JABC123"));
    }

    #[test]
    fn header_truncates_when_inputs_exceed_section_limit() {
        let mut q = Question::new("a".repeat(4000), QuestionType::YesNo);
        q.stage = "b".repeat(2000);
        let blocks = question_to_blocks(
            "run-1",
            "q-1",
            &q,
            Some("http://127.0.0.1:32276/runs/run-1"),
        );
        let header = serde_json::to_value(&blocks).unwrap()[0]["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            header.chars().count() <= 3000,
            "header text exceeded Slack section limit: {} chars",
            header.chars().count()
        );
        assert!(header.ends_with(" …"));
    }

    #[test]
    fn context_display_renders_between_header_and_actions() {
        let mut q = Question::new("Approve Plan", QuestionType::YesNo);
        q.context_display = Some(
            "Plan artifact created and published.\n\n\
             - Local artifact: tmp-docs/fabro-plan.html\n\
             - Dossier canonical URL: https://example.test/s/siv-1067/eng-design-doc"
                .to_string(),
        );
        let blocks_json =
            serde_json::to_value(question_to_blocks("run-1", "q-1", &q, None)).unwrap();
        let arr = blocks_json.as_array().unwrap();
        // 0: header section, 1: context section, 2: divider, 3: actions
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["type"], "section");
        assert_eq!(arr[1]["type"], "section");
        assert_eq!(arr[2]["type"], "divider");
        assert_eq!(arr[3]["type"], "actions");
        let context_text = arr[1]["text"]["text"].as_str().unwrap();
        assert!(context_text.contains("Plan artifact created"));
        assert!(context_text.contains("tmp-docs/fabro-plan.html"));
    }

    #[test]
    fn context_display_truncates_oversized_text_to_fit_slack_budget() {
        let mut q = Question::new("Approve Plan", QuestionType::YesNo);
        q.context_display = Some("x".repeat(10_000));
        let blocks_json =
            serde_json::to_value(question_to_blocks("run-1", "q-1", &q, None)).unwrap();
        let context_text = blocks_json[1]["text"]["text"].as_str().unwrap();
        assert!(
            context_text.chars().count() <= 3000,
            "context block exceeded Slack section text limit: {} chars",
            context_text.chars().count()
        );
        assert!(context_text.contains("truncated"));
    }

    #[test]
    fn empty_context_display_is_skipped() {
        let mut q = Question::new("Approve Plan", QuestionType::YesNo);
        q.context_display = Some("   \n\t  ".to_string());
        let blocks_json =
            serde_json::to_value(question_to_blocks("run-1", "q-1", &q, None)).unwrap();
        let arr = blocks_json.as_array().unwrap();
        // Falls back to header + actions when there's nothing meaningful.
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "section");
        assert_eq!(arr[1]["type"], "actions");
    }

    #[test]
    fn routing_context_becomes_a_plain_language_action_without_machine_state() {
        let mut q = Question::new(
            "CHILD LAUNCH APPROVAL — Open the linked child from this run's Children tab and \
             approve or deny its launch there. This parent cannot approve it.",
            QuestionType::MultipleChoice,
        );
        q.stage = "approval_wait@01JSTAGE".to_string();
        q.options = vec![
            InterviewOption {
                key:         "recheck_child".to_string(),
                label:       "Recheck whether the child was approved and continue orchestration"
                    .to_string(),
                description: None,
                preview:     None,
            },
            InterviewOption {
                key:         "stop_parent".to_string(),
                label:       "Stop the parent orchestrator".to_string(),
                description: None,
                preview:     None,
            },
        ];
        q.context_display = Some(
            json!({
                "preferred_next_label": "approval",
                "outcome": "blocked",
                "failure_reason": null,
                "suggested_next_ids": ["approval_wait@01JSTAGE"],
                "context_updates": {
                    "monitoring": "approval",
                    "human_action": "approve child",
                    "factory_commit": "0123456789abcdef0123456789abcdef01234567",
                    "child_run_id": "01JCHILDRUN",
                    "child_status": "pending approval_required"
                },
                "target_base": "fedcba9876543210",
                "evidence_summary": "machine detail",
                "pr_url": null
            })
            .to_string(),
        );

        let message = question_message(
            "01JPARENTRUN",
            "question-1",
            &q,
            Some("https://fabro.example/runs/01JPARENTRUN"),
            QuestionIdentity {
                task:             Some("Fix the PR list"),
                repository:       Some("maecapozzi/fabro"),
                child_run:        false,
                child_action_url: Some("https://fabro.example/runs/01JCHILDRUN"),
            },
        );
        let rendered = lifecycle_text(&message.blocks);

        assert!(rendered.contains("Approval needed"));
        assert!(rendered.contains("Fix the PR list"));
        assert!(rendered.contains("maecapozzi/fabro"));
        assert!(rendered.contains("Approve or deny this task"));
        assert!(!rendered.contains("waiting for launch approval"));
        assert!(rendered.contains("Open approval in Fabro"));
        assert!(!rendered.contains("https://fabro.example/runs/01JPARENTRUN"));
        assert!(rendered.contains("https://fabro.example/runs/01JCHILDRUN"));
        for internal in [
            "CHILD LAUNCH",
            "approval_wait",
            "preferred_next_label",
            "context_updates",
            "0123456789abcdef",
        ] {
            assert!(!rendered.contains(internal), "{internal} leaked");
        }

        let actions = message
            .blocks
            .iter()
            .find(|block| block["type"] == "actions")
            .unwrap();
        assert_eq!(actions["elements"][0]["text"]["text"], "Check again");
        assert_eq!(actions["elements"][1]["text"]["text"], "Stop");
        assert_eq!(actions["elements"][0]["action_id"], "interview.answer.0");
        assert_eq!(actions["elements"][1]["action_id"], "interview.answer.1");
        let first_value = actions["elements"][0]["value"].as_str().unwrap();
        assert!(first_value.contains("\"run_id\":\"01JPARENTRUN\""));
        assert!(first_value.contains("\"qid\":\"question-1\""));
        assert!(first_value.contains("\"key\":\"recheck_child\""));

        assert!(message.fallback_text.contains("Fix the PR list"));
        assert!(message.fallback_text.contains("Approve or deny this task"));
        assert!(message.fallback_text.contains("Open in Fabro"));
        assert!(
            !message
                .fallback_text
                .contains("waiting for launch approval")
        );
        assert!(!message.fallback_text.contains("preferred_next_label"));
        assert!(!message.fallback_text.contains("approval_wait"));
        assert!(!message.fallback_text.contains("01JPARENTRUN"));
    }

    #[test]
    fn child_choice_question_separates_context_and_preserves_actions() {
        let question_text =
            "Should we keep the café owner’s `release <candidate>` label & deploy 東京?";
        let mut question = Question::new(question_text, QuestionType::MultipleChoice);
        question.options = vec![
            InterviewOption {
                key:         "keep".to_string(),
                label:       "Keep the release candidate label".to_string(),
                description: Some("Continue with the reviewed label.".to_string()),
                preview:     None,
            },
            InterviewOption {
                key:         "remove".to_string(),
                label:       "Remove it before deployment".to_string(),
                description: None,
                preview:     None,
            },
        ];
        let message = question_message(
            "01JCHILD",
            "question-7",
            &question,
            Some("https://fabro.example/runs/01JCHILD"),
            QuestionIdentity {
                task:             Some(
                    "Investigate release safety\n\nDo not include the full operational prompt. ",
                ),
                repository:       Some("acme/widgets & tools"),
                child_run:        true,
                child_action_url: None,
            },
        );
        let rendered = lifecycle_text(&message.blocks);

        assert_eq!(message.blocks[0]["text"]["text"], "*Question for you*");
        assert!(rendered.contains("*Task*\nInvestigate release safety Do not include"));
        assert!(rendered.contains("*Repository*\nacme/widgets &amp; tools"));
        assert!(rendered.contains(
            "Should we keep the café owner’s `release &lt;candidate&gt;` label &amp; deploy 東京?"
        ));
        assert!(rendered.contains("Open question in Fabro"));
        assert!(rendered.contains("https://fabro.example/runs/01JCHILD"));

        let actions = message
            .blocks
            .iter()
            .find(|block| block["type"] == "actions")
            .unwrap();
        assert_eq!(
            actions["elements"][0]["text"]["text"],
            "Keep the release candidate label"
        );
        assert!(
            actions["elements"][0]["value"]
                .as_str()
                .unwrap()
                .contains("\"key\":\"keep\"")
        );
        assert_eq!(actions["elements"][0]["action_id"], "interview.answer.0");
    }

    #[test]
    fn child_freeform_question_points_to_owning_form_and_answer_update_is_readable() {
        let question_text = "Paste the complete rollout note for München & 東京; include <owners>.";
        let question = Question::new(question_text, QuestionType::Freeform);
        let task = format!("{} 🧭", "Long multiline goal\n".repeat(20));
        let identity = QuestionIdentity {
            task:             Some(&task),
            repository:       Some("acme/世界"),
            child_run:        true,
            child_action_url: None,
        };
        let message = question_message(
            "01JCHILD",
            "question-8",
            &question,
            Some("https://fabro.example/runs/01JCHILD"),
            identity,
        );
        let answered = answered_message(
            question_text,
            "Ship after 14:00 <UTC> & notify José.",
            identity,
            Some("https://fabro.example/runs/01JCHILD"),
        );
        let rendered = lifecycle_text(&message.blocks);
        let answered_rendered = lifecycle_text(&answered.blocks);

        assert!(rendered.contains("Question for you"));
        assert!(rendered.contains("Answer in the Fabro form"));
        assert!(rendered.contains("Open question in Fabro"));
        assert!(rendered.contains("München &amp; 東京"));
        assert!(rendered.contains("&lt;owners&gt;"));
        assert!(!rendered.contains("\n\nLong multiline goal"));
        assert!(answered_rendered.contains("*Answered*"));
        assert!(answered_rendered.contains("*Question*"));
        assert!(answered_rendered.contains("*Answer*"));
        assert!(answered_rendered.contains("&lt;UTC&gt; &amp; notify José"));
        assert!(
            answered
                .blocks
                .iter()
                .all(|block| block["type"] != "actions")
        );
    }

    #[test]
    fn unverified_parent_navigation_uses_truthful_parent_fallback() {
        let mut question = Question::new(
            "CHILD LAUNCH APPROVAL — Open the linked child and approve it.",
            QuestionType::MultipleChoice,
        );
        question.context_display = Some(
            json!({
                "preferred_next_label": "approval",
                "context_updates": {
                    "monitoring": "approval",
                    "child_run_id": "unverified",
                    "child_run_url": "https://attacker.example/runs/not-owned",
                    "child_status": "pending_approval"
                }
            })
            .to_string(),
        );
        question.options = vec![InterviewOption {
            key:         "recheck".to_string(),
            label:       "Recheck child".to_string(),
            description: None,
            preview:     None,
        }];
        let message = question_message(
            "01JPARENT",
            "question-9",
            &question,
            Some("https://fabro.example/runs/01JPARENT"),
            QuestionIdentity {
                task:             Some("Coordinate release"),
                repository:       Some("acme/widgets"),
                child_run:        false,
                child_action_url: None,
            },
        );
        let rendered = lifecycle_text(&message.blocks);

        assert!(rendered.contains("Open this task’s Children tab"));
        assert!(rendered.contains("Open parent task in Fabro"));
        assert!(rendered.contains("https://fabro.example/runs/01JPARENT"));
        assert!(!rendered.contains("attacker.example"));
    }

    #[test]
    fn nested_child_question_navigation_keeps_user_questions_distinct() {
        let mut navigation = Question::new(
            "CHILD QUESTION — Open the linked task and answer its question.",
            QuestionType::MultipleChoice,
        );
        navigation.context_display = Some(
            json!({
                "preferred_next_label": "question",
                "outcome": "succeeded",
                "context_updates": {
                    "monitoring": "question",
                    "human_action": "the earlier launch approval was already recorded",
                    "child_run_id": "01JCHILD",
                    "child_status": "waiting for input"
                }
            })
            .to_string(),
        );
        navigation.options = vec![InterviewOption {
            key:         "recheck".to_string(),
            label:       "Recheck child question".to_string(),
            description: None,
            preview:     None,
        }];
        let message = question_message(
            "01JPARENT",
            "question-10",
            &navigation,
            Some("https://fabro.example/runs/01JPARENT"),
            QuestionIdentity {
                task:             Some("Coordinate release"),
                repository:       Some("acme/widgets"),
                child_run:        false,
                child_action_url: Some("https://fabro.example/runs/01JCHILD"),
            },
        );
        let ordinary = Question::new(
            "Is this a CHILD QUESTION label the user wants to keep?",
            QuestionType::YesNo,
        );
        let ordinary_message = question_message(
            "01JPARENT",
            "question-11",
            &ordinary,
            Some("https://fabro.example/runs/01JPARENT"),
            QuestionIdentity::default(),
        );
        let rendered = lifecycle_text(&message.blocks);
        let ordinary_rendered = lifecycle_text(&ordinary_message.blocks);

        assert!(rendered.contains("*Question for you*"));
        assert!(rendered.contains("Answer this task’s question"));
        assert!(rendered.contains("Open question in Fabro"));
        assert!(rendered.contains("https://fabro.example/runs/01JCHILD"));
        assert!(
            ordinary_rendered.contains("Is this a CHILD QUESTION label the user wants to keep?")
        );
    }

    #[test]
    fn incidental_child_metadata_keeps_an_ordinary_question_unchanged() {
        let mut question = Question::new(
            "The earlier launch was approved. Should we notify the on-call owner?",
            QuestionType::YesNo,
        );
        question.context_display = Some(
            json!({
                "preferred_next_label": "continue",
                "context_updates": {
                    "monitoring": "approval",
                    "human_action": "the child was approved earlier",
                    "child_run_id": "01JCHILD",
                    "child_status": "waiting for input"
                }
            })
            .to_string(),
        );
        let message = question_message(
            "01JPARENT",
            "question-12",
            &question,
            Some("https://fabro.example/runs/01JPARENT"),
            QuestionIdentity {
                task:             Some("Coordinate release"),
                repository:       Some("acme/widgets"),
                child_run:        false,
                child_action_url: Some("https://fabro.example/runs/01JCHILD"),
            },
        );
        let rendered = lifecycle_text(&message.blocks);

        assert!(rendered.contains("The earlier launch was approved. Should we notify"));
        assert!(rendered.contains("Mae Factory needs your input"));
        assert!(!rendered.contains("Open approval in Fabro"));
        assert_eq!(message.blocks.last().unwrap()["type"], "actions");
    }

    #[test]
    fn answered_message_bounds_escaped_repository_question_and_answer_sections() {
        let repository = format!("<&>{}", "界".repeat(SLACK_SECTION_TEXT_LIMIT));
        let question = format!("<question>&{}", "q".repeat(SLACK_SECTION_TEXT_LIMIT));
        let answer = format!("<answer>&{}", "a".repeat(SLACK_SECTION_TEXT_LIMIT));
        let message = answered_message(
            &question,
            &answer,
            QuestionIdentity {
                task:             Some("Bound every field"),
                repository:       Some(&repository),
                child_run:        true,
                child_action_url: None,
            },
            Some("https://fabro.example/runs/01JCHILD"),
        );

        for block in &message.blocks {
            if let Some(text) = block["text"]["text"].as_str() {
                assert!(
                    text.chars().count() <= SLACK_SECTION_TEXT_LIMIT,
                    "oversized Slack section: {} chars",
                    text.chars().count()
                );
                assert!(!text.contains("<question>"));
                assert!(!text.contains("<answer>"));
            }
        }
    }

    #[test]
    fn malformed_and_unknown_structured_context_are_hidden() {
        for context in [
            r#"{"preferred_next_label":"Continue","context_updates":"#,
            r#"{"unexpected":{"internal_run_id":"01JSECRET"}}"#,
            r#"[{"factory_commit":"deadbeef"}]"#,
            r#"[{"factory_commit":"deadbeef""#,
        ] {
            let mut q = Question::new("What should I do next?", QuestionType::Freeform);
            q.context_display = Some(context.to_string());
            let message = question_message(
                "run-1",
                "q-1",
                &q,
                Some("https://fabro.example/runs/run-1"),
                QuestionIdentity::default(),
            );
            let rendered = serde_json::to_string(&message.blocks).unwrap();

            assert!(rendered.contains("What should I do next?"));
            assert!(rendered.contains("Open this task in Fabro"));
            assert!(!rendered.contains("preferred_next_label"));
            assert!(!rendered.contains("internal_run_id"));
            assert!(!rendered.contains("factory_commit"));
        }
    }

    #[test]
    fn ordinary_context_and_option_text_remain_authored() {
        let mut q = Question::new(
            "The deployment failed with HTTP 502. Retry it?",
            QuestionType::MultipleChoice,
        );
        q.context_display =
            Some("The upstream returned `connection reset`; no files were changed.".to_string());
        q.options = vec![InterviewOption {
            key:         "retry".to_string(),
            label:       "Retry deployment".to_string(),
            description: Some("Run the deployment again.".to_string()),
            preview:     None,
        }];

        let message = question_message("run-1", "q-1", &q, None, QuestionIdentity::default());
        let rendered = serde_json::to_string(&message.blocks).unwrap();

        assert!(rendered.contains("HTTP 502"));
        assert!(rendered.contains("connection reset"));
        assert!(rendered.contains("Retry deployment"));
        assert!(rendered.contains("Run the deployment again."));
    }

    #[test]
    fn markdown_links_and_bracketed_authored_prose_are_not_machine_context() {
        for context in [
            "[Runbook](https://example.test)",
            "[Production] Migration is blocked",
        ] {
            let mut question = Question::new("What should I do next?", QuestionType::Freeform);
            question.context_display = Some(context.to_string());
            let message = question_message(
                "run-1",
                "q-1",
                &question,
                Some("https://fabro.example/runs/run-1"),
                QuestionIdentity::default(),
            );
            let rendered = serde_json::to_string(&message.blocks).unwrap();

            assert!(rendered.contains(context));
        }
    }

    #[test]
    fn fallback_text_escapes_controls_and_obeys_its_limit() {
        let q = Question::new(
            format!("Approve <!here> & {}?", "x".repeat(1_000)),
            QuestionType::YesNo,
        );
        let message = question_message("run-1", "q-1", &q, None, QuestionIdentity::default());

        assert!(message.fallback_text.chars().count() <= SLACK_FALLBACK_TEXT_LIMIT);
        assert!(!message.fallback_text.contains("<!here>"));
        assert!(message.fallback_text.contains("&lt;!here&gt;"));
        assert!(message.fallback_text.contains("&amp;"));
    }

    #[test]
    fn slack_control_chars_in_question_text_are_escaped() {
        let q = Question::new("Approve <plan> & merge?", QuestionType::YesNo);
        let blocks_json =
            serde_json::to_value(question_to_blocks("run-1", "q-1", &q, None)).unwrap();
        let header = blocks_json[0]["text"]["text"].as_str().unwrap();
        // &, <, > must be escaped so Slack doesn't reinterpret them as link
        // or mention syntax. Other Markdown metacharacters (*, _, ~, `) are
        // intentionally left untouched so legitimate formatting still renders.
        assert!(header.contains("&lt;plan&gt;"));
        assert!(header.contains("&amp;"));
    }

    #[test]
    fn slack_control_chars_in_context_display_are_escaped() {
        // An LLM-produced context_display could embed `<!here>`, `<@U…>`, or
        // `<#C…>` which Slack would treat as a notification or mention. The
        // escape must neutralise them while keeping bullets/bold/code intact.
        let mut q = Question::new("Approve Plan", QuestionType::YesNo);
        q.context_display = Some(
            "Heads up: <!here> please review\n\
             - tagged: <@U12345>\n\
             - moved channel: <#C67890>\n\
             - kept: *bold* _italic_ `code` ~strike~"
                .to_string(),
        );
        let blocks_json =
            serde_json::to_value(question_to_blocks("run-1", "q-1", &q, None)).unwrap();
        let context = blocks_json[1]["text"]["text"].as_str().unwrap();
        // Pings are neutralised.
        assert!(!context.contains("<!here>"));
        assert!(!context.contains("<@U12345>"));
        assert!(!context.contains("<#C67890>"));
        assert!(context.contains("&lt;!here&gt;"));
        assert!(context.contains("&lt;@U12345&gt;"));
        assert!(context.contains("&lt;#C67890&gt;"));
        // Markdown formatting is preserved.
        assert!(context.contains("*bold*"));
        assert!(context.contains("_italic_"));
        assert!(context.contains("`code`"));
        assert!(context.contains("~strike~"));
    }

    #[test]
    fn answered_blocks_escape_slack_control_chars() {
        let blocks = answered_blocks("Approve <plan>?", "Yes & ship");
        let text = serde_json::to_value(&blocks).unwrap()[0]["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!text.contains("<plan>"));
        assert!(text.contains("&lt;plan&gt;"));
        assert!(text.contains("Yes &amp; ship"));
    }

    #[test]
    fn answered_blocks_show_question_and_answer() {
        let blocks = answered_blocks("Do you approve?", "Yes");
        let json: Value = serde_json::to_value(&blocks).unwrap();

        assert_eq!(json.as_array().unwrap().len(), 1);
        let text = json[0]["text"]["text"].as_str().unwrap();
        assert!(text.contains("Do you approve?"));
        assert!(text.contains("Yes"));
    }

    #[test]
    fn answered_blocks_have_no_actions() {
        let blocks = answered_blocks("Pick one:", "Rust");
        let json: Value = serde_json::to_value(&blocks).unwrap();

        let has_actions = json
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "actions");
        assert!(!has_actions);
    }

    #[test]
    fn multi_select_produces_checkboxes_and_submit_button() {
        let mut q = Question::new("Select features:", QuestionType::MultiSelect);
        q.options = vec![
            InterviewOption {
                key:         "a".to_string(),
                label:       "Auth".to_string(),
                description: None,
                preview:     None,
            },
            InterviewOption {
                key:         "b".to_string(),
                label:       "Billing".to_string(),
                description: None,
                preview:     None,
            },
        ];
        let blocks = question_to_blocks("run-1", "q-5", &q, None);
        let blocks_json: Value = serde_json::to_value(&blocks).unwrap();

        // Checkboxes in their own block with a block_id
        let checkbox_block = &blocks_json[1];
        assert_eq!(checkbox_block["type"], "actions");
        assert_eq!(checkbox_block["block_id"], MULTI_SELECT_BLOCK_ID);
        let cb_elements = checkbox_block["elements"].as_array().unwrap();
        assert_eq!(cb_elements[0]["type"], "checkboxes");
        assert_eq!(cb_elements[0]["action_id"], MULTI_SELECT_ACTION_ID);

        // Submit button in a separate actions block
        let submit_block = &blocks_json[2];
        assert_eq!(submit_block["type"], "actions");
        let submit_elements = submit_block["elements"].as_array().unwrap();
        assert_eq!(submit_elements[0]["type"], "button");
        assert_eq!(submit_elements[0]["text"]["text"], "Submit");
        assert_eq!(
            submit_elements[0]["action_id"],
            MULTI_SELECT_SUBMIT_ACTION_ID
        );
        assert!(
            submit_elements[0]["value"]
                .as_str()
                .unwrap()
                .contains("\"qid\":\"q-5\"")
        );
    }

    #[test]
    fn option_descriptions_are_rendered_and_preview_is_not_special_cased() {
        let mut q = Question::new("Pick one:", QuestionType::MultipleChoice);
        q.options = vec![InterviewOption {
            key:         "ship".to_string(),
            label:       "Ship".to_string(),
            description: Some("Deploy <now>".to_string()),
            preview:     Some("preview should not render".to_string()),
        }];

        let blocks_value: Value =
            serde_json::to_value(question_to_blocks("run-1", "q-6", &q, None)).unwrap();
        let text = blocks_value.to_string();

        assert!(text.contains("Deploy &lt;now&gt;"));
        assert!(!text.contains("preview should not render"));
    }

    #[test]
    fn run_started_blocks_include_run_link_workflow_and_run_id_without_actions() {
        let blocks = run_lifecycle_blocks(RunLifecycleKind::Started, &RunLifecycleBlocks {
            run_id:         "01HSTART",
            run_url:        Some("https://fabro.example/runs/01HSTART"),
            workflow_label: "deploy",
            result:         None,
            duration_ms:    None,
            pull_request:   None,
        });

        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[0]["text"]["text"], "Fabro run started");
        assert_eq!(blocks[1]["type"], "section");
        assert_eq!(blocks[1]["text"]["text"], "*deploy*");
        assert_eq!(blocks[2]["type"], "section");
        let text = lifecycle_text(&blocks);
        assert!(text.contains("*Workflow*\n`deploy`"));
        assert!(text.contains("*Run ID*\n`01HSTART`"));
        assert!(text.contains("<https://fabro.example/runs/01HSTART|Open in Fabro>"));
        assert!(!blocks.iter().any(|block| block["type"] == "actions"));
    }

    #[test]
    fn run_completed_blocks_include_result_duration_and_pull_request_title() {
        let blocks = run_lifecycle_blocks(RunLifecycleKind::Completed, &RunLifecycleBlocks {
            run_id:         "01HDONE",
            run_url:        None,
            workflow_label: "release",
            result:         Some("completed"),
            duration_ms:    Some(65_432),
            pull_request:   Some(RunLifecyclePullRequest {
                number: 42,
                title:  Some("Ship <prod> & notify"),
                url:    Some("https://github.com/fabro-sh/fabro/pull/42"),
            }),
        });

        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[0]["text"]["text"], "Fabro run completed");
        assert_eq!(blocks[1]["text"]["text"], "*release*");
        let text = lifecycle_text(&blocks);
        assert!(text.contains("*Result*\ncompleted"));
        assert!(text.contains("*Duration*\n1m 5s"));
        assert!(text.contains("*Pull request*"));
        assert!(text.contains("<https://github.com/fabro-sh/fabro/pull/42|#42>"));
        assert!(text.contains("Ship &lt;prod&gt; &amp; notify"));
    }

    #[test]
    fn run_failed_blocks_include_failure_result_message_and_duration() {
        let blocks = run_lifecycle_blocks(RunLifecycleKind::Failed, &RunLifecycleBlocks {
            run_id:         "01HFAIL",
            run_url:        None,
            workflow_label: "deploy",
            result:         Some("workflow_error — command <failed> & exited"),
            duration_ms:    Some(1_234),
            pull_request:   None,
        });

        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[0]["text"]["text"], "Fabro run failed");
        let text = lifecycle_text(&blocks);
        assert!(text.contains("*Failure*"));
        assert!(text.contains("workflow_error — command &lt;failed&gt; &amp; exited"));
        assert!(text.contains("*Duration*\n1.2s"));
    }

    #[test]
    fn run_lifecycle_blocks_escape_and_truncate_untrusted_text() {
        let blocks = run_lifecycle_blocks(RunLifecycleKind::Completed, &RunLifecycleBlocks {
            run_id:         "01H<&>",
            run_url:        None,
            workflow_label: &format!("deploy <!here> {}", "x".repeat(4_000)),
            result:         Some("partial_success <needs-review> & done"),
            duration_ms:    None,
            pull_request:   None,
        });

        let text = lifecycle_text(&blocks);
        assert!(!text.contains("<!here>"));
        assert!(text.contains("&lt;!here&gt;"));
        assert!(text.contains("01H&lt;&amp;&gt;"));
        assert!(text.contains("partial_success &lt;needs-review&gt; &amp; done"));
        for block in &blocks {
            if let Some(text) = block["text"]["text"].as_str() {
                assert!(
                    text.chars().count() <= SLACK_SECTION_TEXT_LIMIT,
                    "lifecycle block exceeded Slack section limit: {} chars",
                    text.chars().count()
                );
            }
        }
        assert!(text.contains(" …"));
    }

    #[test]
    fn run_lifecycle_pull_request_without_title_uses_number_and_link_only() {
        let blocks = run_lifecycle_blocks(RunLifecycleKind::Completed, &RunLifecycleBlocks {
            run_id:         "01HDONE",
            run_url:        None,
            workflow_label: "release",
            result:         Some("completed"),
            duration_ms:    Some(1000),
            pull_request:   Some(RunLifecyclePullRequest {
                number: 7,
                title:  None,
                url:    Some("https://github.com/fabro-sh/fabro/pull/7"),
            }),
        });

        let text = lifecycle_text(&blocks);
        assert!(text.contains("*Pull request*"));
        assert!(text.contains("<https://github.com/fabro-sh/fabro/pull/7|#7>"));
        assert!(!text.contains(" — "));
    }
}
