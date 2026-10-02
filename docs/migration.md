# Migrating from v1

v2 replaces the Bun daemon, the shell hooks and the channel shim with one Rust
binary. v1 clients cannot talk to a v2 daemon: a v1 hook proves no session.
Move every machine of one setup at once.

## What changes

| v1                                         | v2                                                  |
| ------------------------------------------ | --------------------------------------------------- |
| One lead for every session, `/lead`        | Several teams: `/lead x`, `/join x`, `/solo`        |
| A new session takes part at once           | A new session is solo until you add it to a team    |
| Mesh routing option                        | Removed                                             |
| One `opencode` mailbox                     | One mailbox per OpenCode session, through a plugin  |
| `/prompts:lead` in Codex                   | `$lead x`, `$join x`, `$solo`                        |
| `claim_lead` tool                          | Removed: only the user changes teams                 |
| Claude Code: HTTP server + `inband-channel` | One `inband` MCP server: the shim                   |
| `--dangerously-load-development-channels server:inband-channel` | `... server:inband` |
| Bun, `python3`, `curl`                     | Nothing: one static binary                           |

The install directory, `tokens.env`, `config.json` and the mail in
`bridge.db` stay where they are. The daemon migrates the database on start.

## Steps

On each machine, in this order: first the daemon host, then the client
machines.

1. Stop the agent sessions that use InBand.
2. Get v2:

   ```sh
   git clone https://github.com/ruipedro-pinheiro/InBand ~/src/inband
   cd ~/src/inband
   ```

   The v1 checkout in `~/.local/share/mcp-servers/inband` holds your tokens and
   config. Clone v2 elsewhere; the installer keeps the files it needs there.

3. Install:

   ```sh
   ./install.sh            # the daemon host
   ./install.sh --client   # a machine whose agents use the daemon of another one
   ```

   It replaces the v1 service, hooks, MCP entries and commands, keeps a `.bak`
   of every file it changes, and prints what is left to do.

4. Check the daemon: the installer ends with a health check. Otherwise:
   `journalctl --user -u inband -n 30`.
5. Start the agents again, with the new flags:
   - Claude Code: `claude --dangerously-load-development-channels server:inband`
   - Codex: from a shell that exports `INBAND_CODEX_TOKEN`; trust the new hooks
   - OpenCode: `opencode --port 14096`
6. Form the teams: `/lead x` in the lead session, `/join x` in each worker
   (`$lead x` and `$join x` in Codex).
7. Once everything works, remove the v1 files of the old checkout:

   ```sh
   cd ~/.local/share/mcp-servers/inband
   rm -rf src tests scripts hooks commands node_modules package.json bun.lock install.sh
   ```

   Keep `tokens.env`, `config.json` and `bridge.db`.

## Going back

The `.bak` files hold the v1 configs: `~/.claude/settings.json.bak`,
`~/.codex/hooks.json.bak`, `config.json.bak`. Restore them, run
`systemctl --user disable --now inband`, and run the v1 `install.sh` again.
