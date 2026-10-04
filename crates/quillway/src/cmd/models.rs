//! `quillway models …`

use std::io::{IsTerminal, Write};
use std::time::Instant;

use anyhow::{Context, bail};
use clap::Subcommand;
use quillway_core::catalog::{self, Entry};
use quillway_core::config::Config;
use quillway_core::ipc::{Request, Response};
use quillway_core::paths;
use quillway_engine::download::{PROGRESS_INTERVAL, Rate, human};
use quillway_engine::models::{self, State, is_installed};

#[derive(Subcommand)]
pub enum ModelsCmd {
    /// List catalog models (★ = active, ✓ = installed).
    List,
    /// Download a model, resumable and sha256-verified (`quillway models list` shows the ids).
    Install {
        id: String,
        /// Accept a non-OSI license notice without prompting.
        #[arg(long)]
        yes: bool,
    },
    /// Make a model active (catalog id or `custom:/path/model.gguf`).
    Use { id: String },
    /// Delete a downloaded model.
    Remove { id: String },
}

pub async fn run(cmd: ModelsCmd) -> anyhow::Result<()> {
    let config = Config::load_user()?;
    match cmd {
        ModelsCmd::List => list(&config),
        ModelsCmd::Install { id, yes } => install(&config, catalog::get(&id)?, yes).await,
        ModelsCmd::Use { id } => use_model(&config, &id).await,
        ModelsCmd::Remove { id } => remove(&config, catalog::get(&id)?).await,
    }
}

fn list(config: &Config) -> anyhow::Result<()> {
    // Still list the catalog when the choice is broken: it's where a new one comes from.
    let active = models::active(config).inspect_err(|e| note!("warning: {e:#}")).ok();
    // With `model.endpoint`, requests go there, not to any model listed here.
    let local = active.as_ref().filter(|_| config.model.endpoint.is_none());
    for e in catalog::all() {
        let star = if local.is_some_and(|a| a.id == e.id) { "★" } else { " " };
        let tick = if is_installed(e) { "✓" } else { " " };
        say!("{star} {tick} {:<14} {:<13} {:>8}  {:<8}  {}", e.id, e.name, human(e.size), e.tier, e.license);
    }
    if let Some(custom) = local.filter(|a| a.entry.is_none()) {
        say!("★ {} {}", if custom.is_installed() { "✓" } else { " " }, custom.id);
    }
    if let (Some(url), Some(model)) = (&config.model.endpoint, &config.model.endpoint_model) {
        say!("★   {model} at {url} (`model.endpoint`)");
    }
    if config.model.active.is_some() {
        say!("\n(active model is pinned by `model.active` in {})", paths::config_file().display());
    }
    Ok(())
}

async fn install(config: &Config, e: &Entry, yes: bool) -> anyhow::Result<()> {
    if is_installed(e) {
        say!("{} is already installed ({})", e.name, models::path(e).display());
        return Ok(());
    }
    if let Some(warning) = e.license_warning().filter(|_| !yes) {
        confirm_license(&warning)?;
    }
    download_with_progress(e).await?;
    say!("installed {} → {}", e.name, models::path(e).display());
    if is_active(config, e) {
        // A daemon started before the download shows "not installed" until told.
        reload_daemon().await.context("installed, but the daemon couldn't switch to it")?;
    } else {
        say!("make it active with `quillway models use {}`", e.id);
    }
    Ok(())
}

