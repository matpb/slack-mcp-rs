//! `Host` header allowlist (DNS-rebinding guard) for HTTP mode.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const LOOPBACK: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// One allowlist entry; `port: None` matches any port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedHost {
    pub host: String,
    pub port: Option<u16>,
}

/// Splits `host`, `host:port`, `[v6]` or `[v6]:port`; the host is lowercased.
pub fn parse_authority(authority: &str) -> (String, Option<u16>) {
    let a = authority.trim();
    if let Some(rest) = a.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let port = rest[end + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok());
        let inner = &rest[..end];
        let host = inner
            .parse::<std::net::IpAddr>()
            .map_or_else(|_| inner.to_ascii_lowercase(), |ip| ip.to_string());
        return (host, port);
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h.trim_end_matches('.').to_ascii_lowercase(), p.parse().ok())
        }
        _ => (a.trim_end_matches('.').to_ascii_lowercase(), None),
    }
}

/// Loopback names are always allowed; the bind host and `extra` entries are added.
pub fn build_allowlist(bind_host: &str, extra: &[String]) -> Vec<AllowedHost> {
    let mut out: Vec<AllowedHost> = LOOPBACK
        .iter()
        .map(|h| AllowedHost {
            host: (*h).to_string(),
            port: None,
        })
        .collect();
    let trimmed = bind_host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let bind = match trimmed.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => parse_authority(bind_host).0,
    };
    if !bind.is_empty() {
        out.push(AllowedHost {
            host: bind,
            port: None,
        });
    }
    for e in extra {
        let (host, port) = parse_authority(e);
        if !host.is_empty() {
            out.push(AllowedHost { host, port });
        }
    }
    out
}

/// True when the `Host` value matches the allowlist.
pub fn host_allowed(authority: &str, allowed: &[AllowedHost]) -> bool {
    let (host, port) = parse_authority(authority);
    !host.is_empty()
        && allowed
            .iter()
            .any(|a| a.host == host && (a.port.is_none() || a.port == port))
}

/// `host[:port]` of an `Origin` value, or `None` when it has no usable host (e.g. `null`).
fn origin_authority(origin: &str) -> Option<String> {
    let url = url::Url::parse(origin).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    })
}

/// Middleware answering 403 for a missing or unlisted `Host`, or an `Origin` whose host is unlisted.
pub async fn require_allowed_host(
    State(allowed): State<Arc<Vec<AllowedHost>>>,
    req: Request,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    let origin_ok = req
        .headers()
        .get(axum::http::header::ORIGIN)
        .is_none_or(|o| {
            o.to_str()
                .ok()
                .and_then(origin_authority)
                .is_some_and(|a| host_allowed(&a, &allowed))
        });
    if origin_ok && host.is_some_and(|h| host_allowed(h, &allowed)) {
        return next.run(req).await;
    }
    tracing::warn!("rejected request with disallowed Host or Origin header");
    (
        StatusCode::FORBIDDEN,
        "forbidden host or origin: add the host to MCP_ALLOWED_HOSTS",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_any_port_and_bind_host() {
        let a = build_allowlist("10.0.0.5", &["mcp.example.org:443".into()]);
        assert!(host_allowed("localhost:9999", &a));
        assert!(host_allowed("127.0.0.1", &a));
        assert!(host_allowed("[::1]:8787", &a));
        assert!(host_allowed("10.0.0.5:8787", &a));
        assert!(host_allowed("MCP.example.org:443", &a));
        assert!(!host_allowed("mcp.example.org:80", &a));
        assert!(!host_allowed("evil.example.net", &a));
        assert!(!host_allowed("", &a));
    }

    #[test]
    fn ipv6_bind_host_is_allowed() {
        let a = build_allowlist("2001:db8::5", &["[2001:db8::7]:8443".into()]);
        assert!(host_allowed("[2001:db8::5]:8787", &a));
        assert!(host_allowed("[2001:0db8:0:0:0:0:0:5]", &a));
        assert!(host_allowed("[2001:db8::7]:8443", &a));
        assert!(!host_allowed("[2001:db8::7]:80", &a));
        assert!(!host_allowed("[2001:db8::9]", &a));
        assert!(
            build_allowlist("[::1]", &[])
                .iter()
                .any(|h| h.host == "::1")
        );
    }

    #[test]
    fn origin_parsing() {
        assert_eq!(
            origin_authority("http://localhost:3000").as_deref(),
            Some("localhost:3000")
        );
        assert_eq!(origin_authority("null"), None);
    }
}
