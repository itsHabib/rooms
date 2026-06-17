//! Structured error taxonomy for the rooms substrate.
//!
//! ```text
//! RoomsError
//! ├── Firecracker(FirecrackerError)
//! ├── Rootfs(RootfsError)
//! ├── Transport(TransportError)
//! └── Runner(RunnerError)
//! ```

use std::path::PathBuf;

use thiserror::Error;

/// Top-level error returned by the rooms substrate.
#[derive(Debug, Error)]
pub enum RoomsError {
    #[error(transparent)]
    Firecracker(#[from] FirecrackerError),
    #[error(transparent)]
    Rootfs(#[from] RootfsError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error("internal: {0}")]
    Internal(String),
}

/// Errors from Firecracker process control and guest lifecycle.
#[derive(Debug, Error)]
pub enum FirecrackerError {
    #[error("/dev/kvm missing or permission denied")]
    KvmUnavailable,
    #[error("firecracker binary not found at {path}")]
    BinaryNotFound { path: PathBuf },
    #[error("jailer binary not found at {path}")]
    JailerNotFound { path: PathBuf },
    #[error("system user {user} missing; run scripts/setup-rooms-host.sh")]
    FirecrackerUserMissing { user: String },
    #[error("jailer requires root; run rooms via sudo (e.g. `sudo -E rooms run ...`)")]
    RootRequired,
    #[error("jail staging failed: {reason}")]
    JailPrepareFailed { reason: String },
    #[error("firecracker api socket did not appear within {timeout_ms} ms")]
    ApiSocketNeverAppeared { timeout_ms: u64 },
    #[error("firecracker api PUT {endpoint} failed (curl exit {curl_exit_code}): {body}")]
    ApiCallFailed {
        endpoint: String,
        /// `curl(1)` process exit code, not the guest HTTP status. With
        /// `--fail-with-body`, any HTTP response >= 400 yields curl exit 22;
        /// the actual HTTP status lives in `body`.
        curl_exit_code: i32,
        body: String,
    },
    #[error("firecracker api PUT {endpoint} timed out after {timeout_ms} ms")]
    ApiCallTimedOut { endpoint: String, timeout_ms: u64 },
    #[error("guest unreachable: {reason}")]
    GuestUnreachable { reason: String },
    #[error("firecracker exited early with code {exit_code}: {stderr_tail}")]
    ProcessExitedEarly { exit_code: i32, stderr_tail: String },
    #[error("home env var unset")]
    HomeUnset,
    #[error("{0}")]
    Internal(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors from rootfs image validation and overlay management.
#[derive(Debug, Error)]
pub enum RootfsError {
    #[error("rootfs not found at {path}")]
    NotFound { path: PathBuf },
    #[error("rootfs at {path} is not ext4 (magic {magic:#x})")]
    NotExt4 { path: PathBuf, magic: u16 },
    #[error("rootfs at {path} is too small ({size} bytes; min {min_bytes})")]
    TooSmall {
        path: PathBuf,
        size: u64,
        min_bytes: u64,
    },
    #[error("kernel not found at {path}")]
    KernelNotFound { path: PathBuf },
    #[error("kernel at {path} is not a valid ELF")]
    KernelNotElf { path: PathBuf },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors from HTTP-over-Unix-socket transport to the Firecracker API.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("curl invocation failed: {0}")]
    CurlFailed(String),
    #[error("curl timed out on {endpoint} after {timeout_ms} ms")]
    TimedOut { endpoint: String, timeout_ms: u64 },
    #[error("failed to serialize request body: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors from guest command execution over SSH.
#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("guest unreachable: {reason}")]
    GuestUnreachable { reason: String },
    #[error("ssh probe failed: {0}")]
    SshProbe(String),
    #[error("exec failed: {0}")]
    Exec(String),
    #[error("key path not utf-8")]
    KeyPathNotUtf8,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<RunnerError> for FirecrackerError {
    /// Convert a guest-side runner failure into the substrate's error type.
    ///
    /// Only `GuestUnreachable` keeps its semantic shape — that's a genuine
    /// network/guest reachability signal. Everything else (probe, seed, exec,
    /// key-path, IO) is a substrate setup or local error that collapsed into
    /// `GuestUnreachable` previously; surface it as `Internal` instead so an
    /// operator reading the log can tell "the room is dead" apart from
    /// "rooms binary is broken on this host".
    fn from(err: RunnerError) -> Self {
        match err {
            RunnerError::GuestUnreachable { reason } => Self::GuestUnreachable { reason },
            other => Self::Internal(format!("runner: {other}")),
        }
    }
}

impl From<TransportError> for FirecrackerError {
    /// Convert a Firecracker-API transport failure into the substrate's
    /// error type. Same shape as `From<RunnerError>` — only the explicit
    /// `TimedOut` maps to `ApiCallTimedOut`; serialization / IO / process-spawn
    /// failures are local-side bugs and surface as `Internal`, not as
    /// `GuestUnreachable` (which would falsely implicate the guest VM).
    fn from(err: TransportError) -> Self {
        match err {
            TransportError::TimedOut {
                endpoint,
                timeout_ms,
            } => Self::ApiCallTimedOut {
                endpoint,
                timeout_ms,
            },
            other => Self::Internal(format!("transport: {other}")),
        }
    }
}
