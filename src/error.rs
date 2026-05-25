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
    #[error("firecracker api socket did not appear within {timeout_ms} ms")]
    ApiSocketNeverAppeared { timeout_ms: u64 },
    #[error("firecracker api PUT {endpoint} failed (HTTP {status}): {body}")]
    ApiCallFailed {
        endpoint: String,
        status: u16,
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
    #[error("entropy seed failed: {0}")]
    EntropySeed(String),
    #[error("exec failed: {0}")]
    Exec(String),
    #[error("key path not utf-8")]
    KeyPathNotUtf8,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<RunnerError> for FirecrackerError {
    fn from(err: RunnerError) -> Self {
        match err {
            RunnerError::GuestUnreachable { reason } => Self::GuestUnreachable { reason },
            other => Self::GuestUnreachable {
                reason: other.to_string(),
            },
        }
    }
}

impl From<TransportError> for FirecrackerError {
    fn from(err: TransportError) -> Self {
        match err {
            TransportError::TimedOut {
                endpoint,
                timeout_ms,
            } => Self::ApiCallTimedOut {
                endpoint,
                timeout_ms,
            },
            other => Self::GuestUnreachable {
                reason: other.to_string(),
            },
        }
    }
}
