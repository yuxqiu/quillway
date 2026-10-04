//! Supervised `llama-server` child process on a random localhost port.

use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
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
    log: Log,
    /// Tokens one request can use, as the running server reports it.
    context: u32,
}

impl Server {
    /// Spawn llama-server for `model`; it isn't ready until [`Server::probe`] says so.
    pub fn spawn(model: &Path, cfg: &ModelConfig) -> anyhow::Result<Self> {
        let port = free_port()?;
        let api_key = random_key()?;
        let bin = cfg.llama_server_bin();

        let mut cmd = Command::new(bin);
        cmd.args(args(model, port, cfg))
            .env("LLAMA_API_KEY", &api_key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        die_with_parent(cmd.as_std_mut());
        let mut child = cmd.spawn().with_context(|| format!("spawning {bin}"))?;
        let log = Log::default();
        if let Some(stderr) = child.stderr.take() {
            // Drained for the server's whole life, so it never blocks on a full pipe.
            tokio::spawn(collect(stderr, log.clone()));
        }
        Ok(Self { child, port, api_key, log, context: cfg.context })
    }

    /// Wait until the server answers, then read its per-request context window.
    /// Owns nothing of the process: dropping it leaves the server running.
    pub fn probe(&self) -> impl Future<Output = anyhow::Result<u32>> + Send + 'static {
        let (port, api_key, fallback) = (self.port, self.api_key.clone(), self.context);
        async move {
            wait_ready(port).await?;
            // `extra_args` may change `--ctx-size` or `--parallel` (which splits it).
            Ok(slot_context(port, &api_key).await.unwrap_or_else(|e| {
                eprintln!("quillway: reading llama-server's context size failed, assuming {fallback}: {e:#}");
                fallback
            }))
        }
    }

    pub const fn set_context(&mut self, context: u32) {
        self.context = context;
    }

    pub fn log_tail(&self) -> String {
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        log.iter().map(String::as_str).collect::<Vec<_>>().join("\n")
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Resolves when the process exits (cancel-safe).
    pub async fn exited(&mut self) -> String {
        self.child.wait().await.map_or_else(|e| e.to_string(), |status| status.to_string())
    }

    /// SIGKILL the process and wait until it's gone (and its GPU memory free).
    pub async fn stop(&mut self) {
        let _ = self.child.kill().await; // fails only if it has already exited
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
    pub const fn context(&self) -> u32 {
        self.context
    }
}

async fn wait_ready(port: u16) -> anyhow::Result<()> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?;
    let url = format!("http://127.0.0.1:{port}/health");
    let start = Instant::now();
    loop {
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

/// The per-request context window from `/props`.
async fn slot_context(port: u16, api_key: &str) -> anyhow::Result<u32> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
    let props: serde_json::Value = http
        .get(format!("http://127.0.0.1:{port}/props"))
        .bearer_auth(api_key)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let n = props["default_generation_settings"]["n_ctx"].as_u64().context("no n_ctx in /props")?;
    Ok(u32::try_from(n)?)
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

/// Ask the kernel to SIGKILL the child if the daemon dies without cleaning up.
///
/// Not SIGTERM: llama-server's graceful shutdown can stall until its next HTTP
/// connection when the signal lands mid-request, and with the daemon gone none
/// comes. A Ctrl+C or a `systemctl stop` signals both processes at once, so a
/// SIGTERM here would merge with that one and leave the server running.
fn die_with_parent(cmd: &mut std::process::Command) {
    use rustix::process::{Signal, set_parent_process_death_signal};
    use std::os::unix::process::CommandExt;
    // SAFETY: prctl is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| Ok(set_parent_process_death_signal(Some(Signal::KILL))?));
    }
}

/// The bearer token that keeps other local processes off our server: 128 bits from the OS's CSPRNG.
fn random_key() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("generating the server's API key: {e}"))?;
    Ok(format!("{:032x}", u128::from_ne_bytes(bytes)))
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

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};

    use super::Server;

    impl Server {
        /// The process id, until it has been reaped.
        pub(crate) fn id(&self) -> Option<u32> {
            self.child.id()
        }
    }

    #[test]
    fn a_child_that_ignores_sigterm_still_dies_with_its_parent() {
        // The "parent" is the spawning thread: PR_SET_PDEATHSIG fires when it exits.
        let mut child = std::thread::spawn(|| {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "trap '' TERM; sleep 30"]);
            super::die_with_parent(&mut cmd);
            cmd.spawn().expect("spawn sh")
        })
        .join()
        .expect("spawning thread");
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "child outlived its parent");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.signal(), Some(rustix::process::Signal::KILL.as_raw()));
    }

    #[test]
    fn api_keys_are_random_hex() {
        let (a, b) = (super::random_key().unwrap(), super::random_key().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
