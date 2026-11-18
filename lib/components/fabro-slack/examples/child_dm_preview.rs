use std::fmt::Write as _;
use std::path::PathBuf;

use fabro_interview::Question;
use fabro_slack::blocks::{self, LaunchApprovalIdentity, QuestionIdentity};
use fabro_types::{InterviewOption, QuestionType};
use serde_json::Value;

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn text_html(text: &str) -> String {
    let mut html = String::new();
    let mut remaining = text;
    while let Some(start) = remaining.find('<') {
        let (before, candidate) = remaining.split_at(start);
        html.push_str(&html_escape(before));
        let Some(end) = candidate.find('>') else {
            html.push_str(&html_escape(candidate));
            return html.replace('\n', "<br>");
        };
        let markup = &candidate[1..end];
        if let Some((url, label)) = markup.split_once('|') {
            let _ = write!(
                html,
                "<a href=\"{}\">{}</a>",
                html_escape(url),
                html_escape(label)
            );
        } else {
            html.push_str(&html_escape(&candidate[..=end]));
        }
        remaining = &candidate[end + 1..];
    }
    html.push_str(&html_escape(remaining));
    html.replace("*Approval needed*", "<strong>Approval needed</strong>")
        .replace("*Question for you*", "<strong>Question for you</strong>")
        .replace(
            "*Mae Factory needs your input.*",
            "<strong>Mae Factory needs your input.</strong>",
        )
        .replace("*Next step*", "<strong>Next step</strong>")
        .replace("*Task:*", "<strong>Task:</strong>")
        .replace("*Repository:*", "<strong>Repository:</strong>")
        .replace("*Task*", "<strong>Task</strong>")
        .replace("*Repository*", "<strong>Repository</strong>")
        .replace("*Question*", "<strong>Question</strong>")
        .replace('\n', "<br>")
}

fn render_blocks(blocks: &[Value]) -> String {
    let mut html = String::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("section") => {
                if let Some(text) = block["text"]["text"].as_str() {
                    let _ = write!(html, "<div class=\"block\">{}</div>", text_html(text));
                }
            }
            Some("actions") => {
                html.push_str("<div class=\"actions\">");
                if let Some(elements) = block["elements"].as_array() {
                    for element in elements {
                        let label = element["text"]["text"].as_str().unwrap_or("Action");
                        let class = match element["style"].as_str() {
                            Some("primary") => " primary",
                            Some("danger") => " danger",
                            _ => "",
                        };
                        let _ = write!(
                            html,
                            "<span class=\"button{class}\">{}</span>",
                            html_escape(label)
                        );
                    }
                }
                html.push_str("</div>");
            }
            Some("divider") => html.push_str("<hr>"),
            _ => {}
        }
    }
    html
}

