//! Configuration, read only from environment variables (no `.env` file is ever read).

use std::fmt;
use std::path::Path;

use crate::files::FileJail;

pub const DEFAULT_API_BASE: &str = "https://slack.com/api";
pub const DEFAULT_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Slack token. `Debug` never prints the secret.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Accepts `xoxp-` (user) and `xoxb-` (bot) tokens only.
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let raw = raw.trim();
        let ok = (raw.starts_with("xoxp-") || raw.starts_with("xoxb-")) && raw.len() > 6;
        if !ok {
            return Err(ConfigError(
                "SLACK_USER_TOKEN must be a Slack token starting with `xoxp-` (user token, the \
                 intended mode) or `xoxb-`"
                    .into(),
            ));
        }
        Ok(Self(raw.to_string()))
    }

    /// The raw secret, for the `Authorization` header only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.0.starts_with("xoxb-") {
            "xoxb"
        } else {
            "xoxp"
        };
        write!(f, "{kind}-***")
    }
}

/// A startup configuration problem, printed to stderr.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// Everything the server needs, resolved once at startup.
#[derive(Clone)]
pub struct Config {
    pub token: Token,
    pub api_base: String,
    pub files: Option<FileJail>,
    pub max_file_bytes: u64,
    pub download_hosts: Vec<String>,
    pub read_only: bool,
    pub host: String,
    pub port: u16,
    pub api_keys: Vec<String>,
    pub allowed_hosts: Vec<String>,
}

fn list(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_bool(name: &str, raw: Option<String>) -> Result<bool, ConfigError> {
    match raw.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
        None => Ok(false),
        Some(s) => match s.as_str() {
            "" | "0" | "false" | "no" | "off" => Ok(false),
            "1" | "true" | "yes" | "on" => Ok(true),
            _ => Err(ConfigError(format!("{name} must be true or false"))),
        },
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("token", &self.token)
            .field("api_base", &self.api_base)
            .field("files", &self.files)
            .field("max_file_bytes", &self.max_file_bytes)
            .field("download_hosts", &self.download_hosts)
            .field("read_only", &self.read_only)
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "api_keys",
                &format_args!("[{} redacted]", self.api_keys.len()),
            )
            .field("allowed_hosts", &self.allowed_hosts)
            .finish()
    }
}

/// True for `localhost` and loopback IPs.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn check_base_url(base: &str) -> Result<(), ConfigError> {
    let url = url::Url::parse(base)
        .map_err(|_| ConfigError("SLACK_API_BASE_URL is not a valid URL".into()))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if url.host_str().is_some_and(is_loopback_host) => Ok(()),
        _ => Err(ConfigError(
            "SLACK_API_BASE_URL must use https (plain http is only allowed for localhost or \
             loopback addresses), otherwise the token would travel unencrypted"
                .into(),
        )),
    }
}

/// Minimum length of an `MCP_API_KEYS` entry.
pub const MIN_API_KEY_LEN: usize = 32;

impl Config {
    /// Reads the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Reads configuration through `get`, so tests need not touch the environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let token = get("SLACK_USER_TOKEN")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ConfigError("SLACK_USER_TOKEN is required".into()))
            .and_then(|s| Token::parse(&s))?;

