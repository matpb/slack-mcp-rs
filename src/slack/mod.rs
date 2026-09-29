//! Slack Web API client: bearer auth, typed errors, 429 retry, user-name cache.

mod ops;
pub mod transfer;
pub mod types;

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::{Config, Token};
use crate::files::FileError;

const MAX_RETRIES: u32 = 2;
const MAX_TOTAL_SLEEP: Duration = Duration::from_secs(30);

/// Everything a Slack call can fail with; `Display` is what the agent sees.
#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    #[error("{0}")]
    Invalid(String),
    #[error("{}", api_message(.method, .error, .needed.as_deref(), .provided.as_deref()))]
    Api {
        method: String,
        error: String,
        needed: Option<String>,
        provided: Option<String>,
    },
    #[error("Slack rate limit hit calling {method}; retry after about {retry_after_secs}s")]
    RateLimited {
        method: String,
        retry_after_secs: u64,
    },
    #[error("request to Slack failed: {0}")]
    Http(String),
    #[error("file tools are disabled: set SLACK_FILES_DIR to a directory to enable them")]
    FilesDisabled,
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    File(#[from] FileError),
}

fn api_message(method: &str, error: &str, needed: Option<&str>, provided: Option<&str>) -> String {
    let mut msg = format!("Slack error `{error}` from {method}");
    if let Some(n) = needed {
        msg.push_str(&format!(": the token lacks scope `{n}`"));
        if let Some(p) = provided {
            msg.push_str(&format!(" (it has: {p})"));
        }
    }
    let hint = match error {
        "missing_scope" => " Add the scope in the Slack app settings, then reinstall the app.",
        "not_in_channel" => " The user is not a member of this channel.",
        "channel_not_found" => " The channel ID is wrong or the user cannot see it.",
        "invalid_auth" | "token_revoked" | "account_inactive" | "token_expired" => {
            " The token is invalid or revoked; reinstall the app and copy the new User OAuth Token."
        }
        "not_allowed_token_type" => {
            " This method needs a user token (xoxp-); bot tokens (xoxb-) cannot call it."
        }
        "ratelimited" => " Slack rate limit; retry later.",
        _ => "",
    };
    msg.push('.');
    msg.push_str(hint);
    msg
}

fn http_err(e: &reqwest::Error) -> SlackError {
    let mut msg = e.to_string();
    if let Some(url) = e.url() {
        msg = msg.replace(url.as_str(), "<url>");
    }
    SlackError::Http(msg)
}

enum Payload {
    Form(Vec<(String, String)>),
    Json(Value),
}

/// Slack client shared by every tool call.
pub struct SlackClient {
    http: reqwest::Client,
    xfer_http: reqwest::Client,
    base: String,
    token: Token,
    max_file_bytes: u64,
    extra_hosts: Vec<String>,
    cdn_hosts: Vec<String>,
    users: Mutex<HashMap<String, String>>,
}

