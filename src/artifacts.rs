//! Host-side artifact layout and validation for runner output.
//!
//! See `docs/runner-contract.md` for the full contract.

use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current `result.json` schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Relative paths of required files under an `out/` directory.
pub const RESULT_JSON: &str = "result.json";
pub const STDOUT_LOG: &str = "logs/stdout.log";
pub const STDERR_LOG: &str = "logs/stderr.log";

/// Outcome status written by the substrate (or overridden on timeout/cancel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

/// Versioned `result.json` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultJson {
    pub schema_version: u32,
    pub status: RunStatus,
    pub exit_code: i32,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_path: Option<String>,
    pub command: Vec<String>,
}

impl ResultJson {
    /// Map a normal process exit code to `RunStatus` (timeout/cancel are substrate overrides).
    #[must_use]
    pub const fn status_from_exit_code(exit_code: i32) -> RunStatus {
        if exit_code == 0 {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed
        }
    }

    /// Build a `result.json` value from exec metadata.
    #[must_use]
    pub const fn from_exec(
        exit_code: i32,
        status: RunStatus,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
        command: Vec<String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            status,
            exit_code,
            started_at,
            ended_at,
            summary_path: None,
            patch_path: None,
            events_path: None,
            command,
        }
    }
}

/// Validated artifact bundle loaded from an `out/` directory on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerArtifacts {
    pub result: ResultJson,
    pub summary: Option<String>,
    pub patch: Option<String>,
    /// Path to `events.ndjson` when present; contents are not loaded.
    pub events: Option<PathBuf>,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

/// Validation failures when loading an artifact directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactsError {
    MissingRequired(String),
    UnsupportedSchemaVersion(u32),
    DanglingReference(String),
    UnsafeReference(String),
    InvalidJson(String),
    Io { path: PathBuf, message: String },
}

impl fmt::Display for ArtifactsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequired(path) => write!(f, "missing required artifact: {path}"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(f, "unsupported result.json schema_version: {version}")
            }
            Self::DanglingReference(path) => {
                write!(f, "result.json references missing file: {path}")
            }
            Self::UnsafeReference(path) => {
                write!(f, "result.json reference escapes the artifact dir: {path}")
            }
            Self::InvalidJson(detail) => write!(f, "invalid result.json: {detail}"),
            Self::Io { path, message } => {
                write!(f, "read {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ArtifactsError {}

impl RunnerArtifacts {
    /// Walk `out_dir`, validate required files, parse `result.json`, and load optional contents.
    pub async fn load(out_dir: &Path) -> Result<Self, ArtifactsError> {
        let result_path = out_dir.join(RESULT_JSON);
        let stdout_path = out_dir.join(STDOUT_LOG);
        let stderr_path = out_dir.join(STDERR_LOG);

        ensure_exists(&result_path, RESULT_JSON)?;
        ensure_exists(&stdout_path, STDOUT_LOG)?;
        ensure_exists(&stderr_path, STDERR_LOG)?;

        let raw = tokio::fs::read_to_string(&result_path)
            .await
            .map_err(|err| io_error(result_path, &err))?;
        let result = parse_result_json(&raw)?;

        validate_reference(out_dir, result.summary_path.as_deref())?;
        validate_reference(out_dir, result.patch_path.as_deref())?;
        validate_reference(out_dir, result.events_path.as_deref())?;

        let summary = read_optional_text(out_dir, result.summary_path.as_deref()).await?;
        let patch = read_optional_text(out_dir, result.patch_path.as_deref()).await?;
        let events = match result.events_path.as_deref() {
            Some(rel) => Some(safe_join(out_dir, rel)?),
            None => None,
        };

        Ok(Self {
            result,
            summary,
            patch,
            events,
            stdout: stdout_path,
            stderr: stderr_path,
        })
    }
}

fn ensure_exists(path: &Path, label: &str) -> Result<(), ArtifactsError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(ArtifactsError::MissingRequired(label.to_owned()))
    }
}

fn parse_result_json(raw: &str) -> Result<ResultJson, ArtifactsError> {
    let result: ResultJson =
        serde_json::from_str(raw).map_err(|err| ArtifactsError::InvalidJson(err.to_string()))?;
    if result.schema_version != SCHEMA_VERSION {
        return Err(ArtifactsError::UnsupportedSchemaVersion(
            result.schema_version,
        ));
    }
    Ok(result)
}

