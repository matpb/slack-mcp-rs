//! Tool parameter types, ID validation and output shaping.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::SlackError;

fn id_with(id: &str, prefixes: &[char]) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| prefixes.contains(&c))
        && id.len() >= 3
        && id.len() <= 32
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Channel, group or DM conversation ID (`C`, `G`, `D`).
pub fn valid_conversation_id(id: &str) -> bool {
    id_with(id, &['C', 'G', 'D'])
}

/// User ID (`U`, `W`).
pub fn valid_user_id(id: &str) -> bool {
    id_with(id, &['U', 'W'])
}

/// File ID (`F`).
pub fn valid_file_id(id: &str) -> bool {
    id_with(id, &['F'])
}

/// Message timestamp such as `1700000000.000100`.
pub fn valid_ts(ts: &str) -> bool {
    match ts.split_once('.') {
        Some((a, b)) => {
            !a.is_empty()
                && !b.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.bytes().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Accepts `valid_ts` or a bare integer, for `oldest` and `latest` bounds.
pub fn valid_ts_bound(ts: &str) -> bool {
    valid_ts(ts) || (!ts.is_empty() && ts.bytes().all(|c| c.is_ascii_digit()))
}

pub(crate) fn require(ok: bool, msg: impl Into<String>) -> Result<(), SlackError> {
    if ok {
        Ok(())
    } else {
        Err(SlackError::Invalid(msg.into()))
    }
}

pub(crate) fn range(name: &str, v: Option<u32>, default: u32, max: u32) -> Result<u32, SlackError> {
    let v = v.unwrap_or(default);
    require(
        (1..=max).contains(&v),
        format!("{name} must be between 1 and {max}"),
    )?;
    Ok(v)
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SortField {
    Score,
    Timestamp,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SortDir {
    Asc,
    Desc,
}

/// Parameters of `slack_search`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Slack search syntax, e.g. `in:#general from:@alice after:2026-01-01`.
    pub query: String,
    /// `score` or `timestamp` (default `timestamp`).
    pub sort: Option<SortField>,
    /// `asc` or `desc` (default `desc`).
    pub sort_dir: Option<SortDir>,
    /// Results per page, 1-100 (default 20).
    pub count: Option<u32>,
    /// Page number, starting at 1 (default 1).
    pub page: Option<u32>,
}

/// Parameters of `slack_read_channel`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadChannelParams {
    /// Conversation ID such as `C0123456789`.
    pub channel_id: String,
    /// Messages to return, 1-200 (default 50).
    pub limit: Option<u32>,
    /// Only messages after this Slack timestamp.
    pub oldest: Option<String>,
    /// Only messages before this Slack timestamp.
    pub latest: Option<String>,
    /// Pagination cursor from a previous `next_cursor`.
    pub cursor: Option<String>,
}

/// Parameters of `slack_read_thread`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadThreadParams {
    /// Conversation ID such as `C0123456789`.
    pub channel_id: String,
    /// Timestamp of the thread's parent message.
    pub thread_ts: String,
    /// Messages to return, 1-200 (default 100).
    pub limit: Option<u32>,
    /// Pagination cursor from a previous `next_cursor`.
    pub cursor: Option<String>,
}

/// Parameters of `slack_send_message`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageParams {
    /// Conversation ID (`C…`, `G…`, `D…`) or a user ID (`U…`, `W…`) to DM.
    pub channel_id: String,
    /// Message text in Slack mrkdwn, at most 40,000 characters.
    pub text: String,
    /// Reply in the thread whose parent has this timestamp.
    pub thread_ts: Option<String>,
    /// Also show a thread reply in the channel.
    pub reply_broadcast: Option<bool>,
}

/// Parameters of `slack_upload_file`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UploadFileParams {
    /// Conversation ID to share the file in.
    pub channel_id: String,
    /// File path relative to `SLACK_FILES_DIR`, or absolute inside it.
    pub path: String,
    /// Name shown in Slack (default: the file's base name).
    pub filename: Option<String>,
    /// Title shown in Slack.
    pub title: Option<String>,
    /// Message posted with the file.
    pub initial_comment: Option<String>,
    /// Share into the thread whose parent has this timestamp.
    pub thread_ts: Option<String>,
}

