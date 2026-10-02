#!/usr/bin/env bash
# Installs dependencies, config, Claude Code hooks, /lead commands and the systemd user unit.
# With --client, it prepares a machine that only connects to a daemon on another machine.
# Safe to run again: it keeps backups before migrating local config.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLAUDE_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
UNIT_DIR="$HOME/.config/systemd/user"
DO_HOOKS=1
DO_SERVICE=1
CLIENT=0

for arg in "$@"; do
  case "$arg" in
    --no-hooks)   DO_HOOKS=0 ;;
    --no-service) DO_SERVICE=0 ;;
    --client)     CLIENT=1; DO_SERVICE=0 ;;
    -h|--help)
      echo "usage: ./install.sh [--no-hooks] [--no-service] [--client]"
      echo "  --client  this machine runs agents only; the daemon runs on another machine"
      exit 0 ;;
    *)
      echo "unknown option: $arg" >&2
      exit 2 ;;
  esac
done

say()  { printf '  %s\n' "$1"; }
step() { printf '\n== %s\n' "$1"; }
random_token() { bun -e 'console.log(require("crypto").randomBytes(32).toString("hex"))'; }
# Some clients create their config directory only on first use. An installed binary is enough.
prepare_client_dir() {
  local binary="$1" dir="$2"
  [ -d "$dir" ] && return 0
  command -v "$binary" >/dev/null 2>&1 || return 1
  mkdir -p "$dir"
}
load_tokens() {
  if [ -r "$ROOT/tokens.env" ]; then
    set -a
    # shellcheck disable=SC1091
    . "$ROOT/tokens.env"
    set +a
  fi
}
ensure_token_var() {
  local var="$1"
  if ! grep -q "^${var}=" "$ROOT/tokens.env"; then
    printf '%s=%s\n' "$var" "$(random_token)" >> "$ROOT/tokens.env"
    return 0
  fi
  return 1
}

step "Checking requirements"
if ! command -v bun >/dev/null 2>&1; then
  echo "bun is required. Install it from https://bun.sh, then run this again." >&2
  exit 1
fi
BUN_BIN="$(command -v bun)"
say "bun $(bun --version) at $BUN_BIN"

step "Installing dependencies"
(cd "$ROOT" && bun install --silent)
say "done"

write_server_tokens() {
  step "Writing tokens.env"
  if [ -f "$ROOT/tokens.env" ]; then
    chmod 600 "$ROOT/tokens.env"
    say "tokens.env exists, left untouched"
  else
    umask 077
    : > "$ROOT/tokens.env"
    chmod 600 "$ROOT/tokens.env"
    say "created local auth tokens at $ROOT/tokens.env"
  fi
  load_tokens
  added=0
  for var in INBAND_ADMIN_TOKEN INBAND_CLAUDE_TOKEN INBAND_CODEX_TOKEN INBAND_OPENCODE_TOKEN; do
    if ensure_token_var "$var"; then added=$((added + 1)); fi
  done
  if [ "$added" -gt 0 ]; then
    say "added $added missing token(s)"
    load_tokens
  fi
}

write_config() {
  step "Writing config.json"
  if [ -f "$ROOT/config.json" ]; then
    # Exit codes: 0 auth complete, 1 auth or clients missing, 2 auth disabled by the user.
    local state=0
    bun -e '
      const fs = require("fs");
      const cfg = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
      if (cfg.auth && cfg.auth.required === false) process.exit(2);
      process.exit(cfg.auth && cfg.auth.clients ? 0 : 1);
    ' "$ROOT/config.json" || state=$?
    if [ "$state" = 0 ]; then
      say "config.json exists with auth enabled, left untouched"
    elif [ "$state" = 2 ]; then
      say "WARNING: auth.required is false in config.json. Left as is: every local process can use the daemon"
    else
      BACKUP="$ROOT/config.json.pre-auth.bak"
      cp "$ROOT/config.json" "$BACKUP"
      bun -e '
        const fs = require("fs");
        const [file, example] = process.argv.slice(1);
        const cfg = JSON.parse(fs.readFileSync(file, "utf8"));
        const sample = JSON.parse(fs.readFileSync(example, "utf8"));
        cfg.auth ??= sample.auth;
        cfg.auth.clients ??= sample.auth.clients;
        fs.writeFileSync(file, JSON.stringify(cfg, null, 2) + "\n");
      ' "$ROOT/config.json" "$ROOT/config.example.json"
      say "added the missing auth settings to config.json, backup kept at $BACKUP"
    fi
  else
    CODEX_BIN="$(command -v codex || true)"
    bun -e '
      const [src, dst, codexBin] = process.argv.slice(1);
      const cfg = JSON.parse(require("fs").readFileSync(src, "utf8"));
      if (codexBin) cfg.wake.codex.command = codexBin;
      require("fs").writeFileSync(dst, JSON.stringify(cfg, null, 2) + "\n");
    ' "$ROOT/config.example.json" "$ROOT/config.json" "$CODEX_BIN"
    say "created from config.example.json"
    [ -n "$CODEX_BIN" ] && say "codex binary detected: $CODEX_BIN" \
                        || say "codex not found in PATH, edit wake.codex.command yourself"
  fi
  chmod 600 "$ROOT/config.json"
}

