//! Inference for Quillway.
//!
//! [`Engine`] hands out a [`Client`] for either the supervised llama-server
//! (default) or a user-configured OpenAI-compatible endpoint.

pub mod client;
pub mod download;
pub mod models;
mod server;

use std::sync::Arc;

use anyhow::{Context, bail};
use quillway_core::config::ModelConfig;
use quillway_core::ipc::Endpoint;
use tokio::sync::{Mutex, watch};

pub use client::{Chunk, Client, Interrupted, Rewrite, Timing};
pub use models::Active;

/// Shared handle to the model backend; clones share one server.
#[derive(Clone)]
pub struct Engine {
    shared: Arc<Shared>,
}

struct Shared {
    /// Held only briefly, so `reload` never waits for a llama-server start.
    state: Mutex<State>,
    /// One llama-server start at a time: concurrent requests wait for it rather
    /// than loading a second copy of the model (DECISIONS #20).
    starting: Mutex<()>,
    /// The current [`State::epoch`]; a start in progress gives up when it changes.
    reloads: watch::Sender<u64>,
}

struct State {
    config: ModelConfig,
    /// Resolved once per config ([`models::active`]), so every request uses the
    /// model the caller showed to the user.
    active: Active,
    server: Option<server::Server>,
    /// Bumped by each reload.
    epoch: u64,
}

impl State {
    /// A client for the endpoint or the running llama-server, if there is one.
    fn ready_client(&mut self) -> anyhow::Result<Option<Client>> {
        if let Some(base) = &self.config.endpoint {
            return Ok(Some(Client::new(Endpoint {
                base: base.clone(),
                api_key: self.config.endpoint_api_key.clone(),
                model: self.config.endpoint_model.clone().unwrap_or_default(),
                llama: false,
                context: self.config.context,
                sampling: self.active.sampling,
            })));
        }
        self.active.ensure_installed()?;
        if let Some(s) = self.server.as_mut()
            && !s.is_alive()
        {
            eprintln!("quillway: llama-server exited; restarting. Its last output:\n{}", s.log_tail());
            self.server = None;
        }
        Ok(self.server.as_ref().map(|server| llama_client(server, &self.active)))
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

impl Engine {
    /// An engine for the `[model]` config and its active model; nothing starts
    /// until the first request.
    #[must_use]
    pub fn new(config: ModelConfig, active: Active) -> Self {
        let state = State { config, active, server: None, epoch: 0 };
        let shared = Shared { state: Mutex::new(state), starting: Mutex::new(()), reloads: watch::Sender::new(0) };
        Self { shared: Arc::new(shared) }
    }

    /// Swap the config and stop the server, including one still starting; the
    /// next request starts the new one. Never waits for a start.
    pub async fn reload(&self, config: ModelConfig, active: Active) {
        let epoch = {
            let mut state = self.shared.state.lock().await;
            let epoch = state.epoch + 1;
            *state = State { config, active, server: None, epoch };
            epoch
        };
        self.shared.reloads.send_replace(epoch);
    }

    /// A client ready to accept requests, starting llama-server if needed.
    ///
    /// # Errors
    ///
    /// The model isn't installed, llama-server fails to start, or a reload
    /// cancelled the start.
    pub async fn client(&self) -> anyhow::Result<Client> {
        let epoch = {
            let mut state = self.shared.state.lock().await;
            if let Some(client) = state.ready_client()? {
                return Ok(client);
            }
            state.epoch
        };
        let _one_start = self.shared.starting.lock().await;
        // Another request may have started it, or a reload replaced it, while we waited.
        let (config, active) = {
            let mut state = self.shared.state.lock().await;
            if state.epoch != epoch {
                bail!("cancelled: the model server was reloaded");
            }
            if let Some(client) = state.ready_client()? {
                return Ok(client);
            }
            (state.config.clone(), state.active.clone())
        };
        let mut reloads = self.shared.reloads.subscribe();
        let server = tokio::select! {
            server = server::Server::start(&active.path, &config) => server.context("starting llama-server")?,
            // Dropping the start kills the half-started llama-server.
            _ = reloads.wait_for(|&e| e != epoch) => bail!("cancelled: the model server was reloaded"),
        };
        let mut state = self.shared.state.lock().await;
        if state.epoch != epoch {
            bail!("cancelled: the model server was reloaded"); // dropping `server` stops it
        }
        let client = llama_client(&server, &state.active);
        state.server = Some(server);
        drop(state);
        Ok(client)
    }

    /// Start the server and run one tiny request, so GPU pipelines are built
    /// and the fixed prompt prefix is cached before the first real rewrite.
    ///
    /// # Errors
    ///
    /// As [`Engine::client`], or the warm-up request fails.
    pub async fn warm_up(&self) -> anyhow::Result<()> {
        self.client().await?.warm_up().await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use quillway_core::config::Config;

    use super::*;

    /// A model file and a "llama-server" that never becomes ready.
    fn hanging_server(dir: &std::path::Path) -> (ModelConfig, Active) {
        let model = dir.join("model.gguf");
        std::fs::write(&model, b"gguf").unwrap();
        let bin = dir.join("llama-server");
        std::fs::write(&bin, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut config = Config::default();
        config.model.active = Some(format!("custom:{}", model.display()));
        config.model.llama_server = Some(bin.display().to_string());
        let active = models::active(&config).unwrap();
        (config.model, active)
    }

    #[tokio::test]
    async fn a_reload_cancels_a_start_instead_of_waiting_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let (config, active) = hanging_server(dir.path());
        let engine = Engine::new(config.clone(), active.clone());
        let starting = tokio::spawn({
            let engine = engine.clone();
            async move { engine.client().await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await; // the start is waiting for /health
        tokio::time::timeout(Duration::from_secs(1), engine.reload(config, active))
            .await
            .expect("a reload doesn't wait for the start");
        let error = tokio::time::timeout(Duration::from_secs(2), starting).await.unwrap().unwrap().unwrap_err();
        assert!(error.to_string().contains("reloaded"), "{error}");
    }
}
