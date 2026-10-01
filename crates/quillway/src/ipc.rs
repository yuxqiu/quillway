//! Unix-socket IPC: the CLI sends one JSON line, the daemon answers with one.

use std::sync::{Arc, Mutex};
use std::{os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, bail};
use futures_util::Stream;
use quillway_core::ipc::{Request, Response};
use quillway_core::paths;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

pub async fn send(req: &Request) -> anyhow::Result<Response> {
    let path = paths::socket();
    let stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "the daemon isn't running ({}).\nStart it with `systemctl --user start quillway` or `quillway daemon`",
            path.display()
        )
    })?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut resp = String::new();
    BufReader::new(r).read_line(&mut resp).await?;
    if resp.is_empty() {
        bail!("the daemon closed the connection without answering");
    }
    Ok(serde_json::from_str(&resp)?)
}

/// Lets the UI answer a request after handling it. `Clone` + `Debug` because
/// it travels inside iced messages.
#[derive(Clone)]
pub struct Reply(Arc<Mutex<Option<oneshot::Sender<Response>>>>);

impl Reply {
    pub(crate) fn new(tx: oneshot::Sender<Response>) -> Self {
        Self(Arc::new(Mutex::new(Some(tx))))
    }

    pub fn send(&self, r: Response) {
        let tx = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(tx) = tx {
            let _ = tx.send(r);
        }
    }
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reply")
    }
}

/// Bind the socket (before the UI starts, so a second daemon fails fast),
/// replacing a stale one but refusing to steal a live daemon's.
pub fn bind() -> anyhow::Result<std::os::unix::net::UnixListener> {
    let path = paths::socket();
    bind_at(&path)
}

fn bind_at(path: &Path) -> anyhow::Result<std::os::unix::net::UnixListener> {
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        bail!("another quillway daemon is already running ({})", path.display());
    }
    let _ = std::fs::remove_file(path);
    let l = std::os::unix::net::UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    // `XDG_RUNTIME_DIR` is normally private, but the fallback socket lives in /tmp.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

/// Accepted requests, each with a way to reply.
/// Must be called inside a tokio runtime.
pub fn serve(listener: std::os::unix::net::UnixListener) -> impl Stream<Item = (Request, Reply)> {
    let (tx, rx) = mpsc::channel(16);
    let listener = UnixListener::from_std(listener).expect("tokio runtime");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, tx).await {
                    eprintln!("quillway: ipc: {e:#}");
                }
            });
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) })
}

async fn handle(stream: UnixStream, tx: mpsc::Sender<(Request, Reply)>) -> anyhow::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    let resp = match serde_json::from_str::<Request>(&line) {
        Ok(req) => {
            let (otx, orx) = oneshot::channel();
            tx.send((req, Reply::new(otx))).await?;
            orx.await.unwrap_or_else(|_| Response::Error { message: "daemon dropped the request".into() })
        }
        Err(e) => Response::Error { message: format!("bad request: {e}") },
    };
    let mut out = serde_json::to_string(&resp)?;
    out.push('\n');
    w.write_all(out.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_is_private_and_a_second_daemon_cannot_take_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quillway.sock");
        let _listener = bind_at(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(bind_at(&path).unwrap_err().to_string().contains("already running"));
    }
}