fn validate_reference(out_dir: &Path, rel: Option<&str>) -> Result<(), ArtifactsError> {
    let Some(rel) = rel else {
        return Ok(());
    };
    let path = safe_join(out_dir, rel)?;
    if !path.is_file() {
        return Err(ArtifactsError::DanglingReference(rel.to_owned()));
    }
    // The lexical safe_join above catches `..` and absolute paths, but a
    // symlink at `out_dir/summary.md` pointing at `/etc/passwd` would
    // still pass that check. Resolve the real path and verify it stays
    // under the canonical out_dir before any reader follows it.
    ensure_inside_out_dir(out_dir, &path, rel)
}

async fn read_optional_text(
    out_dir: &Path,
    rel: Option<&str>,
) -> Result<Option<String>, ArtifactsError> {
    let Some(rel) = rel else {
        return Ok(None);
    };
    let path = safe_join(out_dir, rel)?;
    ensure_inside_out_dir(out_dir, &path, rel)?;
    let contents = tokio::fs::read_to_string(&path)
        .await
        .map_err(|err| io_error(path, &err))?;
    Ok(Some(contents))
}

/// Join `rel` onto `out_dir` only if `rel` stays inside the artifact dir.
///
/// `result.json` path fields are documented as relative paths under `out/`.
/// A runner that writes an absolute path (`/etc/passwd`) or one with `..`
/// components could otherwise trick `rooms collect` into reading or
/// validating files outside the room's artifact dir.
fn safe_join(out_dir: &Path, rel: &str) -> Result<PathBuf, ArtifactsError> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(ArtifactsError::UnsafeReference(rel.to_owned()));
    }
    for component in rel_path.components() {
        use std::path::Component;
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(ArtifactsError::UnsafeReference(rel.to_owned()));
            }
        }
    }
    Ok(out_dir.join(rel_path))
}

/// Resolve `path` (which already passed `safe_join`) through any symlinks
/// and confirm the real target still sits under `out_dir`. Rejects
/// `summary.md → /etc/passwd` and similar escapes that the lexical check
/// can't see. The path must exist; callers should `is_file()` first.
fn ensure_inside_out_dir(out_dir: &Path, path: &Path, rel: &str) -> Result<(), ArtifactsError> {
    let canonical_out =
        std::fs::canonicalize(out_dir).map_err(|err| io_error(out_dir.to_path_buf(), &err))?;
    let canonical_target =
        std::fs::canonicalize(path).map_err(|err| io_error(path.to_path_buf(), &err))?;
    if canonical_target.starts_with(&canonical_out) {
        Ok(())
    } else {
        Err(ArtifactsError::UnsafeReference(rel.to_owned()))
    }
}

