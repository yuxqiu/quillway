//! Unix-socket IPC: the CLI sends one JSON line, the daemon answers with one.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::Stream;
use quillway_core::ipc::{Request, Response};
use quillway_core::paths;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

/// Largest request the daemon reads: 1 MiB of `--stdin` text, even if JSON
/// escaping grows it several times over.
const MAX_REQUEST: u64 = 8 << 20;
/// How long the CLI waits for requests the daemon answers at once.
const QUICK_REPLY: Duration = Duration::from_secs(5);
/// Model startup can take up to 120 s, plus context probing and scheduling.
const START_REPLY: Duration = Duration::from_secs(150);
/// A client has this long to send its request; the CLI sends it at once.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// No daemon listens on the socket (it is missing, or nothing accepts on it).
#[derive(Debug)]
pub struct DaemonNotRunning(std::path::PathBuf);

impl std::fmt::Display for DaemonNotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the daemon isn't running ({}).\nStart it with `systemctl --user start quillway` or `quillway daemon`; \
             if it won't start, see `quillway doctor` or `journalctl --user -u quillway`",
            self.0.display()
        )
    }
}

impl std::error::Error for DaemonNotRunning {}

pub async fn send(req: &Request) -> anyhow::Result<Response> {
    let path = paths::socket();
    send_with_deadline(&path, req, reply_timeout(req)).await
}

const fn reply_timeout(req: &Request) -> Duration {
    match req {
        Request::Reload | Request::Connect => START_REPLY,
        _ => QUICK_REPLY,
    }
}

async fn send_with_deadline(path: &Path, req: &Request, limit: Duration) -> anyhow::Result<Response> {
    tokio::time::timeout(limit, send_at(path, req))
        .await
        .map_err(|_| anyhow::anyhow!("the daemon didn't answer within {limit:?}"))?
}

async fn send_at(path: &Path, req: &Request) -> anyhow::Result<Response> {
    check_socket(path, rustix::process::getuid().as_raw())?;
    let stream = match UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
            return Err(DaemonNotRunning(path.to_path_buf()).into());
        }
        Err(e) => return Err(e).with_context(|| format!("connecting to the daemon at {}", path.display())),
    };
    let (r, mut w) = stream.into_split();
    write_line(&mut w, serde_json::to_string(req)?).await?;
    let mut resp = String::new();
    BufReader::new(r).read_line(&mut resp).await?;
    if resp.is_empty() {
        bail!("the daemon closed the connection without answering");
    }
    serde_json::from_str(&resp).context("the daemon's answer isn't understood (an outdated daemon?)")
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
    check_socket(path, rustix::process::getuid().as_raw())?;
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

/// The socket path, if something is there, must be our own socket. Another
/// user's could read our text: without `XDG_RUNTIME_DIR` it lives in the shared
/// temp directory. Any other file isn't a daemon, and binding mustn't delete it.
fn check_socket(path: &Path, uid: u32) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.uid() != uid => bail!(
            "{} belongs to another user; set XDG_RUNTIME_DIR to a private directory (normally /run/user/{uid})",
            path.display()
        ),
        Ok(meta) if !meta.file_type().is_socket() => {
            bail!("{} exists and is not a socket; remove it", path.display())
        }
        Ok(_) => Ok(()),
        // Missing: connecting or binding says what's wrong.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("checking {}", path.display())),
    }
}

/// Accepted requests, each with a way to reply.
/// Must be called inside a tokio runtime.
pub fn serve(listener: std::os::unix::net::UnixListener) -> impl Stream<Item = (Request, Reply)> {
    let (tx, rx) = mpsc::channel(16);
    let listener = UnixListener::from_std(listener).expect("tokio runtime");
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    // E.g. out of file descriptors: back off instead of spinning.
                    eprintln!("quillway: ipc: accepting a connection: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, tx, READ_TIMEOUT).await {
                    eprintln!("quillway: ipc: {e:#}");
                }
            });
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) })
}

async fn handle(stream: UnixStream, tx: mpsc::Sender<(Request, Reply)>, read_timeout: Duration) -> anyhow::Result<()> {
    let (r, mut w) = stream.into_split();
    let resp = match read_request(r, read_timeout).await {
        Ok(req) => {
            let (otx, orx) = oneshot::channel();
            tx.send((req, Reply::new(otx))).await?;
            orx.await.unwrap_or_else(|_| Response::Error { message: "daemon dropped the request".into() })
        }
        Err(message) => Response::Error { message },
    };
    write_line(&mut w, serde_json::to_string(&resp)?).await
}