impl SlackClient {
    /// Builds the API client and the redirect-free transfer client.
    pub fn new(cfg: &Config) -> Result<Self, SlackError> {
        let ua = concat!("slack-mcp/", env!("CARGO_PKG_VERSION"));
        let http = reqwest::Client::builder()
            .user_agent(ua)
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| http_err(&e))?;
        let xfer_http = reqwest::Client::builder()
            .user_agent(ua)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| http_err(&e))?;
        Ok(Self {
            http,
            xfer_http,
            base: cfg.api_base.clone(),
            token: cfg.token.clone(),
            max_file_bytes: cfg.max_file_bytes,
            extra_hosts: cfg.download_hosts.clone(),
            cdn_hosts: Vec::new(),
            users: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) async fn call_form(
        &self,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<Value, SlackError> {
        let form = params
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        self.call(method, &Payload::Form(form)).await
    }

    pub(crate) async fn call_json(&self, method: &str, body: Value) -> Result<Value, SlackError> {
        self.call(method, &Payload::Json(body)).await
    }

    async fn call(&self, method: &str, payload: &Payload) -> Result<Value, SlackError> {
        let url = format!("{}/{method}", self.base);
        let mut slept = Duration::ZERO;
        let mut retries = 0;
        loop {
            let req = self.http.post(&url).bearer_auth(self.token.expose());
            let req = match payload {
                Payload::Form(f) => req.form(f),
                Payload::Json(v) => req.json(v),
            };
            let resp = req.send().await.map_err(|e| http_err(&e))?;
            let status = resp.status();
            if status.as_u16() == 429 {
                let secs = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(1);
                let wait = Duration::from_secs(secs);
                let remaining = MAX_TOTAL_SLEEP.saturating_sub(slept);
                if retries >= MAX_RETRIES || wait > remaining {
                    return Err(SlackError::RateLimited {
                        method: method.to_string(),
                        retry_after_secs: secs,
                    });
                }
                tokio::time::sleep(wait).await;
                slept += wait;
                retries += 1;
                continue;
            }
            let bytes = resp.bytes().await.map_err(|e| http_err(&e))?;
            let v: Value = serde_json::from_slice(&bytes).map_err(|_| {
                SlackError::Http(format!(
                    "unexpected non-JSON response from Slack (HTTP {status})"
                ))
            })?;
            if v.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(v);
            }
            let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
            return Err(SlackError::Api {
                method: method.to_string(),
                error: text("error").unwrap_or_else(|| "unknown_error".into()),
                needed: text("needed"),
                provided: text("provided"),
            });
        }
    }

    /// Display name for a user ID, cached for the process lifetime; falls back to the ID.
    pub async fn user_name(&self, id: &str) -> String {
        self.user_name_batch(id, &mut false).await
    }

    /// Like `user_name`; once a lookup is rate limited, `skip` stops further lookups in the batch.
    pub(crate) async fn user_name_batch(&self, id: &str, skip: &mut bool) -> String {
        if !types::valid_user_id(id) {
            return id.to_string();
        }
        if let Some(n) = self.users.lock().await.get(id) {
            return n.clone();
        }
        if *skip {
            return id.to_string();
        }
        match self
            .call_form("users.info", &[("user", id.to_string())])
            .await
        {
            Ok(v) => {
                let name = types::display_name(&v["user"]).unwrap_or_else(|| id.to_string());
                self.users.lock().await.insert(id.to_string(), name.clone());
                name
            }
            Err(e) => {
                match e {
                    SlackError::Api { .. } => {
                        self.users
                            .lock()
                            .await
                            .insert(id.to_string(), id.to_string());
                    }
                    SlackError::RateLimited { .. } => *skip = true,
                    _ => {}
                }
                id.to_string()
            }
        }
    }

    pub(crate) async fn remember_user(&self, id: &str, name: String) {
        self.users.lock().await.insert(id.to_string(), name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(crate) fn test_config(base: &str, extra: &[(&str, &str)]) -> Config {
        let base = base.to_string();
        let extra: HashMap<String, String> = extra
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Config::from_lookup(move |k| match k {
            "SLACK_USER_TOKEN" => Some("xoxp-test-token".into()),
            "SLACK_API_BASE_URL" => Some(base.clone()),
            other => extra.get(other).cloned(),
        })
        .expect("config")
    }

    #[tokio::test]
    async fn retries_429_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users.info"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/users.info"))
            .and(header("authorization", "Bearer xoxp-test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"ok": true, "user": {"id": "U0123456789", "name": "alice"}}),
            ))
            .mount(&server)
            .await;
        let c = SlackClient::new(&test_config(&server.uri(), &[])).expect("client");
        assert_eq!(c.user_name("U0123456789").await, "alice");
    }

    #[tokio::test]
    async fn retry_after_huge_value_does_not_panic() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429).insert_header("Retry-After", "18446744073709551615"),
            )
            .mount(&server)
            .await;
        let c = SlackClient::new(&test_config(&server.uri(), &[])).expect("client");
        let err = c.call_form("auth.test", &[]).await.expect_err("limited");
        assert!(matches!(err, SlackError::RateLimited { .. }));
        assert_eq!(server.received_requests().await.expect("reqs").len(), 1);
    }

    #[tokio::test]
    async fn failed_user_lookup_is_cached() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/conversations.history"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "messages": [
                    {"user": "U0123456789", "text": "a", "ts": "1.1"},
                    {"user": "U0123456789", "text": "b", "ts": "2.1"},
                    {"user": "U0123456789", "text": "c", "ts": "3.1"}
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/users.info"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": false, "error": "user_not_found"})),
            )
            .mount(&server)
            .await;
        let c = SlackClient::new(&test_config(&server.uri(), &[])).expect("client");
        let out = c
            .read_channel(types::ReadChannelParams {
                channel_id: "C0123456789".into(),
                limit: None,
                oldest: None,
                latest: None,
                cursor: None,
            })
            .await
            .expect("read");
        assert_eq!(out["messages"][2]["user_name"], "U0123456789");
        let hits = server
            .received_requests()
            .await
            .expect("reqs")
            .iter()
            .filter(|r| r.url.path() == "/users.info")
            .count();
        assert_eq!(hits, 1);
    }

    #[tokio::test]
    async fn rate_limited_lookup_stops_batch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users.info"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .mount(&server)
            .await;
        let c = SlackClient::new(&test_config(&server.uri(), &[])).expect("client");
        let mut skip = false;
        assert_eq!(
            c.user_name_batch("U0000000001", &mut skip).await,
            "U0000000001"
        );
        assert!(skip);
        assert_eq!(
            c.user_name_batch("U0000000002", &mut skip).await,
            "U0000000002"
        );
        assert_eq!(server.received_requests().await.expect("reqs").len(), 3);
        assert!(c.users.lock().await.is_empty());
    }

    #[tokio::test]
    async fn gives_up_after_two_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .mount(&server)
            .await;
        let c = SlackClient::new(&test_config(&server.uri(), &[])).expect("client");
        let err = c.call_form("auth.test", &[]).await.expect_err("limited");
        assert!(matches!(err, SlackError::RateLimited { .. }));
        assert_eq!(server.received_requests().await.expect("reqs").len(), 3);
    }

    #[tokio::test]
    async fn api_error_carries_scopes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(
                {"ok": false, "error": "missing_scope", "needed": "search:read", "provided": "chat:write"}
            )))
            .mount(&server)
            .await;
        let c = SlackClient::new(&test_config(&server.uri(), &[])).expect("client");
        let msg = c
            .call_form("search.messages", &[])
            .await
            .expect_err("api error")
            .to_string();
        assert!(
            msg.contains("search:read") && msg.contains("chat:write"),
            "{msg}"
        );
        assert!(!msg.contains("xoxp"));
    }
}