# A client needs the tokens of the daemon host. New random tokens would not match them.
check_client_tokens() {
  step "Checking tokens.env"
  if [ ! -f "$ROOT/tokens.env" ] || ! grep -q '^INBAND_[A-Z]*_TOKEN=.' "$ROOT/tokens.env"; then
    cat >&2 <<MSG
$ROOT/tokens.env is missing or has no token.
Copy from the tokens.env of the daemon host only the lines of the clients that
run on this machine (for example INBAND_CLAUDE_TOKEN). Do not copy the
admin token. Then set mode 600 and run this again.
MSG
    exit 1
  fi
  chmod 600 "$ROOT/tokens.env"
  say "tokens.env found, no token generated"
  load_tokens
}

if [ "$CLIENT" = 1 ]; then
  check_client_tokens
else
  write_server_tokens
  write_config
fi

if [ "$DO_HOOKS" = 1 ]; then
  step "Installing Claude Code hooks"
  if ! prepare_client_dir claude "$CLAUDE_DIR"; then
    say "no $CLAUDE_DIR and no claude binary, skipping (use --no-hooks to silence this)"
  else
    mkdir -p "$CLAUDE_DIR/hooks"
    cp "$ROOT"/hooks/inband-*.sh "$CLAUDE_DIR/hooks/"
    chmod +x "$CLAUDE_DIR"/hooks/inband-*.sh
    rm -f "$CLAUDE_DIR"/hooks/agent-bridge-*.sh
    say "copied hooks to $CLAUDE_DIR/hooks/"

    SETTINGS="$CLAUDE_DIR/settings.json"
    [ -f "$SETTINGS" ] || echo '{}' > "$SETTINGS"
    cp "$SETTINGS" "$SETTINGS.bak"
    bun -e '
      const fs = require("fs");
      const [file, hooksDir] = process.argv.slice(1);
      const s = JSON.parse(fs.readFileSync(file, "utf8"));
      s.hooks ??= {};
      // Drop the hooks of the project before its rename to InBand.
      let removed = 0;
      for (const event of Object.keys(s.hooks)) {
        if (!Array.isArray(s.hooks[event])) continue;
        s.hooks[event] = s.hooks[event].filter((group) => {
          if (!Array.isArray(group?.hooks)) return true;
          const before = group.hooks.length;
          group.hooks = group.hooks.filter((hook) => !String(hook.command ?? "").includes("/agent-bridge-"));
          removed += before - group.hooks.length;
          return group.hooks.length > 0;
        });
      }
      if (removed > 0) console.log(`  removed ${removed} agent-bridge hook(s) from settings.json`);
      const want = [
        ["SessionStart", "inband-name.sh",       null, undefined],
        ["SessionEnd",   "inband-disconnect.sh", null, 5],
        ["PostToolUse",  "inband-mailcheck.sh",  "Bash", 5],
      ];
      let added = 0;
      for (const [event, script, matcher, timeout] of want) {
        s.hooks[event] ??= [];
        const already = JSON.stringify(s.hooks[event]).includes(script);
        if (already) continue;
        const hook = { type: "command", command: `bash "${hooksDir}/${script}"` };
        if (timeout) hook.timeout = timeout;
        s.hooks[event].push(matcher ? { matcher, hooks: [hook] } : { hooks: [hook] });
        added++;
      }
      fs.writeFileSync(file, JSON.stringify(s, null, 2) + "\n");
      console.log(`  ${added} hook(s) added to settings.json, ${want.length - added} already present`);
    ' "$SETTINGS" "$CLAUDE_DIR/hooks"
    say "backup kept at $SETTINGS.bak"
  fi

  step "Installing Codex hooks"
  if ! prepare_client_dir codex "$HOME/.codex"; then
    say "no $HOME/.codex and no codex binary, skipping"
  else
    CODEX_HOOKS="$HOME/.codex/hooks.json"
    [ -f "$CODEX_HOOKS" ] || echo '{}' > "$CODEX_HOOKS"
    cp "$CODEX_HOOKS" "$CODEX_HOOKS.bak"
    bun -e '
      const fs = require("fs");
      const [file, command] = process.argv.slice(1);
      const s = JSON.parse(fs.readFileSync(file, "utf8"));
      s.hooks ??= {};
      let added = 0;
      for (const [event, statusMessage] of [
        ["SessionStart", "Registering the inband mailbox"],
        ["Stop", "Checking the inband mailbox"],
      ]) {
        s.hooks[event] ??= [];
        // An existing entry can point to an old checkout, for example the pre-rename agent-bridge path.
        const existing = s.hooks[event].flatMap((group) => group?.hooks ?? [])
          .filter((hook) => String(hook.command ?? "").includes("codex-hook.ts"));
        if (existing.length > 0) {
          for (const hook of existing) hook.command = command;
          continue;
        }
        s.hooks[event].push({ hooks: [{ type: "command", command, statusMessage, timeout: 5 }] });
        added++;
      }
      fs.writeFileSync(file, JSON.stringify(s, null, 2) + "\n");
      console.log(`  ${added} hook(s) added to ${file}, ${2 - added} already present`);
    ' "$CODEX_HOOKS" "\"$BUN_BIN\" run \"$ROOT/scripts/codex-hook.ts\""
    say "backup kept at $CODEX_HOOKS.bak. Codex asks you to trust new hooks on its next start"
  fi

  step "Installing /lead commands"
  install_command() {
    local client="$1" config_dir="$2" target_dir="$3"
    if ! prepare_client_dir "$client" "$config_dir"; then
      say "$client: no $config_dir and no $client binary, skipping"
      return
    fi
    mkdir -p "$target_dir"
    local target="$target_dir/lead.md"
    if [ -f "$target" ] && ! cmp -s "$ROOT/commands/$client/lead.md" "$target" && ! grep -q 'agent-bridge' "$target"; then
      say "$client: $target exists and differs, left untouched"
      return
    fi
    cp "$ROOT/commands/$client/lead.md" "$target"
    say "$client: installed $target"
  }
  install_command claude "$CLAUDE_DIR" "$CLAUDE_DIR/commands"
  install_command codex "$HOME/.codex" "$HOME/.codex/prompts"
  install_command opencode "$HOME/.config/opencode" "$HOME/.config/opencode/commands"