fn message(body: &str) -> String {
    format!(
        "<div class=\"message\"><div class=\"avatar\">F</div><div class=\"bubble\"><div \
         class=\"sender\">Fabro <span>APP</span></div>{body}</div></div>"
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/slack-child-dm-preview.html"));

    let approval = blocks::launch_approval_message(
        "01JSYNTHETICCHILD",
        "https://fabro.example/runs/01JSYNTHETICCHILD",
        LaunchApprovalIdentity {
            task:       "Review authentication boundaries for the release",
            repository: Some("acme/widgets"),
        },
    );
    let mut question = Question::new(
        "Which authentication boundary should the report cover first?",
        QuestionType::MultipleChoice,
    );
    question.options = vec![
        InterviewOption {
            key:         "public".to_string(),
            label:       "Public endpoints".to_string(),
            description: None,
            preview:     None,
        },
        InterviewOption {
            key:         "admin".to_string(),
            label:       "Admin endpoints".to_string(),
            description: None,
            preview:     None,
        },
    ];
    let before_child_question = blocks::question_message(
        "01JSYNTHETICCHILD",
        "question-1",
        &question,
        Some("https://fabro.example/runs/01JSYNTHETICCHILD"),
        QuestionIdentity {
            task:             Some("Review authentication boundaries for the release"),
            repository:       Some("acme/widgets"),
            child_run:        false,
            child_action_url: None,
        },
    );
    let question = blocks::question_message(
        "01JSYNTHETICCHILD",
        "question-1",
        &question,
        Some("https://fabro.example/runs/01JSYNTHETICCHILD"),
        QuestionIdentity {
            task:             Some("Review authentication boundaries for the release"),
            repository:       Some("acme/widgets"),
            child_run:        true,
            child_action_url: None,
        },
    );
    let mut parent_navigation = Question::new(
        "CHILD LAUNCH APPROVAL — Open the linked child and approve it.",
        QuestionType::MultipleChoice,
    );
    parent_navigation.context_display = Some(
        r#"{"preferred_next_label":"approval","context_updates":{"monitoring":"approval","child_run_id":"01JSYNTHETICCHILD","child_status":"pending approval_required"}}"#.to_string(),
    );
    parent_navigation.options = vec![
        InterviewOption {
            key:         "R".to_string(),
            label:       "[R] I handled the launch decision; recheck the same child".to_string(),
            description: None,
            preview:     None,
        },
        InterviewOption {
            key:         "S".to_string(),
            label:       "[S] Stop monitoring".to_string(),
            description: None,
            preview:     None,
        },
    ];
    let after_parent_navigation = blocks::question_message(
        "01JSYNTHETICPARENT",
        "question-2",
        &parent_navigation,
        Some("https://fabro.example/runs/01JSYNTHETICPARENT"),
        QuestionIdentity {
            task:             Some("Review authentication boundaries for the release"),
            repository:       Some("acme/widgets"),
            child_run:        false,
            child_action_url: Some("https://fabro.example/runs/01JSYNTHETICCHILD"),
        },
    );

    let before_parent_navigation = message(
        "<div class=\"block\"><strong>Mae Factory needs your input.</strong> Task: Review \
         authentication boundaries for the release · Repository: acme/widgets</div><div \
         class=\"block\"><strong>CHILD LAUNCH APPROVAL</strong> — Open the linked child from this \
         run’s Children tab and approve or deny its launch there. This parent cannot approve \
         it.</div><div class=\"actions\"><span class=\"button\">[R] I handled the launch decision; \
         recheck the same child</span><span class=\"button\">[S] Stop monitoring</span></div>",
    );
    let before_question = message(&render_blocks(&before_child_question.blocks));
    let after_approval = message(&render_blocks(&approval.blocks));
    let after_question = message(&render_blocks(&question.blocks));
    let after_parent_navigation = message(&render_blocks(&after_parent_navigation.blocks));

    let html = format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><style>
* {{ box-sizing: border-box; }}
body {{ margin: 0; padding: 34px; background: #f4f5f7; color: #1d1c1d; font: 15px/1.45 Arial, sans-serif; }}
h1 {{ margin: 0 0 8px; font-size: 24px; }} .note {{ margin: 0 0 26px; color: #616061; }}
.grid {{ display: grid; grid-template-columns: 1fr 1fr; gap: 28px; align-items: start; }}
.column {{ background: white; border: 1px solid #ddd; border-radius: 12px; padding: 20px; }}
h2 {{ margin: 0 0 18px; font-size: 19px; }} h3 {{ margin: 22px 0 10px; font-size: 14px; color: #616061; text-transform: uppercase; letter-spacing: .04em; }}
.message {{ display: flex; gap: 10px; margin: 0 0 18px; }} .avatar {{ flex: 0 0 36px; height: 36px; border-radius: 8px; background: #5b45bc; color: white; display: grid; place-items: center; font-weight: bold; }}
.bubble {{ min-width: 0; flex: 1; }} .sender {{ font-weight: bold; margin-bottom: 5px; }} .sender span {{ font-size: 10px; color: #616061; background: #eee; padding: 2px 4px; border-radius: 3px; }}
.block {{ margin: 0 0 12px; overflow-wrap: anywhere; }} .actions {{ display: flex; flex-wrap: wrap; gap: 8px; margin: 8px 0 12px; }}
.button {{ display: inline-block; border: 1px solid #bbb; border-radius: 4px; padding: 7px 11px; background: white; font-weight: bold; }}
.primary {{ background: #007a5a; color: white; border-color: #007a5a; }} .danger {{ color: #b4152b; }} hr {{ border: 0; border-top: 1px solid #ddd; margin: 12px 0; }}
</style></head><body>
<h1>Native child-run Slack DMs</h1>
<p class="note">Offline preview using synthetic data. “After” is rendered from the Rust Block Kit formatter; no Slack message was sent.</p>
<div class="grid">
  <section class="column"><h2>Before</h2><h3>Parent launch navigation</h3>{before_parent_navigation}<h3>Child-owned question</h3>{before_question}</section>
  <section class="column"><h2>After</h2><h3>Parent launch navigation</h3>{after_parent_navigation}<h3>Child-owned question</h3>{after_question}<h3>Child-owned launch approval (separate native message)</h3>{after_approval}</section>
</div></body></html>"#
    );
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&output, html)?;
    println!("{}", output.display());
    Ok(())
}
