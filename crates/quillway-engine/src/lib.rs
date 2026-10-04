//! Inference for Quillway.
//!
//! [`Engine`] hands out a [`Client`] for either the supervised llama-server
//! (default) or a user-configured OpenAI-compatible endpoint.
//!
//! One task, the [`Supervisor`], owns the configuration and the llama-server
//! and handles commands one at a time; [`Engine`] handles send it commands and
//! watch its [`EngineState`]. So reloads apply in the order they're sent, a
//! reload during a start just drops that start (killing the half-started
//! process), and everyone waiting on a start gets its outcome.

pub mod client;
pub mod download;
pub mod models;
mod server;

use std::future::Future;
use std::pin::Pin;

use quillway_core::config::ModelConfig;
use quillway_core::ipc::Endpoint;
use tokio::sync::{mpsc, oneshot, watch};

pub use client::{Chunk, Client, Interrupted, Rewrite, Timing};
pub use models::Active;

/// Where the model backend is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    /// llama-server is starting.
    Starting,
    /// Requests are served (our llama-server, or the configured endpoint).
    Ready,
    /// The model file isn't on disk.
    Missing,
    /// The last start failed.
    Failed(String),
}

impl EngineState {
    /// For `quillway status`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Starting => "starting".to_owned(),
            Self::Ready => "ready".to_owned(),
            Self::Missing => "model not installed".to_owned(),
            Self::Failed(e) => format!("failed: {e}"),
        }
    }
}

/// A handle to the model backend; clones talk to the same [`Supervisor`].
#[derive(Clone)]
pub struct Engine {
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<EngineState>,
}

/// There is one engine, as far as an iced subscription keyed on it is concerned.
impl std::hash::Hash for Engine {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        "quillway-engine".hash(state);
    }
}

type Done = oneshot::Sender<Result<(), String>>;

enum Command {
    /// Switch config: stop the server and bring up the new one.
    Reload { change: Box<(ModelConfig, Active)>, done: Done },
    /// Bring the server up if it isn't, e.g. after the model was installed.
    Start,
    /// A client for the running server, starting it if needed.
    Client(oneshot::Sender<Result<Client, String>>),
}

const CANCELLED: &str = "cancelled: the model server was reloaded";
const STOPPED: &str = "the engine stopped";

impl Engine {
    /// An engine for the `[model]` config and its active model, and the
    /// supervisor to run on the async runtime (`tokio::spawn`, an iced task).
    /// The supervisor brings the server up as soon as it runs.
    #[must_use]
    pub fn new(config: ModelConfig, active: Active) -> (Self, Supervisor) {
        let (commands, inbox) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(EngineState::Starting);
        let supervisor = Supervisor {
            inbox,
            state: state_tx,
            config,
            active,
            llama: Llama::Stopped,
            waiting: Vec::new(),
            reloads: Vec::new(),
        };
        (Self { commands, state }, supervisor)
    }

    /// Switch to `config` and `active`, in the order of calls. The returned
    /// future resolves once the new server is up (or failed), or with the
    /// outcome of a later reload that replaced this one.
    ///
    /// # Errors
    ///
    /// The future resolves to why the server couldn't come up: the model is
    /// missing or llama-server failed to start.
    pub fn reload(&self, config: ModelConfig, active: Active) -> impl Future<Output = Result<(), String>> + use<> {
        let (done, outcome) = oneshot::channel();
        let _ = self.commands.send(Command::Reload { change: Box::new((config, active)), done });
        async { outcome.await.unwrap_or_else(|_| Err(STOPPED.into())) }
    }

    /// Bring the server up if it isn't running or starting.
    pub fn start(&self) {
        let _ = self.commands.send(Command::Start);
    }

    /// A client ready to accept requests, starting llama-server if needed.
    ///
    /// # Errors
    ///
    /// The model isn't installed, llama-server fails to start, or a reload
    /// replaced the server.
    pub async fn client(&self) -> anyhow::Result<Client> {
        let (reply, client) = oneshot::channel();
        self.commands.send(Command::Client(reply)).map_err(|_| anyhow::anyhow!(STOPPED))?;
        client.await.map_err(|_| anyhow::anyhow!(STOPPED))?.map_err(anyhow::Error::msg)
    }