/// One request line, bounded in size and time. Errors are the reply's message;
/// only an unparseable request starts with "bad request", which `rewrite` reads
/// as an outdated daemon.
async fn read_request(r: impl AsyncRead + Unpin, read_timeout: Duration) -> Result<Request, String> {
    let mut line = String::new();
    let mut reader = BufReader::new(r.take(MAX_REQUEST));
    match tokio::time::timeout(read_timeout, reader.read_line(&mut line)).await {
        Err(_) => return Err("timed out waiting for the request".into()),
        Ok(Err(e)) => return Err(format!("unreadable request: {e}")),
        Ok(Ok(_)) => {}
    }
    if !line.ends_with('\n') && line.len() as u64 == MAX_REQUEST {
        return Err(format!("the request is larger than {} MiB", MAX_REQUEST >> 20));
    }
    serde_json::from_str(&line).map_err(|e| format!("bad request: {e}"))
}

/// One JSON value per line.
async fn write_line(w: &mut (impl AsyncWriteExt + Unpin), mut line: String) -> anyhow::Result<()> {
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
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

    #[test]
    fn binding_does_not_delete_a_non_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quillway.sock");
        std::fs::write(&path, b"keep this").unwrap();
        assert!(bind_at(&path).unwrap_err().to_string().contains("not a socket"));
        assert_eq!(std::fs::read(&path).unwrap(), b"keep this");
    }

    #[test]
    fn a_socket_owned_by_another_user_or_a_plain_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quillway.sock");
        let me = rustix::process::getuid().as_raw();
        assert!(check_socket(&path, me).is_ok(), "missing is left to connect/bind");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(check_socket(&path, me).is_ok());
        let err = check_socket(&path, me + 1).unwrap_err().to_string();
        assert!(err.contains("belongs to another user"), "{err}");
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let err = check_socket(&file, me).unwrap_err().to_string();
        assert!(err.contains("is not a socket"), "{err}");
    }

    #[test]
    fn model_start_requests_have_a_long_but_finite_deadline() {
        assert_eq!(reply_timeout(&Request::Reload), START_REPLY);
        assert_eq!(reply_timeout(&Request::Connect), START_REPLY);
        assert_eq!(reply_timeout(&Request::Status), QUICK_REPLY);
    }

    #[tokio::test]
    async fn a_daemon_that_stays_connected_without_reply_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quillway.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let daemon = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            std::future::pending::<()>().await;
        });
        let error = send_with_deadline(&path, &Request::Reload, Duration::from_millis(50)).await.unwrap_err();
        assert!(error.to_string().contains("didn't answer within 50ms"), "{error}");
        daemon.abort();
    }

    #[tokio::test]
    async fn a_daemon_exit_while_loading_returns_an_error_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quillway.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let daemon = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut request = String::new();
            BufReader::new(stream).read_line(&mut request).await.unwrap();
            // The daemon exits before its model finishes loading.
        });
        let error =
            tokio::time::timeout(Duration::from_secs(1), send_with_deadline(&path, &Request::Reload, START_REPLY))
                .await
                .unwrap()
                .unwrap_err();
        assert!(error.to_string().contains("closed the connection"), "{error}");
        daemon.await.unwrap();
    }

    /// Run `handle` on one end of a socket pair; the other end is the client.
    fn spawn_handler(read_timeout: Duration) -> (UnixStream, mpsc::Receiver<(Request, Reply)>) {
        let (client, server) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(handle(server, tx, read_timeout));
        (client, rx)
    }

    async fn response(client: UnixStream) -> Response {
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn error(r: Response) -> String {
        let Response::Error { message } = r else { panic!("expected an error, got {r:?}") };
        message
    }

    #[tokio::test]
    async fn an_oversized_request_is_refused_without_buffering_it() {
        let (client, _rx) = spawn_handler(READ_TIMEOUT);
        let (r, mut w) = client.into_split();
        // More than the cap and no newline; the daemon stops reading at the cap.
        let writer = tokio::spawn(async move {
            let chunk = vec![b'x'; 1 << 20];
            for _ in 0..(MAX_REQUEST >> 20) + 2 {
                if w.write_all(&chunk).await.is_err() {
                    break; // the daemon answered and closed
                }
            }
        });
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await.unwrap();
        let message = error(serde_json::from_str(&line).unwrap());
        assert!(message.contains("larger than 8 MiB"), "{message}");
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn an_idle_client_is_answered_and_dropped() {
        let (client, _rx) = spawn_handler(Duration::from_millis(50));
        assert!(error(response(client).await).contains("timed out"));
    }

    #[tokio::test]
    async fn invalid_utf8_gets_an_answer() {
        let (mut client, _rx) = spawn_handler(READ_TIMEOUT);
        client.write_all(b"\xff\xfe\n").await.unwrap();
        let message = error(response(client).await);
        assert!(message.starts_with("unreadable request"), "{message}");
    }

    #[tokio::test]
    async fn malformed_json_reads_as_a_bad_request() {
        let (mut client, _rx) = spawn_handler(READ_TIMEOUT);
        client.write_all(b"{\"cmd\":\"from-the-future\"}\n").await.unwrap();
        assert!(error(response(client).await).starts_with("bad request"));
    }
}
