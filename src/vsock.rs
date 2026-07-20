//! One-shot vsock secrets delivery — the host side of the
//! first-read-then-delete hand-off (`docs/features/vsock-secrets/spec.md`).
//!
//! Mechanism only: bind the UDS where Firecracker routes guest-initiated
//! vsock connections, serve an opaque blob to the first connection, require
//! the guest's ack, then unlink and drop. Which secrets exist and when the
//! workload may proceed are policy questions owned by the layers above.

use std::path::{Path, PathBuf};

use tokio::sync::oneshot;

/// Guest-side vsock port the fetch hook connects to. Firecracker routes a
/// guest connection to `(cid=2, port)` onto the host UDS at
/// `<uds_path>_<port>`, so this constant also names the listener suffix.
pub const SECRETS_PORT: u32 = 5000;

/// The guest's own CID (must be ≥ 3). With the hybrid UDS model there is no
/// host-wide CID namespace to collide in — isolation comes from the per-jail
/// socket path — so every room uses the same value.
pub const GUEST_CID: u32 = 3;

/// The vsock UDS name inside the jail root, as Firecracker (chrooted) sees it.
pub const UDS_NAME: &str = "v.sock";

/// Host path of the one-shot listener for a room's jail root:
/// `<jail_root>/v.sock_<SECRETS_PORT>`.
#[must_use]
pub fn listener_path(jail_root: &Path) -> PathBuf {
    jail_root.join(format!("{UDS_NAME}_{SECRETS_PORT}"))
}

/// The encoded secrets blob: `NAME=value\n` per secret, nothing else.
///
/// Opaque to this module's callers below the policy layer. Drop attempts to
/// overwrite the bytes (NFR2) — an ordinary write the compiler may elide,
/// not a zeroization guarantee against swap, allocator copies, or
/// optimization; a dedicated zeroize crate is the upgrade path if the
/// threat model ever hardens.
pub struct SecretsPayload(Vec<u8>);

impl SecretsPayload {
    /// Encode `NAME=value` pairs into the wire blob. Validation (non-empty
    /// values, no embedded newlines, name charset) is the caller's admission
    /// policy; this is only the framing.
    #[must_use]
    pub fn encode(pairs: &[(String, String)]) -> Self {
        let mut bytes = Vec::new();
        for (name, value) in pairs {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(b'\n');
        }
        Self(bytes)
    }

    /// A copy of the blob for the serving task. Both copies attempt the
    /// same overwrite-on-drop.
    #[must_use]
    pub fn clone_bytes(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Drop for SecretsPayload {
    fn drop(&mut self) {
        for b in &mut self.0 {
            *b = 0;
        }
    }
}

/// A pending one-shot delivery. The caller awaits the guest's ack through
/// [`Delivery::await_delivered`]; dropping the handle aborts the serving task
/// and best-effort removes the listener socket.
#[derive(Debug)]
pub struct Delivery {
    rx: oneshot::Receiver<Result<(), String>>,
    task: tokio::task::JoinHandle<()>,
    listen_path: PathBuf,
}

impl Delivery {
    /// Wait for the guest's staged-and-acked confirmation, bounded by
    /// `timeout`. `Ok(())` means the guest read the full blob, staged it, and
    /// acked — the delivery signal the workload gate keys on. Any other
    /// outcome (timeout, transport error, malformed ack) is a terminal
    /// delivery failure; the endpoint is gone either way.
    pub async fn await_delivered(mut self, timeout: std::time::Duration) -> Result<(), String> {
        let waited = tokio::time::timeout(timeout, &mut self.rx).await;
        match waited {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("delivery task ended without a result".to_owned()),
            Err(_) => Err(format!(
                "no guest ack within {}s (image predates vsock secrets, or the guest fetch hook failed)",
                timeout.as_secs()
            )),
        }
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.listen_path);
    }
}

/// Bind `listen_path` and serve `payload` to the first connection ever made.
///
/// The listener is closed and unlinked the moment that connection is
/// accepted, so a second connect finds nothing to talk to. `owner` chowns
/// the socket file so the (jailed, de-privileged) Firecracker process may
/// connect to it. Must be called before `InstanceStart` — the guest can
/// never race a listener that outbinds it. Requires a running tokio runtime
/// (the serving task is spawned onto it).
#[cfg(unix)]
pub fn serve_one_shot(
    listen_path: &Path,
    payload: SecretsPayload,
    owner: Option<(u32, u32)>,
) -> std::io::Result<Delivery> {
    // A stale socket from a reused jail dir would shadow this run's listener.
    let _ = std::fs::remove_file(listen_path);
    let listener = tokio::net::UnixListener::bind(listen_path)?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(listen_path, Some(uid), Some(gid))?;
    }
    let (tx, rx) = oneshot::channel();
    let path = listen_path.to_path_buf();
    let task = tokio::spawn(serve(listener, path.clone(), payload, tx));
    Ok(Delivery {
        rx,
        task,
        listen_path: path,
    })
}

