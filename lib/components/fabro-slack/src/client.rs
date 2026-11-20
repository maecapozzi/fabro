use fabro_static::EnvVars;
use serde_json::{Value, json};
use tracing::debug;

const SLACK_API_BASE: &str = "https://slack.com/api";

#[derive(Debug, Clone)]
pub struct PostedMessage {
    pub channel_id: String,
    pub ts:         String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackAuthIdentity {
    pub team_id:     String,
    pub bot_user_id: String,
}

#[derive(Clone)]
pub struct SlackClient {
    bot_token: String,
    api_base:  String,
    http:      fabro_http::HttpClient,
}

impl SlackClient {
    #[expect(
        clippy::disallowed_methods,
        reason = "Slack client supports a documented process-env API base URL override."
    )]
    pub fn new(bot_token: String) -> Self {
        let api_base =
            std::env::var(EnvVars::SLACK_BASE_URL).unwrap_or_else(|_| SLACK_API_BASE.to_string());
        Self {
            bot_token,
            api_base,
            http: fabro_http::http_client().expect("Slack HTTP client should build"),
        }
    }

    pub fn with_api_base_and_http(
        bot_token: String,
        api_base: String,
        http: fabro_http::HttpClient,
    ) -> Self {
        Self {
            bot_token,
            api_base,
            http,
        }
    }

    pub fn http(&self) -> &fabro_http::HttpClient {
        &self.http
    }

    pub async fn auth_identity(&self) -> Result<SlackAuthIdentity, SlackApiError> {
        let resp = self
            .http
            .post(format!("{}/auth.test", self.api_base))
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| SlackApiError::Http(e.to_string()))?;

        let json: Value = resp
            .json()
            .await
            .map_err(|e| SlackApiError::Http(e.to_string()))?;

        parse_auth_identity(&json)
    }

    pub async fn post_message(
        &self,
        channel: &str,
        fallback_text: &str,
        blocks: &[Value],
        thread_ts: Option<&str>,
    ) -> Result<PostedMessage, SlackApiError> {
        let body = build_post_message_body(channel, fallback_text, blocks, thread_ts);
        let resp = self
            .http
            .post(format!("{}/chat.postMessage", self.api_base))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| SlackApiError::Http(e.to_string()))?;

        let json: Value = resp
            .json()
            .await
            .map_err(|e| SlackApiError::Http(e.to_string()))?;

        let posted = parse_post_message_response(&json)?;
        debug!(channel, ts = %posted.ts, "Posted Slack message");
        Ok(posted)
    }

    pub async fn update_message(
        &self,
        channel: &str,
        ts: &str,
        fallback_text: &str,
        blocks: &[Value],
    ) -> Result<(), SlackApiError> {
        let body = build_update_message_body(channel, ts, fallback_text, blocks);
        let resp = self
            .http
            .post(format!("{}/chat.update", self.api_base))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| SlackApiError::Http(e.to_string()))?;

        let json: Value = resp
            .json()
            .await
            .map_err(|e| SlackApiError::Http(e.to_string()))?;

        check_ok(&json)?;
        Ok(())
    }

    pub async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), SlackApiError> {
        let body = build_delete_message_body(channel, ts);
        let resp = self
            .http
            .post(format!("{}/chat.delete", self.api_base))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| SlackApiError::Http(e.to_string()))?;

        let json: Value = resp
            .json()
            .await
            .map_err(|e| SlackApiError::Http(e.to_string()))?;

        check_ok(&json)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SlackApiError {
    #[error("Slack HTTP error: {0}")]
    Http(String),
    #[error("Slack API error: {0}")]
    Api(String),
}

fn check_ok(response: &Value) -> Result<(), SlackApiError> {
    if response["ok"].as_bool() != Some(true) {
        let error = response["error"].as_str().unwrap_or("unknown_error");
        return Err(SlackApiError::Api(error.to_string()));
    }
    Ok(())
}

pub fn parse_post_message_response(response: &Value) -> Result<PostedMessage, SlackApiError> {
    check_ok(response)?;
    let channel_id = response["channel"]
        .as_str()
        .ok_or_else(|| SlackApiError::Api("missing channel in response".to_string()))?;
    let ts = response["ts"]
        .as_str()
        .ok_or_else(|| SlackApiError::Api("missing ts in response".to_string()))?;
    Ok(PostedMessage {
        channel_id: channel_id.to_string(),
        ts:         ts.to_string(),
    })
}

pub fn parse_auth_identity(response: &Value) -> Result<SlackAuthIdentity, SlackApiError> {
    check_ok(response)?;
    let team_id = response["team_id"]
        .as_str()
        .ok_or_else(|| SlackApiError::Api("missing team_id in response".to_string()))?;
    let bot_user_id = response["user_id"]
        .as_str()
        .ok_or_else(|| SlackApiError::Api("missing user_id in response".to_string()))?;
    Ok(SlackAuthIdentity {
        team_id:     team_id.to_string(),
        bot_user_id: bot_user_id.to_string(),
    })
}

