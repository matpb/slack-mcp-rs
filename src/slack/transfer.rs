//! File upload and download. The token is only ever sent to allowlisted hosts.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use url::Url;

use super::types::*;
use super::{SlackClient, SlackError, http_err};
use crate::config::is_loopback_host;
use crate::files::{FileError, FileJail, clean_remote_filename, sanitize_filename};

const MAX_REDIRECTS: usize = 5;
static TMP_COUNTER: AtomicU32 = AtomicU32::new(0);

fn plain_http_ok(url: &Url, host: &str) -> bool {
    url.scheme() == "https" || (url.scheme() == "http" && is_loopback_host(host))
}

fn no_userinfo(url: &Url) -> bool {
    url.username().is_empty() && url.password().is_none()
}

/// Hosts that may receive the token: `files.slack.com` or an operator-listed host.
pub fn is_auth_host(url: &Url, extra: &[String]) -> bool {
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    no_userinfo(url)
        && ((host == "files.slack.com" && url.scheme() == "https")
            || (extra.contains(&host) && plain_http_ok(url, &host)))
}

/// Hosts that may be fetched without the token: Slack domains, auth hosts, `cdn` extras.
pub fn is_cdn_host(url: &Url, extra: &[String], cdn: &[String]) -> bool {
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    is_auth_host(url, extra)
        || (no_userinfo(url)
            && ((url.scheme() == "https"
                && (host.ends_with(".slack.com") || host.ends_with(".slack-edge.com")))
                || (cdn.contains(&host) && plain_http_ok(url, &host))))
}

impl SlackClient {
    fn checked_url(&self, raw: &str, what: &str, initial: bool) -> Result<Url, SlackError> {
        let url = Url::parse(raw)
            .map_err(|_| SlackError::Refused(format!("{what} is not a valid URL")))?;
        let ok = if initial {
            is_auth_host(&url, &self.extra_hosts)
        } else {
            is_cdn_host(&url, &self.extra_hosts, &self.cdn_hosts)
        };
        if !ok {
            return Err(SlackError::Refused(format!(
                "refusing {what} on host `{}`: not an allowed Slack host, and the token was not sent",
                url.host_str().unwrap_or("?")
            )));
        }
        Ok(url)
    }

    /// Three-step external upload: get URL, POST bytes (no auth header), complete.
    pub async fn upload_file(
        &self,
        jail: Option<&FileJail>,
        p: UploadFileParams,
    ) -> Result<Value, SlackError> {
        let jail = jail.ok_or(SlackError::FilesDisabled)?;
        require(
            valid_conversation_id(&p.channel_id),
            "channel_id must be a conversation ID like C0123456789",
        )?;
        if let Some(t) = &p.thread_ts {
            require(valid_ts(t), "thread_ts must look like 1700000000.000100")?;
        }
        let path = jail.resolve_read(&p.path)?;
        let size = tokio::fs::metadata(&path)
            .await
            .map_err(|e| SlackError::Invalid(format!("cannot stat file: {e}")))?
            .len();
        require(size > 0, "file is empty; Slack rejects empty uploads")?;
        require(
            size <= self.max_file_bytes,
            format!(
                "file is {size} bytes, above SLACK_MAX_FILE_BYTES ({})",
                self.max_file_bytes
            ),
        )?;
        let filename = p
            .filename
            .as_deref()
            .filter(|f| !f.trim().is_empty())
            .map(clean_remote_filename)
            .or_else(|| {
                path.file_name()
                    .map(|n| clean_remote_filename(&n.to_string_lossy()))
            })
            .unwrap_or_else(|| "file".into());
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| SlackError::Invalid(format!("cannot read file: {e}")))?;
        require(
            bytes.len() as u64 <= self.max_file_bytes,
            "file grew past SLACK_MAX_FILE_BYTES while reading",
        )?;

        let v = self
            .call_form(
                "files.getUploadURLExternal",
                &[
                    ("filename", filename.clone()),
                    ("length", bytes.len().to_string()),
                ],
            )
            .await?;
        let upload_url = v["upload_url"].as_str().unwrap_or_default();
        let file_id = v["file_id"]
            .as_str()
            .ok_or_else(|| SlackError::Http("getUploadURLExternal returned no file_id".into()))?
            .to_string();
        let url = self.checked_url(upload_url, "upload URL", false)?;
        let resp = self
            .xfer_http
            .post(url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|e| http_err(&e))?;
        if !resp.status().is_success() {
            return Err(SlackError::Http(format!(
                "Slack upload endpoint answered HTTP {}",
                resp.status()
            )));
        }

