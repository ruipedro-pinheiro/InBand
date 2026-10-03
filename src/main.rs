//! The `inband` command: the daemon, the hooks, the shims, the side of the `OpenCode` plugin, and
//! the installer.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use inband::assets::ClientDirs;
use inband::client::Client;
use inband::config::EnvMap;
use inband::hooks::{self, MailcheckState};
use inband::install;
use inband::opencode_cli;
use serde_json::Value;

/// The largest hook input. A hook JSON is small: a larger input is not one.
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
    /// Run the stdio MCP server of a Claude Code session, or of a Codex process.
    Shim {
        #[arg(long, help = "Serve Codex: each tool call names its session")]
        codex: bool,
    },
    /// Install the daemon and connect the agent clients of this machine. Safe to run again.
    Install {
        #[arg(
            long,
            help = "This machine runs agents only: the daemon runs on another machine"
        )]
        client: bool,
        #[arg(long, help = "Do not install the systemd user service")]
        no_service: bool,
    },
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

/// Runs one hook with the hook JSON on stdin.
///
/// A hook never fails, because a broken InBand must never stop a session. Without output, a Claude
/// Code hook prints nothing and a Codex hook prints `{}`.
async fn hook(client: HookClient) -> Result<(), String> {
    let payload: Value = serde_json::from_str(&read_stdin()?)
        .map_err(|error| format!("the hook input is not JSON: {error}"))?;
    let (id, empty) = match client {
        HookClient::Claude => ("claude", ""),
        HookClient::Codex => ("codex", "{}"),
    };
    let files = ClientDirs::from_env(&env());
    let output = match Client::from_env(id, env()) {
        Ok(daemon) => match client {
            HookClient::Claude => {
                hooks::claude_hook(
                    &daemon,
                    &payload,
                    &MailcheckState::from_env(&env()),
                    files.as_ref(),
                    env().get("CLAUDE_PROJECT_DIR").map(String::as_str),
                )
                .await
            }
            HookClient::Codex => hooks::codex_hook(&daemon, &payload, files.as_ref()).await,
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

/// Runs one `inband opencode` action. A failed team command exits with code 1, so the plugin can
/// show the error.
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
        OpencodeAction::Context => {
            let files = ClientDirs::from_env(&env());
            (
                opencode_cli::context(&daemon, session, files.as_ref()).await?,
                false,
            )
        }
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

/// Runs the installer, then prints what it did and what the user must still do.
async fn install(client: bool, no_service: bool) -> Result<(), String> {
    let env = env();
    let home = env
        .get("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or("no home directory: set HOME")?;
    let source_binary =
        std::env::current_exe().map_err(|error| format!("cannot find this binary: {error}"))?;
    let options = install::InstallOptions {
        home,
        source_binary,
        client_only: client,
        service: !no_service,
        env: env.clone(),
    };
    let runner = install::SystemRunner {
        path: env.get("PATH").cloned(),
    };
    let report = install::install(&options, &runner).map_err(|error| error.to_string())?;
    for line in &report.lines {
        println!("{line}");
    }
    if report.port.is_some() && !no_service {
        println!("\n== Health check");
        println!("  {}", health(&report.binary).await);
    }
    if !report.todo.is_empty() {
        println!("\n== Still to do");
        for item in &report.todo {
            println!("  - {item}");
        }
    }
    println!("\nRestart your agents so they load InBand.");
    Ok(())
}

/// Asks the new daemon for its health: 10 tries, 0.5 s apart.
async fn health(binary: &std::path::Path) -> String {
    let daemon = match Client::from_env("admin", env()) {
        Ok(daemon) => daemon,
        Err(error) => return format!("cannot check: {error}"),
    };
    for _ in 0..10 {
        if let Ok(reply) = daemon
            .request(
                reqwest::Method::GET,
                "/health",
                None,
                None,
                std::time::Duration::from_secs(2),
            )
            .await
            && reply["ok"] == true
        {
            return "the daemon answers".to_owned();
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    format!(
        "the daemon does not answer. Look at: journalctl --user -u inband -n 30, or run {} daemon",
        binary.display()
    )
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
        Command::Install { client, no_service } => runtime
            .block_on(install(client, no_service))
            .map(|()| ExitCode::SUCCESS),
        Command::Shim { codex } => runtime
            .block_on(inband::shim::run(codex))
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
