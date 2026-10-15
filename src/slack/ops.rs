//! High-level Slack operations behind each tool.

use serde_json::{Value, json};

use super::types::*;
use super::{SlackClient, SlackError};

const MAX_TEXT_CHARS: usize = 40_000;
const SCAN_PAGES: usize = 10;

/// Trims, strips one pair of colons and checks the emoji name (optional `::skin-tone-2..6`).
fn normalize_emoji_name(raw: &str) -> Result<String, SlackError> {
    let t = raw.trim();
    let t = t.strip_prefix(':').unwrap_or(t);
    let name = t.strip_suffix(':').unwrap_or(t);
    let (base, tone) = match name.split_once("::") {
        Some((b, tone)) => (b, Some(tone)),
        None => (name, None),
    };
    require(!name.is_empty(), "name must not be empty")?;
    require(
        name.chars().count() <= 100,
        "name is longer than 100 characters",
    )?;
    require(
        !base.is_empty()
            && base
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '\'')),
        "name must be an emoji name such as thumbsup or +1, without spaces or colons",
    )?;
    if let Some(tone) = tone {
        require(
            matches!(
                tone,
                "skin-tone-2" | "skin-tone-3" | "skin-tone-4" | "skin-tone-5" | "skin-tone-6"
            ),
            "skin tone suffix must be ::skin-tone-2 to ::skin-tone-6",
        )?;
    }
    Ok(name.to_string())
}

impl SlackClient {
    async fn shape_messages(&self, msgs: &[Value]) -> Vec<Value> {
        let mut out = Vec::with_capacity(msgs.len());
        let mut skip = false;
        for m in msgs {
            let name = match m.get("user").and_then(Value::as_str) {
                Some(id) => Some(self.user_name_batch(id, &mut skip).await),
                None => None,
            };
            out.push(shape_message(m, name.as_deref()));
        }
        out
    }

    /// `search.messages`.
    pub async fn search(&self, p: SearchParams) -> Result<Value, SlackError> {
        require(!p.query.trim().is_empty(), "query must not be empty")?;
        let count = range("count", p.count, 20, 100)?;
        let page = range("page", p.page, 1, 100)?;
        let sort = match p.sort.unwrap_or(SortField::Timestamp) {
            SortField::Score => "score",
            SortField::Timestamp => "timestamp",
        };
        let dir = match p.sort_dir.unwrap_or(SortDir::Desc) {
            SortDir::Asc => "asc",
            SortDir::Desc => "desc",
        };
        let v = self
            .call_form(
                "search.messages",
                &[
                    ("query", p.query),
                    ("sort", sort.into()),
                    ("sort_dir", dir.into()),
                    ("count", count.to_string()),
                    ("page", page.to_string()),
                ],
            )
            .await?;
        let msgs = &v["messages"];
        let mut matches = Vec::new();
        let mut skip = false;
        for m in msgs["matches"].as_array().map_or(&[][..], Vec::as_slice) {
            let user = m.get("user").and_then(Value::as_str);
            let user_name = match user {
                Some(id) => self.user_name_batch(id, &mut skip).await,
                None => m["username"].as_str().unwrap_or_default().to_string(),
            };
            let mut item = json!({
                "channel_id": m["channel"]["id"],
                "channel_name": m["channel"]["name"],
                "ts": m["ts"],
                "user": user,
                "user_name": user_name,
                "text": m["text"],
                "permalink": m["permalink"],
            });
            if let Some(t) = m.get("thread_ts").filter(|t| t.is_string()) {
                item["thread_ts"] = t.clone();
            }
            matches.push(item);
        }
        let paging = &msgs["paging"];
        Ok(json!({
            "matches": matches,
            "total": paging.get("total").or(msgs.get("total")).cloned().unwrap_or(Value::Null),
            "page": paging.get("page").cloned().unwrap_or(json!(page)),
            "pages": paging.get("pages").cloned().unwrap_or(Value::Null),
        }))
    }

