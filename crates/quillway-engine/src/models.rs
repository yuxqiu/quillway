//! Which model is active, and where it lives on disk.

use std::path::{Path, PathBuf};

use anyhow::Context;
use quillway_core::catalog::{self, Sampling};
use quillway_core::config::Config;
use quillway_core::paths;

use crate::download;
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
    /// The saved state; empty if there is none.
    ///
    /// # Errors
    ///
    /// The file exists but can't be read or parsed: a broken choice is an
    /// error, not a silent fallback to the default (DECISIONS #14).
    pub fn load() -> anyhow::Result<Self> {
        let p = paths::state_file();
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
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
        // Write and rename, so a daemon reading it never sees a half-written file.
        let tmp = p.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string(self)?).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &p).with_context(|| format!("replacing {}", p.display()))
    }
}

/// Priority: config `model.active`, then `models use` state, then the catalog default.
///
/// # Errors
///
/// The chosen id isn't in the catalog, e.g. a `models use` choice that a
/// newer version dropped.
pub fn active(config: &Config) -> anyhow::Result<Active> {
    let chosen = config.model.active.clone().map_or_else(
        || State::load().map(|state| (state.active, "`quillway models use`")),
        |id| Ok((Some(id), "`model.active`")),
    );
    let active = chosen.and_then(|(chosen, source)| {
        resolve(chosen.as_deref(), &paths::models_dir()).with_context(|| format!("the model chosen with {source}"))
    });
    match active {
        // With `model.endpoint`, the local model only lends its sampling defaults: a broken choice doesn't matter.
        Err(_) if config.model.endpoint.is_some() => resolve(None, &paths::models_dir()),
        active => active,
    }
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

impl Active {
    /// Whether the model file is on disk.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.path.is_file()
    }

    /// An error saying how to get the model, if it isn't on disk.
    ///
    /// # Errors
    ///
    /// The model file doesn't exist.
    pub fn ensure_installed(&self) -> anyhow::Result<()> {
        match self.entry {
            _ if self.is_installed() => Ok(()),
            Some(e) => anyhow::bail!("model {} is not installed (run `quillway models install {}`)", e.name, e.id),
            None => anyhow::bail!("model file not found: {}", self.path.display()),
        }
    }
}

/// Where a catalog model lives on disk.
#[must_use]
pub fn path(e: &catalog::Entry) -> PathBuf {
    e.path_in(&paths::models_dir())
}

/// Whether the entry's file is downloaded.
#[must_use]
pub fn is_installed(e: &catalog::Entry) -> bool {
    path(e).is_file()
}

/// Download and verify `e` into the models directory, resuming a partial download.
///
/// # Errors
///
/// As [`download::download`].
pub async fn install(e: &catalog::Entry, on_progress: impl FnMut(download::Progress)) -> anyhow::Result<()> {
    let job = download::Job { url: &e.url(), dest: &path(e), size: e.size, sha256: &e.sha256 };
    download::download(job, on_progress).await
}

/// Delete `e`'s files; `Ok(false)` if it wasn't installed.
///
/// # Errors
///
/// As [`download::remove`].
pub fn remove(e: &catalog::Entry) -> anyhow::Result<bool> {
    download::remove(&path(e))
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
    fn a_missing_model_says_how_to_get_it() {
        let dir = Path::new("/nonexistent");
        let catalog = resolve(Some("gemma-4-e4b"), dir).unwrap().ensure_installed().unwrap_err();
        assert!(catalog.to_string().contains("run `quillway models install gemma-4-e4b`"), "{catalog}");
        let custom = resolve(Some("custom:/nonexistent/x.gguf"), dir).unwrap().ensure_installed().unwrap_err();
        assert!(custom.to_string().contains("model file not found: /nonexistent/x.gguf"), "{custom}");
    }

    #[test]
    fn an_endpoint_ignores_a_broken_model_choice() {
        let mut config = Config::default();
        config.model.active = Some("dropped-model".into()); // e.g. left in `state.toml`
        assert!(active(&config).is_err());
        config.model.endpoint = Some("http://127.0.0.1:1/v1".into());
        assert_eq!(active(&config).unwrap().id, "qwen3.5-4b", "only its sampling defaults are used");
    }

    #[test]
    fn an_unknown_id_is_an_error_not_the_default() {
        let error = resolve(Some("dropped-model"), Path::new("/m")).unwrap_err();
        assert!(error.to_string().contains("unknown model \"dropped-model\""), "{error}");
    }
}
