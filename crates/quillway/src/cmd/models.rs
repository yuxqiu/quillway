//! `quillway models …`

use std::io::{IsTerminal, Write};

use anyhow::{Context, bail};
use clap::Subcommand;
use quillway_core::catalog::{self, Entry};
use quillway_core::config::Config;
use quillway_core::ipc::Request;
use quillway_core::paths;
use quillway_engine::download::{self, Job, human};
use quillway_engine::models::{self, State, is_installed};

#[derive(Subcommand)]
pub enum ModelsCmd {
    /// List catalog models (★ = active, ✓ = installed).
    List,
    /// Download a model (default: the catalog default), resumable and sha256-verified.
    Install {
        id: Option<String>,
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
    let config = Config::load(&paths::config_file())?;
    match cmd {
        ModelsCmd::List => {
            let active = models::active(&config);
            for e in catalog::all() {
                let star = if e.id == active.id { "★" } else { " " };
                let tick = if is_installed(e) { "✓" } else { " " };
                println!(
                    "{star} {tick} {:<14} {:<13} {:>7}  {:<8}  {}",
                    e.id,
                    e.name,
                    human(e.size),
                    e.tier,
                    e.license
                );
            }
            if !active.catalog {
                println!("★ ✓ {}", active.id);
            }
            if config.model.active.is_some() {
                println!("\n(active model is pinned by `model.active` in {})", paths::config_file().display());
            }
            Ok(())
        }
        ModelsCmd::Install { id, yes } => {
            let e = match id.as_deref() {
                Some(id) => find(id)?,
                None => catalog::default_entry(),
            };
            if e.license_notice && !yes {
                confirm_license(e)?;
            }
            install(e).await?;
            println!("installed {} → {}", e.name, e.path_in(&paths::models_dir()).display());
            if models::active(&config).id != e.id {
                println!("make it active with `quillway models use {}`", e.id);
            }
            Ok(())
        }
        ModelsCmd::Use { id } => {
            if !id.starts_with("custom:") {
                let e = find(&id)?;
                if !is_installed(e) {
                    bail!("{} is not installed; run `quillway models install {}` first", e.name, e.id);
                }
            }
            State { active: Some(id.clone()) }.save()?;
            if config.model.active.is_some() {
                eprintln!("note: `model.active` in the config overrides this choice");
            }
            println!("active model: {id}");
            reload_daemon().await;
            Ok(())
        }
        ModelsCmd::Remove { id } => {
            let e = find(&id)?;
            let path = e.path_in(&paths::models_dir());
            match std::fs::remove_file(&path) {
                Ok(()) => println!("removed {}", path.display()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => println!("{} is not installed", e.name),
                Err(err) => return Err(err).with_context(|| format!("removing {}", path.display())),
            }
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir); // only if empty
            }
            Ok(())
        }
    }
}

fn find(id: &str) -> anyhow::Result<&'static Entry> {
    catalog::find(id).with_context(|| {
        let ids: Vec<_> = catalog::all().iter().map(|e| e.id.as_str()).collect();
        format!("unknown model {id:?}; available: {}", ids.join(", "))
    })
}

fn confirm_license(e: &Entry) -> anyhow::Result<()> {
    eprintln!("{} is distributed under the {}, which is not an OSI-approved license.", e.name, e.license);
    if let Some(url) = &e.license_url {
        eprintln!("Read it at {url}");
    }
    if !std::io::stdin().is_terminal() {
        bail!("re-run with --yes to accept the license non-interactively");
    }
    eprint!("Accept and download? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("not installed");
    }
    Ok(())
}

async fn install(e: &Entry) -> anyhow::Result<()> {
    let dest = e.path_in(&paths::models_dir());
    let url = e.url();
    let tty = std::io::stderr().is_terminal();
    let mut last_pct = u64::MAX;
    let result = download::download(Job { url: &url, dest: &dest, size: e.size, sha256: &e.sha256 }, |p| {
        let pct = p.done * 100 / p.total.max(1);
        if pct != last_pct && (tty || pct % 10 == 0) {
            last_pct = pct;
            let end = if tty { "\r" } else { "\n" };
            eprint!("{:<13} {pct:>3}%  {} / {}{end}", e.name, human(p.done), human(p.total));
        }
    })
    .await;
    if tty {
        eprintln!();
    }
    result
}

/// Tell a running daemon to pick up the change; fine if none is running.
async fn reload_daemon() {
    if crate::ipc::send(&Request::Reload).await.is_ok() {
        println!("daemon reloaded");
    }
}
