# slack-mcp: MCP server that lets AI clients use Slack as the person who installed it

This codebase is yours: what it's for and how it's built are your call, and you'll keep building on it.

## Ground rules
- Adding, renaming or removing a tool means editing `TOOL_NAMES` in `src/mcp/tools.rs`, the `TOOLS` array in `tests/stdio.rs`, `EXPECTED_TOOLS` in `scripts/smoke.sh` and the README tool table. A write tool also goes in `WRITE_TOOLS`, or `SLACK_READ_ONLY` leaves it registered.
- A scope or config change goes into `slack-app-manifest.yaml`, the README and `CHANGELOG.md` (Unreleased section) in the same change. A new scope only works after the user reinstalls the app.
- Tests never reach Slack: point `SLACK_API_BASE_URL` at a wiremock `MockServer`. Do not run `scripts/smoke.sh` unless the user asks; it hits a live workspace and posts when `SMOKE_ALLOW_POST=1` is set.
- The token goes only to Slack: in the `Authorization` header to the API base, `files.slack.com` and `SLACK_DOWNLOAD_HOSTS`. Never log it, never put it in an error, and never let an HTTP client follow redirects on its own.
- File tools reach paths only through `FileJail` in `src/files.rs`. Never open a path under `SLACK_FILES_DIR` directly, and never overwrite unless `overwrite=true`.

## Commands
- Build: `cargo build --release` (release profile uses LTO, so a cold build is slow; output `target/release/slack-mcp`)
- Test, single: `cargo test --lib -- config::tests::debug_redacts_token`; one integration file: `cargo test --test stdio`
- Test, full suite: `cargo test` (about 2 s once built; no network)
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format check: `cargo fmt --check`
- Run: `target/release/slack-mcp --version`. The `stdio` and `http` modes are started by an MCP client; `http` also needs `MCP_API_KEYS`.

## Layout worth knowing
- Binary and library are one crate: `src/main.rs` only calls `server::run`; the modules are exported from `src/lib.rs`.
- `src/mcp/tools.rs` holds tool names, descriptions and annotations. The Slack calls behind them are in `src/slack/ops.rs`; file transfer is in `src/slack/transfer.rs`.
- `src/slack/mod.rs` owns the HTTP client, 429 retries and the user-name cache. Both of its HTTP clients disable redirects.
- Unit tests sit inline in `src/` under `#[cfg(test)]`. Integration tests are `tests/stdio.rs` (spawns the binary) and `tests/http.rs`.

## Conventions
- Configuration comes only from environment variables (`src/config.rs`). Do not add dotenv or any `.env` reader.
- Tool failures are MCP tool errors whose text is the `SlackError` display string. The model reads that text, so it names the fix (a scope, a reinstall). Keep it that way.
- Tool descriptions and server instructions say Slack text is untrusted third-party content. Keep that wording; SECURITY.md says every tool description carries it.
- `rust-version` is 1.88 and the README advertises it, so do not use newer language or std features. `unsafe_code` is denied.

## Landmines
- Do not call `files.upload` or any other retired Slack method: Slack no longer serves it, so uploads fail. Uploads use `files.getUploadURLExternal` and `files.completeUploadExternal`.
- Do not turn on automatic redirects in the clients. Each hop is checked against the allowlist before it is fetched; automatic redirects would fetch hops nobody checked.
- Do not accept `xoxe.` rotating tokens or loosen `Token::parse`. The server never refreshes tokens, so a rotating token expires and every call fails. The manifest turns rotation off.
- Do not reorder the layers in `build_router` so the API-key check runs before the Host/Origin guard. The guard is the DNS-rebinding defence and must run first (SECURITY.md).
- Do not make `slack_add_reaction` fail on `already_reacted`. Reacting twice is documented as success (`src/slack/ops.rs`), so a retrying agent would otherwise see errors.

## Deeper context
- Setup, Slack app scopes, every env var, HTTP mode, troubleshooting -> `README.md`
- Threat model, file-jail limits, HTTP-mode rules -> `SECURITY.md` (read before changing `src/slack/transfer.rs`, `src/files.rs`, `src/host_guard.rs` or `src/auth.rs`)
- Scopes the installed app requests -> `slack-app-manifest.yaml`
- Environment variable template -> `env.example`
- Change history and Unreleased entries -> `CHANGELOG.md`
- Live smoke test, manual only -> `scripts/smoke.sh`
