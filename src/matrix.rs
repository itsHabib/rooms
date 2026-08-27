//! Strict, immutable case manifests for snapshot-backed command matrices.

use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The only matrix manifest schema accepted by this binary.
pub const SCHEMA: &str = "rooms.matrix.v1";
const MAX_MANIFEST_BYTES: usize = 128 * 1024;
const MAX_CASES: usize = 8;
const MAX_CASE_ID_BYTES: usize = 64;
const MAX_COMMAND_BYTES: usize = 16 * 1024;

/// One immutable case loaded from a matrix manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    pub id: String,
    pub command: String,
    pub command_sha256: String,
}

/// A validated, load-once matrix manifest plus its semantic digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub sha256: String,
    pub cases: Vec<Case>,
}

impl Manifest {
    /// Number of isolated clones required by this matrix.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "validated manifests contain at most eight cases"
    )]
    pub const fn count(&self) -> u8 {
        self.cases.len() as u8
    }
}

/// Matrix manifest admission failures. Every variant is raised before clone
/// allocation, snapshot leasing, or guest execution begins.
#[derive(Debug, Error)]
pub enum MatrixError {
    #[error("cannot read matrix manifest {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("matrix manifest exceeds the {max_bytes}-byte limit")]
    TooLarge { max_bytes: usize },
    #[error("invalid matrix manifest JSON: {0}")]
    Parse(#[source] serde_json::Error),
    #[error("unsupported matrix schema {actual:?}; expected {SCHEMA:?}")]
    Schema { actual: String },
    #[error("matrix must contain between 1 and {MAX_CASES} cases; got {actual}")]
    CaseCount { actual: usize },
    #[error("matrix case id {id:?} must match [a-z0-9][a-z0-9_-]{{0,63}}")]
    InvalidCaseId { id: String },
    #[error("matrix case id {id:?} is duplicated")]
    DuplicateCaseId { id: String },
    #[error("matrix case {id:?} has an empty command")]
    EmptyCommand { id: String },
    #[error("matrix case {id:?} command exceeds the {max_bytes}-byte limit")]
    CommandTooLarge { id: String, max_bytes: usize },
    #[error("matrix case {id:?} command contains a NUL byte")]
    CommandContainsNul { id: String },
    #[error("cannot canonicalize matrix manifest: {0}")]
    Canonicalize(#[source] serde_json::Error),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireManifest {
    schema: String,
    cases: Vec<WireCase>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireCase {
    id: String,
    command: String,
}

/// Read and validate a matrix manifest exactly once.
///
/// The returned value owns every byte that can influence execution. Callers do
/// not reread the source path after effects begin.
pub fn load(path: &Path) -> Result<Manifest, MatrixError> {
    let mut file = File::open(path).map_err(|source| MatrixError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| MatrixError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(MatrixError::TooLarge {
            max_bytes: MAX_MANIFEST_BYTES,
        });
    }

    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let wire = WireManifest::deserialize(&mut deserializer).map_err(MatrixError::Parse)?;
    deserializer.end().map_err(MatrixError::Parse)?;
    validate(&wire)?;

    let canonical = serde_json::to_vec(&wire).map_err(MatrixError::Canonicalize)?;
    let sha256 = digest(&canonical);
    let cases = wire
        .cases
        .into_iter()
        .map(|case| Case {
            command_sha256: digest(case.command.as_bytes()),
            id: case.id,
            command: case.command,
        })
        .collect();
    Ok(Manifest { sha256, cases })
}

fn validate(manifest: &WireManifest) -> Result<(), MatrixError> {
    if manifest.schema != SCHEMA {
        return Err(MatrixError::Schema {
            actual: manifest.schema.clone(),
        });
    }
    if !(1..=MAX_CASES).contains(&manifest.cases.len()) {
        return Err(MatrixError::CaseCount {
            actual: manifest.cases.len(),
        });
    }
    let mut ids = HashSet::with_capacity(manifest.cases.len());
    for case in &manifest.cases {
        validate_case(case, &mut ids)?;
    }
    Ok(())
}

fn validate_case(case: &WireCase, ids: &mut HashSet<String>) -> Result<(), MatrixError> {
    if !valid_case_id(&case.id) {
        return Err(MatrixError::InvalidCaseId {
            id: case.id.clone(),
        });
    }
    if !ids.insert(case.id.clone()) {
        return Err(MatrixError::DuplicateCaseId {
            id: case.id.clone(),
        });
    }
    if case.command.trim().is_empty() {
        return Err(MatrixError::EmptyCommand {
            id: case.id.clone(),
        });
    }
    if case.command.len() > MAX_COMMAND_BYTES {
        return Err(MatrixError::CommandTooLarge {
            id: case.id.clone(),
            max_bytes: MAX_COMMAND_BYTES,
        });
    }
    if case.command.contains('\0') {
        return Err(MatrixError::CommandContainsNul {
            id: case.id.clone(),
        });
    }
    Ok(())
}

fn valid_case_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_CASE_ID_BYTES {
        return false;
    }
    let mut bytes = id.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
    })
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::indexing_slicing, reason = "test module")]

    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::{load, MatrixError, MAX_COMMAND_BYTES, MAX_MANIFEST_BYTES, SCHEMA};

    fn manifest(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("temporary manifest");
        file.write_all(contents.as_bytes()).expect("write manifest");
        file
    }

    #[test]
    fn loads_cases_in_declared_order_and_hashes_semantics() {
        let compact = manifest(
            r#"{"schema":"rooms.matrix.v1","cases":[{"id":"clean","command":"make check"},{"id":"browser-mutant","command":"! npm test"}]}"#,
        );
        let spaced = manifest(
            r#"{
                "schema": "rooms.matrix.v1",
                "cases": [
                    {"id": "clean", "command": "make check"},
                    {"id": "browser-mutant", "command": "! npm test"}
                ]
            }"#,
        );
        let first = load(compact.path()).expect("compact manifest");
        let second = load(spaced.path()).expect("spaced manifest");
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(first.count(), 2);
        assert_eq!(first.cases[0].id, "clean");
        assert_eq!(first.cases[1].id, "browser-mutant");
        assert_ne!(first.cases[0].command_sha256, first.cases[1].command_sha256);
    }

    #[test]
    fn rejects_unknown_fields_and_trailing_json() {
        let unknown = manifest(
            r#"{"schema":"rooms.matrix.v1","cases":[{"id":"clean","command":"true","oracle":"hidden"}]}"#,
        );
        assert!(matches!(load(unknown.path()), Err(MatrixError::Parse(_))));
        let trailing = manifest(
            r#"{"schema":"rooms.matrix.v1","cases":[{"id":"clean","command":"true"}]} {}"#,
        );
        assert!(matches!(load(trailing.path()), Err(MatrixError::Parse(_))));
    }

    #[test]
    fn rejects_invalid_identity_duplicate_and_command() {
        let invalid_id = manifest(
            r#"{"schema":"rooms.matrix.v1","cases":[{"id":"../escape","command":"true"}]}"#,
        );
        assert!(matches!(
            load(invalid_id.path()),
            Err(MatrixError::InvalidCaseId { .. })
        ));
        let duplicate = manifest(
            r#"{"schema":"rooms.matrix.v1","cases":[{"id":"same","command":"true"},{"id":"same","command":"false"}]}"#,
        );
        assert!(matches!(
            load(duplicate.path()),
            Err(MatrixError::DuplicateCaseId { .. })
        ));
        let empty =
            manifest(r#"{"schema":"rooms.matrix.v1","cases":[{"id":"empty","command":"  "}]}"#);
        assert!(matches!(
            load(empty.path()),
            Err(MatrixError::EmptyCommand { .. })
        ));
        let nul = manifest(
            r#"{"schema":"rooms.matrix.v1","cases":[{"id":"nul","command":"before\u0000after"}]}"#,
        );
        assert!(matches!(
            load(nul.path()),
            Err(MatrixError::CommandContainsNul { .. })
        ));
    }

    #[test]
    fn rejects_wrong_schema_and_case_count() {
        let wrong = manifest(r#"{"schema":"rooms.matrix.v2","cases":[]}"#);
        assert!(matches!(
            load(wrong.path()),
            Err(MatrixError::Schema { .. })
        ));
        let empty = manifest(&format!(r#"{{"schema":"{SCHEMA}","cases":[]}}"#));
        assert!(matches!(
            load(empty.path()),
            Err(MatrixError::CaseCount { actual: 0 })
        ));
    }

    #[test]
    fn rejects_case_command_and_manifest_bounds() {
        let cases = (0..9)
            .map(|index| format!(r#"{{"id":"case-{index}","command":"true"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let too_many = manifest(&format!(r#"{{"schema":"{SCHEMA}","cases":[{cases}]}}"#));
        assert!(matches!(
            load(too_many.path()),
            Err(MatrixError::CaseCount { actual: 9 })
        ));

        let command = "x".repeat(MAX_COMMAND_BYTES + 1);
        let too_long = manifest(&format!(
            r#"{{"schema":"{SCHEMA}","cases":[{{"id":"large","command":"{command}"}}]}}"#
        ));
        assert!(matches!(
            load(too_long.path()),
            Err(MatrixError::CommandTooLarge { .. })
        ));

        let oversized = manifest(&" ".repeat(MAX_MANIFEST_BYTES + 1));
        assert!(matches!(
            load(oversized.path()),
            Err(MatrixError::TooLarge { .. })
        ));
    }
}