fn io_error(path: PathBuf, err: &std::io::Error) -> ArtifactsError {
    ArtifactsError::Io {
        path,
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module: panicky lints are noise in tests"
    )]

    use std::path::Path;

    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    use super::{
        ArtifactsError, ResultJson, RunStatus, RunnerArtifacts, RESULT_JSON, SCHEMA_VERSION,
        STDERR_LOG, STDOUT_LOG,
    };

    fn sample_result() -> ResultJson {
        ResultJson {
            schema_version: SCHEMA_VERSION,
            status: RunStatus::Succeeded,
            exit_code: 0,
            started_at: Utc.with_ymd_and_hms(2026, 5, 23, 22, 14, 0).unwrap(),
            ended_at: Utc.with_ymd_and_hms(2026, 5, 23, 22, 18, 42).unwrap(),
            summary_path: Some("summary.md".to_owned()),
            patch_path: None,
            events_path: None,
            command: vec!["claude".to_owned(), "-p".to_owned(), "...".to_owned()],
        }
    }

    async fn write_minimal_out(dir: &Path, result: &ResultJson) {
        tokio::fs::create_dir_all(dir.join("logs"))
            .await
            .expect("create logs dir");
        tokio::fs::write(
            dir.join(RESULT_JSON),
            serde_json::to_string_pretty(result).expect("serialize result"),
        )
        .await
        .expect("write result.json");
        tokio::fs::write(dir.join(STDOUT_LOG), "stdout\n")
            .await
            .expect("write stdout.log");
        tokio::fs::write(dir.join(STDERR_LOG), "stderr\n")
            .await
            .expect("write stderr.log");
    }

    #[test]
    fn result_json_round_trip() {
        let original = sample_result();
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: ResultJson = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, parsed);
    }

    #[test]
    fn status_from_exit_code_maps_zero_and_nonzero() {
        assert_eq!(ResultJson::status_from_exit_code(0), RunStatus::Succeeded);
        assert_eq!(ResultJson::status_from_exit_code(1), RunStatus::Failed);
    }

    #[tokio::test]
    async fn unsupported_schema_version_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let mut result = sample_result();
        result.schema_version = 99;
        write_minimal_out(dir.path(), &result).await;

        let err = RunnerArtifacts::load(dir.path())
            .await
            .expect_err("schema 99 should fail");
        assert_eq!(err, ArtifactsError::UnsupportedSchemaVersion(99));
    }

    #[tokio::test]
    async fn missing_result_json_errors() {
        let dir = tempdir().expect("tempdir");
        tokio::fs::create_dir_all(dir.path().join("logs"))
            .await
            .expect("create logs");
        tokio::fs::write(dir.path().join(STDOUT_LOG), "")
            .await
            .expect("stdout");
        tokio::fs::write(dir.path().join(STDERR_LOG), "")
            .await
            .expect("stderr");

        let err = RunnerArtifacts::load(dir.path())
            .await
            .expect_err("missing result.json");
        assert_eq!(err, ArtifactsError::MissingRequired(RESULT_JSON.to_owned()));
    }

    #[tokio::test]
    async fn missing_stdout_log_errors() {
        let dir = tempdir().expect("tempdir");
        let result = sample_result();
        tokio::fs::write(
            dir.path().join(RESULT_JSON),
            serde_json::to_string(&result).expect("serialize"),
        )
        .await
        .expect("result.json");
        tokio::fs::create_dir_all(dir.path().join("logs"))
            .await
            .expect("logs dir");
        tokio::fs::write(dir.path().join(STDERR_LOG), "")
            .await
            .expect("stderr");

        let err = RunnerArtifacts::load(dir.path())
            .await
            .expect_err("missing stdout.log");
        assert_eq!(err, ArtifactsError::MissingRequired(STDOUT_LOG.to_owned()));
    }

    #[tokio::test]
    async fn optional_files_absent_succeeds() {
        let dir = tempdir().expect("tempdir");
        let mut result = sample_result();
        result.summary_path = None;
        result.patch_path = None;
        result.events_path = None;
        write_minimal_out(dir.path(), &result).await;

        let loaded = RunnerArtifacts::load(dir.path())
            .await
            .expect("load minimal out dir");
        assert!(loaded.summary.is_none());
        assert!(loaded.patch.is_none());
        assert!(loaded.events.is_none());
    }

    #[tokio::test]
    async fn dangling_summary_reference_errors() {
        let dir = tempdir().expect("tempdir");
        let result = sample_result();
        write_minimal_out(dir.path(), &result).await;

        let err = RunnerArtifacts::load(dir.path())
            .await
            .expect_err("summary.md missing");
        assert_eq!(
            err,
            ArtifactsError::DanglingReference("summary.md".to_owned())
        );
    }

    #[tokio::test]
    async fn loads_optional_summary_when_present() {
        let dir = tempdir().expect("tempdir");
        let result = sample_result();
        write_minimal_out(dir.path(), &result).await;
        tokio::fs::write(dir.path().join("summary.md"), "all good")
            .await
            .expect("summary");

        let loaded = RunnerArtifacts::load(dir.path())
            .await
            .expect("load with summary");
        assert_eq!(loaded.summary.as_deref(), Some("all good"));
    }

    #[tokio::test]
    async fn parent_dir_reference_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let mut result = sample_result();
        result.summary_path = Some("../escape.md".to_owned());
        write_minimal_out(dir.path(), &result).await;

        let err = RunnerArtifacts::load(dir.path())
            .await
            .expect_err("../ should be rejected");
        assert_eq!(
            err,
            ArtifactsError::UnsafeReference("../escape.md".to_owned())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let mut result = sample_result();
        // Inside out_dir, place a symlink that points outside.
        let outside = dir.path().parent().expect("temp parent").join("escape.md");
        tokio::fs::write(&outside, "escaped")
            .await
            .expect("write outside");
        let link = dir.path().join("summary.md");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        result.summary_path = Some("summary.md".to_owned());
        write_minimal_out(dir.path(), &result).await;

        let err = RunnerArtifacts::load(dir.path())
            .await
            .expect_err("symlink escape should be rejected");
        assert_eq!(
            err,
            ArtifactsError::UnsafeReference("summary.md".to_owned())
        );
    }

    #[tokio::test]
    async fn absolute_reference_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let mut result = sample_result();
        // Use a platform-appropriate absolute path so the test runs on both
        // Linux CI and the rooms-host Windows builder.
        let abs = if cfg!(windows) {
            "C:\\Windows\\System32\\drivers\\etc\\hosts".to_owned()
        } else {
            "/etc/passwd".to_owned()
        };
        result.summary_path = Some(abs.clone());
        write_minimal_out(dir.path(), &result).await;

        let err = RunnerArtifacts::load(dir.path())
            .await
            .expect_err("absolute path should be rejected");
        assert_eq!(err, ArtifactsError::UnsafeReference(abs));
    }

    mod path_validation_properties {
        use std::path::PathBuf;

        use proptest::prelude::*;

        use super::super::{safe_join, ArtifactsError};

        fn sample_out_dir() -> PathBuf {
            PathBuf::from("/tmp/rooms-artifact-out")
        }

        proptest! {
            #[test]
            fn relative_segments_stay_inside(
                segments in proptest::collection::vec("[a-z0-9_-]+", 1..5),
            ) {
                let rel = segments.join("/");
                let out_dir = sample_out_dir();
                let joined = safe_join(&out_dir, &rel).expect("safe relative path");
                prop_assert!(joined.starts_with(&out_dir));
            }

            #[test]
            fn parent_dir_components_are_rejected(
                prefix in proptest::collection::vec("[a-z0-9]+", 0..3),
                suffix in proptest::collection::vec("[a-z0-9]+", 0..3),
            ) {
                let mut parts = prefix;
                parts.push("..".to_owned());
                parts.extend(suffix);
                let rel = parts.join("/");
                let err = safe_join(&sample_out_dir(), &rel).expect_err(".. must be rejected");
                prop_assert_eq!(err, ArtifactsError::UnsafeReference(rel));
            }

            #[test]
            fn absolute_paths_are_rejected(segments in proptest::collection::vec("[a-z0-9]+", 1..4)) {
                let rel = if cfg!(windows) {
                    format!("C:\\{}", segments.join("\\"))
                } else {
                    format!("/{}", segments.join("/"))
                };
                let err = safe_join(&sample_out_dir(), &rel).expect_err("absolute path");
                prop_assert_eq!(err, ArtifactsError::UnsafeReference(rel));
            }

            #[test]
            fn multi_segment_escapes_are_rejected(
                depth in 1usize..5,
                tail in proptest::collection::vec("[a-z0-9]+", 1..3),
            ) {
                let mut parts = vec!["inside".to_owned(); depth];
                for _ in 0..=depth {
                    parts.push("..".to_owned());
                }
                parts.extend(tail);
                let rel = parts.join("/");
                let err = safe_join(&sample_out_dir(), &rel).expect_err("multi-segment escape");
                prop_assert_eq!(err, ArtifactsError::UnsafeReference(rel));
            }

            #[test]
            fn embedded_nul_with_traversal_is_rejected(
                prefix in proptest::collection::vec("[a-z0-9]+", 1..3),
                tail in proptest::collection::vec("[a-z0-9]+", 0..2),
            ) {
                let mut parts = prefix;
                parts.push("seg\0ment".to_owned());
                parts.push("..".to_owned());
                parts.extend(tail);
                let rel = parts.join("/");
                let err = safe_join(&sample_out_dir(), &rel).expect_err("NUL + .. must reject");
                prop_assert_eq!(err, ArtifactsError::UnsafeReference(rel));
            }
        }
    }
}
