# slack-mcp

[![MSRV](https://img.shields.io/badge/MSRV-1.88-orange)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/Rust-edition%202024-orange?logo=rust)](https://doc.rust-lang.org/edition-guide/)
[![MCP server](https://img.shields.io/badge/MCP-server-8A2BE2)](https://modelcontextprotocol.io)

A Model Context Protocol (MCP) server for Slack, written in Rust. It authenticates with a Slack **user token** (`xoxp-...`), so everything an AI client does through it (searching, reading, posting, uploading) happens **as you**, the person who installed the Slack app, with exactly the access you have. It speaks MCP over stdio (default) or streamable HTTP.

## Features

| Tool | Slack method | What it does |
|---|---|---|
| `slack_search` | `search.messages` | Search messages with Slack search syntax (`in:#general from:@alice after:2026-01-01`). |
| `slack_read_channel` | `conversations.history` | Read recent messages of a channel, private channel, or DM. Messages include their `reactions` (name, count, user IDs). |
| `slack_read_thread` | `conversations.replies` | Read a whole thread. Messages include their `reactions`. |
| `slack_send_message` | `chat.postMessage` | Post or reply in a thread; a user ID opens a DM first. Text is sent as-is, with no signature. |
| `slack_add_reaction` | `reactions.add` | Add an emoji reaction to a message. Reacting twice is not an error (`already_reacted: true`). |
| `slack_upload_file` | `files.getUploadURLExternal`, `files.completeUploadExternal` | Upload a file from `SLACK_FILES_DIR`. |
| `slack_download_file` | `files.info` | Download a Slack file into `SLACK_FILES_DIR`. Refuses to overwrite unless `overwrite=true` (marked destructive). |
| `slack_read_user_profile` | `users.info` | Public profile fields (no email, no phone). |
| `slack_search_channels` | `conversations.list` | Find channels by name substring. |
| `slack_search_users` | `users.list` | Find users by name, real name, or display name. |

User IDs in results come with a resolved `user_name` (cached in memory, falling back to the ID). Results are compact JSON, with `next_cursor` when there is more.

## Install

Pick one. Both give you a single `slack-mcp` binary. Rust 1.88 or newer is required.

**With cargo.** The binary lands in `~/.cargo/bin/slack-mcp`:

```bash
cargo install --locked --git https://github.com/matpb/slack-mcp-rs
```

**From source:**

```bash
git clone https://github.com/matpb/slack-mcp-rs
cd slack-mcp-rs
cargo build --release   # binary: target/release/slack-mcp
```

Windows: build from source. It is untested there.

## Quick start

1. **Create the Slack app.** Go to <https://api.slack.com/apps>, choose **Create New App**, then **From a manifest**, pick your workspace, and paste [`slack-app-manifest.yaml`](slack-app-manifest.yaml).
2. **Install it.** Open the app's settings and click **Install to Workspace** (approve the scopes).
3. **Copy the token.** Under **OAuth & Permissions**, copy the **User OAuth Token** (it starts with `xoxp-`).
4. **Install the binary** (see above) and **register it with your MCP client** (next section).

### Why an app, even for a user token

Every Slack token belongs to an app. This manifest requests only **user** scopes and defines no bot user, so the app has no identity of its own: every call is made with your token and shows up in Slack as you. Bot tokens (`xoxb-`) are accepted by the parser, but `search.messages` does not work with them, and posts would appear from a bot. A user token is the intended mode.

### Scopes

| Scope | Why it is needed |
|---|---|
| `search:read` | `slack_search` |
| `channels:history`, `groups:history`, `im:history`, `mpim:history` | Read messages and threads in public channels, private channels, DMs, and group DMs |
| `channels:read`, `groups:read`, `im:read`, `mpim:read` | Look up and list conversations (`slack_search_channels`, channel metadata) |
| `im:write` | Open a DM when `slack_send_message` gets a user ID |
| `users:read` | `slack_read_user_profile`, `slack_search_users`, name resolution |
| `chat:write` | `slack_send_message` |
| `reactions:write` | `slack_add_reaction` |
| `files:read` | `slack_download_file` (without it Slack answers with an HTML login page and the tool says so) |
| `files:write` | `slack_upload_file` |

A missing scope produces a tool error that names the scope Slack needed and the ones the token has. Adding a scope means reinstalling the app.

## Client setup

Use an absolute path to the binary in every client.

### Claude Code

```bash
claude mcp add slack --scope user -e SLACK_USER_TOKEN=xoxp-... -e SLACK_FILES_DIR=/path/to/files -- /path/to/slack-mcp stdio
```

### Claude Desktop

Edit the config file, then restart the app:

- macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
- Windows: `%APPDATA%\Claude\claude_desktop_config.json`

```json
{
  "mcpServers": {
    "slack": {
      "command": "/absolute/path/to/slack-mcp",
      "args": ["stdio"],
      "env": {
        "SLACK_USER_TOKEN": "xoxp-your-user-token",
        "SLACK_FILES_DIR": "/path/to/files"
      }
    }
  }
}
```

### Any other stdio client

The same `mcpServers` JSON works for most clients (Cursor, Windsurf, and others): command, `["stdio"]` as args, and the two environment variables.

### HTTP mode registration

Start the server (see [HTTP mode](#http-mode)), then register it with the key:

```bash
claude mcp add --transport http slack http://127.0.0.1:8787/mcp --header "Authorization: Bearer <key>"
```

### Keeping the token out of client config files

Put the secrets in a private env file and register a tiny wrapper script instead of the binary:

```bash
mkdir -p ~/.config/slack-mcp && chmod 700 ~/.config/slack-mcp
printf 'SLACK_USER_TOKEN=xoxp-your-user-token\nSLACK_FILES_DIR=/path/to/files\n' > ~/.config/slack-mcp/env
chmod 600 ~/.config/slack-mcp/env
```

```bash
#!/usr/bin/env bash
# ~/.config/slack-mcp/run.sh (chmod +x)
set -euo pipefail
set -a
. "$HOME/.config/slack-mcp/env"
set +a
exec /path/to/slack-mcp stdio
```

Register `~/.config/slack-mcp/run.sh` as the command, with no `env` block. (slack-mcp itself never reads a `.env` file; the wrapper exports the variables.)

## Configuration

Configuration comes **only from environment variables**. No `.env` file is ever read. See [`env.example`](env.example).

| Variable | Default | Notes |
|---|---|---|
| `SLACK_USER_TOKEN` | required | Must start with `xoxp-` (or `xoxb-`, where search does not work). The server refuses to start otherwise. |
| `SLACK_API_BASE_URL` | `https://slack.com/api` | For tests and proxies. Must be `https`; plain `http` is accepted only for `localhost` and loopback IPs, otherwise the server refuses to start. |
| `SLACK_FILES_DIR` | unset | Jail for upload and download. Must exist and be a directory. If unset, the two file tools return a "file tools disabled" error. |
| `SLACK_MAX_FILE_BYTES` | `52428800` (50 MiB) | Cap for uploads and downloads. Downloads abort past the cap and delete the partial file. |
| `SLACK_DOWNLOAD_HOSTS` | unset | Extra hosts that may receive the token and serve files (comma list), meant for tests. Plain `http` is accepted only for `localhost` and loopback IPs. |
| `SLACK_READ_ONLY` | `false` | When `true`, `slack_send_message`, `slack_add_reaction` and `slack_upload_file` are not registered. `slack_download_file` stays: it never posts to Slack, but it does write new files into `SLACK_FILES_DIR`, so it is not labelled read-only. |
| `MCP_HOST` | `127.0.0.1` | HTTP mode only. Must be an IP literal (`127.0.0.1`, `0.0.0.0`, `::1`), not a hostname. |
| `MCP_PORT` | `8787` | HTTP mode only. |
| `MCP_API_KEYS` | required in HTTP mode | Comma list; each key must be at least 32 characters (`openssl rand -hex 32`). `slack-mcp http` refuses to start otherwise. Ignored in stdio mode. |
| `MCP_ALLOWED_HOSTS` | unset | HTTP mode only: extra accepted `Host` header values (comma list). IPv6 entries use brackets, for example `[2001:db8::5]:8787`. |
| `RUST_LOG` | `slack_mcp=info` | Logs always go to stderr. |

## HTTP mode

```bash
SLACK_USER_TOKEN=xoxp-... MCP_API_KEYS=$(openssl rand -hex 32) slack-mcp http
```

- Serves `/mcp` (streamable HTTP, stateless, JSON responses) and `/health` (no auth, answers `ok`).
- Every `/mcp` request needs `Authorization: Bearer <key>` or `X-API-Key: <key>`; keys are compared in constant time and must be at least 32 characters.
- A `Host` and `Origin` guard runs first (DNS-rebinding defence). By default only `localhost`, `127.0.0.1`, `[::1]` (any port) and the bind host are accepted; anything else gets `403`. A request carrying an `Origin` header is also refused unless the origin's host is in that set; requests without `Origin` pass.
- To expose it through a tunnel or reverse proxy, add the public hostname to `MCP_ALLOWED_HOSTS` (for example `MCP_ALLOWED_HOSTS=mcp.example.org`), terminate TLS in front, and keep the API keys secret: whoever holds a key acts as you in Slack.
- Binding a non-loopback address logs a warning. There is no CORS layer, because there is no browser use case.

For local use, **stdio is recommended**: the token never leaves the client's process environment and there is no network listener.

## File handling

`SLACK_FILES_DIR` is the only place the file tools can touch.

- Paths are relative to the directory (or absolute inside it). `..`, absolute paths elsewhere, and symlinks that point outside are rejected, for both upload (reads) and download (writes).
- Uploads use Slack's current external upload flow (`files.upload` was retired in November 2025). The upload URL comes from Slack, must be on `*.slack.com` or `*.slack-edge.com`, and receives **no** `Authorization` header.
- Downloads take `url_private_download` from `files.info`. The first URL must be `https://files.slack.com` (or a host in `SLACK_DOWNLOAD_HOSTS`), and only those hosts ever receive the token. Redirects are followed manually, only to `*.slack.com`, `*.slack-edge.com` or listed hosts. The token is sent only to `files.slack.com` or listed hosts, including when a redirect leads back to one of them; every other hop is fetched without it.
- Files hosted outside Slack (Google Drive, Dropbox and similar links) are refused; open the link instead.
- Downloads never overwrite: an existing destination is refused unless you pass `overwrite=true`. Destination path components starting with `.` are refused.
- Downloads stream to a temp file, stop at `SLACK_MAX_FILE_BYTES`, then are moved into place atomically (a hard link for new files, so a file created in the meantime is never clobbered; a rename when overwriting).
- If Slack answers with an HTML page, the token lacks `files:read`, and the error says so.

## Read-only mode

Set `SLACK_READ_ONLY=true` to leave out `slack_send_message`, `slack_add_reaction` and `slack_upload_file`. Use it for unattended agents. `slack_download_file` remains available: it cannot post to Slack, but it writes new files into `SLACK_FILES_DIR` (never over existing ones unless `overwrite=true`).

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `missing_scope` (the error names the scope) | The token lacks a scope. Add it under **OAuth & Permissions**, then reinstall the app and copy the new token. |
| `not_allowed_token_type`, or search does not work | You are using a bot token (`xoxb-`). Use the **User OAuth Token** (`xoxp-`). |
| `invalid_auth`, `token_revoked`, `account_inactive` | The token is wrong, revoked, or the app was uninstalled. Reinstall and copy the new token. |
| `not_in_channel`, `channel_not_found` | The ID is wrong, or you are not a member of that private channel. Use the `C...` ID from `slack_search_channels`. |
| "file tools are disabled" | Set `SLACK_FILES_DIR` to an existing directory. |
| Download fails saying Slack returned an HTML page | The token lacks `files:read`. Add it and reinstall. |
| "already exists" on download | Downloads never overwrite. Pick another `dest_path` or pass `overwrite=true`. |
| HTTP `401` | Missing or wrong API key. Send `Authorization: Bearer <key>`. |
| HTTP `403` forbidden host or origin | Add the hostname to `MCP_ALLOWED_HOSTS`. |
| "MCP_HOST must be an IP address" | `MCP_HOST` takes an IP literal such as `127.0.0.1` or `0.0.0.0`, not a hostname. |
| Refuses to start: token must start with `xoxp-` | A token beginning with `xoxe.` means token rotation is on. Turn it off (the manifest sets it to false) and reinstall to get a `xoxp-` token. |
| "Installation requires admin approval" | Your workspace restricts app installs. Ask a workspace admin to approve the app. |

## Testing

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests never contact Slack: a `wiremock` mock stands in for the API. To exercise a real workspace, build with `cargo build --release` and run the live smoke test, which needs `jq`:

```bash
cargo build --release
SLACK_USER_TOKEN=xoxp-... SMOKE_CHANNEL_ID=C0123456789 scripts/smoke.sh
```

Read-only steps always run. The write flow (post, reply, read the thread, upload, download and compare bytes) runs only when `SMOKE_POST_CHANNEL_ID` is set **and** `SMOKE_ALLOW_POST=1`. Use a personal test workspace for that, not a shared one. Other knobs: `SMOKE_CHANNEL_QUERY` (default `general`), `SMOKE_SEARCH_QUERY` (default `hello`), and `SLACK_MCP_BIN` (path to the binary, default `target/release/slack-mcp`). With `SLACK_READ_ONLY=true` the script expects 7 tools instead of 10.

## Rate limits

Slack limits calls per method and tier. On HTTP 429 the client honours `Retry-After`, retries at most twice, and never waits more than 30 seconds in total before returning a rate-limited error. Internal, customer-built apps keep the normal tier limits; the 2025 change to one request per minute for `conversations.history` and `conversations.replies` applies only to commercially distributed apps that are not in the Marketplace.

## Security

Read [SECURITY.md](SECURITY.md). In short: the token is you, Slack content is untrusted input that can carry prompt injection, so require approval for posting, uploading and reacting, or run with `SLACK_READ_ONLY=true`.

## License

MIT, see [LICENSE](LICENSE).
