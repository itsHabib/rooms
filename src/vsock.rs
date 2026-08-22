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

/// Dedicated base-provisioning endpoint. It is intentionally distinct from
/// the first-read-then-delete secrets endpoint.
pub const PROVISION_PORT: u32 = 5001;

/// One-shot terminal beacon endpoint. The provisioning connection must be
/// closed before the guest connects here.
pub const QUIESCED_PORT: u32 = 5002;

/// Post-restore hygiene-nudge endpoint.
///
/// The in-guest resume agent polls this port with bounded connect retries (no
/// connection is held across a snapshot); the host binds the listener only in
/// a restored room's jail, so in an ordinary base the poll fails fast.
pub const RESUME_PORT: u32 = 5003;

#[cfg(unix)]
const RESUME_PREFACE: &str = "ROOMS-RESUME/1";
#[cfg(unix)]
const RESUME_PREFACE_MAX_CANDIDATES: usize = 8;
#[cfg(unix)]
const RESUME_PREFACE_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

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
    listener_path_for(jail_root, SECRETS_PORT)
}

/// Host path for one guest vsock port in a room's jail.
#[must_use]
pub fn listener_path_for(jail_root: &Path, port: u32) -> PathBuf {
    jail_root.join(format!("{UDS_NAME}_{port}"))
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
        self.0.fill(0);
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

/// Credential-free input captured in a neutral base.
#[derive(Clone)]
#[cfg_attr(
    not(unix),
    allow(
        dead_code,
        reason = "payload is consumed by the unix-only vsock serving task"
    )
)]
pub struct ProvisioningPayload {
    bundle: Vec<u8>,
    warm: Vec<u8>,
}

impl ProvisioningPayload {
    /// Build the typed provisioning payload. Empty fields still receive phase
    /// acknowledgements so the host observes one deterministic state machine.
    #[must_use]
    pub fn new(bundle: Vec<u8>, warm: Option<&str>) -> Self {
        Self {
            bundle,
            warm: warm.unwrap_or_default().as_bytes().to_vec(),
        }
    }
}

/// Pending base provisioning and terminal quiesce proof.
#[derive(Debug)]
pub struct ProvisioningDelivery {
    rx: oneshot::Receiver<Result<(), String>>,
    task: tokio::task::JoinHandle<()>,
    paths: [PathBuf; 2],
}

impl ProvisioningDelivery {
    /// Wait for all phase ACKs, provisioning connection close, and the exact
    /// one-shot `quiesced` beacon.
    pub async fn await_quiesced(mut self, timeout: std::time::Duration) -> Result<(), String> {
        match tokio::time::timeout(timeout, &mut self.rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("provisioning task ended without a result".to_owned()),
            Err(_) => Err(format!("no quiesced beacon within {}s", timeout.as_secs())),
        }
    }
}

impl Drop for ProvisioningDelivery {
    fn drop(&mut self) {
        self.task.abort();
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Bind the dedicated provisioning and quiesced endpoints before boot.
#[cfg(unix)]
pub fn serve_provisioning(
    jail_root: &Path,
    payload: ProvisioningPayload,
    owner: Option<(u32, u32)>,
) -> std::io::Result<ProvisioningDelivery> {
    let provision_path = listener_path_for(jail_root, PROVISION_PORT);
    let quiesced_path = listener_path_for(jail_root, QUIESCED_PORT);
    let _ = std::fs::remove_file(&provision_path);
    let _ = std::fs::remove_file(&quiesced_path);
    let provision = tokio::net::UnixListener::bind(&provision_path)?;
    let quiesced = match tokio::net::UnixListener::bind(&quiesced_path) {
        Ok(listener) => listener,
        Err(e) => {
            let _ = std::fs::remove_file(&provision_path);
            return Err(e);
        }
    };
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(&provision_path, Some(uid), Some(gid))?;
        std::os::unix::fs::chown(&quiesced_path, Some(uid), Some(gid))?;
    }
    let (tx, rx) = oneshot::channel();
    let paths = [provision_path, quiesced_path];
    let task_paths = paths.clone();
    let task = tokio::spawn(async move {
        let result = serve_provisioning_inner(provision, quiesced, &task_paths, payload).await;
        let _ = tx.send(result);
    });
    Ok(ProvisioningDelivery { rx, task, paths })
}

#[cfg(not(unix))]
pub fn serve_provisioning(
    _jail_root: &Path,
    _payload: ProvisioningPayload,
    _owner: Option<(u32, u32)>,
) -> std::io::Result<ProvisioningDelivery> {
    Err(std::io::Error::other(
        "vsock base provisioning requires a unix host",
    ))
}

#[cfg(unix)]
async fn serve_provisioning_inner(
    provision_listener: tokio::net::UnixListener,
    quiesced_listener: tokio::net::UnixListener,
    paths: &[PathBuf; 2],
    payload: ProvisioningPayload,
) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let [provision_path, quiesced_path] = paths;
    let (mut provision, _) = provision_listener
        .accept()
        .await
        .map_err(|e| format!("accept provisioning agent: {e}"))?;
    drop(provision_listener);
    std::fs::remove_file(provision_path)
        .map_err(|e| format!("retire provisioning endpoint: {e}"))?;