    async fn read_messages(
        &self,
        method: &str,
        mut params: Vec<(&str, String)>,
        cursor: Option<String>,
    ) -> Result<Value, SlackError> {
        if let Some(c) = cursor.filter(|c| !c.is_empty()) {
            params.push(("cursor", c));
        }
        let v = self.call_form(method, &params).await?;
        let msgs = v["messages"].as_array().map_or(&[][..], Vec::as_slice);
        let mut out = json!({"messages": self.shape_messages(msgs).await});
        if let Some(c) = next_cursor(&v) {
            out["next_cursor"] = json!(c);
        }
        Ok(out)
    }

    /// `conversations.history`.
    pub async fn read_channel(&self, p: ReadChannelParams) -> Result<Value, SlackError> {
        require(
            valid_conversation_id(&p.channel_id),
            "channel_id must be a conversation ID like C0123456789",
        )?;
        let limit = range("limit", p.limit, 50, 200)?;
        let mut params = vec![("channel", p.channel_id), ("limit", limit.to_string())];
        for (name, val) in [("oldest", p.oldest), ("latest", p.latest)] {
            if let Some(v) = val.filter(|v| !v.is_empty()) {
                require(
                    valid_ts_bound(&v),
                    format!("{name} must be a Slack timestamp like 1700000000.000100"),
                )?;
                params.push((name, v));
            }
        }
        self.read_messages("conversations.history", params, p.cursor)
            .await
    }

    /// `conversations.replies`.
    pub async fn read_thread(&self, p: ReadThreadParams) -> Result<Value, SlackError> {
        require(
            valid_conversation_id(&p.channel_id),
            "channel_id must be a conversation ID like C0123456789",
        )?;
        require(
            valid_ts(&p.thread_ts),
            "thread_ts must look like 1700000000.000100",
        )?;
        let limit = range("limit", p.limit, 100, 200)?;
        let params = vec![
            ("channel", p.channel_id),
            ("ts", p.thread_ts),
            ("limit", limit.to_string()),
        ];
        self.read_messages("conversations.replies", params, p.cursor)
            .await
    }

