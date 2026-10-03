//! Inference for Quillway.
//!
//! [`Engine`] hands out a [`Client`] for either the supervised llama-server
//! (default) or a user-configured OpenAI-compatible endpoint.

pub mod client;
pub mod download;
pub mod models;
mod server;

use std::sync::Arc;

use anyhow::Context;
use quillway_core::config::ModelConfig;
use quillway_core::ipc::Endpoint;
use tokio::sync::Mutex;

pub use client::{Chunk, Client, Interrupted, Rewrite, Timing};
pub use models::Active;

/// Shared handle to the model backend; clones share one server.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    config: ModelConfig,
    /// Resolved once per config ([`models::active`]), so every request uses the
    /// model the caller showed to the user.
    active: Active,
    server: Option<server::Server>,
}

impl Engine {
    /// An engine for the `[model]` config and its active model; nothing starts
    /// until the first request.
    #[must_use]
    pub fn new(config: ModelConfig, active: Active) -> Self {
        Self { inner: Arc::new(Mutex::new(Inner { config, active, server: None })) }
    }

    /// Swap the config and drop the running server; the next request restarts it.
    pub async fn reload(&self, config: ModelConfig, active: Active) {
        *self.inner.lock().await = Inner { config, active, server: None };
    }

    /// A client ready to accept requests, starting llama-server if needed.
    ///
    /// # Errors
    ///
    /// The model isn't installed, or llama-server fails to start.
    #[expect(clippy::significant_drop_tightening, reason = "held across startup so callers share one server")]
    pub async fn client(&self) -> anyhow::Result<Client> {
        let mut inner = self.inner.lock().await;
        let Inner { config, active, server } = &mut *inner;
        if let Some(base) = &config.endpoint {
            return Ok(Client::new(Endpoint {
                base: base.clone(),
                api_key: config.endpoint_api_key.clone(),
                model: config.endpoint_model.clone().unwrap_or_default(),
                llama: false,
                context: config.context,
                sampling: active.sampling,
            }));
        }
        active.ensure_installed()?;
        if let Some(s) = server.as_mut()
            && !s.is_alive()
        {
            eprintln!("quillway: llama-server exited; restarting. Its last output:\n{}", s.log_tail());
            *server = None;
        }
        let server = match server {
            Some(s) => s,
            None => server.insert(server::Server::start(&active.path, config).await.context("starting llama-server")?),
        };
        Ok(Client::new(Endpoint {
            base: server.base_url(),
            api_key: Some(server.api_key().to_owned()),
            model: active.id.clone(),
            llama: true,
            context: server.context(),
            sampling: active.sampling,
        }))
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
