//! Entry points for the Git subcommand and the stdio MCP server.
mod api;
mod cache;
mod engine;
mod git;
mod landing;
mod layer;
mod mcp;
mod package_manager;
mod reconcile;
mod recovery;
mod registry;
mod runtime;
mod storage;
mod trace;
mod undo;
mod workspace;

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
    Workspace(workspace::Command),
}

fn run(repo: PathBuf, mut command: Entry) -> anyhow::Result<Option<serde_json::Value>> {
    if let Entry::Workspace(
        workspace::Command::Move { path, .. } | workspace::Command::Repair { path, .. },
    ) = &mut command
    {
        *path = std::path::absolute(&*path)?;
    }
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
        Entry::Workspace(command) => workspace::run(&repo, command).map(Some),
    }
}

fn main() -> ExitCode {
    trace::mark("start");
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

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    /// `git graft --help` makes Git open the `git-graft` manual page. Keep the
    /// shipped page in step with the CLI; `UPDATE_MAN=1 cargo test` rewrites it.
    /// One self-contained page: each command is listed with its summary
    /// instead of pointing at per-command pages that are not shipped.
    fn manual_page() -> std::io::Result<Vec<u8>> {
        use std::io::Write;
        let command = Cli::command();
        let man = clap_mangen::Man::new(command.clone());
        let mut page = Vec::new();
        man.render_title(&mut page)?;
        man.render_name_section(&mut page)?;
        man.render_synopsis_section(&mut page)?;
        man.render_description_section(&mut page)?;
        man.render_options_section(&mut page)?;
        let roff = |text: &str| text.replace('\\', "\\\\").replace('-', "\\-");
        writeln!(page, ".SH COMMANDS")?;
        for sub in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
            let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
            writeln!(
                page,
                ".TP\n\\fB{}\\fR\n{}",
                roff(sub.get_name()),
                roff(&about)
            )?;
        }
        writeln!(
            page,
            ".PP\nRun \\fBgit graft\\fR \\fICOMMAND\\fR \\fB\\-\\-help\\fR for a command's options."
        )?;
        writeln!(
            page,
            ".SH SEE ALSO\n\\fBgit\\-worktree\\fR(1), https://github.com/mprync/git\\-graft"
        )?;
        man.render_version_section(&mut page)?;
        Ok(page)
    }

    #[test]
    fn manual_page_matches_the_cli() {
        let page = manual_page().expect("render the manual page");
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/man/git-graft.1");
        if std::env::var_os("UPDATE_MAN").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &page).unwrap();
        }
        let shipped = std::fs::read(&path).unwrap_or_default();
        assert!(
            shipped == page,
            "docs/man/git-graft.1 is stale; run UPDATE_MAN=1 cargo test manual_page"
        );
    }
}
