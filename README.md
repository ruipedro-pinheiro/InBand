# agent-bridge

MCP daemon for message passing between Claude Code, Codex and OpenCode sessions
of one Unix user. Messages persist in SQLite. Every session has its own mailbox.
The daemon binds to loopback and requires a token for every request.

## Requirements

- [Bun](https://bun.sh) 1.3 or later
- systemd user services, optional

## Install

```sh
git clone https://github.com/ruipedro-pinheiro/agent-bridge ~/.local/share/mcp-servers/agent-bridge
cd ~/.local/share/mcp-servers/agent-bridge
./install.sh
```

| Option         | Effect                                                                 |
| -------------- | ---------------------------------------------------------------------- |
| (none)         | Generates `tokens.env` and `config.json`, installs hooks and `/lead`, starts the service |
| `--no-hooks`   | Skips the Claude Code hooks and the `/lead` commands                   |
| `--no-service` | Skips the systemd unit. Run the daemon with `bun run src/index.ts`     |
| `--client`     | Machine without a daemon. See [Client machines](#client-machines)      |

The installer keeps an existing `tokens.env` and only adds missing tokens. It
keeps an existing `config.json`, except a file without `auth`: it adds `auth`
and saves the old file as `config.json.pre-auth.bak`. It does not replace a
different `lead.md`.

## Register the clients

```sh
source ~/.local/share/mcp-servers/agent-bridge/tokens.env

claude mcp add --scope user --transport http agent-bridge http://127.0.0.1:7447/mcp \
  --header "Authorization: Bearer $AGENT_BRIDGE_CLAUDE_TOKEN"
claude mcp add --scope user agent-bridge-channel -- \
  bun ~/.local/share/mcp-servers/agent-bridge/src/channel-shim.ts

codex mcp add agent-bridge --url http://127.0.0.1:7447/mcp \
  --bearer-token-env-var AGENT_BRIDGE_CODEX_TOKEN

opencode mcp add agent-bridge --url http://127.0.0.1:7447/mcp \
  --header "Authorization=Bearer $AGENT_BRIDGE_OPENCODE_TOKEN"
```

Codex reads the token from its process environment. Start Claude Code with
`--dangerously-load-development-channels server:agent-bridge-channel` to
receive messages while idle.

## Mailboxes

Mailbox names match `[a-z0-9_-]{1,64}`.

| Client      | Mailbox                          | Set by                 |
| ----------- | -------------------------------- | ---------------------- |
| Claude Code | `claude-<dir>-<session prefix>`  | SessionStart hook      |
| Codex       | `codex-<session uuid>`           | Codex SessionStart hook |
| OpenCode    | `opencode`                       | Fixed                  |

Recipient aliases: `codex` is the most recent Codex session, `all` is every
known mailbox except the sender.

## Tools

| Tool                 | Purpose                                                                 |
| -------------------- | ----------------------------------------------------------------------- |
| `send_message`       | Send to a mailbox or an alias                                           |
| `get_messages`       | Return unread messages and mark them as read                            |
| `wait_for_messages`  | Block until mail arrives, up to 1800 s. Does not mark mail as read      |
| `get_history`        | Read past messages visible to the token                                 |
| `ping`               | List agents, presence, roles, unread counts and recent wakes            |
| `claim_lead`         | Make the calling session the lead                                       |
| `clear_conversation` | Delete all messages. Admin token and `confirm="wipe"` required          |

`get_messages` is the only call that marks mail as read. A reply lost to a
dropped connection stays unread.

## Roles

One session is the lead and the other sessions are workers. The user picks the
lead with `/lead`, which calls `claim_lead`. The previous lead receives a
notice.

The SessionStart hooks of Claude Code and Codex inject the role, the lead name
and the routing rules from `src/protocol.ts`:

- The user writes only in normal turns. Bridge mail comes from agents.
- A worker sends its results to the lead with `send_message`.
- The lead talks to the user in the terminal and to the workers through the bridge.

Each message stores `sender_role`. Channel events show it as `from_role`.

| Client      | `/lead` file                              | Command          |
| ----------- | ----------------------------------------- | ---------------- |
| Claude Code | `~/.claude/commands/lead.md`              | `/lead`          |
| Codex       | `~/.codex/prompts/lead.md`                | `/prompts:lead`  |
| OpenCode    | `~/.config/opencode/commands/lead.md`     | `/lead`          |

## Idle sessions

| Client      | Delivery                                                                 |
| ----------- | ------------------------------------------------------------------------ |
| Claude Code | `agent-bridge-channel` pushes each message into the session              |
| Codex       | `codex queue --thread <id>`. The CLI must support `queue`                |
| OpenCode    | `POST /session/<id>/prompt_async` to the most recent root session        |

The channel follows the current Claude Code session, also after `/clear`. If it
cannot verify the session, it pauses delivery. Unread mail replays after a
reconnection. Codex and OpenCode wakes use the debounce and the hourly cap in
`config.json`. A failed wake leaves the message queued.

OpenCode must run with `opencode --port 14096`, or the port set in
`wake.opencode.baseUrl`.

## Client machines

A client machine runs agents and connects to the daemon of another machine.

1. Copy to its `tokens.env` the token lines of the clients that run there. Do
   not copy the admin token.
2. Run `./install.sh --client`. It generates no token, writes no `config.json`
   and installs no service.
3. Forward the daemon port from the daemon host:

```sh
ssh -N -R 127.0.0.1:7447:127.0.0.1:7447 <client-machine>
```

With the default sshd setting `GatewayPorts no`, the forwarded port listens
only on the loopback of the client.

## Configuration

`config.json` is created from `config.example.json`.

| Key                               | Description                                                      |
| --------------------------------- | ---------------------------------------------------------------- |
| `port`                            | Listen port, 1 to 65535. Example: 7447                            |
| `maxMessageBytes`                 | Maximum message size in bytes, 1 to 1048576. Example: 65536      |
| `auth.required`                   | `false` disables auth. Any other value keeps it on                |
| `auth.clients.<id>.tokenEnv`      | Variable in `tokens.env` that holds the token                     |
| `auth.clients.<id>.agents`        | Mailbox patterns the token can use, for example `claude-*`        |
| `auth.clients.<id>.directory`     | Mailbox patterns listed by `ping` and `/health`. Default: `agents` |
| `auth.clients.<id>.admin`         | Access to every mailbox and to `clear_conversation`               |
| `wake.codex.command`              | Codex executable. A path or a name, not a shell command           |
| `wake.opencode.baseUrl`           | OpenCode server URL. Loopback only                               |
| `wake.<client>.prompt`            | Text sent with each wake                                          |
| `wake.<client>.debounceSeconds`   | Minimum delay after a successful wake of one mailbox              |
| `wake.<client>.maxWakesPerHour`   | Hourly cap per mailbox                                            |
| `wake.codex.retryDelaysSeconds`   | Retry delays when a Codex wake does not start, at most 16         |

`directory` gives no access to mail or history. Use `["*"]` to let a client
see every agent.

| Variable                          | Description                                                     |
| --------------------------------- | --------------------------------------------------------------- |
| `AGENT_BRIDGE_BIND`               | Bind address. Default `127.0.0.1`                               |
| `AGENT_BRIDGE_UNSAFE_REMOTE_BIND` | Set to `1` to allow a non-loopback bind                         |
| `AGENT_BRIDGE_UNSAFE_REMOTE_URLS` | Set to `1` to allow a non-loopback wake or bridge URL           |
| `AGENT_BRIDGE_TOKENS_FILE`        | Path of `tokens.env`                                            |
| `AGENT_BRIDGE_URL`                | Daemon URL for the channel shim                                 |
| `AGENT_BRIDGE_MAILBOX`            | Fixed mailbox for the channel shim outside Claude Code          |
| `AGENT_BRIDGE_CLIENT_ID`          | Token client used by the shim and the Codex hook                |

## HTTP endpoints

| Endpoint            | Use                                              |
| ------------------- | ------------------------------------------------ |
| `POST /mcp`         | MCP Streamable HTTP                              |
| `GET /health`       | Status, same content as `ping`                   |
| `GET /subscribe`    | Long poll used by the channel shim, up to 300 s  |
| `POST /presence`    | Online and offline state from the hooks          |
| `GET /claude/hook`  | Output for the Claude Code hooks                 |
| `POST /codex/hook`  | Codex SessionStart and Stop hooks                |

## Security

- Every token is limited to its `agents` patterns. The Claude token cannot use
  a Codex mailbox.
- Hooks sign their requests with HMAC-SHA256, with a timestamp and a nonce.
  The daemon rejects a replayed nonce.
- `tokens.env`, `config.json` and `bridge.db` have mode 600.
- Mail content comes from other agents. Clients must handle it as untrusted
  text.
- All processes of the Unix user can read the token file and the database.
  The daemon does not isolate agents of the same user from each other.

## Operations

```sh
systemctl --user status agent-bridge
journalctl --user -u agent-bridge -f

source tokens.env
curl -s -H "Authorization: Bearer $AGENT_BRIDGE_ADMIN_TOKEN" http://127.0.0.1:7447/health
```

## Development

```sh
bun test
bun run typecheck
bun run prepublish:security
```

## License

[MIT](LICENSE)
