//! Which model is active, and where it lives on disk.

use std::path::{Path, PathBuf};

use anyhow::Context;
use quillway_core::catalog::{self, Sampling};
use quillway_core::config::Config;
use quillway_core::paths;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct Active {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub sampling: Sampling,
    /// A catalog entry (installable) rather than a custom path.
    pub catalog: bool,
}

/// Mutable choices made through the CLI (`models use`), kept apart from the
/// possibly read-only, Nix-generated config file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub active: Option<String>,
}

impl State {
    pub fn load() -> Self {
        std::fs::read_to_string(paths::state_file()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = paths::state_file();
        std::fs::create_dir_all(p.parent().expect("state file has a parent"))?;
        std::fs::write(&p, toml::to_string(self)?).with_context(|| format!("writing {}", p.display()))
    }
}

/// Priority: config `model.active`, then `models use` state, then the catalog default.
pub fn active(config: &Config) -> Active {
    let chosen = config.model.active.clone().or_else(|| State::load().active);
    resolve(chosen.as_deref(), &paths::models_dir())
}

pub fn resolve(id: Option<&str>, models_dir: &Path) -> Active {
    if let Some(path) = id.and_then(|i| i.strip_prefix("custom:")) {
        let path = PathBuf::from(path);
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        return Active {
            id: format!("custom:{}", path.display()),
            name,
            path,
            sampling: Sampling { top_p: 0.9, top_k: 40, min_p: 0.05 },
            catalog: false,
        };
    }
    let e = id.and_then(catalog::find).unwrap_or_else(catalog::default_entry);
    Active { id: e.id.clone(), name: e.name.clone(), path: e.path_in(models_dir), sampling: e.sampling, catalog: true }
}

pub fn is_installed(e: &catalog::Entry) -> bool {
    e.path_in(&paths::models_dir()).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_catalog_custom_and_fallback() {
        let dir = Path::new("/m");
        assert_eq!(resolve(Some("gemma-4-e4b"), dir).name, "Gemma 4 E4B");
        assert_eq!(resolve(None, dir).id, "qwen3.5-4b");
        assert_eq!(resolve(Some("no-such-model"), dir).id, "qwen3.5-4b");
        let c = resolve(Some("custom:/x/My-Model.gguf"), dir);
        assert_eq!((c.name.as_str(), c.catalog), ("My-Model", false));
        assert_eq!(c.path, PathBuf::from("/x/My-Model.gguf"));
    }
}
