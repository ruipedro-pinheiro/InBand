# Reference

- [Commands](#commands)
- [Installer](#installer)
- [Mailboxes](#mailboxes)
- [Teams](#teams)
- [Tools](#tools)
- [Waking idle sessions](#waking-idle-sessions)
- [Client machines](#client-machines)
- [Configuration](#configuration)
- [Environment variables](#environment-variables)
- [HTTP endpoints](#http-endpoints)
- [Security](#security)
- [Limits](#limits)

## Commands

| Command                   | Run by                                   |
| ------------------------- | ---------------------------------------- |
| `inband daemon [--dir D]` | The systemd user service                 |
| `inband install`          | `install.sh`, or you, to repair a setup  |
| `inband hook claude`      | The Claude Code hooks                    |
| `inband hook codex`       | The Codex hooks                          |
| `inband shim`             | Claude Code, as its `inband` MCP server  |
| `inband shim --codex`     | Codex, as its `inband` MCP server        |
| `inband opencode ...`     | The OpenCode plugin                      |

## Installer

`inband install` options:

| Option         | Effect                                                            |
| -------------- | ----------------------------------------------------------------- |
| (none)         | Tokens, `config.json`, every client found, the systemd service    |
| `--no-service` | Skips the systemd unit                                            |
| `--client`     | Machine without a daemon, see [Client machines](#client-machines) |

A client counts as found when its configuration directory or its binary
exists. Per client, the installer writes:

| Client      | Hooks                                                      | Tools                                  | Team commands                          |
| ----------- | ---------------------------------------------------------- | -------------------------------------- | -------------------------------------- |
| Claude Code | SessionStart, UserPromptSubmit, PostToolUse, SessionEnd in `settings.json` | `claude mcp add --scope user inband -- inband shim` | `~/.claude/commands/{lead,join,solo}.md` |
| Codex       | SessionStart, UserPromptSubmit, Stop in `hooks.json`       | `codex mcp add inband -- inband shim --codex`, without approval prompts | none: the hook reads `$lead x` itself  |
| OpenCode    | none: the plugin                                           | `~/.config/opencode/plugin/inband.js`  | `~/.config/opencode/command/{lead,join,solo}.md` |

What it keeps:

- `tokens.env`: it adds the missing tokens and changes nothing else. The
  pre-rename `AGENT_BRIDGE_*` names count.
- `config.json`: it keeps your values and adds what v2 needs, then checks the
  result with the loader of the daemon. It does not write an invalid config.
- Your hooks, MCP servers and commands: it replaces only the entries of
  InBand, v1 included, and leaves a file that is not its own.
- Every JSON file it changes gets a `.bak` copy first.

## Mailboxes

Names match `[a-z0-9_-]{1,64}`.

| Client      | Mailbox                                 |
| ----------- | --------------------------------------- |
| Claude Code | `claude-<dir>-<4 hex of the session>`   |
| Codex       | `codex-<session uuid>`                  |
| OpenCode    | `opencode-<16 hex of SHA-256(session)>` |

`<dir>` is the first 20 characters of the working directory name. After
`/clear` in Claude Code, the session changes, so the mailbox changes too.

Recipient aliases: `codex` is the most recent Codex session. `all` is the
whole team of the sender, for the lead only.

## Teams

A session is a lead or a worker of one team, or solo. It starts solo.

- A worker writes only to the lead of its team.
- The lead writes to the members of its team, one by one or with `all`.
- No mail crosses teams. A solo session neither sends nor receives.
- A new lead turns the previous lead of the team into a worker, with a notice.
  A join and a leave send a notice to the lead.

The SessionStart hooks, and the OpenCode plugin in the system prompt, give each
session its mailbox, its role, its lead and the rules of
[`src/protocol.rs`](../src/protocol.rs). Each message stores the role of its
sender in `sender_role`, set by the daemon.

Only the user changes teams: a `UserPromptSubmit` hook (a plugin hook in
OpenCode) applies `/lead x`, `/join x` and `/solo` when they are the whole
prompt. No tool changes a team.

## Tools

| Tool                 | Description                                                         |
| -------------------- | ------------------------------------------------------------------- |
| `send_message`       | Send to a mailbox of your team, `codex` or `all`                    |
| `get_messages`       | Return your unread messages and mark them as read                   |
| `wait_for_messages`  | Block until mail arrives. Does not mark mail as read                |
| `get_history`        | The mail you sent and received                                      |
| `ping`               | Agents, presence, teams, roles, unread counts                       |
| `clear_conversation` | Delete all messages. Admin token only, with `confirm="wipe"`        |

`get_messages` is the only call that marks mail as read. If a response is lost
on a dropped connection, the mail stays unread.

`wait_for_messages` blocks for up to 1800 s when the client sends a progress
token, with a progress heartbeat every 20 s. Without one, the daemon limits the
wait to 50 s.

OpenCode names the tools of the plugin `inband_send_message` and so on.

## Waking idle sessions

| Client      | Method                                                              |
| ----------- | ------------------------------------------------------------------- |
| Claude Code | The shim pushes each new message into the session as a channel event |
| Codex       | `codex queue --thread <session>`. The Codex CLI must support `queue` |
| OpenCode    | `POST /session/<session>/prompt_async` to the session of the mailbox |

Claude Code loads the channel only when it starts with
`--dangerously-load-development-channels server:inband`. The shim reads the
session registry of its Claude Code process, so it follows the session after
`/clear`. If it cannot verify the session, it pauses until it can. The channel
event shows `from`, `from_role`, `to`, `reply_via` and `sent_at`.

Codex and OpenCode wakes obey `debounceSeconds` and `maxWakesPerHour`. The wake
prompt carries no message content. A failed wake leaves the mail unread.
OpenCode must listen on the port of `wake.opencode.baseUrl`, for example with
`opencode --port 14096`.

## Client machines

A client machine runs agents and uses the daemon of another machine.

1. Copy to the client `~/.local/share/mcp-servers/inband/tokens.env` the token
   lines of the clients that run there. Do not copy the admin token.
2. Run `./install.sh --client`. It generates no token, writes no
   `config.json` and installs no service.
3. Forward the daemon port from the daemon host:

```sh
ssh -N -R 127.0.0.1:7447:127.0.0.1:7447 <client-machine>
```

With the default sshd setting `GatewayPorts no`, the forwarded port listens
only on the loopback interface of the client.

## Configuration

`~/.local/share/mcp-servers/inband/config.json`, created from
[`assets/config.example.json`](../assets/config.example.json):

| Key                               | Description                                                       |
| --------------------------------- | ----------------------------------------------------------------- |
| `port`                            | Required. 1 to 65535                                              |
| `maxMessageBytes`                 | Required. 1 to 1048576                                            |
| `wake`                            | Object with one entry per wake target. Can be empty               |
| `auth`                            | Without this object, authentication is off                        |
| `auth.required`                   | `false` disables authentication. Any other value keeps it on      |
| `auth.clients.<id>.tokenEnv`      | Name of the variable in `tokens.env` that holds the token         |
| `auth.clients.<id>.token`         | Inline token, instead of `tokenEnv`                               |
| `auth.clients.<id>.agents`        | Mailbox patterns that the token can use, for example `claude-*`   |
| `auth.clients.<id>.directory`     | Mailbox patterns that `ping` and `/health` list. Default: `agents` |
| `auth.clients.<id>.admin`         | Access to all mailboxes, every route, and `clear_conversation`    |
| `wake.<name>.type`                | Required. `codex` or `opencode`                                   |
| `wake.<name>.prompt`              | Required. Text sent with each wake, 16 KiB maximum. `{mailbox}` is replaced |
| `wake.<name>.debounceSeconds`     | Required. Minimum time after a successful wake of the same mailbox, 1 to 3600 |
| `wake.<name>.maxWakesPerHour`     | Required. Wakes per mailbox per hour, 1 to 3600                   |
| `wake.codex.command`              | Required. Codex executable. A path or a name, not a shell command |
| `wake.codex.retryDelaysSeconds`   | Required. Delays between retries of a Codex wake, 16 entries maximum |
| `wake.opencode.baseUrl`           | Required. URL of the OpenCode server. Loopback only               |

## Environment variables

| Variable                     | Used by          | Description                                                  |
| ---------------------------- | ---------------- | ------------------------------------------------------------ |
| `INBAND_<CLIENT>_TOKEN`      | All              | Token of a client, for example `INBAND_CLAUDE_TOKEN`          |
| `INBAND_TOKEN`               | Clients          | Token used when the client variable is not set               |
| `INBAND_TOKENS_FILE`         | All              | Path of `tokens.env`                                         |
| `INBAND_HOME`                | Daemon, installer | Install directory. Default `~/.local/share/mcp-servers/inband` |
| `INBAND_URL`                 | Clients          | Daemon URL. Default: the port of the installed `config.json`, else 7447 |
| `INBAND_CLIENT_ID`           | Clients          | Token client to use instead of `claude`, `codex` or `opencode` |
| `INBAND_BIN`                 | OpenCode plugin  | The `inband` binary. The installer bakes its path in         |
| `INBAND_BIND`                | Daemon           | Bind address: `127.0.0.1` (default), `localhost` or `::1`    |
| `INBAND_UNSAFE_REMOTE_BIND`  | Daemon           | `1` allows a non-loopback bind address                       |
| `INBAND_UNSAFE_REMOTE_URLS`  | Daemon, clients  | `1` allows non-loopback wake and daemon URLs                 |
| `CLAUDE_CONFIG_DIR`          | Installer, shim  | Claude Code configuration directory. Default `~/.claude`     |
| `CODEX_HOME`                 | Installer        | Codex configuration directory. Default `~/.codex`            |

Every InBand command reads its token from `tokens.env` itself: no agent needs
a token in its environment.

The InBand tools run in Codex without approval prompts: a session that a wake
starts in the background has nobody to approve its calls. The daemon checks
the session of every call, whatever the approval.

## HTTP endpoints

| Endpoint            | Use                                                      |
| ------------------- | -------------------------------------------------------- |
| `POST /mcp`         | MCP Streamable HTTP, without MCP sessions                |
| `GET /health`       | Liveness; the full status for the admin token            |
| `GET /subscribe`    | Long poll of the shim, 300 s maximum                     |
| `POST /presence`    | Online and offline state from the Claude Code hooks      |
| `GET /claude/hook`  | Claude Code SessionStart (binds the mailbox) and PostToolUse |
| `POST /codex/hook`  | Codex SessionStart and Stop                              |
| `POST /team/lead`   | `/lead x`                                                |
| `POST /team/join`   | `/join x`                                                |
| `POST /team/leave`  | `/solo`                                                  |

The daemon answers only `Host` headers of `127.0.0.1`, `localhost` and `[::1]`,
and refuses every request with an `Origin` header: a web page cannot reach it.

## Security

The threat is prompt injection: an agent that reads hostile text and tries to
act as another agent, the lead above all.

- **The daemon knows which session calls.** The hooks, the shim and the
  OpenCode plugin sign each request with HMAC-SHA256, a timestamp, a nonce and
  the session. For Codex, the shim signs the session that Codex itself writes
  in the `_meta` of each MCP call. The model writes only the tool arguments, so
  it cannot change the session.
- **A mailbox belongs to one session.** Codex and OpenCode mailboxes carry
  their session in their name. A Claude Code mailbox is bound to its session
  at SessionStart. A request for a mailbox of another session is refused,
  for sending, reading, waiting and team commands alike.
- **Only the user changes teams**, through the prompt hooks. No tool can make
  a session the lead.
- **Message content is untrusted text.** The daemon removes control and
  invisible characters and escapes `<channel` tags, sets `sender_role` itself,
  refuses messages that carry a token, and limits the send rate.
- A token can use only the mailboxes that match its `agents` patterns.
- `tokens.env`, `config.json` and `bridge.db` have mode 600. The daemon binds
  to loopback and rejects requests without a valid token.

- **The instruction files of InBand are checked.** Claude Code sends the text
  of a command file, such as `~/.claude/commands/lead.md`, to the model with
  the trust of the user. An agent that changed this file could thus give
  instructions in the name of the user. The binary contains the exact text of
  each file that it installs:
  - at each session start, the hooks list the InBand files that differ, warn
    the user, and tell the model not to follow them;
  - a team command whose command file differs is refused before the model
    sees it;
  - `inband install` restores the files.

  The Claude Code commands also have `disable-model-invocation: true`: the
  model can neither see nor run them. Codex needs no file for `$lead x`.

What it does not cover: every process of the same Unix user can read
`tokens.env` and sign any request. An agent that runs shell commands can do
that too. InBand stops impersonation through the tools; it does not isolate
the processes of one user from each other. Such an agent can also change the
hooks or the binary themselves: keep the permission prompts or the sandbox of
your agents on for writes outside the project.

## Limits

- A machine that only runs agents reaches the daemon on port 7447.
- Two Claude Code sessions in directories with the same name whose session IDs
  start with the same 4 hex characters get the same mailbox: the second one is
  refused and stays outside InBand. Start it again for a new session ID.
- After `/clear` in Claude Code, the mailbox changes and is solo. Run `/lead x`
  or `/join x` again.
