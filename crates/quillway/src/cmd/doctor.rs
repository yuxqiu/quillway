//! `quillway doctor`: is everything Quillway needs present?

use quillway_core::config::Config;
use quillway_core::ipc::{Request, Response};
use quillway_core::paths;
use quillway_engine::models;

pub async fn run() -> anyhow::Result<()> {
    let mut ok = true;
    let mut check = |good: bool, what: &str, detail: String| {
        ok &= good;
        println!("{} {what:<16} {detail}", if good { "✓" } else { "✗" });
    };

    let cfg_path = paths::config_file();
    let config = match Config::load(&cfg_path) {
        Ok(c) => {
            check(
                true,
                "config",
                format!("{}{}", cfg_path.display(), if cfg_path.exists() { "" } else { " (defaults)" }),
            );
            c
        }
        Err(e) => {
            check(false, "config", format!("{e:#}"));
            Config::default()
        }
    };

    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    check(
        !wayland.is_empty(),
        "wayland",
        if wayland.is_empty() { "WAYLAND_DISPLAY is not set".into() } else { wayland },
    );

    match quillway_wl::read() {
        Ok(t) => check(true, "clipboard", format!("readable ({} chars)", t.map_or(0, |t| t.chars().count()))),
        Err(e) => check(false, "clipboard", format!("{e:#}")),
    }
    match quillway_wl::ClipboardWatch::start() {
        Ok(_) => check(true, "clipboard watch", "copy times are tracked".into()),
        Err(e) => check(false, "clipboard watch", format!("{e:#} (the clipboard is always treated as recent)")),
    }

    if let Some(e) = &config.model.endpoint {
        check(true, "endpoint", format!("{e} (built-in llama-server disabled)"));
    } else {
        let bin = config.model.llama_server_bin();
        match std::process::Command::new(bin).arg("--version").output() {
            Ok(o) => {
                let all = String::from_utf8_lossy(&o.stderr).into_owned() + &String::from_utf8_lossy(&o.stdout);
                if o.status.success() {
                    let v = all.lines().find(|l| l.starts_with("version")).unwrap_or("found");
                    check(true, "llama-server", v.to_owned());
                } else {
                    // E.g. missing shared libraries (exit 127): it is there but can't run.
                    let why = all.lines().find(|l| !l.trim().is_empty()).unwrap_or("no output");
                    check(false, "llama-server", format!("{bin} --version failed ({}): {why}", o.status));
                }
            }
            Err(e) => check(false, "llama-server", format!("{bin}: {e}")),
        }
        match models::active(&config).and_then(|a| a.ensure_installed().map(|()| a)) {
            Ok(active) => check(true, "model", format!("{} ({})", active.name, active.path.display())),
            Err(e) => check(false, "model", format!("{e:#}")),
        }
    }

    match crate::ipc::send(&Request::Status).await {
        // Running isn't enough: its model server must be up or coming up.
        Ok(Response::Status { engine, .. }) => {
            check(matches!(engine.as_str(), "ready" | "starting"), "daemon", format!("running, {engine}"));
        }
        Ok(other) => check(false, "daemon", format!("{other:?}")),
        Err(_) => check(false, "daemon", format!("not running ({})", paths::socket().display())),
    }

    if !ok {
        std::process::exit(1);
    }
    Ok(())
}