    /// The supervisor's state, to watch for changes.
    #[must_use]
    pub fn state(&self) -> watch::Receiver<EngineState> {
        self.state.clone()
    }
}

/// Resolves once a spawned llama-server answers, with its per-slot context.
/// Dropping it leaves the process alone.
type Probe = Pin<Box<dyn Future<Output = anyhow::Result<u32>> + Send>>;

/// The llama-server process, if there is one.
enum Llama {
    /// None: an endpoint, a missing model, or the last one failed or exited.
    Stopped,
    /// Spawned and waiting for it to answer.
    Starting { server: server::Server, probe: Probe },
    /// Answering requests.
    Running(server::Server),
}

/// What happened to the llama-server.
enum Event {
    Probed(anyhow::Result<u32>),
    Exited(String),
}

/// Owns the config and the llama-server process; see the crate docs.
///
/// A process is stopped by killing it and waiting for it to exit before
/// anything else happens, never by dropping a future, so two models never
/// share the GPU's memory (DECISIONS #20).
pub struct Supervisor {
    inbox: mpsc::UnboundedReceiver<Command>,
    state: watch::Sender<EngineState>,
    config: ModelConfig,
    active: Active,
    llama: Llama,
    /// Clients waiting for the start in progress.
    waiting: Vec<oneshot::Sender<Result<Client, String>>>,
    /// Reloads waiting for the start in progress.
    reloads: Vec<Done>,
}

impl Supervisor {
    /// Handle commands until every [`Engine`] handle is gone.
    pub async fn run(mut self) {
        self.bring_up().await;
        loop {
            let event = async {
                match &mut self.llama {
                    Llama::Stopped => std::future::pending().await,
                    Llama::Starting { server, probe } => tokio::select! {
                        context = probe => Event::Probed(context),
                        status = server.exited() => Event::Exited(status),
                    },
                    Llama::Running(server) => Event::Exited(server.exited().await),
                }
            };
            tokio::select! {
                command = self.inbox.recv() => match command {
                    Some(command) => self.handle(command).await,
                    None => return,
                },
                event = event => self.on_event(event).await,
            }
        }
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Reload { change, done } => {
                (self.config, self.active) = *change;
                for client in self.waiting.drain(..) {
                    let _ = client.send(Err(CANCELLED.into()));
                }
                self.reloads.push(done);
                self.bring_up().await;
            }
            Command::Start => {
                if self.ready_client().is_none() && !self.starting() {
                    self.bring_up().await;
                }
            }
            Command::Client(reply) => {
                if let Some(client) = self.ready_client() {
                    let _ = reply.send(Ok(client));
                    return;
                }
                self.waiting.push(reply);
                if !self.starting() {
                    self.bring_up().await;
                }
            }
        }
    }

    async fn on_event(&mut self, event: Event) {
        match (std::mem::replace(&mut self.llama, Llama::Stopped), event) {
            (Llama::Starting { mut server, .. }, Event::Probed(Ok(context))) => {
                server.set_context(context);
                self.llama = Llama::Running(server);
                self.settle(EngineState::Ready, &Ok(()));
            }
            (Llama::Starting { mut server, .. }, Event::Probed(Err(e))) => {
                let tail = server.log_tail();
                server.stop().await;
                self.fail(format!("starting llama-server: {e:#}\n--- llama-server log (tail) ---\n{tail}"));
            }
            (Llama::Starting { server, .. }, Event::Exited(status)) => {
                self.fail(format!("llama-server exited during startup ({status}):\n{}", server.log_tail()));
            }
            // Noticed at once, so the state (and `quillway status`) don't claim it's ready.
            (Llama::Running(server), Event::Exited(status)) => self.fail(format!(
                "llama-server exited ({status}); the next request restarts it. Its last output:\n{}",
                server.log_tail()
            )),
            // Events come from the current state; nothing else can happen.
            (llama, _) => self.llama = llama,
        }
    }

    const fn starting(&self) -> bool {
        matches!(self.llama, Llama::Starting { .. })
    }

    /// A client for the endpoint or the running llama-server, if there is one.
    fn ready_client(&mut self) -> Option<Client> {
        if let Some(base) = &self.config.endpoint {
            return Some(Client::new(Endpoint {
                base: base.clone(),
                api_key: self.config.endpoint_api_key.clone(),
                model: self.config.endpoint_model.clone().unwrap_or_default(),
                llama: false,
                context: self.config.context,
                sampling: self.active.sampling,
            }));
        }
        let Llama::Running(server) = &mut self.llama else { return None };
        // It may have exited just now, before its event was handled.
        server.is_alive().then(|| llama_client(server, &self.active))
    }

    /// Stop the current server, then start one for the current config, or
    /// settle at once: an endpoint needs no server, and a missing model can't start.
    async fn bring_up(&mut self) {
        if let Llama::Starting { mut server, .. } | Llama::Running(mut server) =
            std::mem::replace(&mut self.llama, Llama::Stopped)
        {
            server.stop().await;
        }
        if self.config.endpoint.is_some() {
            return self.settle(EngineState::Ready, &Ok(()));
        }
        if let Err(e) = self.active.ensure_installed() {
            return self.settle(EngineState::Missing, &Err(format!("{e:#}")));
        }
        self.publish(EngineState::Starting);
        match server::Server::spawn(&self.active.path, &self.config) {
            Ok(server) => self.llama = Llama::Starting { probe: Box::pin(server.probe()), server },
            Err(e) => self.fail(format!("starting llama-server: {e:#}")),
        }
    }

    fn fail(&mut self, message: String) {
        eprintln!("quillway: {message}");
        self.settle(EngineState::Failed(message.clone()), &Err(message));
    }

    /// Publish `state` if it changed (watchers act on each publish).
    fn publish(&self, state: EngineState) {
        self.state.send_if_modified(|current| {
            let changed = *current != state;
            *current = state;
            changed
        });
    }

    /// Publish `state` and answer everyone waiting on the start.
    fn settle(&mut self, state: EngineState, outcome: &Result<(), String>) {
        self.publish(state);
        let client = outcome.clone().map(|()| self.ready_client());
        for reply in self.waiting.drain(..) {
            let _ = reply.send(client.clone().and_then(|c| c.ok_or_else(|| CANCELLED.into())));
        }
        for done in self.reloads.drain(..) {
            let _ = done.send(outcome.clone());
        }
    }
}