#[cfg(not(unix))]
pub fn serve_one_shot(
    _listen_path: &Path,
    _payload: SecretsPayload,
    _owner: Option<(u32, u32)>,
) -> std::io::Result<Delivery> {
    Err(std::io::Error::other(
        "vsock secrets delivery requires a unix host",
    ))
}

/// The serving task: accept once, immediately retire the endpoint, then
/// write the length-prefixed blob and wait for the guest's `OK` ack.
#[cfg(unix)]
async fn serve(
    listener: tokio::net::UnixListener,
    listen_path: PathBuf,
    payload: SecretsPayload,
    tx: oneshot::Sender<Result<(), String>>,
) {
    use tracing::{debug, warn};

    let outcome = async {
        let (stream, _addr) = listener
            .accept()
            .await
            .map_err(|e| format!("accept: {e}"))?;
        // First-read-then-delete: no second connection may even be attempted —
        // close the listener and unlink the path before serving the first.
        drop(listener);
        if let Err(e) = std::fs::remove_file(&listen_path) {
            warn!(path = %listen_path.display(), error = %e, "failed to unlink secrets listener");
        }
        serve_stream(stream, &payload).await
    }
    .await;
    drop(payload);
    debug!(ok = outcome.is_ok(), "secrets delivery finished");
    let _ = tx.send(outcome);
}

