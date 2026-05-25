//! HTTP-over-Unix-socket transport to the Firecracker REST API.
//!
//! POC shells out to `curl --unix-socket` with per-call timeouts.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use tracing::debug;

use crate::config::RoomsConfig;
use crate::error::{FirecrackerError, TransportError};

/// Issue a PUT to the Firecracker API socket with a configurable timeout.
pub async fn api_put(
    socket: &Path,
    endpoint: &str,
    body: &serde_json::Value,
    config: &RoomsConfig,
) -> Result<(), FirecrackerError> {
    let timeout = config.timeout_for_endpoint(endpoint);
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    let body_str = serde_json::to_string(body).map_err(TransportError::Serialize)?;

    debug!(endpoint, body = %body_str, timeout_ms, "PUT");
    let output = Command::new("curl")
        .arg("--unix-socket")
        .arg(socket)
        .arg("-X")
        .arg("PUT")
        .arg(format!("http://localhost{endpoint}"))
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(&body_str)
        .arg("--fail-with-body")
        .arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg(timeout.as_secs_f64().to_string())
        .output()
        .await
        .map_err(|e| TransportError::CurlFailed(e.to_string()))?;

    if output.status.code() == Some(28) {
        return Err(FirecrackerError::ApiCallTimedOut {
            endpoint: endpoint.to_owned(),
            timeout_ms,
        });
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let status = u16::try_from(output.status.code().unwrap_or(-1)).unwrap_or(0);
        return Err(FirecrackerError::ApiCallFailed {
            endpoint: endpoint.to_owned(),
            status,
            body: format!("stderr={stderr}, stdout={stdout}"),
        });
    }

    Ok(())
}

/// Transport-layer PUT without mapping to `FirecrackerError` (for tests).
pub async fn api_put_raw(
    socket: &Path,
    endpoint: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<(), TransportError> {
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    let body_str = serde_json::to_string(body)?;

    let output = Command::new("curl")
        .arg("--unix-socket")
        .arg(socket)
        .arg("-X")
        .arg("PUT")
        .arg(format!("http://localhost{endpoint}"))
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(&body_str)
        .arg("--fail-with-body")
        .arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg(timeout.as_secs_f64().to_string())
        .output()
        .await
        .map_err(|e| TransportError::CurlFailed(e.to_string()))?;

    if output.status.code() == Some(28) {
        return Err(TransportError::TimedOut {
            endpoint: endpoint.to_owned(),
            timeout_ms,
        });
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(TransportError::CurlFailed(format!(
            "PUT {endpoint} failed (exit {}): stderr={stderr}, stdout={stdout}",
            output.status
        )));
    }

    Ok(())
}
