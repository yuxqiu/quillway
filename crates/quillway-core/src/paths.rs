//! XDG locations.

use std::env;
use std::path::PathBuf;

const APP: &str = "quillway";

fn xdg(var: &str, fallback: &str) -> PathBuf {
    env::var_os(var).filter(|v| !v.is_empty()).map_or_else(|| home().join(fallback), PathBuf::from).join(APP)
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// `$XDG_CONFIG_HOME/quillway/config.toml`
#[must_use]
pub fn config_file() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("config.toml")
}

/// `$XDG_DATA_HOME/quillway/models`
#[must_use]
pub fn models_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("models")
}

/// `$XDG_STATE_HOME/quillway/state.toml`: mutable choices such as `models use`.
#[must_use]
pub fn state_file() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("state.toml")
}

/// `$XDG_RUNTIME_DIR/quillway.sock`
pub fn socket() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR").map_or_else(env::temp_dir, PathBuf::from).join(format!("{APP}.sock"))
}
