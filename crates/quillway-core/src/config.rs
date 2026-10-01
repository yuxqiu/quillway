//! `config.toml` schema. Every field has a default, so an empty or missing file works.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::catalog;

/// The whole config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `[model]`
    pub model: ModelConfig,
    /// `[ui]`
    pub ui: UiConfig,
    /// `[behavior]`
    pub behavior: Behavior,
    /// Replaces the built-in presets when non-empty.
    #[serde(rename = "preset")]
    pub presets: Vec<Preset>,
}

/// `[model]`: which model runs and how.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    /// Catalog id or `custom:<path to .gguf>`. Unset: `models use` state, then the catalog default.
    pub active: Option<String>,
    /// OpenAI-compatible base URL (e.g. `http://127.0.0.1:11434/v1`). Set to skip the built-in llama-server.
    pub endpoint: Option<String>,
    /// Model name sent to `endpoint`; required with it.
    pub endpoint_model: Option<String>,
    /// Bearer token for `endpoint`.
    pub endpoint_api_key: Option<String>,
    /// Path to `llama-server`; defaults to `$PATH` lookup.
    pub llama_server: Option<String>,
    /// How many of the model's layers run on the GPU; the rest run on the CPU.
    /// 99 means "all" (no catalog model has that many). Lower it only if the
    /// GPU runs out of memory; 0 is CPU-only and several times slower.
    pub gpu_layers: u32,
    /// The model's working memory in tokens (~¾ of an English word each). One
    /// rewrite must fit the fixed instructions (~400), your text and the result,
    /// so 8192 handles roughly 3,000 words. Higher allows longer text but uses
    /// more memory.
    pub context: u32,
    /// Extra arguments appended to the llama-server command line.
    pub extra_args: Vec<String>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            active: None,
            endpoint: None,
            endpoint_model: None,
            endpoint_api_key: None,
            llama_server: None,
            gpu_layers: 99,
            context: 8192,
            extra_args: Vec::new(),
        }
    }
}

/// Popup colour scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Light text on a dark panel.
    #[default]
    Dark,
    /// Dark text on a light panel.
    Light,
}

/// `[ui]`: popup look and placement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Colour scheme.
    pub theme: ThemeChoice,
    /// Accent colour, `#rrggbb`.
    pub accent: String,
    /// Panel width in logical pixels.
    pub width: u32,
    /// Distance from the top of the output, in logical pixels.
    pub top_margin: u32,
    /// Panel opacity. Lower it when the compositor blurs behind the popup.
    pub opacity: f32,
    /// Draw the shadow ourselves (needs a transparent margin). Turn off when a
    /// compositor rule draws it, e.g. niri `layer-rule { shadow { on; } }`.
    pub client_shadow: bool,
    /// Font family name; the system sans-serif when unset. Applies after a daemon restart.
    pub font: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            accent: "#7c6cf2".into(),
            width: 680,
            top_margin: 220,
            opacity: 0.94,
            client_shadow: true,
            font: None,
        }
    }
}

/// `[behavior]`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Behavior {
    /// The popup starts with the clipboard text only if it was copied within
    /// this many seconds; otherwise it opens empty for you to type or paste.
    pub recent_secs: u64,
}

impl Default for Behavior {
    fn default() -> Self {
        Self { recent_secs: 60 }
    }
}

/// `[[preset]]`: a one-key rewrite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    /// Chip label.
    pub name: String,
    /// The task given to the model.
    pub instruction: String,
    /// Sampling temperature; 0.7 when unset. Low for proofreading, higher for rewording.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Open the result in the word-diff view.
    #[serde(default)]
    pub show_diff: bool,
}

