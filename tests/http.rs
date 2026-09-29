use std::io::Read;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

fn command(port: u16) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_slack-mcp"));
    cmd.arg("http")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("SLACK_USER_TOKEN", "xoxp-test-token")
        .env("MCP_PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(keys: &str) -> Self {
        let port = free_port();
        let child = command(port)
            .env("MCP_API_KEYS", keys)
            .spawn()
            .expect("spawn");
        let s = Self { child, port };
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return s;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("server did not start");
    }

    fn url(&self, p: &str) -> String {
        format!("http://127.0.0.1:{}{p}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn init_body() -> serde_json::Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
           "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                      "clientInfo": {"name": "test", "version": "0"}}})
}

async fn post_mcp_with(
    s: &Server,
    bearer: Option<&str>,
    host: Option<&str>,
    origin: Option<&str>,
) -> ::reqwest::Response {
    let client = ::reqwest::Client::new();
    let mut req = client
        .post(s.url("/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .json(&init_body());
    if let Some(b) = bearer {
        req = req.header("Authorization", format!("Bearer {b}"));
    }
    if let Some(h) = host {
        req = req.header("Host", h);
    }
    if let Some(o) = origin {
        req = req.header("Origin", o);
    }
    req.send().await.expect("send")
}

async fn post_mcp(s: &Server, bearer: Option<&str>, host: Option<&str>) -> ::reqwest::Response {
    post_mcp_with(s, bearer, host, None).await
}

#[test]
fn http_refuses_to_start_without_api_keys() {
    let mut child = command(free_port()).spawn().expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "server kept running without keys"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    let mut err = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut err)
        .expect("read");
    assert!(err.contains("MCP_API_KEYS"), "{err}");
}

#[tokio::test]
async fn http_rejects_missing_bearer() {
    let s = Server::start(KEY);
    assert_eq!(post_mcp(&s, None, None).await.status(), 401);
    assert_eq!(
        post_mcp(&s, Some("wrong-key-wrong-key-wrong-key-wrong"), None)
            .await
            .status(),
        401
    );
}

#[tokio::test]
async fn http_accepts_valid_bearer() {
    let s = Server::start(KEY);
    let resp = post_mcp(&s, Some(KEY), None).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("slack-mcp"), "{body}");
    let health = ::reqwest::get(s.url("/health")).await.expect("health");
    assert_eq!(health.status(), 200);
}

#[tokio::test]
async fn http_rejects_foreign_host_header() {
    let s = Server::start(KEY);
    let resp = post_mcp(&s, Some(KEY), Some("evil.example.net")).await;
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn http_rejects_foreign_origin() {
    let s = Server::start(KEY);
    let bad = post_mcp_with(&s, Some(KEY), None, Some("https://evil.example")).await;
    assert_eq!(bad.status(), 403);
    let local = format!("http://127.0.0.1:{}", s.port);
    let ok = post_mcp_with(&s, Some(KEY), None, Some(&local)).await;
    assert_eq!(ok.status(), 200);
}

#[test]
fn http_rejects_short_api_key() {
    let mut child = command(free_port())
        .env("MCP_API_KEYS", "too-short")
        .spawn()
        .expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "server kept running with a short key"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    let mut err = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut err)
        .expect("read");
    assert!(err.contains("32"), "{err}");
    assert!(!err.contains("too-short"));
}
