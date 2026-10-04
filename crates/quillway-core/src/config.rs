//! `config.toml` schema. Every field has a default, so an empty or missing file works.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::Context;
use serde::Deserialize;

use crate::{catalog, paths};

/// The whole config file.
#[derive(Debug, Clone, Default, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
    /// Extra arguments appended to the llama-server command line, except those
    /// in [`RESERVED_ARGS`].
    pub extra_args: Vec<String>,
}

/// llama-server flags Quillway sets itself, refused in `model.extra_args`.
pub const RESERVED_ARGS: [&str; 11] = [
    "-m",
    "--model",
    "--host",
    "--port",
    "-c",
    "--ctx-size",
    "-np",
    "--parallel",
    "--api-key",
    "--api-key-file",
    "--no-jinja",
];

impl ModelConfig {
    /// The `llama-server` to run: `llama_server`, else the one on `$PATH`.
    #[must_use]
    pub fn llama_server_bin(&self) -> &str {
        self.llama_server.as_deref().unwrap_or("llama-server")
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Light text on a dark panel.
    #[default]
    Dark,
    /// Dark text on a light panel.
    Light,
}

/// `[ui]`: popup look and placement.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Colour scheme.
    pub theme: ThemeChoice,
    /// Accent colour, `#rrggbb`.
    pub accent: Rgb,
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
            theme: ThemeChoice::default(),
            accent: Rgb([0x7c, 0x6c, 0xf2]),
            width: 680,
            top_margin: 220,
            opacity: 0.94,
            client_shadow: true,
            font: None,
        }
    }
}

/// A colour written `#rrggbb`, as red, green and blue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Rgb(pub [u8; 3]);

impl TryFrom<String> for Rgb {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let invalid = || format!("{s:?} must be a colour like \"#7c6cf2\"");
        let hex = s.strip_prefix('#').filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        let hex = hex.ok_or_else(invalid)?;
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| invalid());
        Ok(Self([channel(0)?, channel(2)?, channel(4)?]))
    }
}

/// `[behavior]`
#[derive(Debug, Clone, Deserialize)]
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

/// Temperature of a preset that sets none, and of typed instructions.
pub const DEFAULT_TEMPERATURE: f32 = 0.7;

const fn default_temperature() -> f32 {
    DEFAULT_TEMPERATURE
}

/// `[[preset]]`: a one-key rewrite.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    /// Chip label.
    pub name: String,
    /// The task given to the model.
    pub instruction: String,
    /// Sampling temperature. Low for proofreading, higher for rewording.
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// Open the result in the word-diff view.
    #[serde(default)]
    pub show_diff: bool,
}

impl Config {
    /// Reads the user's `config.toml` ([`paths::config_file`]).
    ///
    /// # Errors
    ///
    /// As [`Config::load`].
    pub fn load_user() -> anyhow::Result<Self> {
        Self::load(&paths::config_file())
    }

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
            } else {
                catalog::get(active)?;
            }
        }
        for p in &config.presets {
            if p.name.trim().is_empty() || p.instruction.trim().is_empty() {
                anyhow::bail!("every `[[preset]]` needs a non-empty `name` and `instruction` (got name {:?})", p.name);
            }
            if !(0.0..=2.0).contains(&p.temperature) {
                anyhow::bail!("preset {:?}: `temperature = {}` must be between 0 and 2", p.name, p.temperature);
            }
        }
        let ui = &config.ui;
        if !(300..=4096).contains(&ui.width) {
            anyhow::bail!("`ui.width = {}` must be between 300 and 4096 pixels", ui.width);
        }
        if !(0.2..=1.0).contains(&ui.opacity) {
            anyhow::bail!("`ui.opacity = {}` must be between 0.2 and 1.0", ui.opacity);
        }
        if config.model.context < 1024 {
            anyhow::bail!(
                "`model.context = {}` must be at least 1024: the fixed instructions alone take ~400 tokens",
                config.model.context
            );
        }
        if config.model.endpoint.is_some() && config.model.endpoint_model.is_none() {
            anyhow::bail!("`model.endpoint` needs `model.endpoint_model`, the model name the server expects");
        }
        if let Some(url) = &config.model.endpoint
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            anyhow::bail!("`model.endpoint = {url:?}` must be an http:// or https:// URL");
        }
        // Quillway sets these and relies on them: llama-server takes the last of a repeated flag.
        for arg in &config.model.extra_args {
            let flag = arg.split('=').next().unwrap_or_default();
            if RESERVED_ARGS.contains(&flag) {
                anyhow::bail!(
                    "`model.extra_args` can't set `{flag}`: Quillway sets it (the context size is `model.context`)"
                );
            }
        }
        Ok(config)
    }

    /// The configured presets, or the built-in ones if none are configured.
    #[must_use]
    pub fn presets(&self) -> &[Preset] {
        if self.presets.is_empty() { &DEFAULT_PRESETS } else { &self.presets }
    }
}

