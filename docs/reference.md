# Reference

- [Installer](#installer)
- [Mailboxes](#mailboxes)
- [Tools](#tools)
- [Roles](#roles)
- [Waking idle sessions](#waking-idle-sessions)
- [Client machines](#client-machines)
- [Configuration](#configuration)
- [Environment variables](#environment-variables)
- [HTTP endpoints](#http-endpoints)
- [Security](#security)
- [Limits](#limits)

## Installer

`install.sh` requires Bun. The Claude Code hooks also use `python3` and `curl`.

| Option         | Effect                                                                  |
| -------------- | ----------------------------------------------------------------------- |
| (none)         | Tokens, `config.json`, hooks, `/lead` commands and the systemd service  |
| `--no-hooks`   | Skips the Claude Code hooks, the Codex hooks and the `/lead` commands   |
| `--no-service` | Skips the systemd unit                                                  |
| `--client`     | Machine without a daemon, see [Client machines](#client-machines)       |

What the installer keeps:

- `tokens.env`: it adds the missing tokens and changes nothing else.
- `config.json`: it keeps the file. A file without `auth` or without
  `auth.clients` gets the values of `config.example.json`, and the old file is
  saved as `config.json.pre-auth.bak`. A file with `auth.required: false` stays
  as is, with a warning.
- `~/.claude/settings.json` and `~/.codex/hooks.json`: it adds the missing
  agent-bridge hooks and saves a `.bak` copy first.
- `lead.md`: it does not replace a file that differs.

The systemd unit uses the `bun` binary found in `PATH` at install time.

## Mailboxes

Names match `[a-z0-9_-]{1,64}`.

| Client      | Mailbox                          | Set by                                   |
| ----------- | -------------------------------- | ---------------------------------------- |
| Claude Code | `claude-<dir>-<session prefix>`  | `hooks/agent-bridge-name.sh`             |
| Codex       | `codex-<session uuid>`           | The daemon, on the Codex SessionStart hook |
| OpenCode    | `opencode`                       | Fixed                                    |

The Claude Code name uses the first 20 characters of the working directory
name and the first 4 characters of the session ID. After `/clear`, the session
ID changes, so the mailbox changes too.

Recipient aliases: `codex` is the most recent Codex session. `all` is every
known mailbox except the sender.

## Tools

| Tool                 | Description                                                              |
| -------------------- | ------------------------------------------------------------------------ |
| `send_message`       | Send to a mailbox or an alias                                            |
| `get_messages`       | Return unread messages and mark them as read                             |
| `wait_for_messages`  | Block until mail arrives. Does not mark mail as read                     |
| `get_history`        | Read past messages that the token can see                                |
| `ping`               | Agents, presence, roles, unread counts and recent wakes                  |
| `claim_lead`         | Make the calling session the lead                                        |
| `clear_conversation` | Delete all messages. Requires the admin token and `confirm="wipe"`       |

`get_messages` is the only call that marks mail as read. If a response is lost
on a dropped connection, the mail stays unread.

`wait_for_messages` blocks for up to 1800 s when the client sends a progress
token. Without one, the daemon limits the wait to 50 s.

## Roles

`claim_lead` stores the lead in the database and sends a notice to the previous
lead. `ping` and `/health` return `lead` and a `role` for each agent. Each
message stores the role of its sender in `sender_role`.

The SessionStart hooks of Claude Code and Codex inject the role of the session,
the name of the lead and the rules in [`src/protocol.ts`](../src/protocol.ts).

| Client      | `/lead` file                              | Command          |
| ----------- | ----------------------------------------- | ---------------- |
| Claude Code | `~/.claude/commands/lead.md`              | `/lead`          |
| Codex       | `~/.codex/prompts/lead.md`                | `/prompts:lead`  |
| OpenCode    | `~/.config/opencode/commands/lead.md`     | `/lead`          |

## Waking idle sessions

| Client      | Method                                                                    |
| ----------- | ------------------------------------------------------------------------- |
| Claude Code | The `agent-bridge-channel` MCP server pushes each message into the session |
| Codex       | `codex queue --thread <id>`. The Codex CLI must support `queue`           |
| OpenCode    | `POST /session/<id>/prompt_async` to the most recent root session         |

Claude Code loads the channel only when it starts with
`--dangerously-load-development-channels server:agent-bridge-channel`. The
channel reads the session registry of its parent process, so it follows the
session after `/clear`. If it cannot verify the session, it stops delivery
until it can. Unread mail replays after a reconnection. The channel event
shows `from`, `from_role`, `to`, `reply_via` and `sent_at`.

Codex and OpenCode wakes obey `debounceSeconds` and `maxWakesPerHour`. A failed
wake leaves the mail unread. OpenCode must listen on the port of
`wake.opencode.baseUrl`, for example with `opencode --port 14096`.

On Windows, `scripts/setup-windows-channel.ps1` copies the channel files to
`%LOCALAPPDATA%\agent-bridge-channel` and prints the `.claude.json` entry. That
entry uses a fixed mailbox from `-Mailbox`.

## Client machines

A client machine runs agents and uses the daemon of another machine.

1. Copy to the client `tokens.env` the token lines of the clients that run
   there. Do not copy the admin token. Set mode 600.
2. Run `./install.sh --client`. It generates no token, writes no `config.json`
   and installs no service. It installs the hooks and `/lead`, unless you add
   `--no-hooks`.
3. Forward the daemon port from the daemon host:

```sh
ssh -N -R 127.0.0.1:7447:127.0.0.1:7447 <client-machine>
```

With the default sshd setting `GatewayPorts no`, the forwarded port listens
only on the loopback interface of the client.

## Configuration

`config.json`, created from `config.example.json`:

| Key                               | Description                                                       |
| --------------------------------- | ----------------------------------------------------------------- |
| `port`                            | Required. 1 to 65535. The hooks expect 7447, see [Limits](#limits) |
| `maxMessageBytes`                 | Required. 1 to 1048576                                             |
| `wake`                            | Required. Object with one entry per wake target. Can be empty     |
| `auth`                            | Without this object, authentication is off                        |
| `auth.required`                   | `false` disables authentication. Any other value keeps it on      |
| `auth.clients.<id>.tokenEnv`      | Name of the variable in `tokens.env` that holds the token         |
| `auth.clients.<id>.token`         | Inline token, instead of `tokenEnv`                               |
| `auth.clients.<id>.agents`        | Mailbox patterns that the token can use, for example `claude-*`   |
| `auth.clients.<id>.directory`     | Mailbox patterns that `ping` and `/health` list. Default: `agents` |
| `auth.clients.<id>.admin`         | Access to all mailboxes and to `clear_conversation`               |
| `wake.<name>.type`                | Required. `codex` or `opencode`                                   |
| `wake.<name>.prompt`              | Required. Text sent with each wake, 16 KiB maximum                |
| `wake.<name>.debounceSeconds`     | Required. Minimum time after a successful wake of the same mailbox, 1 to 3600 |
| `wake.<name>.maxWakesPerHour`     | Required. Wakes per mailbox per hour, 1 to 3600                   |
| `wake.codex.command`              | Required. Codex executable. A path or a name, not a shell command |
| `wake.codex.retryDelaysSeconds`   | Required. Delays between retries of a Codex wake, 16 entries maximum |
| `wake.opencode.baseUrl`           | Required. URL of the OpenCode server. Loopback only               |

`directory` gives no access to mail or history. Set it to `["*"]` to let a
client see all agents.

## Environment variables

| Variable                             | Used by               | Description                                       |
| ------------------------------------ | --------------------- | ------------------------------------------------- |
| `AGENT_BRIDGE_<CLIENT>_TOKEN`        | All                   | Token of a client, for example `AGENT_BRIDGE_CLAUDE_TOKEN` |
| `AGENT_BRIDGE_TOKEN`                 | Shim, Codex hook, hooks | Token used when the client variable is not set |
| `AGENT_BRIDGE_TOKENS_FILE`           | All                   | Path of `tokens.env`                              |
| `AGENT_BRIDGE_BIND`                  | Daemon                | Bind address: `127.0.0.1` (default), `localhost` or `::1` |
| `AGENT_BRIDGE_UNSAFE_REMOTE_BIND`    | Daemon                | `1` allows a non-loopback bind address            |
| `AGENT_BRIDGE_UNSAFE_REMOTE_URLS`    | Daemon, shim, Codex hook | `1` allows non-loopback wake and bridge URLs  |
| `AGENT_BRIDGE_URL`                   | Shim                  | Daemon URL. Default `http://127.0.0.1:7447`       |
| `AGENT_BRIDGE_MAILBOX`               | Shim                  | Fixed mailbox, for a shim outside Claude Code     |
| `AGENT_BRIDGE_CLIENT_ID`             | Shim, Codex hook      | Token client to use. Default `claude` and `codex` |
| `AGENT_BRIDGE_CODEX_HOOK_URL`        | Codex hook            | Default `http://127.0.0.1:7447/codex/hook`        |
| `AGENT_BRIDGE_CODEX_HOOK_TIMEOUT_MS` | Codex hook            | Request timeout. Default 2000                     |
| `CLAUDE_CONFIG_DIR`                  | Installer, shim       | Claude Code configuration directory. Default `~/.claude` |

`tokens.env` contains `NAME=value` lines without `export`. To give the Codex
token to Codex, export it in the environment that starts `codex`:

```sh
set -a; . ~/.local/share/mcp-servers/agent-bridge/tokens.env; set +a
codex
```

## HTTP endpoints

The MCP clients use `/mcp`. The other endpoints serve the hooks, the channel
and monitoring.

| Endpoint            | Use                                                  |
| ------------------- | ---------------------------------------------------- |
| `POST /mcp`         | MCP Streamable HTTP                                  |
| `GET /health`       | Same content as `ping`                               |
| `GET /subscribe`    | Long poll for the channel shim, 300 s maximum        |
| `POST /presence`    | Online and offline state from the Claude Code hooks  |
| `GET /claude/hook`  | Output of the Claude Code SessionStart and PostToolUse hooks |
| `POST /codex/hook`  | Codex SessionStart and Stop hooks                    |

## Security

- The daemon binds to `127.0.0.1` and rejects requests without a valid token.
  Authentication is off when `config.json` has no `auth` object or sets
  `auth.required` to `false`. The installer always writes an `auth` object.
- MCP clients send a bearer token. The hooks, the channel shim and the Codex
  hook sign their requests with HMAC-SHA256, a timestamp and a nonce. The
  daemon rejects a reused nonce and a timestamp more than 5 minutes from its
  own clock.
- A token can use only the mailboxes that match its `agents` patterns. The
  Claude Code token cannot read or send as a Codex mailbox.
- `tokens.env`, `config.json` and `bridge.db` have mode 600.
- Message content comes from other agents. Clients must treat it as untrusted
  text.
- Every process of the same Unix user can read `tokens.env` and `bridge.db`.
  The daemon does not isolate agents of one user from each other.

## Limits

- The hooks, the Codex hook and the installer health check use port 7447. If
  you change `port`, also set `AGENT_BRIDGE_URL` and
  `AGENT_BRIDGE_CODEX_HOOK_URL`, and edit the URLs in `hooks/*.sh`.
- One lead for all sessions of the daemon.
- After `/clear` in Claude Code, the mailbox changes. Run `/lead` again if that
  session was the lead.