        let mut body = json!({
            "files": [{"id": file_id, "title": p.title.clone().unwrap_or(filename)}],
            "channel_id": p.channel_id,
        });
        if let Some(c) = p.initial_comment.filter(|c| !c.is_empty()) {
            body["initial_comment"] = json!(c);
        }
        if let Some(t) = p.thread_ts {
            body["thread_ts"] = json!(t);
        }
        let done = self.call_json("files.completeUploadExternal", body).await?;
        let file = &done["files"][0];
        Ok(json!({
            "id": file["id"].as_str().unwrap_or(&file_id),
            "permalink": file["permalink"],
        }))
    }

    /// `files.info`, then a host-checked, size-capped streaming download into the jail.
    pub async fn download_file(
        &self,
        jail: Option<&FileJail>,
        p: DownloadFileParams,
    ) -> Result<Value, SlackError> {
        let jail = jail.ok_or(SlackError::FilesDisabled)?;
        require(
            valid_file_id(&p.file_id),
            "file_id must look like F0123456789",
        )?;
        let info = self
            .call_form("files.info", &[("file", p.file_id.clone())])
            .await?;
        let file = &info["file"];
        if file["is_external"].as_bool() == Some(true) {
            return Err(SlackError::Refused(
                "external files are hosted outside Slack; open the link instead".into(),
            ));
        }
        let name = file["name"].as_str().unwrap_or("file").to_string();
        let mimetype = file["mimetype"].as_str().unwrap_or_default().to_string();
        let raw_url = file["url_private_download"]
            .as_str()
            .or_else(|| file["url_private"].as_str())
            .ok_or_else(|| {
                SlackError::Invalid("Slack returned no download URL for this file".into())
            })?;
        if let Some(size) = file["size"].as_u64() {
            require(
                size <= self.max_file_bytes,
                format!(
                    "file is {size} bytes, above SLACK_MAX_FILE_BYTES ({})",
                    self.max_file_bytes
                ),
            )?;
        }
        let mut url = self.checked_url(raw_url, "download URL", true)?;

        let dest_input = p
            .dest_path
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| format!("{}-{}", p.file_id, sanitize_filename(&name)));
        let overwrite = p.overwrite == Some(true);
        let dest = if overwrite {
            jail.resolve_write_overwrite(&dest_input)?
        } else {
            jail.resolve_write(&dest_input)?
        };

        let mut hops = 0;
        let resp = loop {
            let mut req = self.xfer_http.get(url.clone());
            if is_auth_host(&url, &self.extra_hosts) {
                req = req.bearer_auth(self.token.expose());
            }
            let resp = req.send().await.map_err(|e| http_err(&e))?;
            if !resp.status().is_redirection() {
                break resp;
            }
            hops += 1;
            let next = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url.join(l).ok())
                .filter(|_| hops <= MAX_REDIRECTS)
                .ok_or_else(|| SlackError::Refused("bad or excessive redirect".into()))?;
            url = self.checked_url(next.as_str(), "redirect target", false)?;
        };
        if !resp.status().is_success() {
            return Err(SlackError::Http(format!(
                "file download answered HTTP {}",
                resp.status()
            )));
        }
        let is_html = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.to_ascii_lowercase().starts_with("text/html"));
        if is_html && !mimetype.starts_with("text/html") {
            return Err(SlackError::Refused(
                "Slack answered with an HTML page instead of the file: the token probably lacks \
                 the `files:read` scope"
                    .into(),
            ));
        }
        let declared = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(n) = declared {
            require(
                n <= self.max_file_bytes,
                format!("download is {n} bytes, above SLACK_MAX_FILE_BYTES"),
            )?;
        }

        let tmp = temp_path(&dest);
        let written = match stream_to(resp, &tmp, self.max_file_bytes).await {
            Ok(n) => n,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e);
            }
        };
        finalize(&tmp, &dest, overwrite, &dest_input).await?;
        Ok(json!({
            "path": dest.display().to_string(),
            "size_bytes": written,
            "mimetype": mimetype,
            "name": name,
        }))
    }
}

/// Moves `tmp` to `dest`, removing `tmp` on every path. Without `overwrite` an existing
/// destination is never replaced: hard link first, then an exclusive-create copy as fallback.
async fn finalize(tmp: &Path, dest: &Path, overwrite: bool, input: &str) -> Result<(), SlackError> {
    let exists = || SlackError::File(FileError::Exists(input.to_string()));
    let fail = |e: std::io::Error| SlackError::Invalid(format!("cannot finalize download: {e}"));
    let result = if overwrite {
        tokio::fs::rename(tmp, dest).await.map_err(fail)
    } else {
        match tokio::fs::hard_link(tmp, dest).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(exists()),
            Err(_) => copy_exclusive(tmp, dest).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    exists()
                } else {
                    fail(e)
                }
            }),
        }
    };
    let _ = tokio::fs::remove_file(tmp).await;
    result
}

