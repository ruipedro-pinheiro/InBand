#!/usr/bin/env bash
# PostToolUse hook: remind a WORKING session that agent-bridge mail is waiting.
# Read-only check: the daemon peeks at unread mail and does not mark it as read.
# Rate-limited per identity so it never spams the context.
set -euo pipefail

RATE_LIMIT_SECONDS=120

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=hooks/agent-bridge-env.sh
. "$HOOK_DIR/agent-bridge-env.sh"
agent_bridge_load_auth claude

input=$(cat)
name=$(printf '%s' "$input" | bash "$HOOK_DIR/agent-bridge-compute-name.sh")

state_dir="${XDG_RUNTIME_DIR:-$HOME/.cache}/agent-bridge"
mkdir -p "$state_dir"
chmod 700 "$state_dir" 2>/dev/null || true
stamp="${state_dir}/mailcheck-${name}"
now=$(date +%s)
if [ -f "$stamp" ]; then
  last=$(cat "$stamp" 2>/dev/null || echo 0)
  [ $((now - last)) -lt "$RATE_LIMIT_SECONDS" ] && exit 0
fi
printf '%s\n' "$now" > "$stamp"

hook_url="http://127.0.0.1:7447/claude/hook?agent=${name}&event=PostToolUse"
agent_bridge_sign_request GET "$hook_url"
output=$(curl -sf -m 2 "${AGENT_BRIDGE_CURL_AUTH[@]}" "$hook_url" 2>/dev/null) || exit 0
# The daemon answers {} when no mail waits.
[ "$output" = "{}" ] || printf '%s\n' "$output"
