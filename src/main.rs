//! Entry points for the Git subcommand and the stdio MCP server.
mod api;
mod engine;
mod git;
mod landing;
mod layer;
mod mcp;
mod reconcile;
mod registry;
mod runtime;
mod storage;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Git checkout or subdirectory to manage.
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,
    /// Print compact JSON (the default is indented JSON).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Entry,
}

#[derive(Subcommand)]
enum Entry {
    /// Serve the local repository over MCP on stdin/stdout.
    Mcp {
        /// Advertise tools that execute programs and package managers.
        #[arg(long)]
        allow_exec: bool,
    },
    #[command(flatten)]
    Workspace(api::Operation),
}

fn run(repo: PathBuf, command: Entry) -> anyhow::Result<Option<serde_json::Value>> {
    #[cfg(windows)]
    let repo = {
        // Windows pins a process's working directory for its lifetime. Release
        // our handle before starting any MCP threads or retiring the checkout.
        let path = std::path::absolute(repo)?;
        if let Some(root) = path.ancestors().last() {
            std::env::set_current_dir(root)?;
        }
        path
    };
    match command {
        Entry::Mcp { allow_exec } => mcp::serve(repo, allow_exec).map(|_| None),
        Entry::Workspace(op) => engine::run(&repo, op).map(Some),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.repo, cli.command) {
        Ok(Some(value)) => {
            if cli.json {
                println!("{value}");
            } else {
                println!("{value:#}");
            }
            if value["success"] == false {
                return ExitCode::from(
                    value["exit_code"]
                        .as_u64()
                        .filter(|v| (1..=255).contains(v))
                        .unwrap_or(1) as u8,
                );
            }
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", api::error_json(&error));
            ExitCode::FAILURE
        }
    }
}