/// Parameters of `slack_download_file`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DownloadFileParams {
    /// Slack file ID such as `F0123456789`.
    pub file_id: String,
    /// Destination relative to `SLACK_FILES_DIR` (default `<file_id>-<name>`).
    pub dest_path: Option<String>,
    /// Replace an existing file at the destination (default false: refuse).
    pub overwrite: Option<bool>,
}

/// Parameters of `slack_read_user_profile`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UserProfileParams {
    /// User ID such as `U0123456789`.
    pub user_id: String,
}

/// Parameters of `slack_search_channels`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchChannelsParams {
    /// Case-insensitive substring of the channel name.
    pub query: String,
    /// Skip archived channels (default true).
    pub exclude_archived: Option<bool>,
    /// Maximum results, 1-100 (default 20).
    pub limit: Option<u32>,
}

/// Parameters of `slack_search_users`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchUsersParams {
    /// Case-insensitive substring of name, real name or display name.
    pub query: String,
    /// Include deactivated users (default false).
    pub include_deleted: Option<bool>,
    /// Maximum results, 1-100 (default 20).
    pub limit: Option<u32>,
}

fn nonempty(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

/// Best human name for a Slack `user` object.
pub fn display_name(user: &Value) -> Option<String> {
    let profile = &user["profile"];
    [
        &profile["display_name"],
        &profile["real_name"],
        &user["real_name"],
        &user["name"],
    ]
    .into_iter()
    .find_map(nonempty)
    .map(str::to_string)
}

/// Compact user record shared by the profile and user-search tools.
pub fn shape_user(u: &Value) -> Value {
    let profile = &u["profile"];
    let real_name = nonempty(&u["real_name"]).or_else(|| nonempty(&profile["real_name"]));
    json!({
        "id": u["id"],
        "name": u["name"],
        "real_name": real_name,
        "display_name": nonempty(&profile["display_name"]),
        "title": nonempty(&profile["title"]),
        "tz": u["tz"],
        "is_bot": u["is_bot"].as_bool().unwrap_or(false),
        "deleted": u["deleted"].as_bool().unwrap_or(false),
    })
}

/// Compact file record.
pub fn shape_file(f: &Value) -> Value {
    json!({"id": f["id"], "name": f["name"], "mimetype": f["mimetype"], "size": f["size"]})
}

/// Message with user IDs unresolved; the caller adds `user_name`.
pub fn shape_message(m: &Value, user_name: Option<&str>) -> Value {
    let mut out = Map::new();
    out.insert("ts".into(), m["ts"].clone());
    if let Some(u) = m.get("user").filter(|u| u.is_string()) {
        out.insert("user".into(), u.clone());
        out.insert("user_name".into(), json!(user_name.unwrap_or_default()));
    }
    out.insert("text".into(), m["text"].clone());
    for k in ["thread_ts", "reply_count", "subtype"] {
        if let Some(v) = m.get(k).filter(|v| !v.is_null()) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(files) = m["files"].as_array().filter(|a| !a.is_empty()) {
        out.insert(
            "files".into(),
            Value::Array(files.iter().map(shape_file).collect()),
        );
    }
    Value::Object(out)
}

/// `response_metadata.next_cursor` when non-empty.
pub fn next_cursor(v: &Value) -> Option<String> {
    nonempty(&v["response_metadata"]["next_cursor"]).map(str::to_string)
}

/// Truncates to `max` characters.
pub fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_validation() {
        assert!(valid_conversation_id("C0123456789"));
        assert!(valid_conversation_id("D0123456789"));
        assert!(!valid_conversation_id("U0123456789"));
        assert!(!valid_conversation_id("c0123456789"));
        assert!(!valid_conversation_id("C"));
        assert!(!valid_conversation_id("C01 23"));
        assert!(valid_user_id("W0123456789"));
        assert!(valid_file_id("F0123456789"));
        assert!(valid_ts("1700000000.000100"));
        assert!(!valid_ts("1700000000"));
        assert!(!valid_ts("17.0a"));
        assert!(valid_ts_bound("0"));
    }
}