static DEFAULT_PRESETS: LazyLock<Vec<Preset>> = LazyLock::new(default_presets);

/// Proofread, Rewrite, Friendly, Professional, Concise, Summary, Key points.
fn default_presets() -> Vec<Preset> {
    let p = |name: &str, instruction: &str, temperature: f32, show_diff: bool| Preset {
        name: name.into(),
        instruction: instruction.into(),
        temperature,
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
        assert!((c.presets()[0].temperature - DEFAULT_TEMPERATURE).abs() < f32::EPSILON);
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
    fn accent_must_be_a_hex_colour() {
        for bad in ["purple", "#7c6cf", "#7c6cf2ff", "7c6cf2", "#zzzzzz"] {
            assert!(Config::parse(&format!("[ui]\naccent = '{bad}'")).is_err(), "{bad}");
        }
        let ok = Config::parse("[ui]\naccent = '#7C6CF2'").unwrap();
        assert_eq!(ok.ui.accent, Rgb([0x7c, 0x6c, 0xf2]));
        let error = Config::parse("[ui]\naccent = 'purple'").unwrap_err();
        assert!(format!("{error:#}").contains("must be a colour like"), "{error:#}");
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        for bad in [
            "[ui]\nwidth = 0",
            "[ui]\nwidth = 70000",
            "[ui]\nopacity = 0.1",
            "[ui]\nopacity = nan",
            "[model]\ncontext = 512",
            "[[preset]]\nname = 'X'\ninstruction = 'Do it.'\ntemperature = -1.0",
            "[[preset]]\nname = 'X'\ninstruction = 'Do it.'\ntemperature = nan",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        assert!(Config::parse("[ui]\nwidth = 300\nopacity = 1.0\n[model]\ncontext = 1024").is_ok());
    }

    #[test]
    fn presets_need_a_name_and_instruction() {
        assert!(Config::parse("[[preset]]\nname = 'X'\ninstruction = ' '").is_err());
        assert!(Config::parse("[[preset]]\nname = ''\ninstruction = 'Do it.'").is_err());
    }

    #[test]
    fn extra_args_cannot_override_what_quillway_sets() {
        for arg in ["--port", "--port=8080", "-c", "--api-key"] {
            let toml = format!("[model]\nextra_args = [\"{arg}\", \"1\"]");
            assert!(Config::parse(&toml).unwrap_err().to_string().contains("can't set"), "{arg}");
        }
        assert!(Config::parse("[model]\nextra_args = [\"--threads\", \"4\"]").is_ok());
    }

    #[test]
    fn an_endpoint_must_be_a_url() {
        assert!(Config::parse("[model]\nendpoint = \"\"\nendpoint_model = \"m\"").is_err());
        assert!(Config::parse("[model]\nendpoint = \"https://h/v1\"\nendpoint_model = \"m\"").is_ok());
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
