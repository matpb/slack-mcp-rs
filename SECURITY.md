# Security

## Threat model

- **The token is the user.** A `xoxp-` token can do whatever its scopes allow, as the person who installed the app. Anything the connected AI agent can be talked into doing with these tools, it does as you. Treat the token like a password: keep it in the client's environment, never in a repository or chat.
- **Slack content is untrusted input.** Messages, file names, topics, and profiles are written by other people (and sometimes by bots or guests) and can contain prompt injection aimed at your agent. Every tool description says so, but the model can still be fooled.
- **Recommendations.** Configure your MCP client to require human approval for `slack_send_message` and `slack_upload_file`. For unattended or automated use, set `SLACK_READ_ONLY=true`, which removes both tools. Grant only the scopes you need; drop `chat:write` and `files:write` from the app if you never post.

## Token exfiltration defences

- **Redaction.** The token lives in a type whose `Debug` output is `xoxp-***`. It is only ever placed in the `Authorization` header of requests to the configured Slack API and to allowed download hosts. It does not appear in logs, errors, or tool results.
- **Download host allowlist.** `slack_download_file` only starts from an `https` URL on `files.slack.com` (plus hosts the operator lists in `SLACK_DOWNLOAD_HOSTS`, intended for tests). Only those hosts receive the token. URLs with embedded credentials are refused, and plain `http` is accepted only for `localhost` and loopback IPs. The same loopback rule applies to `SLACK_API_BASE_URL` at startup.
- **Redirects.** Automatic redirects are disabled. Each hop is checked before it is fetched: hops may only go to `*.slack.com` or `*.slack-edge.com` (or listed hosts), and they are fetched without the token unless the hop host is `files.slack.com` or a listed host.
- **Upload URL.** The upload URL returned by Slack is host-checked and receives no `Authorization` header.
- **Remote files.** Files hosted outside Slack (`is_external`) are refused before any request, so the server never fetches an arbitrary third-party URL on behalf of a message author.
- **Read-only escape hatch.** `SLACK_READ_ONLY=true` removes the tools that publish content to Slack. `slack_download_file` stays available because it cannot post, but it writes local files (and with `overwrite=true` can replace them), so it is marked non-read-only and destructive.

## File jail

File tools are disabled unless `SLACK_FILES_DIR` is set. When enabled, every path is resolved against the canonicalized directory: `..` traversal, absolute paths outside it, and symlinks that point outside are rejected for both reads (upload) and writes (download). Writes go to a temp file that is renamed into place, never through a symlink at the target. Downloads never overwrite an existing file unless the caller passes `overwrite=true`, and new files are finalized with a hard link so a concurrent creator is never clobbered; destination components starting with `.` are refused. Uploads and downloads are capped by `SLACK_MAX_FILE_BYTES`; oversized downloads are aborted and the partial file is deleted. Point the directory at a dedicated folder, not your home directory.

Limits of the jail, stated plainly:

- Path checks and the later open are separate steps (check-then-use), so a local process that can swap paths inside the directory between them can defeat the jail.
- A hard link that already exists inside the directory and points to a file outside is not detected.
- The jail therefore assumes no untrusted local writer has access to `SLACK_FILES_DIR`.

## HTTP mode

- `slack-mcp http` refuses to start without `MCP_API_KEYS`, and every key must be at least 32 characters (`openssl rand -hex 32`). Keys are compared in constant time.
- A `Host` and `Origin` allowlist (DNS-rebinding guard) runs before authentication; a request with an `Origin` whose host is not allowed gets 403. Only loopback names and the bind host are accepted unless you add hosts with `MCP_ALLOWED_HOSTS`.
- The default bind address is loopback. Binding elsewhere logs a warning. Put TLS in front if you expose it, and remember that anyone holding a key can act as you in Slack.
- No CORS headers are sent; there is no browser use case.
- Prefer stdio for local use. It opens no network listener.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting for this repository (the **Security** tab, then **Report a vulnerability**). Do not open a public issue for a security problem. Include the version, a description, and reproduction steps if you have them.
