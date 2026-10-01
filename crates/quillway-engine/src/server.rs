//! Supervised `llama-server` child process on a random localhost port.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use quillway_core::config::ModelConfig;
use tokio::process::{Child, Command};

const READY_TIMEOUT: Duration = Duration::from_secs(120);
const LOG_LINES: usize = 20;

/// The last lines of llama-server's stderr, for error reports.
type Log = Arc<Mutex<VecDeque<String>>>;

pub struct Server {
    child: Child,
    port: u16,
    api_key: String,
    model: PathBuf,
    log: Log,
}

impl Server {
    pub async fn start(model: &Path, cfg: &ModelConfig) -> anyhow::Result<Self> {
        let port = free_port()?;
        let api_key: String = std::iter::repeat_with(fastrand::alphanumeric).take(32).collect();
        let bin = cfg.llama_server.clone().unwrap_or_else(|| "llama-server".into());

        let mut cmd = Command::new(&bin);
        cmd.args(args(model, port, cfg))
            .env("LLAMA_API_KEY", &api_key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // SAFETY: prctl is async-signal-safe; this only asks the kernel to
        // SIGTERM the child if the daemon dies without cleaning up.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd.spawn().with_context(|| format!("spawning {bin}"))?;
        let log = Log::default();
        if let Some(stderr) = child.stderr.take() {
            // Drained for the server's whole life, so it never blocks on a full pipe.
            tokio::spawn(collect(stderr, log.clone()));
        }

        let mut server = Self { child, port, api_key, model: model.to_owned(), log };
        if let Err(e) = server.wait_ready().await {
            // Stop it before reporting: a hung server would otherwise live on.
            let _ = server.child.kill().await;
            tokio::time::sleep(Duration::from_millis(100)).await; // let the last lines arrive
            bail!("{e}\n--- llama-server log (tail) ---\n{}", server.log_tail());
        }
        Ok(server)
    }

    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?;
        let url = format!("http://127.0.0.1:{}/health", self.port);
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("llama-server exited during startup ({status})");
            }
            if let Ok(r) = http.get(&url).send().await
                && r.status().is_success()
            {
                return Ok(());
            }
            if start.elapsed() > READY_TIMEOUT {
                bail!("llama-server not ready after {READY_TIMEOUT:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn log_tail(&self) -> String {
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        log.iter().map(String::as_str).collect::<Vec<_>>().join("\n")
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    #[must_use]
    pub fn model_path(&self) -> &Path {
        &self.model
    }
}

fn args(model: &Path, port: u16, cfg: &ModelConfig) -> Vec<String> {
    let mut a: Vec<String> = [
        "--model",
        &model.to_string_lossy(),
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--ctx-size",
        &cfg.context.to_string(),
        "--n-gpu-layers",
        &cfg.gpu_layers.to_string(),
        "--parallel",
        "1",
        "--jinja",
        "--reasoning-budget",
        "0",
        "--cache-reuse",
        "256",
        "--no-webui",
    ]
    .iter()
    .map(std::string::ToString::to_string)
    .collect();
    a.extend(cfg.extra_args.iter().cloned());
    a
}

fn free_port() -> anyhow::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// Keep the last lines of stderr until the process exits.
async fn collect(stderr: tokio::process::ChildStderr, log: Log) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
        if log.len() == LOG_LINES {
            log.pop_front();
        }
        log.push_back(l);
    }
}