pub fn parse_wss_url(response: &Value) -> Result<String, SlackApiError> {
    check_ok(response)?;
    response["url"]
        .as_str()
        .map(std::string::ToString::to_string)
        .ok_or_else(|| SlackApiError::Api("missing url in response".to_string()))
}

fn build_post_message_body(
    channel: &str,
    fallback_text: &str,
    blocks: &[Value],
    thread_ts: Option<&str>,
) -> Value {
    let mut body = json!({
        "channel": channel,
        "text": fallback_text,
        "blocks": blocks
    });
    if let Some(ts) = thread_ts {
        body["thread_ts"] = json!(ts);
    }
    body
}

fn build_update_message_body(
    channel: &str,
    ts: &str,
    fallback_text: &str,
    blocks: &[Value],
) -> Value {
    json!({
        "channel": channel,
        "ts": ts,
        "text": fallback_text,
        "blocks": blocks
    })
}

fn build_delete_message_body(channel: &str, ts: &str) -> Value {
    json!({
        "channel": channel,
        "ts": ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_message_request_body_format() {
        let body = build_post_message_body(
            "#general",
            "Mae Factory needs your input",
            &[serde_json::json!({"type": "section"})],
            None,
        );
        assert_eq!(body["channel"], "#general");
        assert_eq!(body["text"], "Mae Factory needs your input");
        assert_eq!(body["blocks"][0]["type"], "section");
        assert!(body["thread_ts"].is_null());
    }

    #[test]
    fn post_message_request_body_with_thread() {
        let body = build_post_message_body(
            "#general",
            "Mae Factory needs your input",
            &[serde_json::json!({"type": "section"})],
            Some("1234.5678"),
        );
        assert_eq!(body["thread_ts"], "1234.5678");
    }

    #[test]
    fn update_message_request_body_format() {
        let body = build_update_message_body(
            "#general",
            "1234.5678",
            "Approved: Improve approval actions",
            &[serde_json::json!({"type": "section"})],
        );
        assert_eq!(body["channel"], "#general");
        assert_eq!(body["ts"], "1234.5678");
        assert_eq!(body["text"], "Approved: Improve approval actions");
        assert_eq!(body["blocks"][0]["type"], "section");
    }

    #[test]
    fn delete_message_request_body_format() {
        let body = build_delete_message_body("#general", "1234.5678");
        assert_eq!(body["channel"], "#general");
        assert_eq!(body["ts"], "1234.5678");
    }

    #[test]
    fn parse_post_message_response_extracts_channel_and_ts() {
        let response = serde_json::json!({
            "ok": true,
            "channel": "C1234567890",
            "ts": "1234.5678"
        });
        let posted = parse_post_message_response(&response).unwrap();
        assert_eq!(posted.channel_id, "C1234567890");
        assert_eq!(posted.ts, "1234.5678");
    }

    #[test]
    fn parse_post_message_response_api_error() {
        let response = serde_json::json!({
            "ok": false,
            "error": "not_in_channel"
        });
        let err = parse_post_message_response(&response).unwrap_err();
        assert!(err.to_string().contains("not_in_channel"));
    }

    #[test]
    fn parse_auth_identity_extracts_workspace_and_bot_user() {
        let identity = parse_auth_identity(&serde_json::json!({
            "ok": true,
            "team_id": "T123",
            "user_id": "UBOT",
        }))
        .unwrap();

        assert_eq!(identity, SlackAuthIdentity {
            team_id:     "T123".to_string(),
            bot_user_id: "UBOT".to_string(),
        });
    }

    #[test]
    fn parse_auth_identity_requires_workspace_and_bot_user() {
        let error = parse_auth_identity(&serde_json::json!({
            "ok": true,
            "team_id": "T123",
        }))
        .unwrap_err();

        assert!(error.to_string().contains("missing user_id"));
    }

    #[test]
    fn parse_wss_url_success() {
        let response = serde_json::json!({
            "ok": true,
            "url": "wss://wss-primary.slack.com/link/?ticket=abc123"
        });
        let url = parse_wss_url(&response).unwrap();
        assert!(url.starts_with("wss://"));
        assert!(url.contains("ticket=abc123"));
    }

    #[test]
    fn parse_wss_url_api_error() {
        let response = serde_json::json!({
            "ok": false,
            "error": "invalid_auth"
        });
        let err = parse_wss_url(&response).unwrap_err();
        assert!(err.to_string().contains("invalid_auth"));
    }

    #[test]
    fn parse_wss_url_missing_url() {
        let response = serde_json::json!({
            "ok": true
        });
        let err = parse_wss_url(&response).unwrap_err();
        assert!(err.to_string().contains("missing url"));
    }

    #[test]
    fn slack_api_error_display() {
        let http_err = SlackApiError::Http("timeout".to_string());
        assert_eq!(http_err.to_string(), "Slack HTTP error: timeout");

        let api_err = SlackApiError::Api("channel_not_found".to_string());
        assert_eq!(api_err.to_string(), "Slack API error: channel_not_found");
    }
}