impl Config {
    /// Reads `path`; a missing file yields the defaults.
    ///
    /// # Errors
    ///
    /// The file exists but can't be read or isn't a valid config.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn parse(s: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(s)?;
        if let Some(active) = &config.model.active {
            if let Some(path) = active.strip_prefix("custom:") {
                if path.is_empty() {
                    anyhow::bail!("`model.active = \"custom:\"` needs a model file path");
                }
                // The daemon's working directory is arbitrary, and `~` isn't expanded.
                if !Path::new(path).is_absolute() {
                    anyhow::bail!("`model.active`: the custom model path {path:?} must be absolute");
                }
            } else if catalog::find(active).is_none() {
                let ids: Vec<_> = catalog::all().iter().map(|e| e.id.as_str()).collect();
                anyhow::bail!("unknown model {active:?} in `model.active`; available: {}", ids.join(", "));
            }
        }
        if config.model.endpoint.is_some() && config.model.endpoint_model.is_none() {
            anyhow::bail!("`model.endpoint` needs `model.endpoint_model`, the model name the server expects");
        }
        Ok(config)
    }

    /// The configured presets, or the built-in ones if none are configured.
    #[must_use]
    pub fn presets(&self) -> Vec<Preset> {
        if self.presets.is_empty() { default_presets() } else { self.presets.clone() }
    }
}

/// Proofread, Rewrite, Friendly, Professional, Concise, Summary, Key points.
#[must_use]
pub fn default_presets() -> Vec<Preset> {
    let p = |name: &str, instruction: &str, temperature: f32, show_diff: bool| Preset {
        name: name.into(),
        instruction: instruction.into(),
        temperature: Some(temperature),
        show_diff,
    };
    vec![
        p(
            "Proofread",
            "Fix only clear errors in spelling, grammar and punctuation. Do not change wording or style.",
            0.2,
            true,
        ),
        p("Rewrite", "Rewrite to improve clarity and flow while keeping the meaning and tone.", 0.7, false),
        p("Friendly", "Rewrite in a warm, friendly tone.", 0.7, false),
        p("Professional", "Rewrite in a clear, professional tone.", 0.6, false),
        p("Concise", "Make it more concise without losing important information.", 0.5, false),
        p("Summary", "Summarize the text in a few sentences.", 0.5, false),
        p("Key points", "List the key points as a short bulleted list using '- '.", 0.5, false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.model.context, 8192);
        assert_eq!(c.presets().len(), 7);
        assert_eq!(c.behavior.recent_secs, 60);
    }

    #[test]
    fn user_presets_replace_defaults() {
        let c: Config = toml::from_str(
            r#"
            [ui]
            theme = "light"
            [[preset]]
            name = "Pirate"
            instruction = "Rewrite like a pirate."
            "#,
        )
        .unwrap();
        assert_eq!(c.ui.theme, ThemeChoice::Light);
        assert_eq!(c.presets().len(), 1);
        assert_eq!(c.presets()[0].name, "Pirate");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("[ui]\nbogus = 1").is_err());
    }

    #[test]
    fn unknown_active_model_is_rejected() {
        let error = Config::parse("[model]\nactive = 'qwen3.5-typo'").unwrap_err();
        assert!(error.to_string().contains("unknown model"), "{error}");
    }

    #[test]
    fn unknown_active_model_error_lists_the_catalog() {
        let error = Config::parse("[model]\nactive = 'qwen3.5-typo'").unwrap_err();
        assert!(error.to_string().contains("available: qwen3.5-2b"), "{error}");
    }

    #[test]
    fn custom_model_path_must_be_absolute() {
        for path in ["models/x.gguf", "~/x.gguf"] {
            let error = Config::parse(&format!("[model]\nactive = 'custom:{path}'")).unwrap_err();
            assert!(error.to_string().contains("absolute"), "{error}");
        }
        assert!(Config::parse("[model]\nactive = 'custom:/m/x.gguf'").is_ok());
    }

    #[test]
    fn custom_model_requires_a_path() {
        let error = Config::parse("[model]\nactive = 'custom:'").unwrap_err();
        assert!(error.to_string().contains("path"), "{error}");
    }

    #[test]
    fn endpoint_requires_a_model_name() {
        assert!(Config::parse("[model]\nendpoint = \"http://h/v1\"").is_err());
        assert!(Config::parse("[model]\nendpoint = \"http://h/v1\"\nendpoint_model = \"m\"").is_ok());
    }

    #[test]
    fn missing_file_is_default() {
        let c = Config::load(Path::new("/nonexistent/quillway.toml")).unwrap();
        assert!(c.model.active.is_none());
    }
}
