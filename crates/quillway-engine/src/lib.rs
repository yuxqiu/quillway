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
use quillway_core::config::Config;
use quillway_core::ipc::Endpoint;
use tokio::sync::Mutex;

pub use client::{Client, Rewrite};
pub use models::Active;

/// Shared handle to the model backend; clones share one server.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    config: Config,
    server: Option<server::Server>,
}

impl Engine {
    /// An engine for `config`; nothing starts until the first request.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self { inner: Arc::new(Mutex::new(Inner { config, server: None })) }
    }

    /// Swap the config and drop the running server; the next request restarts it.
    pub async fn reload(&self, config: Config) {
        let mut inner = self.inner.lock().await;
        inner.config = config;
        inner.server = None;
    }

    /// The model the next request will use.
    pub async fn active(&self) -> Active {
        models::active(&self.inner.lock().await.config)
    }

    /// A client ready to accept requests, starting llama-server if needed.
    ///
    /// # Errors
    ///
    /// The model isn't installed, or llama-server fails to start.
    #[expect(clippy::significant_drop_tightening, reason = "held across startup so callers share one server")]
    pub async fn client(&self) -> anyhow::Result<Client> {
        let mut inner = self.inner.lock().await;
        let cfg = inner.config.model.clone();
        let active = models::active(&inner.config);
        if let Some(base) = cfg.endpoint {
            drop(inner);
            return Ok(Client::new(Endpoint {
                base,
                api_key: cfg.endpoint_api_key,
                model: cfg.endpoint_model.unwrap_or_default(),
                llama: false,
                context: cfg.context,
                sampling: active.sampling,
            }));
        }
        if !active.path.is_file() {
            if active.entry.is_none() {
                bail!("model file not found: {}", active.path.display());
            }
            bail!("model {} is not installed (run `quillway models install {}`)", active.name, active.id);
        }
        // The lock is held across startup on purpose: concurrent callers wait
        // for this server instead of starting their own.
        if let Some(s) = inner.server.as_mut()
            && !s.is_alive()
        {
            eprintln!("quillway: llama-server exited; restarting. Its last output:\n{}", s.log_tail());
        }
        let reusable = inner.server.as_mut().is_some_and(|s| s.model_path() == active.path && s.is_alive());
        let server = match inner.server.take() {
            Some(s) if reusable => s,
            old => {
                drop(old); // stop the previous server before starting another
                server::Server::start(&active.path, &cfg).await.context("starting llama-server")?
            }
        };
        let client = Client::new(Endpoint {
            base: server.base_url(),
            api_key: Some(server.api_key().to_owned()),
            model: active.id,
            llama: true,
            context: server.context(),
            sampling: active.sampling,
        });
        inner.server = Some(server);
        Ok(client)
    }

    /// Start the server and run one tiny request, so GPU pipelines are built
    /// and the fixed prompt prefix is cached before the first real rewrite.
    ///
    /// # Errors
    ///
    /// As [`Engine::client`], or the warm-up request fails.
    pub async fn warm_up(&self) -> anyhow::Result<()> {
        let client = self.client().await?;
        if client.is_llama() {
            client.warm_up().await?;
        }
        Ok(())
    }
}
