//! `quillway rewrite`: stdin → model → stdout, without the popup.

use std::io::{Read, Write};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use quillway_core::config::{Config, DEFAULT_TEMPERATURE};
use quillway_core::ipc::{Request, Response};
use quillway_core::{clean, paths};
use quillway_engine::{Chunk, Client, Engine, Rewrite, Timing};

#[derive(clap::Args)]
pub struct RewriteArgs {
    /// Preset name (case-insensitive), e.g. `proofread`.
    #[arg(long, short, conflicts_with = "instruction")]
    preset: Option<String>,
    /// Free-form instruction instead of a preset.
    #[arg(long, short)]
    instruction: Option<String>,
    /// Print the raw model output without cleanup.
    #[arg(long)]
    raw: bool,
    /// Report timing on stderr.
    #[arg(long)]
    stats: bool,
}

pub async fn run(a: RewriteArgs) -> anyhow::Result<()> {
    let config = Config::load(&paths::config_file())?;
    let presets = config.presets();
    let (instruction, temperature) = match (&a.preset, &a.instruction) {
        (_, Some(i)) => (i.clone(), DEFAULT_TEMPERATURE),
        (Some(name), None) => {
            let p = presets
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))
                .with_context(|| format!("no preset named {name:?}"))?;
            (p.instruction.clone(), p.temperature)
        }
        (None, None) => bail!("pass --preset or --instruction"),
    };
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    if text.trim().is_empty() {
        bail!("nothing to rewrite on stdin");
    }
    let t0 = std::time::Instant::now();
    // Share the daemon's model server; load our own only when no daemon runs.
    // `_engine` keeps that own server alive until we finish.
    let daemon = match crate::ipc::send(&Request::Connect).await {
        Ok(Response::Server(endpoint)) => Some(Client::new(endpoint)),
        // A daemon older than this command doesn't know `connect`.
        Ok(Response::Error { message }) if message.starts_with("bad request") => {
            eprintln!("quillway: the running daemon is outdated; restart it to share its model server");
            None
        }
        Ok(Response::Error { message }) => bail!("daemon: {message}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(_) => None,
    };
    let (client, _engine) = if let Some(client) = daemon {
        (client, None)
    } else {
        let engine = Engine::new(config);
        (engine.client().await?, Some(engine))
    };
    let loaded = t0.elapsed();
    let req = Rewrite { instruction, max_tokens: None, text: text.clone(), temperature };
    let mut timing = Timing::start();
    let mut raw = String::new();
    let mut stream = std::pin::pin!(client.stream(&req).await?);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        timing.record(&chunk);
        if let Chunk::Text(t) = chunk {
            raw.push_str(&t);
        }
    }
    let rate = timing.rate();
    if raw.trim().is_empty() {
        bail!("the model returned nothing");
    }
    let out = if a.raw { raw } else { clean::clean(&raw, &text, true) };
    if out.trim().is_empty() {
        bail!("the model returned nothing after cleanup");
    }
    std::io::stdout().write_all(out.as_bytes())?;
    if !out.ends_with('\n') {
        println!();
    }
    if a.stats {
        eprintln!(
            "model {} · startup {:.1}s · first token {:.2}s · {} tokens · {rate:.1} tok/s",
            client.endpoint().model,
            loaded.as_secs_f64(),
            timing.first_token().unwrap_or_default().as_secs_f64(),
            timing.tokens(),
        );
    }
    Ok(())
}
