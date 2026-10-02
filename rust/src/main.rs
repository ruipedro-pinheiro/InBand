//! The `inband` binary.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use inband::client::Client;
use inband::config::EnvMap;
use inband::hooks::{self, MailcheckState};
use inband::opencode_cli;
use serde_json::Value;

/// Hook payloads are small; anything larger is not one.
const MAX_STDIN: u64 = 1024 * 1024;

#[derive(Parser)]
#[command(
    name = "inband",
    version,
    about = "Local MCP message bus for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum HookClient {
    Claude,
    Codex,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: the hook routes and the MCP endpoint.
    Daemon {
        #[arg(
            long,
            help = "Directory with config.json, tokens.env and bridge.db \
                    [default: $INBAND_HOME, else ~/.local/share/mcp-servers/inband]"
        )]
        dir: Option<PathBuf>,
    },
    /// Run one hook of a client, with the hook JSON on stdin.
    Hook {
        #[arg(value_enum)]
        client: HookClient,
    },
    /// Run the stdio MCP server of a Claude Code session.
    Shim,
    /// What the OpenCode plugin runs, for one session.
    Opencode {
        #[arg(long, help = "The OpenCode sessionID")]
        session: Option<String>,
        #[command(subcommand)]
        action: OpencodeAction,
    },
}

#[derive(Subcommand)]
enum OpencodeAction {
    /// Print the identity and protocol of the session.
    Context,
    /// Run a team command: lead <team>, join <team> or solo.
    Team {
        command: String,
        #[arg(default_value = "")]
        arguments: String,
    },
    /// Call one tool, with its JSON arguments on stdin.
    Tool { name: String },
    /// Print the tool list, as JSON.
    Tools,
}

fn read_stdin() -> Result<String, String> {
    let mut text = String::new();
    std::io::stdin()
        .take(MAX_STDIN)
        .read_to_string(&mut text)
        .map_err(|error| format!("cannot read stdin: {error}"))?;
    Ok(text)
}

fn env() -> EnvMap {
    std::env::vars().collect()
}

/// Hooks fail open: a broken InBand must never block a session.
async fn hook(client: HookClient) -> Result<(), String> {
    let payload: Value = serde_json::from_str(&read_stdin()?)
        .map_err(|error| format!("the hook input is not JSON: {error}"))?;
    let (id, empty) = match client {
        HookClient::Claude => ("claude", ""),
        HookClient::Codex => ("codex", "{}"),
    };
    let output = match Client::from_env(id, env()) {
        Ok(daemon) => match client {
            HookClient::Claude => {
                hooks::claude_hook(&daemon, &payload, &MailcheckState::from_env(&env())).await
            }
            HookClient::Codex => hooks::codex_hook(&daemon, &payload).await,
        },
        Err(error) => {
            eprintln!("inband: {error}");
            None
        }
    };
    match output {
        Some(output) => println!("{output}"),
        None if !empty.is_empty() => println!("{empty}"),
        None => {}
    }
    Ok(())
}

async fn opencode(session: Option<&str>, action: OpencodeAction) -> Result<ExitCode, String> {
    if let OpencodeAction::Tools = action {
        let tools =
            serde_json::to_string(&inband::mcp::tool_list()).map_err(|error| error.to_string())?;
        println!("{tools}");
        return Ok(ExitCode::SUCCESS);
    }
    let daemon = Client::from_env("opencode", env()).map_err(|error| error.to_string())?;
    let session = session.ok_or("--session is required")?;
    let (text, failed) = match action {
        OpencodeAction::Tools => unreachable!("handled above"),
        OpencodeAction::Context => (opencode_cli::context(&daemon, session).await?, false),
        OpencodeAction::Team { command, arguments } => {
            match opencode_cli::team(&daemon, session, &command, &arguments).await {
                Ok(text) => (text, false),
                Err(error) => (format!("inband: {error}"), true),
            }
        }
        OpencodeAction::Tool { name } => {
            let arguments: Value = serde_json::from_str(&read_stdin()?)
                .map_err(|error| format!("the tool arguments are not JSON: {error}"))?;
            opencode_cli::tool(&daemon, session, &name, &arguments)
                .await
                .map_err(|error| error.to_string())?
        }
    };
    println!("{text}");
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("inband: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = match cli.command {
        Command::Daemon { dir } => runtime
            .block_on(inband::daemon::run(dir))
            .map(|()| ExitCode::SUCCESS)
            .map_err(|error| error.to_string()),
        Command::Hook { client } => runtime.block_on(hook(client)).map(|()| ExitCode::SUCCESS),
        Command::Shim => runtime
            .block_on(inband::shim::run())
            .map(|()| ExitCode::SUCCESS),
        Command::Opencode { session, action } => {
            runtime.block_on(opencode(session.as_deref(), action))
        }
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("inband: {error}");
            ExitCode::FAILURE
        }
    }
}
