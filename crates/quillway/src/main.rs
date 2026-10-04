//! `quillway`: a shortcut-summoned rewrite popup for Wayland, backed by local models.

/// `println!` for CLI output: a closed stdout becomes [`StdoutClosed`] instead of a panic.
macro_rules! say {
    ($($arg:tt)*) => { $crate::say(format_args!($($arg)*))? };
}

/// `eprintln!` for notes and progress; a closed stderr is ignored instead of panicking.
macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

mod cmd;
mod ipc;
mod ui;

use std::io::{Read, Write};

/// Stdout was closed by whatever reads it; `main` stops quietly.
#[derive(Debug)]
struct StdoutClosed;

impl std::fmt::Display for StdoutClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stdout was closed")
    }
}

impl std::error::Error for StdoutClosed {}

/// Write one line to stdout; see [`say!`].
fn say(line: std::fmt::Arguments<'_>) -> anyhow::Result<()> {
    writeln!(std::io::stdout().lock(), "{line}").map_err(|e| match e.kind() {
        std::io::ErrorKind::BrokenPipe => anyhow::Error::new(StdoutClosed),
        _ => e.into(),
    })
}

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
        // Bytes first: a cap can split a character, which isn't the error to report.
        let mut bytes = Vec::new();
        std::io::stdin().take(quillway_wl::MAX_BYTES + 1).read_to_end(&mut bytes).context("reading stdin")?;
        if bytes.len() as u64 > quillway_wl::MAX_BYTES {
            anyhow::bail!("stdin text is larger than the 1 MiB limit");
        }
        Ok(Input::Text(String::from_utf8(bytes).context("stdin isn't UTF-8 text")?))
    }
}

fn main() -> anyhow::Result<()> {
    match run() {
        // Output piped into something that stopped reading (`quillway models list | head -1`).
        Err(e) if e.downcast_ref::<StdoutClosed>().is_some() => Ok(()),
        result => result,
    }
}

fn run() -> anyhow::Result<()> {
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
    // These are answered at once; a hung daemon mustn't leave a key binding's process waiting.
    let answer = runtime()?.block_on(async { tokio::time::timeout(ipc::QUICK_REPLY, ipc::send(&request)).await });
    match answer.map_err(|_| anyhow::anyhow!("the daemon didn't answer within {:?}", ipc::QUICK_REPLY))?? {
        Response::Ok => Ok(()),
        Response::Status { visible, model, engine } => {
            let popup = if visible { "visible" } else { "hidden" };
            say!("popup:  {popup}\nmodel:  {model}\nengine: {engine}");
            Ok(())
        }
        Response::Error { message } => anyhow::bail!(message),
        Response::Server(_) => anyhow::bail!("unexpected daemon response"),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}