        let api_base = get("SLACK_API_BASE_URL")
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string());

        check_base_url(&api_base)?;

        let files = match get("SLACK_FILES_DIR").filter(|s| !s.trim().is_empty()) {
            Some(dir) => Some(
                FileJail::new(Path::new(dir.trim()))
                    .map_err(|e| ConfigError(format!("SLACK_FILES_DIR: {e}")))?,
            ),
            None => None,
        };

        let max_file_bytes = match get("SLACK_MAX_FILE_BYTES").filter(|s| !s.trim().is_empty()) {
            Some(s) => s
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| {
                    ConfigError("SLACK_MAX_FILE_BYTES must be a positive integer".into())
                })?,
            None => DEFAULT_MAX_FILE_BYTES,
        };

        let port = match get("MCP_PORT").filter(|s| !s.trim().is_empty()) {
            Some(s) => s
                .trim()
                .parse::<u16>()
                .map_err(|_| ConfigError("MCP_PORT must be a port number".into()))?,
            None => 8787,
        };

        Ok(Self {
            token,
            api_base,
            files,
            max_file_bytes,
            download_hosts: list(get("SLACK_DOWNLOAD_HOSTS"))
                .into_iter()
                .map(|h| h.to_ascii_lowercase())
                .collect(),
            read_only: parse_bool("SLACK_READ_ONLY", get("SLACK_READ_ONLY"))?,
            host: get("MCP_HOST")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            port,
            api_keys: list(get("MCP_API_KEYS")),
            allowed_hosts: list(get("MCP_ALLOWED_HOSTS")),
        })
    }

    /// HTTP mode refuses to start without at least one API key.
    pub fn require_api_keys(&self) -> Result<(), ConfigError> {
        if self.api_keys.is_empty() {
            return Err(ConfigError(
                "MCP_API_KEYS is required in http mode: set a comma-separated list of secret \
                 API keys (for example the output of `openssl rand -hex 32`)"
                    .into(),
            ));
        }
        if self
            .api_keys
            .iter()
            .any(|k| k.chars().count() < MIN_API_KEY_LEN)
        {
            return Err(ConfigError(format!(
                "every MCP_API_KEYS entry must be at least {MIN_API_KEY_LEN} characters; \
                 generate one with `openssl rand -hex 32`"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Config::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn token_is_required_and_validated() {
        assert!(cfg(&[]).is_err());
        assert!(cfg(&[("SLACK_USER_TOKEN", "abc")]).is_err());
        assert!(cfg(&[("SLACK_USER_TOKEN", "xoxp-test-1")]).is_ok());
        assert!(cfg(&[("SLACK_USER_TOKEN", "xoxb-test-1")]).is_ok());
    }

    #[test]
    fn debug_redacts_token() {
        let c = cfg(&[("SLACK_USER_TOKEN", "xoxp-secret-value")]).expect("config");
        let shown = format!("{c:?}");
        assert!(!shown.contains("secret-value"));
        assert!(shown.contains("xoxp-***"));
    }

    #[test]
    fn config_debug_redacts_api_keys() {
        let key = "supersecretkey".repeat(3);
        let c = cfg(&[("SLACK_USER_TOKEN", "xoxp-a-b"), ("MCP_API_KEYS", &key)]).expect("config");
        let shown = format!("{c:?}");
        assert!(!shown.contains("supersecretkey"), "{shown}");
        assert!(shown.contains("1 redacted"));
    }

    #[test]
    fn defaults_and_bools() {
        let c = cfg(&[
            ("SLACK_USER_TOKEN", "xoxp-a-b"),
            ("SLACK_READ_ONLY", "true"),
        ])
        .expect("config");
        assert!(c.read_only);
        assert_eq!(c.port, 8787);
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        assert!(
            cfg(&[
                ("SLACK_USER_TOKEN", "xoxp-a-b"),
                ("SLACK_READ_ONLY", "maybe")
            ])
            .is_err()
        );
    }

    #[test]
    fn http_needs_keys() {
        let c = cfg(&[("SLACK_USER_TOKEN", "xoxp-a-b")]).expect("config");
        assert!(c.require_api_keys().is_err());
        let k1 = "a".repeat(32);
        let k2 = "b".repeat(40);
        let keys = format!(" {k1}, {k2} ");
        let c = cfg(&[("SLACK_USER_TOKEN", "xoxp-a-b"), ("MCP_API_KEYS", &keys)]).expect("config");
        assert_eq!(c.api_keys, vec![k1, k2]);
        assert!(c.require_api_keys().is_ok());
        let c =
            cfg(&[("SLACK_USER_TOKEN", "xoxp-a-b"), ("MCP_API_KEYS", "short")]).expect("config");
        assert!(c.require_api_keys().is_err());
    }

    #[test]
    fn insecure_base_url_refused() {
        let with = |u: &str| cfg(&[("SLACK_USER_TOKEN", "xoxp-a-b"), ("SLACK_API_BASE_URL", u)]);
        assert!(with("http://example.com/api").is_err());
        assert!(with("http://10.0.0.5:8080").is_err());
        assert!(with("ftp://localhost").is_err());
        assert!(with("https://proxy.example.com/api").is_ok());
        assert!(with("http://127.0.0.1:9000").is_ok());
        assert!(with("http://localhost:9000").is_ok());
        assert!(with("http://[::1]:9000").is_ok());
    }
}