    /// `chat.postMessage`, opening a DM first when given a user ID.
    pub async fn send_message(&self, p: SendMessageParams) -> Result<Value, SlackError> {
        require(!p.text.trim().is_empty(), "text must not be empty")?;
        require(
            p.text.chars().count() <= MAX_TEXT_CHARS,
            "text is longer than 40,000 characters",
        )?;
        if let Some(t) = &p.thread_ts {
            require(valid_ts(t), "thread_ts must look like 1700000000.000100")?;
        }
        let channel = if valid_conversation_id(&p.channel_id) {
            p.channel_id
        } else if valid_user_id(&p.channel_id) {
            let v = self
                .call_form("conversations.open", &[("users", p.channel_id)])
                .await?;
            v["channel"]["id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| SlackError::Http("conversations.open returned no channel".into()))?
        } else {
            return Err(SlackError::Invalid(
                "channel_id must be a conversation ID (C, G, D) or a user ID (U, W)".into(),
            ));
        };
        let mut body = json!({"channel": channel, "text": p.text});
        if let Some(t) = p.thread_ts {
            body["thread_ts"] = json!(t);
            if p.reply_broadcast == Some(true) {
                body["reply_broadcast"] = json!(true);
            }
        }
        let v = self.call_json("chat.postMessage", body).await?;
        let channel_id = v["channel"].as_str().unwrap_or(&channel).to_string();
        let ts = v["ts"].as_str().unwrap_or_default().to_string();
        let permalink = self
            .call_form(
                "chat.getPermalink",
                &[("channel", channel_id.clone()), ("message_ts", ts.clone())],
            )
            .await
            .ok()
            .and_then(|v| v["permalink"].as_str().map(str::to_string));
        Ok(json!({"channel_id": channel_id, "ts": ts, "permalink": permalink}))
    }

    /// `reactions.add`; already_reacted counts as success.
    pub async fn add_reaction(&self, p: AddReactionParams) -> Result<Value, SlackError> {
        require(
            valid_conversation_id(&p.channel_id),
            "channel_id must look like C0123456789",
        )?;
        require(valid_ts(&p.ts), "ts must look like 1700000000.000100")?;
        let name = normalize_emoji_name(&p.name)?;
        let result = self
            .call_form(
                "reactions.add",
                &[
                    ("channel", p.channel_id.clone()),
                    ("timestamp", p.ts.clone()),
                    ("name", name.clone()),
                ],
            )
            .await;
        let already_reacted = match result {
            Ok(_) => false,
            Err(SlackError::Api { error, .. }) if error == "already_reacted" => true,
            Err(e) => return Err(e),
        };
        Ok(json!({
            "channel_id": p.channel_id,
            "ts": p.ts,
            "name": name,
            "already_reacted": already_reacted,
        }))
    }

    /// `users.info`.
    pub async fn read_user_profile(&self, p: UserProfileParams) -> Result<Value, SlackError> {
        require(
            valid_user_id(&p.user_id),
            "user_id must look like U0123456789",
        )?;
        let v = self.call_form("users.info", &[("user", p.user_id)]).await?;
        Ok(shape_user(&v["user"]))
    }

    /// `conversations.list`, filtered locally by name.
    pub async fn search_channels(&self, p: SearchChannelsParams) -> Result<Value, SlackError> {
        let limit = range("limit", p.limit, 20, 100)? as usize;
        let needle = p.query.trim().to_lowercase();
        let archived = p.exclude_archived.unwrap_or(true);
        let mut found = Vec::new();
        let mut cursor: Option<String> = None;
        let mut exhausted = false;
        for _ in 0..SCAN_PAGES {
            let mut params = vec![
                ("types", "public_channel,private_channel".to_string()),
                ("exclude_archived", archived.to_string()),
                ("limit", "200".to_string()),
            ];
            if let Some(c) = cursor.take() {
                params.push(("cursor", c));
            }
            let v = self.call_form("conversations.list", &params).await?;
            for c in v["channels"].as_array().map_or(&[][..], Vec::as_slice) {
                let name = c["name"].as_str().unwrap_or_default();
                if !name.to_lowercase().contains(&needle) {
                    continue;
                }
                found.push(json!({
                    "id": c["id"],
                    "name": name,
                    "is_private": c["is_private"].as_bool().unwrap_or(false),
                    "num_members": c["num_members"],
                    "topic": truncate(c["topic"]["value"].as_str().unwrap_or_default(), 200),
                    "purpose": truncate(c["purpose"]["value"].as_str().unwrap_or_default(), 200),
                }));
                if found.len() >= limit {
                    return Ok(json!({"channels": found}));
                }
            }
            cursor = next_cursor(&v);
            if cursor.is_none() {
                exhausted = true;
                break;
            }
        }
        let mut out = json!({"channels": found});
        if !exhausted {
            out["scan_truncated"] = json!(true);
        }
        Ok(out)
    }

    /// `users.list`, filtered locally.
    pub async fn search_users(&self, p: SearchUsersParams) -> Result<Value, SlackError> {
        let limit = range("limit", p.limit, 20, 100)? as usize;
        let needle = p.query.trim().to_lowercase();
        let deleted_ok = p.include_deleted.unwrap_or(false);
        let mut found = Vec::new();
        let mut cursor: Option<String> = None;
        let mut exhausted = false;
        for _ in 0..SCAN_PAGES {
            let mut params = vec![("limit", "200".to_string())];
            if let Some(c) = cursor.take() {
                params.push(("cursor", c));
            }
            let v = self.call_form("users.list", &params).await?;
            for u in v["members"].as_array().map_or(&[][..], Vec::as_slice) {
                if !deleted_ok && u["deleted"].as_bool().unwrap_or(false) {
                    continue;
                }
                let shaped = shape_user(u);
                let hit = ["name", "real_name", "display_name"].iter().any(|k| {
                    shaped[*k]
                        .as_str()
                        .is_some_and(|s| s.to_lowercase().contains(&needle))
                });
                if !hit {
                    continue;
                }
                if let (Some(id), Some(name)) = (u["id"].as_str(), display_name(u)) {
                    self.remember_user(id, name).await;
                }
                found.push(shaped);
                if found.len() >= limit {
                    return Ok(json!({"users": found}));
                }
            }
            cursor = next_cursor(&v);
            if cursor.is_none() {
                exhausted = true;
                break;
            }
        }
        let mut out = json!({"users": found});
        if !exhausted {
            out["scan_truncated"] = json!(true);
        }
        Ok(out)
    }
}
