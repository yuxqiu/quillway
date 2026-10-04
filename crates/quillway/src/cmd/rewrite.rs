//! `quillway rewrite`: stdin → model → stdout, without the popup.

use std::io::Read;
use std::time::Instant;

use anyhow::{Context, bail};
use quillway_core::config::{Config, DEFAULT_TEMPERATURE};
use quillway_core::ipc::{Request, Response};
use quillway_engine::{Client, Engine, Interrupted, Rewrite, models};

#[derive(clap::Args)]
#[command(group = clap::ArgGroup::new("task").required(true).args(["preset", "instruction"]))]
pub struct RewriteArgs {
    /// Preset name (case-insensitive), e.g. `proofread`.
    #[arg(long, short)]
    preset: Option<String>,
    /// Free-form instruction instead of a preset.
    #[arg(long, short)]
    instruction: Option<String>,
    /// Report timing on stderr.
    #[arg(long)]
    stats: bool,
}

pub async fn run(a: RewriteArgs) -> anyhow::Result<()> {
    let config = Config::load_user()?;
    let (instruction, temperature) = task(&a, &config)?;
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).context("reading stdin")?;
    if text.trim().is_empty() {
        bail!("nothing to rewrite on stdin");
    }

    let t0 = Instant::now();
    // `engine` keeps our own server, if we had to start one, alive until we finish.
    let (client, engine) = connect(config).await?;
    let loaded = t0.elapsed();
    let (raw, timing) =
        client.complete(&Rewrite { instruction, text: text.clone(), temperature }).await.map_err(|e| {
            // The daemon may have restarted its model server under us (`quillway reload`).
            if engine.is_none() && e.downcast_ref::<Interrupted>().is_some() {
                e.context(
                    "the daemon's model server stopped mid-rewrite (restarted by a reload?); run the command again",
                )
            } else {
                e
            }
        })?;
    let rate = timing.rate();

    // The model's text as it wrote it (`complete` refuses an empty one).
    say!("{}", raw.trim());
    if a.stats {
        note!(
            "model {} · startup {:.1}s · first token {:.2}s · {} tokens · {rate:.1} tok/s",
            client.endpoint().model,
            loaded.as_secs_f64(),
            timing.first_token().unwrap_or_default().as_secs_f64(),
            timing.tokens(),
        );
    }
    Ok(())
}

/// The instruction and temperature from `--instruction` or `--preset` (clap requires one).
fn task(a: &RewriteArgs, config: &Config) -> anyhow::Result<(String, f32)> {
    if let Some(i) = &a.instruction {
        return Ok((i.clone(), DEFAULT_TEMPERATURE));
    }
    let name = a.preset.as_deref().unwrap_or_default();
    let preset = config
        .presets()
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .with_context(|| format!("no preset named {name:?}"))?;
    Ok((preset.instruction.clone(), preset.temperature))
}

/// A client for the daemon's model server, or for our own when no daemon runs,
/// so two copies of a model never compete for GPU memory (DECISIONS #20).
async fn connect(config: Config) -> anyhow::Result<(Client, Option<Engine>)> {
    match crate::ipc::send(&Request::Connect).await {
        Ok(Response::Server(endpoint)) => return Ok((Client::new(endpoint), None)),
        // A daemon older than this command doesn't know `connect`.
        Ok(Response::Error { message }) if message.starts_with("bad request") => {
            note!("quillway: the running daemon is outdated; restart it to share its model server");
        }
        Ok(Response::Error { message }) => bail!("daemon: {message}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) if e.downcast_ref::<crate::ipc::DaemonNotRunning>().is_some() => {}
        // A daemon is there but didn't answer usably: don't load a second copy of the model beside it.
        Err(e) => return Err(e.context("asking the daemon for its model server")),
    }
    let (engine, supervisor) = Engine::new(config.model.clone(), models::active(&config)?);
    tokio::spawn(supervisor.run()); // stops, with its server, when `engine` is dropped
    Ok((engine.client().await?, Some(engine)))
}