    provision
        .write_all(b"ROOMS-PROVISION/1\n")
        .await
        .map_err(|e| format!("write provisioning preface: {e}"))?;
    write_typed_frame(&mut provision, "BUNDLE", &payload.bundle).await?;
    write_typed_frame(&mut provision, "WARM", &payload.warm).await?;
    write_typed_frame(&mut provision, "END", &[]).await?;
    for phase in ["stage", "clone", "warm"] {
        let ack = read_bounded_line(&mut provision).await?;
        if ack != format!("ACK {phase}") {
            return Err(format!("provisioning {phase} ack malformed: {ack:?}"));
        }
    }
    let mut trailing = [0_u8; 1];
    let read = provision
        .read(&mut trailing)
        .await
        .map_err(|e| format!("wait for provisioning connection close: {e}"))?;
    if read != 0 {
        return Err("provisioning connection carried trailing bytes".to_owned());
    }
    drop(provision);

    let (mut beacon, _) = quiesced_listener
        .accept()
        .await
        .map_err(|e| format!("accept quiesced beacon: {e}"))?;
    drop(quiesced_listener);
    std::fs::remove_file(quiesced_path).map_err(|e| format!("retire beacon endpoint: {e}"))?;
    let mut bytes = Vec::new();
    beacon
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("read quiesced beacon: {e}"))?;
    if bytes != b"quiesced\n" {
        return Err(format!(
            "quiesced beacon malformed: {:?}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    Ok(())
}

#[cfg(unix)]
async fn write_typed_frame(
    stream: &mut tokio::net::UnixStream,
    kind: &str,
    bytes: &[u8],
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let header = format!("{kind} {}\n", bytes.len());
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|e| format!("write {kind} header: {e}"))?;
    stream
        .write_all(bytes)
        .await
        .map_err(|e| format!("write {kind} body: {e}"))
}

#[cfg(unix)]
async fn read_bounded_line(stream: &mut tokio::net::UnixStream) -> Result<String, String> {
    use tokio::io::AsyncReadExt;

    let mut bytes = Vec::new();
    loop {
        let byte = stream
            .read_u8()
            .await
            .map_err(|e| format!("read protocol line: {e}"))?;
        if byte == b'\n' {
            break;
        }
        bytes.push(byte);
        if bytes.len() > 64 {
            return Err("protocol line exceeds 64 bytes".to_owned());
        }
    }
    String::from_utf8(bytes).map_err(|e| format!("protocol line is not utf-8: {e}"))
}

/// The per-restore hygiene nudge served to the resumed guest's agent.
///
/// Carries the new room/run identity, the host clock, fresh entropy, and the
/// admitted secrets (empty when none were requested — the guest still walks
/// one deterministic protocol either way).
pub struct ResumePayload {
    /// The restored room's id — the guest's new identity.
    pub room_id: String,
    /// Host wall-clock seconds since the epoch, to step the resumed guest's
    /// stale clock.
    pub epoch_secs: i64,
    /// Fresh host randomness the guest mixes into its CRNG so two restores of
    /// one snapshot diverge immediately.
    pub entropy: Vec<u8>,
    /// Encoded `NAME=value\n` secrets blob (empty = no secrets). Reuses the
    /// [`SecretsPayload`] wire shape; overwritten on drop with it.
    pub secrets: SecretsPayload,
}

/// Pending post-restore hygiene handshake.
///
/// The restore flow awaits the guest's terminal ack through
/// [`ResumeDelivery::await_acked`]; readiness (SSH probe, workload) is gated
/// on it. Dropping the handle aborts the serving task and the listener.
#[derive(Debug)]
pub struct ResumeDelivery {
    rx: oneshot::Receiver<Result<(), String>>,
    task: tokio::task::JoinHandle<()>,
    listen_path: PathBuf,
    room_id: String,
    progress: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl ResumeDelivery {
    /// Wait for the guest's `ACK resume`, bounded by `timeout`. `Ok(())` means
    /// every hygiene step (reseed, clock, identity, secrets staging, fresh
    /// sshd host key) succeeded in-guest — the only signal that may unblock
    /// readiness.
    pub async fn await_acked(mut self, timeout: std::time::Duration) -> Result<(), String> {
        match tokio::time::timeout(timeout, &mut self.rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("resume nudge task ended without a result".to_owned()),
            Err(_) => {
                let step = latest_resume_step(&self.progress);
                Err(format!(
                    "no resume ack for room {} within {}s after last successful STEP {step} (image predates the resume agent, or hygiene failed in-guest)",
                    self.room_id,
                    timeout.as_secs()
                ))
            }
        }
    }
}

fn latest_resume_step(progress: &std::sync::Mutex<Option<String>>) -> String {
    progress.lock().map_or_else(
        |_| "unknown".to_owned(),
        |step| step.as_deref().unwrap_or("none").to_owned(),
    )
}

impl Drop for ResumeDelivery {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.listen_path);
    }
}

