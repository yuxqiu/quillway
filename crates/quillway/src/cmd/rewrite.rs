//! `quillway rewrite`: stdin → model → stdout, without the popup.

use std::io::{Read, Write};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use quillway_core::config::Config;
use quillway_core::ipc::{Request, Response};
use quillway_core::{clean, paths, prompt};
use quillway_engine::{Client, Engine, Rewrite};

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
        (_, Some(i)) => (i.clone(), 0.7),
        (Some(name), None) => {
            let p = presets
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))
                .with_context(|| format!("no preset named {name:?}"))?;
            (p.instruction.clone(), p.temperature.unwrap_or(0.7))
        }
        (None, None) => bail!("pass --preset or --instruction"),
    };
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    if text.trim().is_empty() {
        bail!("nothing to rewrite on stdin");
    }

    if text.contains(prompt::STOP) {
        bail!("the text contains `{}`, which Quillway uses as a delimiter", prompt::STOP);
    }
    let t0 = std::time::Instant::now();
    // Share the daemon's model server; load our own only when no daemon runs.
    // `_engine` keeps that own server alive until we finish.
    let (client, model, sampling, _engine) = match crate::ipc::send(&Request::Connect).await {
        Ok(Response::Server { base, api_key, model, llama, context, sampling }) => {
            (Client::new(&base, api_key, model.clone(), llama, context), model, sampling, None)
        }
        Ok(Response::Error { message }) => bail!("daemon: {message}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(_) => {
            let engine = Engine::new(config);
            let active = engine.active().await;
            (engine.client().await?, active.name, active.sampling, Some(engine))
        }
    };
    let loaded = t0.elapsed();
    let req = Rewrite { instruction, max_tokens: None, text: text.clone(), temperature, sampling };
    let t1 = std::time::Instant::now();
    let mut first = None;
    let mut deltas = 0usize;
    let mut raw = String::new();
    let mut stream = std::pin::pin!(client.stream(&req).await?);
    while let Some(d) = stream.next().await {
        first.get_or_insert_with(|| t1.elapsed());
        deltas += 1;
        raw.push_str(&d?);
    }
    let total = t1.elapsed();
    let out = if a.raw { raw } else { clean::clean(&raw, &text, true) };
    std::io::stdout().write_all(out.as_bytes())?;
    if !out.ends_with('\n') {
        println!();
    }
    if a.stats {
        let first = first.unwrap_or_default();
        let gen_secs = total.saturating_sub(first).as_secs_f64().max(1e-3);
        eprintln!(
            "model {} · startup {:.1}s · first token {:.2}s · {} tokens · {:.1} tok/s",
            model,
            loaded.as_secs_f64(),
            first.as_secs_f64(),
            deltas,
            f64::from(u32::try_from(deltas.saturating_sub(1)).unwrap_or(u32::MAX)) / gen_secs
        );
    }
    Ok(())
}
