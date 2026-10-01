#!/usr/bin/env bash
# SessionStart hook: unique agent-bridge identity per session, presence beacon and role protocol.
set -euo pipefail

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=hooks/agent-bridge-env.sh
. "$HOOK_DIR/agent-bridge-env.sh"
agent_bridge_load_auth claude

input=$(cat)
name=$(printf '%s' "$input" | bash "$HOOK_DIR/agent-bridge-compute-name.sh")

# Announce presence. Best effort only: the daemon can be down.
# This must never block the session.
presence_url="http://127.0.0.1:7447/presence"
presence_body="{\"agent\":\"${name}\",\"online\":true}"
agent_bridge_sign_request POST "$presence_url" "$presence_body"
curl -s -m 2 -X POST "$presence_url" \
  -H 'content-type: application/json' \
  "${AGENT_BRIDGE_CURL_AUTH[@]}" \
  -d "$presence_body" > /dev/null 2>&1 || true

# The daemon returns the complete hook output: identity, current role and protocol.
hook_url="http://127.0.0.1:7447/claude/hook?agent=${name}&event=SessionStart"
agent_bridge_sign_request GET "$hook_url"
if output=$(curl -sf -m 2 "${AGENT_BRIDGE_CURL_AUTH[@]}" "$hook_url" 2>/dev/null) && [ -n "$output" ]; then
  printf '%s\n' "$output"
  exit 0
fi

# Daemon down: give the identity only. The name contains only [a-z0-9_-], so it is safe in JSON.
printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"Your agent-bridge mailbox for this session is `%s`. Use exactly this name as `from` and `for` in the agent-bridge tools. The bridge daemon did not answer, so the role protocol is not loaded."}}\n' "$name"