/// Bind the resume-nudge endpoint in a restored room's jail root, before the
/// VM resumes — the polling guest agent can never race a listener that
/// outbinds it.
#[cfg(unix)]
pub fn serve_resume(
    jail_root: &Path,
    payload: ResumePayload,
    owner: Option<(u32, u32)>,
) -> std::io::Result<ResumeDelivery> {
    let room_id = payload.room_id.clone();
    let listen_path = listener_path_for(jail_root, RESUME_PORT);
    let _ = std::fs::remove_file(&listen_path);
    let listener = tokio::net::UnixListener::bind(&listen_path)?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(&listen_path, Some(uid), Some(gid))?;
    }
    let (tx, rx) = oneshot::channel();
    let path = listen_path.clone();
    let progress = std::sync::Arc::new(std::sync::Mutex::new(None));
    let task_progress = std::sync::Arc::clone(&progress);
    let task = tokio::spawn(async move {
        let result = serve_resume_inner(listener, &path, payload, &task_progress).await;
        let _ = tx.send(result);
    });
    Ok(ResumeDelivery {
        rx,
        task,
        listen_path,
        room_id,
        progress,
    })
}

#[cfg(not(unix))]
pub fn serve_resume(
    _jail_root: &Path,
    _payload: ResumePayload,
    _owner: Option<(u32, u32)>,
) -> std::io::Result<ResumeDelivery> {
    Err(std::io::Error::other(
        "vsock resume nudge requires a unix host",
    ))
}

#[cfg(unix)]
enum ResumePreface {
    RetiredPrefix { received: usize },
    Line(String),
}

#[cfg(unix)]
async fn read_resume_preface(stream: &mut tokio::net::UnixStream) -> Result<ResumePreface, String> {
    use tokio::io::AsyncReadExt;

    let mut bytes = Vec::new();
    loop {
        let byte = match stream.read_u8().await {
            Ok(byte) => byte,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                if RESUME_PREFACE.as_bytes().starts_with(&bytes) {
                    return Ok(ResumePreface::RetiredPrefix {
                        received: bytes.len(),
                    });
                }
                return Err(format!(
                    "resume preface diverged before EOF: {:?}",
                    String::from_utf8_lossy(&bytes)
                ));
            }
            Err(error) => return Err(format!("read resume preface: {error}")),
        };
        if byte == b'\n' {
            break;
        }
        bytes.push(byte);
        if bytes.len() > 64 {
            return Err("resume preface exceeds 64 bytes".to_owned());
        }
    }
    String::from_utf8(bytes)
        .map(ResumePreface::Line)
        .map_err(|error| format!("resume preface is not utf-8: {error}"))
}

