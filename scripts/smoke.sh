#!/usr/bin/env bash
# Live smoke test against a real Slack workspace, over stdio, using the `cargo build --release` output.
set -euo pipefail

: "${SLACK_USER_TOKEN:?set SLACK_USER_TOKEN to a user token (xoxp-...)}"
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${SLACK_MCP_BIN:-$ROOT/target/release/slack-mcp}"
[ -x "$BIN" ] || { echo "build first: cargo build --release" >&2; exit 2; }

CHANNEL_QUERY="${SMOKE_CHANNEL_QUERY:-general}"
SEARCH_QUERY="${SMOKE_SEARCH_QUERY:-hello}"
FILES_DIR="$(mktemp -d)"
trap 'rm -rf "$FILES_DIR"' EXIT
export SLACK_FILES_DIR="$FILES_DIR"

case "${SLACK_READ_ONLY:-}" in
  1 | true | TRUE | True | yes | on) EXPECTED_TOOLS=7 ;;
  *) EXPECTED_TOOLS=9 ;;
esac

FAILS=0
REQ_ID=10

# rpc <tool> <arguments-json>: prints the tool's text result, returns 1 on isError.
rpc() {
  local tool="$1" args="$2" out
  REQ_ID=$((REQ_ID + 1))
  out="$(
    {
      echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
      echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
      jq -nc --arg t "$tool" --argjson a "$args" --argjson id "$REQ_ID" \
        '{jsonrpc:"2.0",id:$id,method:"tools/call",params:{name:$t,arguments:$a}}'
      sleep 3
    } | timeout 60 "$BIN" stdio 2>/dev/null | jq -c "select(.id == $REQ_ID)"
  )"
  [ -n "$out" ] || { echo "no response"; return 1; }
  jq -r '.result.content[0].text // .error.message' <<<"$out"
  [ "$(jq -r '.result.isError // false' <<<"$out")" = "false" ]
}

step() {
  local name="$1"
  shift
  if "$@"; then
    echo "PASS $name"
  else
    echo "FAIL $name"
    FAILS=$((FAILS + 1))
  fi
}

t_list() {
  local names
  names="$(
    {
      echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
      echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
      echo '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
      sleep 2
    } | timeout 30 "$BIN" stdio 2>/dev/null | jq -r 'select(.id == 2) | .result.tools | length'
  )"
  [ "$names" = "$EXPECTED_TOOLS" ]
}

t_channels() {
  local out
  out="$(rpc slack_search_channels "$(jq -nc --arg q "$CHANNEL_QUERY" '{query:$q}')")" || { echo "$out"; return 1; }
  jq -e '.channels | type == "array"' <<<"$out" >/dev/null
}

t_read_channel() {
  local out
  out="$(rpc slack_read_channel "$(jq -nc --arg c "$SMOKE_CHANNEL_ID" '{channel_id:$c,limit:5}')")" || { echo "$out"; return 1; }
  jq -e '.messages | type == "array"' <<<"$out" >/dev/null
}

t_search() {
  local out
  out="$(rpc slack_search "$(jq -nc --arg q "$SEARCH_QUERY" '{query:$q,count:3}')")" || { echo "$out"; return 1; }
  jq -e '.matches | type == "array"' <<<"$out" >/dev/null
}

step "tools/list returns $EXPECTED_TOOLS tools" t_list
step "slack_search_channels" t_channels
if [ -n "${SMOKE_CHANNEL_ID:-}" ]; then
  step "slack_read_channel" t_read_channel
else
  echo "SKIP slack_read_channel (set SMOKE_CHANNEL_ID)"
fi
step "slack_search" t_search

t_write_flow() {
  local ch="$SMOKE_POST_CHANNEL_ID" stamp out ts thread_out file_id
  stamp="slack-mcp smoke test $(date -u +%Y-%m-%dT%H:%M:%SZ)"

  out="$(rpc slack_send_message "$(jq -nc --arg c "$ch" --arg t "$stamp" '{channel_id:$c,text:$t}')")" || { echo "$out"; return 1; }
  ts="$(jq -r '.ts' <<<"$out")"
  [ -n "$ts" ] && [ "$ts" != "null" ] || return 1

  rpc slack_send_message "$(jq -nc --arg c "$ch" --arg ts "$ts" '{channel_id:$c,text:"smoke test reply",thread_ts:$ts}')" >/dev/null || return 1

  thread_out="$(rpc slack_read_thread "$(jq -nc --arg c "$ch" --arg ts "$ts" '{channel_id:$c,thread_ts:$ts}')")" || return 1
  [ "$(jq '.messages | length' <<<"$thread_out")" -ge 2 ] || return 1

  printf 'slack-mcp smoke payload %s\n' "$stamp" >"$FILES_DIR/smoke-upload.txt"
  out="$(rpc slack_upload_file "$(jq -nc --arg c "$ch" --arg ts "$ts" '{channel_id:$c,path:"smoke-upload.txt",thread_ts:$ts}')")" || { echo "$out"; return 1; }
  file_id="$(jq -r '.id' <<<"$out")"
  [ -n "$file_id" ] && [ "$file_id" != "null" ] || return 1

  sleep 2
  rpc slack_download_file "$(jq -nc --arg f "$file_id" '{file_id:$f,dest_path:"smoke-download.txt"}')" >/dev/null || return 1
  diff -q "$FILES_DIR/smoke-upload.txt" "$FILES_DIR/smoke-download.txt" >/dev/null
}

if [ -n "${SMOKE_POST_CHANNEL_ID:-}" ] && [ "${SMOKE_ALLOW_POST:-0}" = "1" ]; then
  step "post, reply, read thread, upload, download (byte-identical)" t_write_flow
else
  echo "SKIP write flow (set SMOKE_POST_CHANNEL_ID and SMOKE_ALLOW_POST=1)"
fi

if [ "$FAILS" -ne 0 ]; then
  echo "$FAILS step(s) failed"
  exit 1
fi
echo "all steps passed"
