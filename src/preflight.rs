//! Doctor-as-a-gate: turn `rooms doctor` from an advisory report into a hard
//! precheck a host run passes before it boots anything.
//!
//! `rooms doctor` already exits non-zero on any FAIL, so a shell harness can gate
//! on its exit code alone. This module is the *programmatic* form for the Rust
//! e2e harness: parse the `--json` report, split it into hard failures (abort)
//! and warnings (allowed, logged), and surface each failure's remediation — so a
//! misprovisioned host fails loud at the door with the exact fix instead of deep
//! in boot with a confusing error.
//!
//! Fail-safe: a report that can't be read — unparseable, or a schema version this
//! build doesn't understand — is a *failed* gate, never a silent pass (the same
//! "couldn't verify ≠ clean" discipline `rooms diff` follows).

use std::path::Path;
use std::process::Command;

use thiserror::Error;

use crate::doctor::{CheckResult, DoctorReport, DOCTOR_SCHEMA_VERSION};

/// The result of gating a doctor report: the hard failures that must abort a run
/// and the warnings that are allowed through but worth logging.
#[derive(Debug, Clone)]
pub struct Preflight {
    pub failures: Vec<CheckResult>,
    pub warnings: Vec<CheckResult>,
}

impl Preflight {
    /// True when no check hard-failed — the run may proceed (warnings and all).
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// One `name: message` line per failing check. Doctor's message *is* the
    /// remediation, so this is what a caller prints before aborting.
    #[must_use]
    pub fn remediations(&self) -> Vec<String> {
        self.failures
            .iter()
            .map(|c| format!("{}: {}", c.name, c.message))
            .collect()
    }
}

/// Why a gate could not be decided cleanly. Every variant is a *failed* gate: a
/// caller must never read one as "clean".
#[derive(Debug, Error)]
pub enum PreflightError {
    #[error("could not parse doctor --json report: {0}")]
    Parse(String),
    #[error("doctor report schema_version {found} != supported {supported}; regenerate with this rooms build")]
    SchemaVersion { found: u32, supported: u32 },
    #[error("could not run doctor: {0}")]
    Doctor(String),
}

/// Gate an already-parsed report: partition its checks into hard failures
/// (not `ok`) and warnings ([`CheckResult::is_warning`]); a clean `ok` check
/// contributes to neither.
#[must_use]
pub fn gate(report: &DoctorReport) -> Preflight {
    let mut failures = Vec::new();
    let mut warnings = Vec::new();
    for check in &report.checks {
        if !check.ok {
            failures.push(check.clone());
        } else if check.is_warning() {
            warnings.push(check.clone());
        }
    }
    Preflight { failures, warnings }
}

/// Parse `rooms doctor --json` output and gate it. A parse error or a schema
/// version this build doesn't understand is a failed gate, never a silent pass.
pub fn from_json(json: &str) -> Result<Preflight, PreflightError> {
    let report: DoctorReport =
        serde_json::from_str(json).map_err(|e| PreflightError::Parse(e.to_string()))?;
    if report.schema_version != DOCTOR_SCHEMA_VERSION {
        return Err(PreflightError::SchemaVersion {
            found: report.schema_version,
            supported: DOCTOR_SCHEMA_VERSION,
        });
    }
    Ok(gate(&report))
}

