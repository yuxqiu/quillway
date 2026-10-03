//! Which model is active, and where it lives on disk.

use std::path::{Path, PathBuf};

use anyhow::Context;
use quillway_core::catalog::{self, Sampling};
use quillway_core::config::Config;
use quillway_core::paths;
use serde::{Deserialize, Serialize};

/// The model requests will use.
#[derive(Debug, Clone)]
pub struct Active {
    /// Catalog id, or `custom:<path>`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The GGUF file (may not exist yet).
    pub path: PathBuf,
    /// Sampling defaults.
    pub sampling: Sampling,
    /// The catalog entry (installable); `None` for a custom path.
    pub entry: Option<&'static catalog::Entry>,
}

/// Mutable choices made through the CLI (`models use`), kept apart from the
/// possibly read-only, Nix-generated config file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Chosen with `quillway models use`.
    pub active: Option<String>,
}

impl State {
    /// The saved state; empty if missing or unreadable.
    #[must_use]
    pub fn load() -> Self {
        std::fs::read_to_string(paths::state_file()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    /// Write the state file.
    ///
    /// # Errors
    ///
    /// The state directory or file can't be written.
    pub fn save(&self) -> anyhow::Result<()> {
        let p = paths::state_file();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&p, toml::to_string(self)?).with_context(|| format!("writing {}", p.display()))
    }
}

/// Priority: config `model.active`, then `models use` state, then the catalog default.
///
/// # Errors
///
/// The chosen id isn't in the catalog, e.g. a `models use` choice that a
/// newer version dropped.
pub fn active(config: &Config) -> anyhow::Result<Active> {
    let (chosen, source) = config
        .model
        .active
        .clone()
        .map_or_else(|| (State::load().active, "`quillway models use`"), |id| (Some(id), "`model.active`"));
    resolve(chosen.as_deref(), &paths::models_dir()).with_context(|| format!("the model chosen with {source}"))
}

fn resolve(id: Option<&str>, models_dir: &Path) -> anyhow::Result<Active> {
    if let Some(path) = id.and_then(|i| i.strip_prefix("custom:")) {
        let path = PathBuf::from(path);
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        return Ok(Active {
            id: format!("custom:{}", path.display()),
            name,
            path,
            sampling: Sampling { top_p: 0.9, top_k: 40, min_p: 0.05 },
            entry: None,
        });
    }
    let e = id.map_or_else(|| Ok(catalog::default_entry()), catalog::get)?;
    Ok(Active {
        id: e.id.clone(),
        name: e.name.clone(),
        path: e.path_in(models_dir),
        sampling: e.sampling,
        entry: Some(e),
    })
}

/// Whether the entry's file is downloaded.
#[must_use]
pub fn is_installed(e: &catalog::Entry) -> bool {
    e.path_in(&paths::models_dir()).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_catalog_custom_and_fallback() {
        let dir = Path::new("/m");
        assert_eq!(resolve(Some("gemma-4-e4b"), dir).unwrap().name, "Gemma 4 E4B");
        assert_eq!(resolve(None, dir).unwrap().id, "qwen3.5-4b");
        let c = resolve(Some("custom:/x/My-Model.gguf"), dir).unwrap();
        assert_eq!((c.name.as_str(), c.entry.is_none()), ("My-Model", true));
        assert_eq!(c.path, PathBuf::from("/x/My-Model.gguf"));
    }

    #[test]
    fn an_unknown_id_is_an_error_not_the_default() {
        let error = resolve(Some("dropped-model"), Path::new("/m")).unwrap_err();
        assert!(error.to_string().contains("unknown model \"dropped-model\""), "{error}");
    }
}
