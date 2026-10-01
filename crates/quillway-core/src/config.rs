//! `config.toml` schema. Every field has a default, so an empty or missing file works.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: ModelConfig,
    pub ui: UiConfig,
    pub behavior: Behavior,
    /// Replaces the built-in presets when non-empty.
    #[serde(rename = "preset")]
    pub presets: Vec<Preset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    /// Catalog id or `custom:<path to .gguf>`. Unset: `models use` state, then the catalog default.
    pub active: Option<String>,
    /// OpenAI-compatible base URL (e.g. `http://127.0.0.1:11434/v1`). Set to skip the built-in llama-server.
    pub endpoint: Option<String>,
    /// Model name sent to `endpoint`.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub theme: ThemeChoice,
    /// `#rrggbb`
    pub accent: String,
    pub width: u32,
    /// Distance from the top of the output, in logical pixels.
    pub top_margin: u32,
    /// Panel opacity. Lower it when the compositor blurs behind the popup.
    pub opacity: f32,
    /// Draw the shadow ourselves (needs a transparent margin). Turn off when a
    /// compositor rule draws it, e.g. niri `layer-rule { shadow { on; } }`.
    pub client_shadow: bool,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub name: String,
    pub instruction: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Open the result in the word-diff view.
    #[serde(default)]
    pub show_diff: bool,
}

impl Config {
    /// Reads `path`; a missing file yields the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn presets(&self) -> Vec<Preset> {
        if self.presets.is_empty() { default_presets() } else { self.presets.clone() }
    }
}

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
    fn missing_file_is_default() {
        let c = Config::load(Path::new("/nonexistent/quillway.toml")).unwrap();
        assert!(c.model.active.is_none());
    }
}