#[cfg(unix)]
async fn accept_resume_agent(
    listener: &tokio::net::UnixListener,
    room_id: &str,
) -> Result<(tokio::net::UnixStream, std::time::Instant), String> {
    let mut deadline = None;
    for candidate in 1..=RESUME_PREFACE_MAX_CANDIDATES {
        let accepted = match deadline {
            None => listener.accept().await,
            Some(limit) => tokio::time::timeout_at(limit, listener.accept())
                .await
                .map_err(|_| {
                    format!(
                        "resume protocol for room {room_id}: preface retry deadline expired after {} candidate(s)",
                        candidate - 1
                    )
                })?,
        };
        let (mut stream, _) = accepted.map_err(|error| {
            format!("resume protocol for room {room_id}: accept agent: {error}")
        })?;
        let connected_at = std::time::Instant::now();
        let limit = *deadline
            .get_or_insert_with(|| tokio::time::Instant::now() + RESUME_PREFACE_RETRY_WINDOW);
        tracing::debug!(room = %room_id, candidate, "resume: agent candidate connected");
        let preface = tokio::time::timeout_at(limit, read_resume_preface(&mut stream))
            .await
            .map_err(|_| {
                format!(
                    "resume protocol for room {room_id}: candidate {candidate} sent no preface before the retry deadline"
                )
            })??;
        match preface {
            ResumePreface::Line(line) if line == RESUME_PREFACE => {
                tracing::debug!(room = %room_id, candidate,
                    elapsed_ms = %connected_at.elapsed().as_millis(),
                    "resume: candidate preface accepted");
                return Ok((stream, connected_at));
            }
            ResumePreface::Line(line) => {
                return Err(format!(
                    "resume protocol for room {room_id}: candidate {candidate} preface malformed: {line:?}"
                ));
            }
            ResumePreface::RetiredPrefix { received } => {
                tracing::warn!(room = %room_id, candidate,
                    elapsed_ms = %connected_at.elapsed().as_millis(),
                    received_bytes = received,
                    "resume: agent candidate retired during preface");
            }
        }
    }
    Err(format!(
        "resume protocol for room {room_id}: {RESUME_PREFACE_MAX_CANDIDATES} agent candidates retired before a complete preface"
    ))
}

