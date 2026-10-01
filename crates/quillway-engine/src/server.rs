//! Supervised `llama-server` child process on a random localhost port.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use quillway_core::config::ModelConfig;
use tokio::process::{Child, Command};

const READY_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Server {
    child: Child,
    port: u16,
    api_key: String,
    model: PathBuf,
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
        let stderr = child.stderr.take().expect("piped");
        let log = tokio::spawn(tail(stderr));

        let mut server = Self { child, port, api_key, model: model.to_owned() };
        if let Err(e) = server.wait_ready().await {
            let log = log.await.unwrap_or_default();
            bail!("{e}\n--- llama-server log (tail) ---\n{log}");
        }
        log.abort();
        Ok(server)
    }

    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        let http = reqwest::Client::new();
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

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

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
    .map(|s| s.to_string())
    .collect();
    a.extend(cfg.extra_args.iter().cloned());
    a
}

fn free_port() -> anyhow::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// Keep the last lines of stderr for error reports.
async fn tail(stderr: tokio::process::ChildStderr) -> String {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    let mut last = std::collections::VecDeque::with_capacity(20);
    while let Ok(Some(l)) = lines.next_line().await {
        if last.len() == 20 {
            last.pop_front();
        }
        last.push_back(l);
    }
    last.into_iter().collect::<Vec<_>>().join("\n")
}
