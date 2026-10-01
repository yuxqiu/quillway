//! The curated model catalog, embedded at compile time.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::Deserialize;

/// One downloadable model.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Stable id used on the command line and in the config, e.g. `qwen3.5-4b`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `tiny`, `balanced` or `quality`.
    pub tier: String,
    /// The model used when nothing else is chosen (exactly one entry).
    #[serde(default)]
    pub default: bool,
    /// Hugging Face repository, `owner/name`.
    pub repo: String,
    /// Pinned commit of `repo`.
    pub revision: String,
    /// GGUF file within the repository.
    pub file: String,
    /// Exact size in bytes.
    pub size: u64,
    /// Expected sha256 of the file (the HF `lfs.oid`).
    pub sha256: String,
    /// License name, shown to the user.
    pub license: String,
    /// Where to read the license, for non-OSI ones.
    #[serde(default)]
    pub license_url: Option<String>,
    /// Non-OSI license: show it and ask before installing.
    #[serde(default)]
    pub license_notice: bool,
    /// Sampling defaults for this model; temperature comes from the preset.
    pub sampling: Sampling,
}

/// Per-model sampling parameters sent with each request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sampling {
    /// Nucleus sampling cutoff.
    pub top_p: f32,
    /// Sample among the `top_k` most likely tokens (llama-server only).
    pub top_k: u32,
    /// Drop tokens below `min_p` × the top probability (llama-server only).
    pub min_p: f32,
}

#[derive(Deserialize)]
struct File {
    model: Vec<Entry>,
}

static CATALOG: LazyLock<Vec<Entry>> = LazyLock::new(|| {
    toml::from_str::<File>(include_str!("../assets/catalog.toml")).expect("embedded catalog.toml is valid").model
});

/// Every catalog entry, in display order.
#[must_use]
pub fn all() -> &'static [Entry] {
    &CATALOG
}

/// The entry with this id.
#[must_use]
pub fn find(id: &str) -> Option<&'static Entry> {
    all().iter().find(|e| e.id == id)
}

/// The model used when nothing else is chosen.
///
/// # Panics
///
/// Never in practice: a unit test checks the embedded catalog has a default.
#[must_use]
pub fn default_entry() -> &'static Entry {
    all().iter().find(|e| e.default).expect("catalog has a default model")
}

impl Entry {
    /// Download URL for the pinned revision.
    #[must_use]
    pub fn url(&self) -> String {
        format!("https://huggingface.co/{}/resolve/{}/{}", self.repo, self.revision, self.file)
    }

    /// `<models_dir>/<owner>__<repo>/<revision>/<file>`
    #[must_use]
    pub fn path_in(&self, models_dir: &Path) -> PathBuf {
        models_dir.join(self.repo.replace('/', "__")).join(&self.revision).join(&self.file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_parses_and_is_consistent() {
        assert!(all().len() >= 4);
        assert_eq!(all().iter().filter(|e| e.default).count(), 1);
        assert_eq!(default_entry().id, "qwen3.5-4b");
        for e in all() {
            assert_eq!(e.revision.len(), 40, "{}", e.id);
            assert_eq!(e.sha256.len(), 64, "{}", e.id);
            assert!(Path::new(&e.file).extension().is_some_and(|x| x == "gguf"));
        }
        let mut ids: Vec<_> = all().iter().map(|e| &e.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), all().len());
    }

    #[test]
    fn lfm_requires_license_notice() {
        assert!(find("lfm2.5-1.2b").unwrap().license_notice);
        assert!(!find("qwen3.5-4b").unwrap().license_notice);
    }

    #[test]
    fn storage_path() {
        let e = find("qwen3.5-4b").unwrap();
        assert_eq!(
            e.path_in(Path::new("/m")),
            PathBuf::from(
                "/m/unsloth__Qwen3.5-4B-GGUF/e87f176479d0855a907a41277aca2f8ee7a09523/Qwen3.5-4B-Q4_K_M.gguf"
            )
        );
    }
}
