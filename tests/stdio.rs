use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOOLS: [&str; 9] = [
    "slack_search",
    "slack_read_channel",
    "slack_read_thread",
    "slack_send_message",
    "slack_upload_file",
    "slack_download_file",
    "slack_read_user_profile",
    "slack_search_channels",
    "slack_search_users",
];

struct Session {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start(envs: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_slack-mcp"));
        cmd.arg("stdio")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("SLACK_USER_TOKEN", "xoxp-test-token")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn slack-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let out = BufReader::new(child.stdout.take().expect("stdout"));
        let mut s = Self {
            child,
            stdin,
            out,
            next_id: 1,
        };
        let init = s.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}),
        );
        assert!(
            init["result"]["serverInfo"]["name"] == "slack-mcp",
            "{init}"
        );
        s.notify("notifications/initialized");
        s
    }

    fn send(&mut self, v: &Value) {
        writeln!(self.stdin, "{v}").expect("write");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": method}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            let n = self.out.read_line(&mut line).expect("read");
            assert!(n > 0, "server closed stdout before answering");
            let v: Value = serde_json::from_str(&line).expect("json line");
            if v["id"] == json!(id) {
                return v;
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tool_names(resp: &Value) -> Vec<String> {
    let mut names: Vec<String> = resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect();
    names.sort();
    names
}

fn expected() -> Vec<String> {
    let mut v: Vec<String> = TOOLS.iter().map(|s| (*s).to_string()).collect();
    v.sort();
    v
}

#[test]
fn stdio_lists_all_tools() {
    let mut s = Session::start(&[]);
    let resp = s.request("tools/list", json!({}));
    let names = tool_names(&resp);
    println!("tools/list: {names:?}");
    assert_eq!(names, expected());
}

#[test]
fn stdio_read_only_hides_write_tools() {
    let mut s = Session::start(&[("SLACK_READ_ONLY", "true")]);
    let names = tool_names(&s.request("tools/list", json!({})));
    assert_eq!(names.len(), 7);
    assert!(!names.contains(&"slack_send_message".to_string()));
    assert!(!names.contains(&"slack_upload_file".to_string()));
    assert!(names.contains(&"slack_download_file".to_string()));
}

#[test]
fn stdio_refuses_bad_token() {
    let out = Command::new(env!("CARGO_BIN_EXE_slack-mcp"))
        .arg("stdio")
        .env_clear()
        .env("SLACK_USER_TOKEN", "not-a-token")
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("SLACK_USER_TOKEN"), "{err}");
    assert!(!err.contains("not-a-token"));
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_read_channel_against_mock_slack() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/conversations.history"))
        .and(header("authorization", "Bearer xoxp-test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "messages": [{"type": "message", "user": "U0123456789", "text": "hello from the mock",
                          "ts": "1700000000.000100"}],
            "response_metadata": {"next_cursor": ""}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/users.info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "user": {"id": "U0123456789", "name": "alice", "profile": {"display_name": "Alice"}}
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let result = tokio::task::spawn_blocking(move || {
        let mut s = Session::start(&[("SLACK_API_BASE_URL", &uri)]);
        s.request(
            "tools/call",
            json!({"name": "slack_read_channel", "arguments": {"channel_id": "C0123456789"}}),
        )
    })
    .await
    .expect("join");

    assert_ne!(result["result"]["isError"], json!(true), "{result}");
    let text = result["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(text.contains("hello from the mock"), "{text}");
    assert!(text.contains("Alice"), "{text}");
    let reqs = server.received_requests().await.expect("requests");
    assert!(!reqs.is_empty());
    for r in reqs {
        let auth = r.headers.get("authorization").expect("auth header");
        assert!(
            auth.to_str()
                .expect("ascii")
                .starts_with("Bearer xoxp-test")
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_tool_error_is_reported_not_fatal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"ok": false, "error": "missing_scope", "needed": "channels:history", "provided": "chat:write"}),
        ))
        .mount(&server)
        .await;
    let uri = server.uri();
    let (bad, good) = tokio::task::spawn_blocking(move || {
        let mut s = Session::start(&[("SLACK_API_BASE_URL", &uri)]);
        let bad = s.request(
            "tools/call",
            json!({"name": "slack_read_channel", "arguments": {"channel_id": "nope"}}),
        );
        let good = s.request(
            "tools/call",
            json!({"name": "slack_read_channel", "arguments": {"channel_id": "C0123456789"}}),
        );
        (bad, good)
    })
    .await
    .expect("join");
    assert_eq!(bad["result"]["isError"], json!(true), "{bad}");
    assert_eq!(good["result"]["isError"], json!(true), "{good}");
    let text = good["result"]["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("channels:history"), "{text}");
}