async fn copy_exclusive(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    let mut src = tokio::fs::File::open(tmp).await?;
    let mut out = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .await?;
    if let Err(e) = tokio::io::copy(&mut src, &mut out).await {
        drop(out);
        let _ = tokio::fs::remove_file(dest).await;
        return Err(e);
    }
    out.flush().await
}

fn temp_path(dest: &Path) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(".{name}.{}.{n}.part", std::process::id()))
}

async fn stream_to(mut resp: reqwest::Response, tmp: &Path, cap: u64) -> Result<u64, SlackError> {
    let io = |e: std::io::Error| SlackError::Invalid(format!("cannot write download: {e}"));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .await
        .map_err(io)?;
    let mut total: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| http_err(&e))? {
        total += chunk.len() as u64;
        if total > cap {
            return Err(SlackError::Invalid(format!(
                "download exceeded SLACK_MAX_FILE_BYTES ({cap}); partial file deleted"
            )));
        }
        file.write_all(&chunk).await.map_err(io)?;
    }
    file.flush().await.map_err(io)?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_config;
    use super::*;
    use crate::config::Config;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn scratch(label: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "slack-mcp-xfer-{label}-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    async fn mock_info(api: &MockServer, url: &str, size: u64) {
        Mock::given(method("POST"))
            .and(path("/files.info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "file": {"id": "F0123456789", "name": "a b.txt", "mimetype": "text/plain",
                          "size": size, "url_private_download": url}
            })))
            .mount(api)
            .await;
    }

    #[test]
    fn host_rules() {
        let u = |s: &str| Url::parse(s).expect("url");
        let ex = ["127.0.0.1".to_string()];
        assert!(is_auth_host(&u("https://files.slack.com/x"), &[]));
        assert!(!is_auth_host(&u("https://team.slack.com/x"), &[]));
        assert!(!is_auth_host(&u("http://files.slack.com/x"), &[]));
        assert!(!is_auth_host(&u("https://files.slack.com@evil.net/x"), &[]));
        assert!(!is_auth_host(&u("https://u:p@files.slack.com/x"), &[]));
        assert!(is_cdn_host(&u("https://team.slack.com/x"), &[], &[]));
        assert!(is_cdn_host(&u("https://a.slack-edge.com/x"), &[], &[]));
        assert!(!is_cdn_host(&u("https://evilslack.com/x"), &[], &[]));
        assert!(!is_cdn_host(&u("https://slack.com.evil.net/x"), &[], &[]));
        assert!(is_auth_host(&u("http://127.0.0.1:1/x"), &ex));
        assert!(!is_auth_host(&u("http://localhost:1/x"), &ex));
        let remote = ["files.example.org".to_string()];
        assert!(is_auth_host(&u("https://files.example.org/x"), &remote));
        assert!(!is_auth_host(&u("http://files.example.org/x"), &remote));
    }

    #[tokio::test]
    async fn download_refuses_non_slack_host() {
        let api = MockServer::start().await;
        let evil = MockServer::start().await;
        mock_info(&api, &format!("{}/f", evil.uri()), 3).await;
        let dir = scratch("refuse");
        let cfg = test_config(
            &api.uri(),
            &[("SLACK_FILES_DIR", dir.to_str().expect("utf8"))],
        );
        let client = SlackClient::new(&cfg).expect("client");
        let err = client
            .download_file(
                cfg.files.as_ref(),
                DownloadFileParams {
                    file_id: "F0123456789".into(),
                    dest_path: None,
                    overwrite: None,
                },
            )
            .await
            .expect_err("must refuse");
        assert!(matches!(err, SlackError::Refused(_)), "{err}");
        assert!(evil.received_requests().await.expect("reqs").is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn download_redirect_to_unlisted_host_is_refused() {
        let api = MockServer::start().await;
        let files = MockServer::start().await;
        let port = files.address().port();
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("http://localhost:{port}/final")),
            )
            .mount(&files)
            .await;
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"abc".to_vec()))
            .mount(&files)
            .await;
        mock_info(&api, &format!("{}/start", files.uri()), 3).await;
        let dir = scratch("redirect");
        let cfg = test_config(
            &api.uri(),
            &[
                ("SLACK_FILES_DIR", dir.to_str().expect("utf8")),
                ("SLACK_DOWNLOAD_HOSTS", "127.0.0.1"),
            ],
        );
        let client = SlackClient::new(&cfg).expect("client");
        let err = client
            .download_file(
                cfg.files.as_ref(),
                DownloadFileParams {
                    file_id: "F0123456789".into(),
                    dest_path: None,
                    overwrite: None,
                },
            )
            .await
            .expect_err("must refuse");
        assert!(matches!(err, SlackError::Refused(_)), "{err}");
        let reqs = files.received_requests().await.expect("reqs");
        assert!(reqs.iter().all(|r| r.url.path() == "/start"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn download_writes_file_and_enforces_cap() {
        let api = MockServer::start().await;
        let files = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/f"))
            .and(header("authorization", "Bearer xoxp-test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&files)
            .await;
        mock_info(&api, &format!("{}/f", files.uri()), 5).await;
        let dir = scratch("ok");
        let cfg = test_config(
            &api.uri(),
            &[
                ("SLACK_FILES_DIR", dir.to_str().expect("utf8")),
                ("SLACK_DOWNLOAD_HOSTS", "127.0.0.1"),
            ],
        );
        let client = SlackClient::new(&cfg).expect("client");
        let out = client
            .download_file(
                cfg.files.as_ref(),
                DownloadFileParams {
                    file_id: "F0123456789".into(),
                    dest_path: Some("sub/out.txt".into()),
                    overwrite: None,
                },
            )
            .await
            .expect("download");
        assert_eq!(out["size_bytes"], 5);
        assert_eq!(
            std::fs::read(dir.join("sub/out.txt")).expect("read"),
            b"hello"
        );

        let cfg = test_config(
            &api.uri(),
            &[
                ("SLACK_FILES_DIR", dir.to_str().expect("utf8")),
                ("SLACK_DOWNLOAD_HOSTS", "127.0.0.1"),
                ("SLACK_MAX_FILE_BYTES", "3"),
            ],
        );
        let client = SlackClient::new(&cfg).expect("client");
        let err = client
            .download_file(
                cfg.files.as_ref(),
                DownloadFileParams {
                    file_id: "F0123456789".into(),
                    dest_path: Some("big.txt".into()),
                    overwrite: None,
                },
            )
            .await
            .expect_err("over cap");
        assert!(err.to_string().contains("SLACK_MAX_FILE_BYTES"), "{err}");
        assert!(!dir.join("big.txt").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    fn dl_cfg(api: &MockServer, dir: &std::path::Path) -> Config {
        test_config(
            &api.uri(),
            &[
                ("SLACK_FILES_DIR", dir.to_str().expect("utf8")),
                ("SLACK_DOWNLOAD_HOSTS", "127.0.0.1"),
            ],
        )
    }

    async fn serve_file(files: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/f"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"new".to_vec()))
            .mount(files)
            .await;
    }

    fn params(dest: &str, overwrite: Option<bool>) -> DownloadFileParams {
        DownloadFileParams {
            file_id: "F0123456789".into(),
            dest_path: Some(dest.into()),
            overwrite,
        }
    }

    #[tokio::test]
    async fn download_refuses_existing_file() {
        let api = MockServer::start().await;
        let files = MockServer::start().await;
        serve_file(&files).await;
        mock_info(&api, &format!("{}/f", files.uri()), 3).await;
        let dir = scratch("exists");
        std::fs::write(dir.join("keep.txt"), b"original").expect("write");
        let cfg = dl_cfg(&api, &dir);
        let client = SlackClient::new(&cfg).expect("client");
        let err = client
            .download_file(cfg.files.as_ref(), params("keep.txt", None))
            .await
            .expect_err("must refuse");
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(
            std::fs::read(dir.join("keep.txt")).expect("read"),
            b"original"
        );
        assert!(files.received_requests().await.expect("reqs").is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn download_does_not_clobber_file_created_during_download() {
        let api = MockServer::start().await;
        let files = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/f"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"new".to_vec())
                    .set_delay(std::time::Duration::from_millis(1500)),
            )
            .mount(&files)
            .await;
        mock_info(&api, &format!("{}/f", files.uri()), 3).await;
        let dir = scratch("race");
        let cfg = dl_cfg(&api, &dir);
        let client = SlackClient::new(&cfg).expect("client");
        let target = dir.join("out.txt");
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            std::fs::write(target, b"precious").expect("write");
        });
        let err = client
            .download_file(cfg.files.as_ref(), params("out.txt", None))
            .await
            .expect_err("must not clobber");
        writer.await.expect("writer");
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(
            std::fs::read(dir.join("out.txt")).expect("read"),
            b"precious"
        );
        let leftovers = std::fs::read_dir(&dir).expect("dir").count();
        assert_eq!(leftovers, 1, "temp file left behind");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn copy_fallback_is_exclusive() {
        let dir = scratch("copy");
        std::fs::write(dir.join("tmp"), b"data").expect("write");
        std::fs::write(dir.join("taken"), b"keep").expect("write");
        let err = copy_exclusive(&dir.join("tmp"), &dir.join("taken"))
            .await
            .expect_err("exists");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        copy_exclusive(&dir.join("tmp"), &dir.join("fresh"))
            .await
            .expect("copy");
        assert_eq!(std::fs::read(dir.join("fresh")).expect("read"), b"data");
        assert_eq!(std::fs::read(dir.join("taken")).expect("read"), b"keep");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn download_overwrite_true_replaces() {
        let api = MockServer::start().await;
        let files = MockServer::start().await;
        serve_file(&files).await;
        mock_info(&api, &format!("{}/f", files.uri()), 3).await;
        let dir = scratch("overwrite");
        std::fs::write(dir.join("keep.txt"), b"original").expect("write");
        let cfg = dl_cfg(&api, &dir);
        let client = SlackClient::new(&cfg).expect("client");
        client
            .download_file(cfg.files.as_ref(), params("keep.txt", Some(true)))
            .await
            .expect("overwrite");
        assert_eq!(std::fs::read(dir.join("keep.txt")).expect("read"), b"new");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn download_refuses_external_file() {
        let api = MockServer::start().await;
        let files = MockServer::start().await;
        serve_file(&files).await;
        Mock::given(method("POST"))
            .and(path("/files.info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "file": {"id": "F0123456789", "name": "x", "is_external": true,
                          "url_private_download": format!("{}/f", files.uri())}
            })))
            .mount(&api)
            .await;
        let dir = scratch("external");
        let cfg = dl_cfg(&api, &dir);
        let client = SlackClient::new(&cfg).expect("client");
        let err = client
            .download_file(cfg.files.as_ref(), params("x.bin", None))
            .await
            .expect_err("must refuse");
        assert!(err.to_string().contains("hosted outside Slack"), "{err}");
        assert!(files.received_requests().await.expect("reqs").is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn download_token_only_to_files_host() {
        let api = MockServer::start().await;
        let files = MockServer::start().await;
        let port = files.address().port();
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("http://localhost:{port}/final")),
            )
            .mount(&files)
            .await;
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"abc".to_vec()))
            .mount(&files)
            .await;
        mock_info(&api, &format!("{}/start", files.uri()), 3).await;
        let dir = scratch("cdn");
        let cfg = dl_cfg(&api, &dir);
        let mut client = SlackClient::new(&cfg).expect("client");
        client.cdn_hosts = vec!["localhost".into()];
        client
            .download_file(cfg.files.as_ref(), params("out.bin", None))
            .await
            .expect("download");
        let reqs = files.received_requests().await.expect("reqs");
        let start = reqs
            .iter()
            .find(|r| r.url.path() == "/start")
            .expect("start");
        let fin = reqs
            .iter()
            .find(|r| r.url.path() == "/final")
            .expect("final");
        assert!(start.headers.get("authorization").is_some());
        assert!(fin.headers.get("authorization").is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn upload_sends_no_auth_to_upload_url() {
        let api = MockServer::start().await;
        let up = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&up)
            .await;
        Mock::given(method("POST"))
            .and(path("/files.getUploadURLExternal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "file_id": "F0123456789", "upload_url": format!("{}/upload", up.uri())
            })))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/files.completeUploadExternal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "files": [{"id": "F0123456789", "permalink": "https://example.invalid/f"}]
            })))
            .mount(&api)
            .await;
        let dir = scratch("upload");
        std::fs::write(dir.join("note.txt"), b"payload").expect("write");
        let cfg = test_config(
            &api.uri(),
            &[
                ("SLACK_FILES_DIR", dir.to_str().expect("utf8")),
                ("SLACK_DOWNLOAD_HOSTS", "127.0.0.1"),
            ],
        );
        let client = SlackClient::new(&cfg).expect("client");
        let out = client
            .upload_file(
                cfg.files.as_ref(),
                UploadFileParams {
                    channel_id: "C0123456789".into(),
                    path: "note.txt".into(),
                    filename: None,
                    title: None,
                    initial_comment: None,
                    thread_ts: None,
                },
            )
            .await
            .expect("upload");
        assert_eq!(out["id"], "F0123456789");
        let reqs = up.received_requests().await.expect("reqs");
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].headers.get("authorization").is_none());
        assert_eq!(reqs[0].body, b"payload");
        std::fs::remove_dir_all(dir).ok();
    }
}