async fn use_model(config: &Config, id: &str) -> anyhow::Result<()> {
    let id = if let Some(path) = id.strip_prefix("custom:") {
        // Absolute, so the daemon finds it whatever its working directory.
        let path = std::fs::canonicalize(path).with_context(|| format!("model file {path}"))?;
        if !path.is_file() {
            bail!("{} is not a file", path.display());
        }
        format!("custom:{}", path.display())
    } else {
        let e = catalog::get(id)?;
        if !is_installed(e) {
            bail!("{} is not installed; run `quillway models install {}` first", e.name, e.id);
        }
        id.to_owned()
    };
    State { active: Some(id.clone()) }.save()?;
    // In both cases the daemon would reload into the same setup.
    if config.model.active.is_some() {
        note!("note: saved, but `model.active` in the config overrides it until that line is removed");
        return Ok(());
    }
    if config.model.endpoint.is_some() {
        note!("note: saved, but requests go to `model.endpoint` until that line is removed");
        return Ok(());
    }
    say!("active model: {id}");
    reload_daemon().await.context("saved, but the daemon couldn't switch to it")
}

async fn remove(config: &Config, e: &Entry) -> anyhow::Result<()> {
    let removed = models::remove(e)?;
    match (removed.file, removed.partial) {
        (true, _) => say!("removed {}", models::path(e).display()),
        (false, true) => say!("{} wasn't installed; removed its partial download", e.name),
        (false, false) => say!("{} is not installed", e.name),
    }
    if !removed.file || !is_active(config, e) {
        return Ok(());
    }
    // The daemon stops the removed model's server; it has no model until one is installed or chosen.
    match reload_daemon().await {
        // Expected after removing the active model; anything else is a real failure.
        Err(e) if format!("{e:#}").contains("is not installed") => {
            note!("note: the daemon has no model now; install one, or pick another with `quillway models use`");
            Ok(())
        }
        result => result.context("removed, but the daemon couldn't reload"),
    }
}

/// Whether requests use `e`, so the daemon must pick up its install or removal.
fn is_active(config: &Config, e: &Entry) -> bool {
    models::active(config).is_ok_and(|a| a.id == e.id)
}

fn confirm_license(warning: &str) -> anyhow::Result<()> {
    note!("{warning}");
    if !std::io::stdin().is_terminal() {
        bail!("re-run with --yes to accept the license non-interactively");
    }
    let _ = write!(std::io::stderr(), "Accept and download? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("not installed");
    }
    Ok(())
}

async fn download_with_progress(e: &Entry) -> anyhow::Result<()> {
    let tty = std::io::stderr().is_terminal();
    let (mut last_pct, mut drawn_at) = (u64::MAX, None::<Instant>);
    let mut rate = Rate::default();
    let result = models::install(e, |p| {
        rate.update(p.done, p.total);
        let pct = p.done * 100 / p.total.max(1);
        // A terminal redraws on a timer, so a slow download still moves; logs get a line per 10%.
        let due = if tty {
            p.done == p.total || drawn_at.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL)
        } else {
            pct != last_pct && pct % 10 == 0
        };
        if due {
            (last_pct, drawn_at) = (pct, Some(Instant::now()));
            let speed = rate.describe().map_or_else(String::new, |r| format!("  {r}"));
            // On a terminal, redraw in place and clear what a longer previous line left.
            let end = if tty { "\x1b[K\r" } else { "\n" };
            let line = format!("{:<13} {pct:>3}%  {} / {}{speed}{end}", e.name, human(p.done), human(p.total));
            let _ = std::io::stderr().write_all(line.as_bytes()); // a closed stderr mustn't stop the download
        }
    })
    .await;
    if tty {
        note!();
    }
    result
}

/// Tell a running daemon to pick up the change; fine if none is running.
///
/// # Errors
///
/// Why the daemon couldn't reload, e.g. llama-server failed to start the model.
async fn reload_daemon() -> anyhow::Result<()> {
    match crate::ipc::send(&Request::Reload).await {
        Ok(Response::Ok) => say!("daemon reloaded"),
        Ok(Response::Error { message }) => bail!(message),
        Ok(other) => bail!("unexpected response {other:?}"),
        Err(e) if e.downcast_ref::<crate::ipc::DaemonNotRunning>().is_some() => {} // it reads the change on start
        Err(e) => return Err(e),
    }
    Ok(())
}
