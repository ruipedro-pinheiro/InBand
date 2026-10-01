#!/usr/bin/env bash
# SessionEnd hook: tell the bridge this session's mailbox owner is gone,
# so agents messaging it get a "disconnected" warning instead of silence.
set -euo pipefail

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=hooks/inband-env.sh
. "$HOOK_DIR/inband-env.sh"
inband_load_auth claude

input=$(cat)
name=$(printf '%s' "$input" | bash "$HOOK_DIR/inband-compute-name.sh")

presence_url="http://127.0.0.1:7447/presence"
presence_body="{\"agent\":\"${name}\",\"online\":false}"
inband_sign_request POST "$presence_url" "$presence_body"
curl -s -m 2 -X POST "$presence_url" \
  -H 'content-type: application/json' \
  "${INBAND_CURL_AUTH[@]}" \
  -d "$presence_body" > /dev/null 2>&1 || true

exit 0
