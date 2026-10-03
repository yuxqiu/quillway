//! XDG locations.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

const APP: &str = "quillway";

/// An `XDG_*` base directory. The spec says to ignore relative paths, which
/// covers unset and empty values too.
fn xdg_base(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|p| p.is_absolute())
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    xdg_base(env::var_os(var)).unwrap_or_else(|| home().join(fallback)).join(APP)
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
    xdg_base(env::var_os("XDG_RUNTIME_DIR")).unwrap_or_else(env::temp_dir).join(format!("{APP}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_empty_xdg_values_are_ignored() {
        assert_eq!(xdg_base(Some("/run/user/1000".into())), Some(PathBuf::from("/run/user/1000")));
        for ignored in [None, Some(""), Some("relative/dir"), Some("~/.config")] {
            assert_eq!(xdg_base(ignored.map(OsString::from)), None, "{ignored:?}");
        }
    }
}