/// Run `<rooms_bin> doctor --json [--image <image>]` and gate its stdout — the
/// shell-out the e2e harness calls at the door.
///
/// The doctor binary always writes the JSON report to stdout (logs stay on
/// stderr) regardless of its own exit code, so the parsed report — not the exit
/// status — drives the gate.
pub fn run(rooms_bin: &Path, image: Option<&Path>) -> Result<Preflight, PreflightError> {
    let mut cmd = Command::new(rooms_bin);
    cmd.arg("doctor").arg("--json");
    if let Some(image) = image {
        cmd.arg("--image").arg(image);
    }
    let output = cmd
        .output()
        .map_err(|e| PreflightError::Doctor(format!("spawn {}: {e}", rooms_bin.display())))?;
    from_json(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test module")]

    use super::{from_json, gate, PreflightError};
    use crate::doctor::{CheckResult, DoctorReport, DOCTOR_SCHEMA_VERSION};

    fn check(name: &str, ok: bool, message: &str) -> CheckResult {
        CheckResult {
            name: name.to_owned(),
            ok,
            message: message.to_owned(),
        }
    }

    /// Serialize a real `DoctorReport` so the fixtures round-trip through the
    /// exact production schema — a field rename can't silently drift the gate.
    fn report_json(checks: Vec<CheckResult>) -> String {
        let report = DoctorReport {
            schema_version: DOCTOR_SCHEMA_VERSION,
            checks,
        };
        serde_json::to_string(&report).unwrap()
    }

    #[test]
    fn all_checks_ok_passes_clean() {
        let json = report_json(vec![
            check("kvm", true, "/dev/kvm present"),
            check("firecracker", true, "1.10.1"),
        ]);
        let pf = from_json(&json).expect("gate a clean report");
        assert!(pf.passed());
        assert!(pf.failures.is_empty() && pf.warnings.is_empty());
    }

    #[test]
    fn a_failing_check_aborts_the_gate_and_surfaces_its_remediation() {
        let json = report_json(vec![
            check("kvm", true, "/dev/kvm present"),
            check(
                "rooms_fwd",
                false,
                "ROOMS_FWD not installed; run `sudo bash scripts/setup-tap.sh --host`",
            ),
        ]);
        let pf = from_json(&json).expect("gate returns a decision, not an error");
        assert!(!pf.passed(), "a FAIL check must fail the gate");
        assert_eq!(pf.failures.len(), 1);
        assert!(
            pf.remediations()
                .iter()
                .any(|line| line.contains("setup-tap.sh")),
            "the failing check's remediation must be surfaced: {:?}",
            pf.remediations()
        );
    }

    #[test]
    fn a_warn_check_is_allowed_through_but_recorded() {
        let json = report_json(vec![
            check("api_key", true, "warn: ANTHROPIC_API_KEY unset"),
            check("kvm", true, "/dev/kvm present"),
        ]);
        let pf = from_json(&json).expect("gate a report with a warning");
        assert!(pf.passed(), "a warning must not fail the gate");
        assert_eq!(pf.warnings.len(), 1, "the warning is recorded");
        assert!(pf.failures.is_empty());
    }

    #[test]
    fn a_foreign_schema_version_is_a_failed_gate_not_a_pass() {
        // An escape-shaped report under a schema this build can't read must not
        // slip through as clean.
        let json = format!(
            r#"{{"schema_version":{},"checks":[]}}"#,
            DOCTOR_SCHEMA_VERSION + 1
        );
        assert!(matches!(
            from_json(&json),
            Err(PreflightError::SchemaVersion { .. })
        ));
    }

    #[test]
    fn unparseable_output_is_a_failed_gate() {
        // A doctor that errored before emitting JSON (empty/garbage stdout) must
        // read as a failed gate, never a silent pass.
        assert!(matches!(from_json(""), Err(PreflightError::Parse(_))));
        assert!(matches!(
            from_json("not json"),
            Err(PreflightError::Parse(_))
        ));
    }

    #[test]
    fn gate_partitions_failures_and_warnings_by_convention() {
        let report = DoctorReport {
            schema_version: DOCTOR_SCHEMA_VERSION,
            checks: vec![
                check("ok_check", true, "fine"),
                check("warn_check", true, "warn: soft"),
                check("fail_check", false, "hard: fix me"),
                // ok:false wins even if the message is warn-shaped — not-ok is
                // always a hard failure.
                check("fail_warnish", false, "warn: still a failure"),
            ],
        };
        let pf = gate(&report);
        assert_eq!(pf.failures.len(), 2);
        assert_eq!(pf.warnings.len(), 1);
        assert!(!pf.passed());
    }
}