/// Write the length-prefixed blob and require the `OK` ack the guest sends
/// only after the file is durably staged. The write alone proves nothing —
/// it can sit in a socket buffer of a guest that never staged anything.
///
/// Framing is `<decimal len>\n<blob>`, no half-close: Firecracker's hybrid
/// vsock does not propagate a host `shutdown(WR)` as a guest-side EOF with
/// the reverse path intact — the ack never comes back (observed on a real
/// boot). The explicit length lets the guest know where the blob ends while
/// the connection stays fully open for the ack.
#[cfg(unix)]
async fn serve_stream(
    mut stream: tokio::net::UnixStream,
    payload: &SecretsPayload,
) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let header = format!("{}\n", payload.0.len());
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|e| format!("write header: {e}"))?;
    stream
        .write_all(&payload.0)
        .await
        .map_err(|e| format!("write blob: {e}"))?;
    // Read the ack a byte at a time up to the terminating newline — the guest
    // keeps the connection open (no half-close survives the hybrid vsock), so
    // a fixed-size `read_to_end` would block for a fourth byte that never
    // comes. Cap the scan so a chatty/garbage peer can't stream forever.
    let mut ack = Vec::with_capacity(4);
    loop {
        let b = stream
            .read_u8()
            .await
            .map_err(|e| format!("read ack: {e}"))?;
        if b == b'\n' {
            break;
        }
        ack.push(b);
        if ack.len() > 8 {
            break;
        }
    }
    // Exactly `OK` — the gate's invariant rides on this signal, so a prefix
    // match that admits `OKxx` would mask protocol bugs and partial acks.
    if ack == b"OK" {
        return Ok(());
    }
    Err(format!(
        "guest ack malformed: {:?}",
        String::from_utf8_lossy(&ack)
    ))
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module: panicky lints are noise in tests"
    )]

    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{listener_path, serve_one_shot, SecretsPayload, SECRETS_PORT};

    fn payload() -> SecretsPayload {
        SecretsPayload::encode(&[
            ("CURSOR_API_KEY".to_owned(), "k-123".to_owned()),
            ("GH_TOKEN".to_owned(), "t-456".to_owned()),
        ])
    }

    /// Read the guest's side of the frame: `<decimal len>\n`, then exactly
    /// `len` blob bytes. Mirrors what the in-guest stage script does — no
    /// EOF is involved, the connection stays fully open.
    async fn read_framed_blob(guest: &mut tokio::net::UnixStream) -> String {
        let mut header = Vec::new();
        loop {
            let b = guest.read_u8().await.unwrap();
            if b == b'\n' {
                break;
            }
            header.push(b);
        }
        let len: usize = String::from_utf8(header).unwrap().parse().unwrap();
        let mut blob = vec![0u8; len];
        guest.read_exact(&mut blob).await.unwrap();
        String::from_utf8(blob).unwrap()
    }

    #[test]
    fn encode_frames_name_value_lines() {
        let blob = payload();
        assert_eq!(blob.0, b"CURSOR_API_KEY=k-123\nGH_TOKEN=t-456\n".to_vec());
    }

    #[test]
    fn listener_path_carries_the_port_suffix() {
        let path = listener_path(std::path::Path::new("/jail/root"));
        assert_eq!(
            path,
            std::path::PathBuf::from(format!("/jail/root/v.sock_{SECRETS_PORT}"))
        );
    }

    #[tokio::test]
    async fn delivers_to_first_connection_and_acks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sock_5000");
        let delivery = serve_one_shot(&path, payload(), None).unwrap();

        let mut guest = tokio::net::UnixStream::connect(&path).await.unwrap();
        let blob = read_framed_blob(&mut guest).await;
        assert!(blob.contains("CURSOR_API_KEY=k-123"));
        guest.write_all(b"OK\n").await.unwrap();
        drop(guest);

        delivery
            .await_delivered(Duration::from_secs(5))
            .await
            .expect("acked delivery succeeds");
        assert!(!path.exists(), "listener must be unlinked after delivery");
    }

    #[tokio::test]
    async fn endpoint_is_gone_after_the_first_accept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sock_5000");
        let delivery = serve_one_shot(&path, payload(), None).unwrap();

        let mut first = tokio::net::UnixStream::connect(&path).await.unwrap();
        let _blob = read_framed_blob(&mut first).await;
        // The listener retired on accept: a second connect must fail even
        // before the first connection acks.
        let second = tokio::net::UnixStream::connect(&path).await;
        assert!(second.is_err(), "second connection must be refused");
        first.write_all(b"OK\n").await.unwrap();
        delivery
            .await_delivered(Duration::from_secs(5))
            .await
            .expect("first connection still completes");
    }

    #[tokio::test]
    async fn no_ack_times_out_as_a_delivery_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sock_5000");
        let delivery = serve_one_shot(&path, payload(), None).unwrap();

        // A guest that connects, reads, and vanishes without acking.
        let mut guest = tokio::net::UnixStream::connect(&path).await.unwrap();
        let _blob = read_framed_blob(&mut guest).await;
        // Hold the connection open, silent: the gate must not read a socket
        // write as delivery.
        let err = delivery
            .await_delivered(Duration::from_millis(300))
            .await
            .expect_err("silent guest must not count as delivered");
        assert!(err.contains("no guest ack"), "got: {err}");
        drop(guest);
    }

    #[tokio::test]
    async fn malformed_ack_is_a_delivery_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sock_5000");
        let delivery = serve_one_shot(&path, payload(), None).unwrap();

        let mut guest = tokio::net::UnixStream::connect(&path).await.unwrap();
        let _blob = read_framed_blob(&mut guest).await;
        guest.write_all(b"NO\n").await.unwrap();
        drop(guest);

        let err = delivery
            .await_delivered(Duration::from_secs(5))
            .await
            .expect_err("malformed ack must fail the delivery");
        assert!(err.contains("malformed"), "got: {err}");
    }
}