/// Serve the nudge to the first agent candidate that sends the exact preface:
/// entropy first, then identity / clock / secrets frames and the terminal ack.
/// Entropy is deliberately first so the retained receiver can force a kernel
/// CRNG reseed before it parses any other post-resume field or forks.
/// Empty or exact-prefix candidates are retried without sending data; after
/// the complete preface, the endpoint retires before entropy or secrets leave.
#[cfg(unix)]
async fn serve_resume_inner(
    listener: tokio::net::UnixListener,
    listen_path: &Path,
    payload: ResumePayload,
    progress: &std::sync::Mutex<Option<String>>,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let room_id = payload.room_id.clone();
    let (mut stream, connected_at) = accept_resume_agent(&listener, &room_id).await?;
    drop(listener);
    std::fs::remove_file(listen_path)
        .map_err(|error| format!("resume protocol for room {room_id}: retire endpoint: {error}"))?;
    tracing::debug!(room = %room_id, elapsed_ms = %connected_at.elapsed().as_millis(),
        "resume: preface ok, sending nudge frames");
    write_typed_frame(&mut stream, "ENTROPY", &payload.entropy)
        .await
        .map_err(|error| format!("resume protocol for room {room_id}: {error}"))?;
    stream
        .write_all(format!("IDENTITY {}\n", payload.room_id).as_bytes())
        .await
        .map_err(|error| format!("resume protocol for room {room_id}: write identity: {error}"))?;
    stream
        .write_all(format!("CLOCK {}\n", payload.epoch_secs).as_bytes())
        .await
        .map_err(|error| format!("resume protocol for room {room_id}: write clock: {error}"))?;
    write_typed_frame(&mut stream, "SECRETS", &payload.secrets.0)
        .await
        .map_err(|error| format!("resume protocol for room {room_id}: {error}"))?;
    write_typed_frame(&mut stream, "END", &[])
        .await
        .map_err(|error| format!("resume protocol for room {room_id}: {error}"))?;
    tracing::debug!(room = %room_id, elapsed_ms = %connected_at.elapsed().as_millis(),
        "resume: frames sent, awaiting hygiene steps + ack");
    // The guest streams `STEP <name>` progress lines as it applies each
    // hygiene action, then the terminal `ACK resume`. Logging the steps gives
    // host-side visibility a snapshot-resumed guest's detached serial can't.
    // Bounded so a chatty/looping peer can't stream forever.
    let mut last_step: Option<String> = None;
    for _ in 0..64 {
        let line = match read_bounded_line(&mut stream).await {
            Ok(line) => line,
            Err(error) => {
                let step = last_step.as_deref().unwrap_or("none");
                tracing::warn!(room = %room_id, elapsed_ms = %connected_at.elapsed().as_millis(),
                    last_step = step, %error,
                    "resume: guest stream ended before ack");
                return Err(format!(
                    "resume protocol for room {room_id} ended after last successful STEP {step}: {error}"
                ));
            }
        };
        if line == "ACK resume" {
            let step = last_step.as_deref().unwrap_or("none");
            tracing::debug!(room = %room_id, elapsed_ms = %connected_at.elapsed().as_millis(),
                last_step = step, "resume: ack received");
            return Ok(());
        }
        if let Some(error) = line.strip_prefix("ERR ").filter(|value| !value.is_empty()) {
            let step = last_step.as_deref().unwrap_or("none");
            tracing::warn!(room = %room_id, elapsed_ms = %connected_at.elapsed().as_millis(),
                last_step = step, guest_error = error,
                "resume: guest hygiene failed");
            return Err(format!(
                "resume guest error for room {room_id} after last successful STEP {step}: {error}"
            ));
        }
        if let Some(step) = line.strip_prefix("STEP ") {
            if step.is_empty() {
                return Err(format!(
                    "resume protocol for room {room_id}: empty STEP after {:?}",
                    last_step.as_deref().unwrap_or("none")
                ));
            }
            tracing::debug!(room = %room_id, elapsed_ms = %connected_at.elapsed().as_millis(),
                step, "resume: guest hygiene step");
            if let Ok(mut latest) = progress.lock() {
                *latest = Some(step.to_owned());
            }
            last_step = Some(step.to_owned());
            continue;
        }
        return Err(format!(
            "resume protocol for room {room_id}: unexpected line {line:?} after last successful STEP {}",
            last_step.as_deref().unwrap_or("none")
        ));
    }
    Err(format!(
        "resume protocol for room {room_id}: too many lines without an ack after last successful STEP {}",
        last_step.as_deref().unwrap_or("none")
    ))
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

    use super::{
        listener_path, listener_path_for, serve_one_shot, serve_provisioning, serve_resume,
        ProvisioningPayload, ResumePayload, SecretsPayload, PROVISION_PORT, QUIESCED_PORT,
        RESUME_PORT, RESUME_PREFACE_MAX_CANDIDATES, SECRETS_PORT,
    };

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

    async fn read_line(guest: &mut tokio::net::UnixStream) -> String {
        let mut bytes = Vec::new();
        loop {
            let byte = guest.read_u8().await.unwrap();
            if byte == b'\n' {
                break;
            }
            bytes.push(byte);
        }
        String::from_utf8(bytes).unwrap()
    }

    async fn read_typed_frame(guest: &mut tokio::net::UnixStream) -> (String, Vec<u8>) {
        let header = read_line(guest).await;
        let (kind, len) = header.split_once(' ').unwrap();
        let mut bytes = vec![0_u8; len.parse().unwrap()];
        guest.read_exact(&mut bytes).await.unwrap();
        (kind.to_owned(), bytes)
    }

    #[tokio::test]
    async fn resume_nudge_streams_frames_then_gates_on_the_ack() {
        let dir = tempfile::tempdir().unwrap();
        let payload = ResumePayload {
            room_id: "01resumeroomidresumeroomid".to_owned(),
            epoch_secs: 1_700_000_000,
            entropy: vec![7_u8; 64],
            secrets: SecretsPayload::encode(&[("GH_TOKEN".to_owned(), "t-9".to_owned())]),
        };
        let delivery = serve_resume(dir.path(), payload, None).unwrap();

        let path = listener_path_for(dir.path(), RESUME_PORT);
        let mut guest = tokio::net::UnixStream::connect(path).await.unwrap();
        // The guest speaks first; entropy must be the first host frame.
        guest.write_all(b"ROOMS-RESUME/1\n").await.unwrap();
        assert_eq!(
            read_typed_frame(&mut guest).await,
            ("ENTROPY".to_owned(), vec![7_u8; 64])
        );
        assert_eq!(
            read_line(&mut guest).await,
            "IDENTITY 01resumeroomidresumeroomid"
        );
        assert_eq!(read_line(&mut guest).await, "CLOCK 1700000000");
        assert_eq!(
            read_typed_frame(&mut guest).await,
            ("SECRETS".to_owned(), b"GH_TOKEN=t-9\n".to_vec())
        );
        assert_eq!(
            read_typed_frame(&mut guest).await,
            ("END".to_owned(), Vec::new())
        );
        // Progress steps are logged and tolerated; only ACK resume completes it.
        guest
            .write_all(b"STEP reseeded\nSTEP sshd\nACK resume\n")
            .await
            .unwrap();
        drop(guest);

        delivery
            .await_acked(Duration::from_secs(5))
            .await
            .expect("a well-formed hygiene handshake acks");
    }

    #[tokio::test]
    async fn resume_nudge_retries_a_clean_preface_eof_without_reopening_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let payload = ResumePayload {
            room_id: "01resumeroomidresumeroomid".to_owned(),
            epoch_secs: 1_700_000_000,
            entropy: vec![7_u8; 64],
            secrets: SecretsPayload::encode(&[("TOKEN".to_owned(), "once".to_owned())]),
        };
        let delivery = serve_resume(dir.path(), payload, None).unwrap();
        let path = listener_path_for(dir.path(), RESUME_PORT);

        let empty = tokio::net::UnixStream::connect(&path).await.unwrap();
        drop(empty);
        tokio::task::yield_now().await;
        assert!(
            path.exists(),
            "empty candidate must not retire the endpoint"
        );

        let mut guest = tokio::net::UnixStream::connect(&path).await.unwrap();
        guest.write_all(b"ROOMS-RESUME/1\n").await.unwrap();
        assert_eq!(read_typed_frame(&mut guest).await.0, "ENTROPY");
        assert!(read_line(&mut guest).await.starts_with("IDENTITY "));
        assert!(read_line(&mut guest).await.starts_with("CLOCK "));
        assert_eq!(
            read_typed_frame(&mut guest).await,
            ("SECRETS".to_owned(), b"TOKEN=once\n".to_vec())
        );
        assert_eq!(read_typed_frame(&mut guest).await.0, "END");
        guest
            .write_all(b"STEP reseeded\nSTEP sshd\nACK resume\n")
            .await
            .unwrap();
        drop(guest);

        delivery
            .await_acked(Duration::from_secs(5))
            .await
            .expect("a valid candidate after clean EOF completes");
        assert!(!path.exists(), "valid preface must retire the endpoint");
    }

    #[tokio::test]
    async fn resume_nudge_retries_an_exact_partial_preface_without_sending_data() {
        let dir = tempfile::tempdir().unwrap();
        let payload = ResumePayload {
            room_id: "01resumeroomidresumeroomid".to_owned(),
            epoch_secs: 1_700_000_000,
            entropy: vec![7_u8; 64],
            secrets: SecretsPayload::encode(&[("TOKEN".to_owned(), "once".to_owned())]),
        };
        let delivery = serve_resume(dir.path(), payload, None).unwrap();
        let path = listener_path_for(dir.path(), RESUME_PORT);

        let mut partial = tokio::net::UnixStream::connect(&path).await.unwrap();
        partial.write_all(b"ROOMS-RES").await.unwrap();
        partial.shutdown().await.unwrap();
        let mut leaked = Vec::new();
        partial.read_to_end(&mut leaked).await.unwrap();
        assert!(leaked.is_empty(), "partial candidate received host data");

        let mut guest = tokio::net::UnixStream::connect(&path).await.unwrap();
        guest.write_all(b"ROOMS-RESUME/1\n").await.unwrap();
        assert_eq!(read_typed_frame(&mut guest).await.0, "ENTROPY");
        assert!(read_line(&mut guest).await.starts_with("IDENTITY "));
        assert!(read_line(&mut guest).await.starts_with("CLOCK "));
        assert_eq!(
            read_typed_frame(&mut guest).await,
            ("SECRETS".to_owned(), b"TOKEN=once\n".to_vec())
        );
        assert_eq!(read_typed_frame(&mut guest).await.0, "END");
        guest.write_all(b"ACK resume\n").await.unwrap();
        drop(guest);

        delivery
            .await_acked(Duration::from_secs(5))
            .await
            .expect("an exact-prefix EOF may retry once");
    }

    #[tokio::test]
    async fn resume_nudge_rejects_a_divergent_partial_preface_without_sending_data() {
        let dir = tempfile::tempdir().unwrap();
        let payload = ResumePayload {
            room_id: "01resumeroomidresumeroomid".to_owned(),
            epoch_secs: 1_700_000_000,
            entropy: vec![7_u8; 64],
            secrets: SecretsPayload::encode(&[("TOKEN".to_owned(), "never".to_owned())]),
        };
        let delivery = serve_resume(dir.path(), payload, None).unwrap();
        let path = listener_path_for(dir.path(), RESUME_PORT);

        let mut divergent = tokio::net::UnixStream::connect(&path).await.unwrap();
        divergent.write_all(b"ROOMS-X").await.unwrap();
        divergent.shutdown().await.unwrap();
        let mut leaked = Vec::new();
        divergent.read_to_end(&mut leaked).await.unwrap();
        assert!(leaked.is_empty(), "divergent candidate received host data");

        let error = delivery
            .await_acked(Duration::from_secs(5))
            .await
            .expect_err("a divergent preface must remain terminal");
        assert!(error.contains("preface diverged before EOF"), "{error}");
    }

    #[tokio::test]
    async fn resume_nudge_bounds_clean_preface_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let payload = ResumePayload {
            room_id: "01resumeroomidresumeroomid".to_owned(),
            epoch_secs: 1,
            entropy: vec![0_u8; 64],
            secrets: SecretsPayload::encode(&[]),
        };
        let delivery = serve_resume(dir.path(), payload, None).unwrap();
        let path = listener_path_for(dir.path(), RESUME_PORT);

        for _ in 0..RESUME_PREFACE_MAX_CANDIDATES {
            let empty = tokio::net::UnixStream::connect(&path).await.unwrap();
            drop(empty);
        }

        let error = delivery
            .await_acked(Duration::from_secs(5))
            .await
            .expect_err("empty candidates must exhaust the bounded handshake");
        assert!(
            error.contains("8 agent candidates retired before a complete preface"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn resume_nudge_surfaces_guest_error_with_room_and_last_step() {
        let dir = tempfile::tempdir().unwrap();
        let room_id = "01resumeroomidresumeroomid";
        let payload = ResumePayload {
            room_id: room_id.to_owned(),
            epoch_secs: 1_700_000_000,
            entropy: vec![7_u8; 64],
            secrets: SecretsPayload::encode(&[]),
        };
        let delivery = serve_resume(dir.path(), payload, None).unwrap();

        let path = listener_path_for(dir.path(), RESUME_PORT);
        let mut guest = tokio::net::UnixStream::connect(path).await.unwrap();
        guest.write_all(b"ROOMS-RESUME/1\n").await.unwrap();
        let _ = read_typed_frame(&mut guest).await;
        let _ = read_line(&mut guest).await;
        let _ = read_line(&mut guest).await;
        let _ = read_typed_frame(&mut guest).await;
        let _ = read_typed_frame(&mut guest).await;
        guest
            .write_all(b"STEP reseeded\nSTEP identity\nERR hostkeys key generation failed\n")
            .await
            .unwrap();
        drop(guest);

        let error = delivery
            .await_acked(Duration::from_secs(5))
            .await
            .expect_err("an explicit guest error must fail the handshake");
        assert!(error.contains(room_id), "{error}");
        assert!(error.contains("last successful STEP identity"), "{error}");
        assert!(error.contains("hostkeys key generation failed"), "{error}");
    }

    #[tokio::test]
    async fn resume_nudge_eof_surfaces_room_and_last_step() {
        let dir = tempfile::tempdir().unwrap();
        let room_id = "01resumeroomidresumeroomid";
        let payload = ResumePayload {
            room_id: room_id.to_owned(),
            epoch_secs: 1_700_000_000,
            entropy: vec![7_u8; 64],
            secrets: SecretsPayload::encode(&[]),
        };
        let delivery = serve_resume(dir.path(), payload, None).unwrap();

        let path = listener_path_for(dir.path(), RESUME_PORT);
        let mut guest = tokio::net::UnixStream::connect(path).await.unwrap();
        guest.write_all(b"ROOMS-RESUME/1\n").await.unwrap();
        let _ = read_typed_frame(&mut guest).await;
        let _ = read_line(&mut guest).await;
        let _ = read_line(&mut guest).await;
        let _ = read_typed_frame(&mut guest).await;
        let _ = read_typed_frame(&mut guest).await;
        guest.write_all(b"STEP reseeded\n").await.unwrap();
        drop(guest);

        let error = delivery
            .await_acked(Duration::from_secs(5))
            .await
            .expect_err("guest EOF before ack must fail the handshake");
        assert!(error.contains(room_id), "{error}");
        assert!(error.contains("last successful STEP reseeded"), "{error}");
        assert!(error.contains("unexpected end of file"), "{error}");
    }

    #[tokio::test]
    async fn resume_nudge_without_ack_is_a_delivery_failure() {
        let dir = tempfile::tempdir().unwrap();
        let room_id = "01resumeroomidresumeroomid";
        let payload = ResumePayload {
            room_id: room_id.to_owned(),
            epoch_secs: 1,
            entropy: vec![0_u8; 64],
            secrets: SecretsPayload::encode(&[]),
        };
        let delivery = serve_resume(dir.path(), payload, None).unwrap();
        let path = listener_path_for(dir.path(), RESUME_PORT);
        let mut guest = tokio::net::UnixStream::connect(path).await.unwrap();
        guest.write_all(b"ROOMS-RESUME/1\n").await.unwrap();
        let _ = read_typed_frame(&mut guest).await;
        let _ = read_line(&mut guest).await;
        let _ = read_line(&mut guest).await;
        let _ = read_typed_frame(&mut guest).await;
        let _ = read_typed_frame(&mut guest).await;
        // Report progress but never ack — hygiene stalled in-guest.
        guest
            .write_all(b"STEP reseeded\nSTEP hostkeys\n")
            .await
            .unwrap();
        let err = delivery
            .await_acked(Duration::from_millis(300))
            .await
            .expect_err("no ack must fail the delivery");
        assert!(err.contains("no resume ack"), "got: {err}");
        assert!(err.contains(room_id), "got: {err}");
        assert!(err.contains("last successful STEP hostkeys"), "got: {err}");
        drop(guest);
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

    #[tokio::test]
    async fn provisioning_requires_ordered_acks_closed_stream_and_exact_beacon() {
        let dir = tempfile::tempdir().unwrap();
        let payload = ProvisioningPayload::new(b"bundle".to_vec(), Some("echo warm"));
        let delivery = serve_provisioning(dir.path(), payload, None).unwrap();

        let provision_path = listener_path_for(dir.path(), PROVISION_PORT);
        let mut guest = tokio::net::UnixStream::connect(provision_path)
            .await
            .unwrap();
        assert_eq!(read_line(&mut guest).await, "ROOMS-PROVISION/1");
        assert_eq!(
            read_typed_frame(&mut guest).await,
            ("BUNDLE".to_owned(), b"bundle".to_vec())
        );
        guest.write_all(b"ACK stage\nACK clone\n").await.unwrap();
        assert_eq!(
            read_typed_frame(&mut guest).await,
            ("WARM".to_owned(), b"echo warm".to_vec())
        );
        assert_eq!(
            read_typed_frame(&mut guest).await,
            ("END".to_owned(), Vec::new())
        );
        guest.write_all(b"ACK warm\n").await.unwrap();
        drop(guest);

        let beacon_path = listener_path_for(dir.path(), QUIESCED_PORT);
        let mut beacon = tokio::net::UnixStream::connect(beacon_path).await.unwrap();
        beacon.write_all(b"quiesced\n").await.unwrap();
        drop(beacon);

        delivery
            .await_quiesced(Duration::from_secs(5))
            .await
            .expect("all phases and exact terminal beacon succeed");
    }

    #[tokio::test]
    async fn malformed_quiesced_beacon_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let payload = ProvisioningPayload::new(Vec::new(), None);
        let delivery = serve_provisioning(dir.path(), payload, None).unwrap();

        let provision_path = listener_path_for(dir.path(), PROVISION_PORT);
        let mut guest = tokio::net::UnixStream::connect(provision_path)
            .await
            .unwrap();
        assert_eq!(read_line(&mut guest).await, "ROOMS-PROVISION/1");
        assert_eq!(read_typed_frame(&mut guest).await.0, "BUNDLE");
        assert_eq!(read_typed_frame(&mut guest).await.0, "WARM");
        assert_eq!(read_typed_frame(&mut guest).await.0, "END");
        guest
            .write_all(b"ACK stage\nACK clone\nACK warm\n")
            .await
            .unwrap();
        drop(guest);

        let beacon_path = listener_path_for(dir.path(), QUIESCED_PORT);
        let mut beacon = tokio::net::UnixStream::connect(beacon_path).await.unwrap();
        beacon.write_all(b"quiesced-late\n").await.unwrap();
        drop(beacon);

        let err = delivery
            .await_quiesced(Duration::from_secs(5))
            .await
            .expect_err("only the exact terminal beacon seals");
        assert!(err.contains("malformed"), "{err}");
    }
}
