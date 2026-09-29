# Changelog

## [0.1.0] - 2026-09-29

Initial release.

- MCP server for Slack that authenticates with a user token (`xoxp-`), so every action happens as the installing user.
- Nine tools: `slack_search`, `slack_read_channel`, `slack_read_thread`, `slack_send_message`, `slack_upload_file`, `slack_download_file`, `slack_read_user_profile`, `slack_search_channels`, `slack_search_users`.
- Transports: stdio (default) and streamable HTTP with mandatory API keys (at least 32 characters) and a `Host` and `Origin` allowlist.
- Download safety: never overwrites an existing file unless `overwrite=true` (new files are hard-linked into place), refuses dot-prefixed destination components and files hosted outside Slack, sends the token only to `files.slack.com`, and fetches redirect hops without it.
- File exchange confined to `SLACK_FILES_DIR`, with a size cap and a download host allowlist.
- `SLACK_READ_ONLY` mode that leaves out the posting and uploading tools.
- Uploads use `files.getUploadURLExternal` and `files.completeUploadExternal`, since `files.upload` was retired in November 2025.
- Rate-limit handling: honours `Retry-After`, at most 2 retries, never more than 30 seconds of waiting.