fi

if [ "$DO_SERVICE" = 1 ]; then
  step "Installing the systemd user service"
  if ! command -v systemctl >/dev/null 2>&1; then
    say "no systemctl, start the daemon yourself: bun run src/index.ts"
  else
    mkdir -p "$UNIT_DIR"
    if [ -f "$UNIT_DIR/agent-bridge.service" ]; then
      # The pre-rename unit holds the same port.
      systemctl --user disable --now agent-bridge >/dev/null 2>&1 || true
      rm -f "$UNIT_DIR/agent-bridge.service"
      say "stopped and removed the old agent-bridge service"
    fi
    sed -e "s|%h/.bun/bin/bun|$BUN_BIN|g" \
        -e "s|%h/.local/share/mcp-servers/inband|$ROOT|g" \
      "$ROOT/inband.service.example" > "$UNIT_DIR/inband.service"
    systemctl --user daemon-reload
    systemctl --user enable inband
    systemctl --user restart inband
    say "service enabled and restarted"

    sleep 2
    if curl -sf --max-time 5 \
      -H "Authorization: Bearer ${INBAND_ADMIN_TOKEN:-}" \
      http://127.0.0.1:7447/health >/dev/null; then
      say "health check passed on http://127.0.0.1:7447"
    else
      say "health check FAILED, look at: journalctl --user -u inband -n 30"
    fi
  fi
fi

step "Remaining manual step: connect your agents"
cat <<'EOF'
  Tokens are in:
    ~/.local/share/mcp-servers/inband/tokens.env

  Claude Code:
    set -a; . ~/.local/share/mcp-servers/inband/tokens.env; set +a
    claude mcp add --scope user --transport http inband http://127.0.0.1:7447/mcp \
      --header "Authorization: Bearer $INBAND_CLAUDE_TOKEN"
    claude mcp add --scope user inband-channel -- \
      "$(command -v bun)" ~/.local/share/mcp-servers/inband/src/channel-shim.ts
    Start Claude Code with:
      --dangerously-load-development-channels server:inband-channel

  Codex:
    set -a; . ~/.local/share/mcp-servers/inband/tokens.env; set +a
    codex mcp add inband --url http://127.0.0.1:7447/mcp \
      --bearer-token-env-var INBAND_CODEX_TOKEN

  OpenCode:
    set -a; . ~/.local/share/mcp-servers/inband/tokens.env; set +a
    opencode mcp add inband --url http://127.0.0.1:7447/mcp \
      --header "Authorization=Bearer $INBAND_OPENCODE_TOKEN"

  Then restart your agents so they pick up the new server.
EOF

if [ "$CLIENT" = 1 ]; then
  cat <<'EOF'

  Client mode: the agents on this machine expect the daemon on 127.0.0.1:7447.
  From the daemon host, forward that port to this machine, for example with:
    ssh -N -R 127.0.0.1:7447:127.0.0.1:7447 <this-machine>
EOF
fi
