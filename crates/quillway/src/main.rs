//! `quillway`: a shortcut-summoned rewrite popup for Wayland, backed by local models.

mod cmd;
mod ipc;
mod ui;

use std::io::Read;

use anyhow::Context;
use clap::{Parser, Subcommand};
use quillway_core::ipc::{Input, Request, Response};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the resident daemon (normally via the systemd user service).
    Daemon,
    /// Show the popup, or hide it if it is open. Bind this to a key in your compositor.
    /// With `--stdin`, an open popup is kept and the command fails, so the text isn't lost.
    Toggle(SourceArg),
    /// Show the popup (no-op if it is open). With `--stdin`, an open popup makes the command fail.
    Show(SourceArg),
    /// Hide the popup.
    Hide,
    /// Re-read config and restart the model server.
    Reload,
    /// Print daemon status.
    Status,
    /// Stop the daemon.
    Quit,
    /// Manage local models.
    #[command(subcommand)]
    Models(cmd::models::ModelsCmd),
    /// Rewrite stdin to stdout without the popup (scripting, testing).
    Rewrite(cmd::rewrite::RewriteArgs),
    /// Check the environment: config, Wayland, clipboard, llama-server, model, daemon.
    Doctor,
}

#[derive(clap::Args)]
struct SourceArg {
    /// Rewrite text piped to this command (e.g. from an editor) instead of the clipboard.
    #[arg(long)]
    stdin: bool,
}

impl SourceArg {
    fn input(&self) -> anyhow::Result<Input> {
        if !self.stdin {
            return Ok(Input::Clipboard);
        }
        let mut s = String::new();
        std::io::stdin().take(quillway_wl::MAX_BYTES + 1).read_to_string(&mut s).context("reading stdin")?;
        if s.len() as u64 > quillway_wl::MAX_BYTES {
            anyhow::bail!("stdin text is larger than the 1 MiB limit");
        }
        Ok(Input::Text(s))
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let request = match cli.command {
        Command::Daemon => return ui::run(),
        Command::Models(c) => return runtime()?.block_on(cmd::models::run(c)),
        Command::Rewrite(a) => return runtime()?.block_on(cmd::rewrite::run(a)),
        Command::Doctor => return runtime()?.block_on(cmd::doctor::run()),
        Command::Toggle(s) => Request::Toggle { input: s.input()? },
        Command::Show(s) => Request::Show { input: s.input()? },
        Command::Hide => Request::Hide,
        Command::Reload => Request::Reload,
        Command::Status => Request::Status,
        Command::Quit => Request::Quit,
    };
    match runtime()?.block_on(ipc::send(&request))? {
        Response::Ok => Ok(()),
        Response::Status { visible, model, engine } => {
            println!("popup:  {}\nmodel:  {model}\nengine: {engine}", if visible { "visible" } else { "hidden" });
            Ok(())
        }
        Response::Error { message } => anyhow::bail!(message),
        Response::Server(_) => anyhow::bail!("unexpected daemon response"),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}
