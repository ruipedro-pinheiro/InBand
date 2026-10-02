//! The `inband` binary.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
        Command::Daemon { dir } => runtime.block_on(inband::daemon::run(dir)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("inband: {error}");
            ExitCode::FAILURE
        }
    }
}
