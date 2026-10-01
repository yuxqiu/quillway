//! Inference for Quillway.
//!
//! [`Engine`] hands out a [`Client`] for either the supervised llama-server
//! (default) or a user-configured OpenAI-compatible endpoint.

pub mod client;
pub mod download;
pub mod models;
pub mod server;

use std::sync::Arc;

use anyhow::{Context, bail};
use quillway_core::config::Config;
use tokio::sync::Mutex;

pub use client::{Client, Rewrite};
pub use models::Active;

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    config: Config,
    server: Option<server::Server>,
}

impl Engine {
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
    pub async fn client(&self) -> anyhow::Result<Client> {
        let mut inner = self.inner.lock().await;
        let cfg = inner.config.model.clone();
        if let Some(endpoint) = cfg.endpoint {
            return Ok(Client::new(
                endpoint,
                cfg.endpoint_api_key,
                cfg.endpoint_model.unwrap_or_else(|| "default".into()),
                false,
            ));
        }
        let active = models::active(&inner.config);
        if !active.path.exists() {
            bail!("model {} is not installed (run `quillway models install {}`)", active.name, active.id);
        }
        let reuse = match inner.server.as_mut() {
            Some(s) => s.model_path() == active.path && s.is_alive(),
            None => false,
        };
        if !reuse {
            inner.server = None; // kill the old one before starting another
            let s = server::Server::start(&active.path, &cfg).await.context("starting llama-server")?;
            inner.server = Some(s);
        }
        let s = inner.server.as_ref().expect("server just ensured");
        Ok(Client::new(s.base_url(), Some(s.api_key().to_owned()), active.id.clone(), true))
    }

    /// Start the server and run one tiny request, so GPU pipelines are built
    /// and the fixed prompt prefix is cached before the first real rewrite.
    pub async fn warm_up(&self) -> anyhow::Result<()> {
        let client = self.client().await?;
        if client.is_llama() {
            let sampling = self.active().await.sampling;
            let r = Rewrite {
                instruction: "Proofread.".into(),
                text: "ok".into(),
                temperature: 0.0,
                sampling,
                max_tokens: 1,
            };
            client.complete(&r).await?;
        }
        Ok(())
    }

    pub async fn describe(&self) -> String {
        let inner = self.inner.lock().await;
        match &inner.config.model.endpoint {
            Some(e) => format!("endpoint {e}"),
            None if inner.server.is_some() => "llama-server (running)".into(),
            None => "llama-server (stopped)".into(),
        }
    }
}