fn llama_client(server: &server::Server, active: &Active) -> Client {
    Client::new(Endpoint {
        base: server.base_url(),
        api_key: Some(server.api_key().to_owned()),
        model: active.id.clone(),
        llama: true,
        context: server.context(),
        sampling: active.sampling,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use quillway_core::config::Config;

    use super::*;

    /// A model file and a "llama-server" running `script`; each run is logged to `runs`.
    fn fake_server(dir: &std::path::Path, script: &str) -> (ModelConfig, Active) {
        let model = dir.join("model.gguf");
        std::fs::write(&model, b"gguf").unwrap();
        let bin = dir.join("llama-server");
        std::fs::write(&bin, format!("#!/bin/sh\necho run >> {}\n{script}\n", dir.join("runs").display())).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut config = Config::default();
        config.model.active = Some(format!("custom:{}", model.display()));
        config.model.llama_server = Some(bin.display().to_string());
        let active = models::active(&config).unwrap();
        (config.model, active)
    }

    fn runs(dir: &std::path::Path) -> usize {
        std::fs::read_to_string(dir.join("runs")).map_or(0, |s| s.lines().count())
    }

    fn start(config: ModelConfig, active: Active) -> Engine {
        let (engine, supervisor) = Engine::new(config, active);
        tokio::spawn(supervisor.run());
        engine
    }

    #[tokio::test]
    async fn a_reload_cancels_a_start_instead_of_waiting_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let (config, active) = fake_server(dir.path(), "exec sleep 60");
        let engine = start(config.clone(), active.clone());
        let waiting = tokio::spawn({
            let engine = engine.clone();
            async move { engine.client().await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await; // the start is waiting for /health
        drop(engine.reload(config, active)); // sent now; its outcome isn't needed here
        let error = tokio::time::timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap().unwrap_err();
        assert!(error.to_string().contains("reloaded"), "{error}");
    }

    #[tokio::test]
    async fn requests_waiting_for_a_failed_start_share_its_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (config, active) = fake_server(dir.path(), "sleep 0.3; exit 1");
        let engine = start(config, active);
        let (a, b) = tokio::join!(engine.client(), engine.client());
        assert!(a.is_err() && b.is_err());
        assert_eq!(runs(dir.path()), 1, "one start, not one per waiting request");
        assert!(matches!(*engine.state().borrow(), EngineState::Failed(_)));
        // A request after the failure tries again.
        assert!(engine.client().await.is_err());
        assert_eq!(runs(dir.path()), 2);
    }

    #[tokio::test]
    async fn a_caller_giving_up_does_not_end_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let (config, active) = fake_server(dir.path(), "exec sleep 60");
        let engine = start(config, active);
        let first = tokio::spawn({
            let engine = engine.clone();
            async move { engine.client().await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        first.abort(); // e.g. Esc on the popup during the first rewrite
        let second = tokio::spawn({
            let engine = engine.clone();
            async move { engine.client().await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(runs(dir.path()), 1, "the second request waits for the same start");
        second.abort();
    }

    #[tokio::test]
    async fn overlapping_reloads_apply_in_order_and_all_get_the_last_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let (config, active) = fake_server(dir.path(), "exec sleep 60");
        let engine = start(config.clone(), active.clone());
        // The first reload would hang; the last switches to a model that isn't there.
        let mut missing = Config::default();
        missing.model.active = Some(format!("custom:{}", dir.path().join("gone.gguf").display()));
        let first = engine.reload(config, active);
        let last = engine.reload(missing.model.clone(), models::active(&missing).unwrap());
        let (first, last) =
            tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(first, last) }).await.unwrap();
        assert!(last.as_ref().is_err_and(|e| e.contains("model file not found")), "{last:?}");
        assert_eq!(first, last, "a replaced reload reports the latest outcome");
        assert_eq!(*engine.state().borrow(), EngineState::Missing);
    }

    #[tokio::test]
    async fn start_brings_up_a_model_installed_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let (config, active) = fake_server(dir.path(), "exec sleep 60");
        std::fs::remove_file(&active.path).unwrap();
        let engine = start(config, active.clone());
        let mut state = engine.state();
        state.wait_for(|s| *s == EngineState::Missing).await.unwrap();
        std::fs::write(&active.path, b"gguf").unwrap(); // e.g. `quillway models install`
        engine.start();
        tokio::time::timeout(Duration::from_secs(2), state.wait_for(|s| *s == EngineState::Starting))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_new_server_spawns_only_after_the_old_one_has_exited() {
        let dir = tempfile::tempdir().unwrap();
        let (config, active) = fake_server(dir.path(), "exec sleep 60");
        let (_engine, mut supervisor) = Engine::new(config, active);
        supervisor.bring_up().await;
        let Llama::Starting { server, .. } = &supervisor.llama else { panic!("not starting") };
        let old = server.id().unwrap();
        supervisor.bring_up().await; // what a reload does
        // Reaped, not just killed: a killed process can hold GPU memory until it exits.
        assert!(!std::path::Path::new(&format!("/proc/{old}")).exists(), "the old server is gone");
        assert!(supervisor.starting(), "and a new one is starting");
    }

    #[tokio::test]
    async fn an_endpoint_is_ready_at_once() {
        let mut config = Config::default();
        config.model.endpoint = Some("http://127.0.0.1:1/v1".into());
        config.model.endpoint_model = Some("m".into());
        let active = models::active(&config).unwrap();
        let engine = start(config.model.clone(), active.clone());
        assert_eq!(engine.client().await.unwrap().endpoint().model, "m");
        assert_eq!(engine.reload(config.model, active).await, Ok(()));
        assert_eq!(*engine.state().borrow(), EngineState::Ready);
    }
}
