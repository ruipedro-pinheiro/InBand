# agent-bridge

agent-bridge is a local MCP daemon that carries messages between Claude Code,
Codex and OpenCode sessions running on the same machine.

Each agent session gets a mailbox. Messages persist in SQLite and are read
through MCP tools. An idle recipient can be woken so it starts a turn on its
own. The daemon binds to loopback and requires a bearer token.

It is built for a single workstation under a single Unix user. It is not a
remote collaboration service.

## Installing

Requires [Bun](https://bun.sh) 1.3 or later.

```sh
git clone https://github.com/ruipedro-pinheiro/agent-bridge ~/.local/share/mcp-servers/agent-bridge
cd ~/.local/share/mcp-servers/agent-bridge
./install.sh
```

The installer generates `tokens.env`, writes `config.json`, installs the Claude
Code hooks and starts the systemd user service. Use `--no-hooks` or
`--no-service` to skip either step, and `bun run src/index.ts` to run without a
supervisor.

Register the clients, then restart each agent:

```sh
source tokens.env

claude mcp add --scope user --transport http agent-bridge http://127.0.0.1:7447/mcp \
  --header "Authorization: Bearer $AGENT_BRIDGE_CLAUDE_TOKEN"

codex mcp add agent-bridge --url http://127.0.0.1:7447/mcp \
  --bearer-token-env-var AGENT_BRIDGE_CODEX_TOKEN

opencode mcp add agent-bridge --url http://127.0.0.1:7447/mcp \
  --header "Authorization=Bearer $AGENT_BRIDGE_OPENCODE_TOKEN"
```

Codex reads its token from the process environment, because its MCP config
stores the variable name rather than the value.

## Tools

`send_message`, `get_messages`, `wait_for_messages`, `get_history`, `ping`, and
`clear_conversation`. Call `tools/list` for the full schemas.

`wait_for_messages` blocks server-side and previews mail without consuming it.
`get_messages` is the only consumer, so a reply lost to a dropped connection
never marks mail read.

Mailboxes match `[a-z0-9_-]{1,64}`. Claude Code and Codex sessions register
their own name through a SessionStart hook. Send to `codex` to reach the most
recent Codex session, or to `all` to broadcast.

## Waking idle agents

Codex receives wake prompts through `codex queue --thread`, OpenCode through
`POST /session/{id}/prompt_async`. Set `wake.codex.command` to the Codex binary
you actually run, and launch OpenCode with `opencode --port 14096` so the daemon
can find its server. The Codex CLI must support the `queue` command. An open,
idle Codex CLI processes the queued prompt automatically. If the CLI is closed,
the prompt waits until that thread is resumed; the daemon does not launch a new CLI.

Claude Code receives channel notifications through `agent-bridge-channel`.
Start it with `--dangerously-load-development-channels server:agent-bridge-channel`.
The shim subscribes to one exact mailbox. On the installed Claude Code CLI,
it reads the current session from that parent process's local session registry,
including after `/clear`. It uses the same mailbox names as the SessionStart hook.
If the owning session cannot be verified, delivery pauses. There is no Claude
family subscription fallback. Other clients must set `AGENT_BRIDGE_MAILBOX` to
one concrete mailbox. Unread messages for that mailbox replay after startup or
reconnection without being marked as read. Messages already inserted into a
Claude conversation cannot be removed by the shim.

Wakes are debounced 30 seconds and capped at 20 per hour. A failed wake is
logged and never fatal: the message stays queued.

## Security

The daemon binds `127.0.0.1` and rejects unauthenticated calls. Tokens live in
`tokens.env` with mode 0600 and are scoped per agent family, so the Claude token
cannot act as a Codex mailbox. The admin token is required for full visibility
and for `clear_conversation`. Hooks sign their requests with HMAC. A
non-loopback bind or wake URL requires an explicit unsafe flag.

This is IPC between processes running as the same Unix user. Any process under
that account can read the token file, the database, or daemon memory. Mailbox
content is written by other agents and must be treated as untrusted text.

## Operations

```sh
systemctl --user status agent-bridge
journalctl --user -u agent-bridge -f

source tokens.env
curl -s -H "Authorization: Bearer $AGENT_BRIDGE_ADMIN_TOKEN" http://127.0.0.1:7447/health

bun test
bun run typecheck
bun run prepublish:security
```

Configuration lives in `config.json`, created from `config.example.json`. Wake
targets are under `wake`, client tokens under `auth.clients`. Token values stay
in `tokens.env` and are referenced by name.

## License

agent-bridge is released under the [MIT license](LICENSE).
