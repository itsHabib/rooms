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
    /// Branch the runner pushed the agent's changes to (cursor `--push-branch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_branch: Option<String>,
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
            pushed_branch: None,
            command,
        }
    }
}

/// Relative path of the overlay change-set artifact under an `out/` directory.
pub const CHANGESET_JSON: &str = "changeset.json";

/// Schema version for `changeset.json` (independent of `result.json`).
pub const CHANGESET_SCHEMA_VERSION: u32 = 1;

/// Guest path prefix (sans leading `/`) that holds the agent's expected work.
const WORKSPACE_PREFIX: &str = "workspace/";

/// Runtime / log / temp prefixes (sans leading `/`) the OS writes on every boot.
/// The overlay captures *everything* written since boot, so these would drown
/// the real signal; they're filtered out of the lane-escape tripwire.
const EPHEMERAL_PREFIXES: &[&str] = &[
    "run/",
    "var/run/",
    "var/log/",
    "var/cache/",
    "var/tmp/",
    "tmp/",
    "dev/",
    "proc/",
    "sys/",
];

/// True when `path` is a lane escape: a change outside `/workspace`.
///
/// Excludes the OS's expected ephemeral churn (`/run`, `/var/log`, `/tmp`, ...),
/// so it flags only writes to persistent locations (`/etc`, `/usr`, `/root`,
/// ...) the agent had no business touching — the writes a `git diff` of the
/// repo structurally cannot see.
#[must_use]
pub fn is_lane_escape(path: &str) -> bool {
    if path.starts_with(WORKSPACE_PREFIX) {
        return false;
    }
    !EPHEMERAL_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// The filesystem changes an overlay run made, derived from the tmpfs upperdir.
///
/// Paths are guest-absolute without the leading `/` (e.g. `workspace/repo/x.rs`,
/// `etc/hosts`). `overlay_active` is false for a writable-rootfs run, where
/// there is no overlay to read. The set is *everything* written since boot —
/// on the cursor path that includes the repo clone — so the sharp signal is
/// [`Changeset::lane_escapes`], the persistent writes a git diff can't see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset {
    pub schema_version: u32,
    pub overlay_active: bool,
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl Changeset {
    /// An inactive change set (a writable-rootfs run has no overlay to read).
    #[must_use]
    pub const fn inactive() -> Self {
        Self {
            schema_version: CHANGESET_SCHEMA_VERSION,
            overlay_active: false,
            added: Vec::new(),
            modified: Vec::new(),
            deleted: Vec::new(),
        }
    }

    /// No files changed (whether or not the overlay was active).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }

    /// The lane-escape tripwire: changed paths (any op) outside `/workspace` to
    /// a persistent location, excluding expected OS churn (see
    /// [`is_lane_escape`]). The sharp signal `rooms diff` leads with.
    #[must_use]
    pub fn lane_escapes(&self) -> Vec<&str> {
        self.added
            .iter()
            .chain(&self.modified)
            .chain(&self.deleted)
            .map(String::as_str)
            .filter(|path| is_lane_escape(path))
            .collect()
    }

    /// Count of outside-`/workspace` writes filtered as expected OS churn
    /// (`/run`, `/var/log`, `/tmp`, ...) — surfaced as a quiet note, not hidden.
    #[must_use]
    pub fn ephemeral_count(&self) -> usize {
        self.added
            .iter()
            .chain(&self.modified)
            .chain(&self.deleted)
            .filter(|path| !path.starts_with(WORKSPACE_PREFIX) && !is_lane_escape(path))
            .count()
    }

    /// `(added, modified, deleted)` counts under `/workspace` — the agent's
    /// expected lane (noisy: includes the repo clone on the cursor path).
    #[must_use]
    pub fn workspace_counts(&self) -> (usize, usize, usize) {
        let under = |paths: &[String]| {
            paths
                .iter()
                .filter(|path| path.starts_with(WORKSPACE_PREFIX))
                .count()
        };
        (
            under(&self.added),
            under(&self.modified),
            under(&self.deleted),
        )
    }
}

/// Parse the guest upperdir enumeration into a [`Changeset`].
///
/// The guest emits NUL-delimited `<op>\t<relpath>` records, where `op` is
/// `A` (added) / `M` (modified) / `D` (deleted). A stream that begins with the
/// `NOOVERLAY` sentinel (no upperdir — a writable-rootfs run) yields an
/// inactive set. Malformed records are skipped rather than failing the parse;
/// this reads attacker-influenceable guest output, so it stays total.
#[must_use]
pub fn parse_changeset_stream(raw: &[u8]) -> Changeset {
    if raw.starts_with(b"NOOVERLAY") {
        return Changeset::inactive();
    }
    let mut changeset = Changeset {
        schema_version: CHANGESET_SCHEMA_VERSION,
        overlay_active: true,
        added: Vec::new(),
        modified: Vec::new(),
        deleted: Vec::new(),
    };
    for record in raw.split(|&byte| byte == 0) {
        let Some((&op, rest)) = record.split_first() else {
            continue;
        };
        let Some(path_bytes) = rest.strip_prefix(b"\t") else {
            continue;
        };
        let path = String::from_utf8_lossy(path_bytes).into_owned();
        if path.is_empty() {
            continue;
        }
        match op {
            b'A' => changeset.added.push(path),
            b'M' => changeset.modified.push(path),
            b'D' => changeset.deleted.push(path),
            _ => {}
        }
    }
    changeset.added.sort();
    changeset.modified.sort();
    changeset.deleted.sort();
    changeset
}

/// Relative path of the host-witness summary under an `out/` directory.
pub const WITNESS_JSON: &str = "witness.json";

/// Relative path of the raw host-witness capture under an `out/` directory.
pub const WITNESS_PCAP: &str = "witness.pcap";

/// Schema version for `witness.json` (independent of `result.json`).
pub const WITNESS_SCHEMA_VERSION: u32 = 1;

/// One egress destination the guest contacted, keyed on `(ip, port, proto)`;
/// `packets` counts guest-originated frames (volume, not success — a lone SYN
/// still records the destination).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    pub ip: String,
    pub port: u16,
    pub proto: String,
    pub packets: u64,
}

/// Host-side egress evidence for one room, derived from `witness.pcap`.
///
/// Observed outside the guest's trust boundary (packets physically transiting
/// the tap — a compromised guest can neither forge nor suppress them).
/// `capture_complete` is the honesty bit: false whenever the raw capture may be
/// partial (started late, died early, or hit the size cap), so truncation is
/// never read as exhaustive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    pub schema_version: u32,
    pub tap: String,
    pub capture_complete: bool,
    pub destinations: Vec<Destination>,
    pub dns_queries: Vec<String>,
}

impl Witness {
    /// An empty witness for `tap` with the given completeness — the summary of
    /// a capture that produced no guest-originated egress (or none parseable).
    #[must_use]
    pub const fn empty(tap: String, capture_complete: bool) -> Self {
        Self {
            schema_version: WITNESS_SCHEMA_VERSION,
            tap,
            capture_complete,
            destinations: Vec::new(),
            dns_queries: Vec::new(),
        }
    }
}

/// The room's own /30 endpoints, excluded from a summary as non-egress.
///
/// Traffic to the gateway (host-side tap peer) or the guest itself never leaves
/// the room's link. `main` supplies these to [`summarize_pcap`], keeping the
/// parser free of network knowledge.
#[derive(Debug, Clone, Copy)]
pub struct GatewayLocal {
    pub gateway: std::net::Ipv4Addr,
    pub guest: std::net::Ipv4Addr,
}

/// Summarize a raw libpcap capture into a [`Witness`].
///
/// Total by construction — it reads adversary-adjacent bytes, so every
/// short/garbled record is skipped, never panicked on. Only guest-originated
/// IPv4 TCP/UDP frames count; ARP, DHCP (UDP 67/68), and gateway-local /30
/// peers are excluded as link-local noise. `complete` threads through the
/// capture's completeness. The link layer is assumed Ethernet (DLT 1, what
/// `tcpdump -i <tap>` produces); any other link type yields an empty set rather
/// than misparsing raw bytes as frames.
#[must_use]
pub fn summarize_pcap(raw: &[u8], tap: &str, local: GatewayLocal, complete: bool) -> Witness {
    let Some((byte_order, mut rest)) = parse_pcap_global_header(raw) else {
        return Witness::empty(tap.to_owned(), complete);
    };
    let mut acc = FlowAccumulator::new(local);
    while let Some((packet, tail)) = next_packet(rest, byte_order) {
        rest = tail;
        acc.observe(packet);
    }
    acc.into_witness(tap.to_owned(), complete)
}

/// Byte order of a pcap file, decided by its magic number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    const fn u32(self, b: [u8; 4]) -> u32 {
        match self {
            Self::Little => u32::from_le_bytes(b),
            Self::Big => u32::from_be_bytes(b),
        }
    }
}

/// Classic libpcap global header length.
const PCAP_GLOBAL_HEADER_LEN: usize = 24;
/// Per-packet record header length (`ts_sec`, `ts_usec`, `incl_len`, `orig_len`).
const PCAP_RECORD_HEADER_LEN: usize = 16;
/// `LINKTYPE_ETHERNET` — the only link layer `tcpdump -i <tap>` produces here.
const DLT_ETHERNET: u32 = 1;
/// Ethernet header length and the IPv4 ethertype.
const ETHERNET_HEADER_LEN: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
/// IP protocol numbers we summarize.
const IP_PROTO_TCP: u8 = 6;
const IP_PROTO_UDP: u8 = 17;
/// DNS server port; DHCP client/server ports (link-local noise).
const PORT_DNS: u16 = 53;
const PORT_DHCP_SERVER: u16 = 67;
const PORT_DHCP_CLIENT: u16 = 68;

/// Parse and validate the global header, returning the byte order and the
/// remaining bytes. `None` for a header that is short, has an unknown magic, or
/// declares a non-Ethernet link layer.
fn parse_pcap_global_header(raw: &[u8]) -> Option<(ByteOrder, &[u8])> {
    let header = raw.get(..PCAP_GLOBAL_HEADER_LEN)?;
    let magic: [u8; 4] = header.get(0..4)?.try_into().ok()?;
    // 0xa1b2c3d4 microsecond / 0xa1b23c4d nanosecond captures, either endian.
    let byte_order = match magic {
        [0xa1, 0xb2, 0xc3, 0xd4] | [0xa1, 0xb2, 0x3c, 0x4d] => ByteOrder::Big,
        [0xd4, 0xc3, 0xb2, 0xa1] | [0x4d, 0x3c, 0xb2, 0xa1] => ByteOrder::Little,
        _ => return None,
    };
    let linktype = byte_order.u32(header.get(20..24)?.try_into().ok()?);
    if linktype != DLT_ETHERNET {
        return None;
    }
    Some((byte_order, raw.get(PCAP_GLOBAL_HEADER_LEN..)?))
}

/// Split the next captured frame off the record stream, returning its bytes and
/// the tail. `None` at a truncated record header or a record that claims more
/// bytes than remain — truncation ends the walk, it never panics.
fn next_packet(rest: &[u8], order: ByteOrder) -> Option<(&[u8], &[u8])> {
    let header = rest.get(..PCAP_RECORD_HEADER_LEN)?;
    let incl_len = order.u32(header.get(8..12)?.try_into().ok()?) as usize;
    let start = PCAP_RECORD_HEADER_LEN;
    let end = start.checked_add(incl_len)?;
    let packet = rest.get(start..end)?;
    Some((packet, rest.get(end..)?))
}

/// A parsed guest-originated flow tuple plus any DNS name it carried.
struct Flow {
    dst_ip: std::net::Ipv4Addr,
    dst_port: u16,
    proto: &'static str,
    dns_name: Option<String>,
}

/// Accumulate flow tuples into deduplicated, packet-counted destinations, plus
/// the set of DNS names observed. Excludes non-egress before counting.
struct FlowAccumulator {
    local: GatewayLocal,
    counts: std::collections::BTreeMap<(std::net::Ipv4Addr, u16, &'static str), u64>,
    dns: std::collections::BTreeSet<String>,
}

impl FlowAccumulator {
    const fn new(local: GatewayLocal) -> Self {
        Self {
            local,
            counts: std::collections::BTreeMap::new(),
            dns: std::collections::BTreeSet::new(),
        }
    }

    /// Fold one captured frame in, ignoring everything that isn't
    /// guest-originated egress (short frames, non-IPv4, gateway-local peers,
    /// DHCP noise).
    fn observe(&mut self, frame: &[u8]) {
        let Some(flow) = parse_guest_flow(frame, self.local) else {
            return;
        };
        if let Some(name) = flow.dns_name {
            self.dns.insert(name);
        }
        *self
            .counts
            .entry((flow.dst_ip, flow.dst_port, flow.proto))
            .or_insert(0) += 1;
    }

    fn into_witness(self, tap: String, capture_complete: bool) -> Witness {
        let destinations = self
            .counts
            .into_iter()
            .map(|((ip, port, proto), packets)| Destination {
                ip: ip.to_string(),
                port,
                proto: proto.to_owned(),
                packets,
            })
            .collect();
        Witness {
            schema_version: WITNESS_SCHEMA_VERSION,
            tap,
            capture_complete,
            destinations,
            dns_queries: self.dns.into_iter().collect(),
        }
    }
}

/// Extract the egress flow from an Ethernet frame, or `None` when it is not
/// egress. Filters, in order: non-IPv4 ethertype (ARP, IPv6), a gateway-local
/// destination (the /30 peer or the guest itself), non-TCP/UDP, and DHCP
/// client/server ports.
///
/// Egress is keyed on the *destination*, never on a trusted source. A
/// root-compromised guest can forge the IPv4 source address, but the packet
/// still physically leaves the tap toward the destination it wants to reach —
/// so keying on `src_ip == guest` would let the guest suppress its own egress
/// from the summary by spoofing the source. The destination it contacts is
/// what leaves the room, and it's what the raw `witness.pcap` records; the
/// summary keys on the same thing to keep the same suppression-resistance.
/// Return traffic (destined to the guest) is excluded by the local-destination
/// check below, which is why source is not needed to tell direction here.
fn parse_guest_flow(frame: &[u8], local: GatewayLocal) -> Option<Flow> {
    let ethertype = u16::from_be_bytes(frame.get(12..14)?.try_into().ok()?);
    if ethertype != ETHERTYPE_IPV4 {
        return None;
    }
    let ip = frame.get(ETHERNET_HEADER_LEN..)?;
    let (_src_ip, dst_ip, proto, l4) = parse_ipv4(ip)?;
    if dst_ip == local.gateway || dst_ip == local.guest {
        return None;
    }
    let (dst_port, payload, proto_str) = parse_l4(proto, l4)?;
    if dst_port == PORT_DHCP_SERVER || dst_port == PORT_DHCP_CLIENT {
        return None;
    }
    let dns_name = (dst_port == PORT_DNS)
        .then(|| parse_dns_qname(payload))
        .flatten();
    Some(Flow {
        dst_ip,
        dst_port,
        proto: proto_str,
        dns_name,
    })
}

/// Parse an IPv4 packet into `(src, dst, protocol, l4_payload)`. Honors IHL for
/// the header length; `None` on a short header, a non-IPv4 version, or an IHL
/// that runs past the buffer.
fn parse_ipv4(ip: &[u8]) -> Option<(std::net::Ipv4Addr, std::net::Ipv4Addr, u8, &[u8])> {
    let version_ihl = ip.first()?;
    if version_ihl >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(version_ihl & 0x0f).checked_mul(4)?;
    if ihl < 20 {
        return None;
    }
    let proto = *ip.get(9)?;
    let src = octets(ip.get(12..16)?)?;
    let dst = octets(ip.get(16..20)?)?;
    let l4 = ip.get(ihl..)?;
    Some((src, dst, proto, l4))
}

/// Parse a TCP/UDP header into `(dst_port, payload, proto_str)`. `None` for a
/// protocol we don't summarize or a header shorter than its port fields.
fn parse_l4(proto: u8, l4: &[u8]) -> Option<(u16, &[u8], &'static str)> {
    match proto {
        IP_PROTO_TCP => {
            let dst_port = u16::from_be_bytes(l4.get(2..4)?.try_into().ok()?);
            let data_offset = usize::from(l4.get(12)? >> 4).checked_mul(4)?;
            let payload = l4.get(data_offset..).unwrap_or(&[]);
            Some((dst_port, payload, "tcp"))
        }
        IP_PROTO_UDP => {
            let dst_port = u16::from_be_bytes(l4.get(2..4)?.try_into().ok()?);
            let payload = l4.get(8..).unwrap_or(&[]);
            Some((dst_port, payload, "udp"))
        }
        _ => None,
    }
}

/// Build an `Ipv4Addr` from a 4-byte slice.
fn octets(bytes: &[u8]) -> Option<std::net::Ipv4Addr> {
    let o: [u8; 4] = bytes.try_into().ok()?;
    Some(std::net::Ipv4Addr::from(o))
}

/// Extract the first question name from a DNS message payload (best-effort).
///
/// Reads the QNAME label sequence after the 12-byte header; returns the
/// dotted name (e.g. `example.com`). `None` on a short payload, a zero-question
/// message, or a compression pointer in the question (queries don't use them).
/// Stays total: a malformed length byte ends the name rather than over-reading.
fn parse_dns_qname(payload: &[u8]) -> Option<String> {
    let qdcount = u16::from_be_bytes(payload.get(4..6)?.try_into().ok()?);
    if qdcount == 0 {
        return None;
    }
    let mut labels = Vec::new();
    let mut pos = 12usize;
    loop {
        let len = usize::from(*payload.get(pos)?);
        if len == 0 {
            break;
        }
        // A compression pointer (top two bits set) can't be resolved from the
        // question alone; treat the name as unparseable rather than guessing.
        if len & 0xc0 != 0 {
            return None;
        }
        pos = pos.checked_add(1)?;
        let label = payload.get(pos..pos.checked_add(len)?)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos = pos.checked_add(len)?;
    }
    if labels.is_empty() {
        return None;
    }
    Some(labels.join("."))
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
        parse_changeset_stream, ArtifactsError, Changeset, ResultJson, RunStatus, RunnerArtifacts,
        CHANGESET_SCHEMA_VERSION, RESULT_JSON, SCHEMA_VERSION, STDERR_LOG, STDOUT_LOG,
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
            pushed_branch: None,
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
    fn pushed_branch_round_trips_and_is_omitted_when_none() {
        let mut result = sample_result();
        result.pushed_branch = Some("feature/x".to_owned());
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(
            json.contains("\"pushed_branch\":\"feature/x\""),
            "pushed_branch should serialize when Some; got: {json}"
        );
        let parsed: ResultJson = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, parsed);
        // skip_serializing_if omits the field entirely when None.
        let omitted = serde_json::to_string(&sample_result()).expect("serialize");
        assert!(
            !omitted.contains("pushed_branch"),
            "pushed_branch should be omitted when None; got: {omitted}"
        );
    }

    #[test]
    fn status_from_exit_code_maps_zero_and_nonzero() {
        assert_eq!(ResultJson::status_from_exit_code(0), RunStatus::Succeeded);
        assert_eq!(ResultJson::status_from_exit_code(1), RunStatus::Failed);
    }

    #[test]
    fn parse_changeset_classifies_ops_and_sorts() {
        let raw = b"M\tworkspace/repo/b.rs\0A\tworkspace/repo/a.rs\0D\tworkspace/repo/old.rs\0A\tetc/hosts\0";
        let cs = parse_changeset_stream(raw);
        assert!(cs.overlay_active);
        assert_eq!(cs.added, vec!["etc/hosts", "workspace/repo/a.rs"]);
        assert_eq!(cs.modified, vec!["workspace/repo/b.rs"]);
        assert_eq!(cs.deleted, vec!["workspace/repo/old.rs"]);
    }

    #[test]
    fn parse_changeset_nooverlay_sentinel_is_inactive() {
        let cs = parse_changeset_stream(b"NOOVERLAY");
        assert!(!cs.overlay_active);
        assert!(cs.is_empty());
        assert_eq!(cs.schema_version, CHANGESET_SCHEMA_VERSION);
    }

    #[test]
    fn parse_changeset_skips_malformed_records() {
        // no tab, unknown op, empty path, empty record — each skipped, the one
        // valid record survives.
        let raw = b"Xnotab\0Z\tunknown-op\0A\t\0\0M\tworkspace/keep\0";
        let cs = parse_changeset_stream(raw);
        assert_eq!(cs.modified, vec!["workspace/keep"]);
        assert!(cs.added.is_empty());
        assert!(cs.deleted.is_empty());
    }

    #[test]
    fn lane_escapes_exclude_workspace_and_ephemeral_churn() {
        let cs = Changeset {
            schema_version: CHANGESET_SCHEMA_VERSION,
            overlay_active: true,
            added: vec![
                "workspace/repo/a.rs".to_owned(),
                "root/.bashrc".to_owned(),
                "var/log/dmesg".to_owned(),
            ],
            modified: vec!["etc/hosts".to_owned()],
            deleted: vec!["workspace/repo/old.rs".to_owned(), "run/lock".to_owned()],
        };
        let mut escapes = cs.lane_escapes();
        escapes.sort_unstable();
        // /var/log + /run are expected OS churn -> filtered; /etc + /root persist -> flagged.
        assert_eq!(escapes, vec!["etc/hosts", "root/.bashrc"]);
        assert_eq!(cs.ephemeral_count(), 2); // var/log/dmesg + run/lock
                                             // under /workspace: a.rs added, old.rs deleted, nothing modified.
        assert_eq!(cs.workspace_counts(), (1, 0, 1));
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

    mod witness_summary {
        #![allow(
            clippy::indexing_slicing,
            reason = "test module: indexing a just-asserted-nonempty vec is clear"
        )]

        use std::net::Ipv4Addr;

        use super::super::{summarize_pcap, GatewayLocal, Witness, WITNESS_SCHEMA_VERSION};

        /// The slot-1 /30: gateway `.5`, guest `.6` — the addresses a witness
        /// treats as gateway-local and excludes from the egress summary.
        fn local() -> GatewayLocal {
            GatewayLocal {
                gateway: Ipv4Addr::new(172, 16, 0, 5),
                guest: Ipv4Addr::new(172, 16, 0, 6),
            }
        }

        /// A classic little-endian libpcap global header declaring Ethernet.
        fn pcap_header() -> Vec<u8> {
            let mut h = Vec::new();
            h.extend_from_slice(&[0xd4, 0xc3, 0xb2, 0xa1]); // magic (LE, microsecond)
            h.extend_from_slice(&2u16.to_le_bytes()); // version major
            h.extend_from_slice(&4u16.to_le_bytes()); // version minor
            h.extend_from_slice(&0i32.to_le_bytes()); // thiszone
            h.extend_from_slice(&0u32.to_le_bytes()); // sigfigs
            h.extend_from_slice(&65535u32.to_le_bytes()); // snaplen
            h.extend_from_slice(&1u32.to_le_bytes()); // linktype = DLT_ETHERNET
            h
        }

        /// Wrap one Ethernet frame in a pcap record header (LE).
        fn record(frame: &[u8]) -> Vec<u8> {
            let mut r = Vec::new();
            r.extend_from_slice(&0u32.to_le_bytes()); // ts_sec
            r.extend_from_slice(&0u32.to_le_bytes()); // ts_usec
            let len = u32::try_from(frame.len()).expect("frame len fits u32");
            r.extend_from_slice(&len.to_le_bytes()); // incl_len
            r.extend_from_slice(&len.to_le_bytes()); // orig_len
            r.extend_from_slice(frame);
            r
        }

        /// Build an Ethernet + IPv4 + (TCP|UDP) frame from `src` to `dst:port`,
        /// carrying `payload` as the L4 body. `proto` is 6 (TCP) or 17 (UDP).
        fn frame(
            src: Ipv4Addr,
            dst: Ipv4Addr,
            proto: u8,
            dst_port: u16,
            payload: &[u8],
        ) -> Vec<u8> {
            let mut eth = Vec::new();
            eth.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]); // dst MAC
            eth.extend_from_slice(&[0x02, 0, 0, 0, 0, 2]); // src MAC
            eth.extend_from_slice(&0x0800u16.to_be_bytes()); // ethertype IPv4

            let l4 = l4_header(proto, dst_port, payload);
            let total = 20 + l4.len();
            let mut ip = Vec::new();
            ip.push(0x45); // version 4, IHL 5
            ip.push(0); // DSCP/ECN
            ip.extend_from_slice(&u16::try_from(total).unwrap_or(0).to_be_bytes());
            ip.extend_from_slice(&0u16.to_be_bytes()); // id
            ip.extend_from_slice(&0u16.to_be_bytes()); // flags/frag
            ip.push(64); // ttl
            ip.push(proto);
            ip.extend_from_slice(&0u16.to_be_bytes()); // checksum (unchecked)
            ip.extend_from_slice(&src.octets());
            ip.extend_from_slice(&dst.octets());
            ip.extend_from_slice(&l4);

            eth.extend_from_slice(&ip);
            eth
        }

        #[allow(
            clippy::branches_sharing_code,
            reason = "the trailing checksum happens to coincide, but the TCP/UDP header shapes differ; hoisting it obscures the layouts"
        )]
        fn l4_header(proto: u8, dst_port: u16, payload: &[u8]) -> Vec<u8> {
            let mut l4 = Vec::new();
            l4.extend_from_slice(&40000u16.to_be_bytes()); // src port
            l4.extend_from_slice(&dst_port.to_be_bytes()); // dst port
            if proto == 6 {
                l4.extend_from_slice(&0u32.to_be_bytes()); // seq
                l4.extend_from_slice(&0u32.to_be_bytes()); // ack
                l4.push(0x50); // data offset 5 words, no flags-nibble
                l4.push(0); // flags
                l4.extend_from_slice(&0u16.to_be_bytes()); // window
                l4.extend_from_slice(&0u16.to_be_bytes()); // checksum
                l4.extend_from_slice(&0u16.to_be_bytes()); // urgent
            } else {
                let ulen = u16::try_from(8 + payload.len()).unwrap_or(8);
                l4.extend_from_slice(&ulen.to_be_bytes()); // length
                l4.extend_from_slice(&0u16.to_be_bytes()); // checksum
            }
            l4.extend_from_slice(payload);
            l4
        }

        /// A minimal DNS query payload asking for `example.com` (one question).
        fn dns_query_example_com() -> Vec<u8> {
            let mut p = Vec::new();
            p.extend_from_slice(&0x1234u16.to_be_bytes()); // id
            p.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: standard query
            p.extend_from_slice(&1u16.to_be_bytes()); // qdcount = 1
            p.extend_from_slice(&0u16.to_be_bytes()); // ancount
            p.extend_from_slice(&0u16.to_be_bytes()); // nscount
            p.extend_from_slice(&0u16.to_be_bytes()); // arcount
            for label in ["example", "com"] {
                p.push(u8::try_from(label.len()).expect("label len"));
                p.extend_from_slice(label.as_bytes());
            }
            p.push(0); // root label
            p.extend_from_slice(&1u16.to_be_bytes()); // qtype A
            p.extend_from_slice(&1u16.to_be_bytes()); // qclass IN
            p
        }

        #[test]
        fn empty_capture_yields_empty_witness() {
            // Header only, no records: a valid-but-silent capture.
            let w = summarize_pcap(&pcap_header(), "tap-fc1", local(), true);
            assert_eq!(w.schema_version, WITNESS_SCHEMA_VERSION);
            assert_eq!(w.tap, "tap-fc1");
            assert!(w.capture_complete);
            assert!(w.destinations.is_empty());
            assert!(w.dns_queries.is_empty());
        }

        #[test]
        fn garbage_or_missing_header_is_empty_not_a_panic() {
            // Too short for a header, and a bad magic: both total-parse to empty.
            let short = summarize_pcap(b"\xd4\xc3", "tap-fc1", local(), false);
            assert_eq!(short, Witness::empty("tap-fc1".to_owned(), false));
            let bad_magic = summarize_pcap(&[0u8; 24], "tap-fc1", local(), true);
            assert!(bad_magic.destinations.is_empty());
        }

        #[test]
        fn counts_guest_egress_and_dedups_by_tuple() {
            let dst = Ipv4Addr::new(93, 184, 216, 34);
            let mut pcap = pcap_header();
            // Two packets to the same tcp:80 tuple → one destination, count 2.
            pcap.extend(record(&frame(local().guest, dst, 6, 80, b"")));
            pcap.extend(record(&frame(local().guest, dst, 6, 80, b"")));
            // One packet to a different port → a second destination.
            pcap.extend(record(&frame(local().guest, dst, 6, 443, b"")));

            let w = summarize_pcap(&pcap, "tap-fc1", local(), true);
            assert_eq!(w.destinations.len(), 2, "deduped by (ip,port,proto)");
            let http = w
                .destinations
                .iter()
                .find(|d| d.port == 80)
                .expect("port 80 destination present");
            assert_eq!(http.ip, "93.184.216.34");
            assert_eq!(http.proto, "tcp");
            assert_eq!(http.packets, 2);
        }

        #[test]
        fn extracts_dns_query_names() {
            let resolver = Ipv4Addr::new(1, 1, 1, 1);
            let mut pcap = pcap_header();
            pcap.extend(record(&frame(
                local().guest,
                resolver,
                17,
                53,
                &dns_query_example_com(),
            )));
            let w = summarize_pcap(&pcap, "tap-fc1", local(), true);
            assert_eq!(w.dns_queries, vec!["example.com".to_owned()]);
            // The DNS packet is still egress: the resolver is a destination.
            assert!(w
                .destinations
                .iter()
                .any(|d| d.ip == "1.1.1.1" && d.port == 53));
        }

        #[test]
        fn excludes_gateway_local_and_dhcp_noise() {
            let mut pcap = pcap_header();
            // To the gateway (the /30 host peer): not egress.
            pcap.extend(record(&frame(local().guest, local().gateway, 6, 22, b"")));
            // DHCP discover to the broadcast server port: link-local noise.
            pcap.extend(record(&frame(
                local().guest,
                Ipv4Addr::BROADCAST,
                17,
                67,
                b"",
            )));
            // A reply *from* the gateway toward the guest: not guest-originated.
            pcap.extend(record(&frame(
                local().gateway,
                local().guest,
                6,
                40000,
                b"",
            )));
            let w = summarize_pcap(&pcap, "tap-fc1", local(), true);
            assert!(
                w.destinations.is_empty(),
                "gateway-local, DHCP, and inbound frames are all excluded: {:?}",
                w.destinations
            );
        }

        #[test]
        fn truncated_record_ends_the_walk_keeping_prior_flows() {
            let dst = Ipv4Addr::new(93, 184, 216, 34);
            let mut pcap = pcap_header();
            pcap.extend(record(&frame(local().guest, dst, 6, 80, b"")));
            // A record header claiming more bytes than remain (truncated capture).
            pcap.extend_from_slice(&0u32.to_le_bytes()); // ts_sec
            pcap.extend_from_slice(&0u32.to_le_bytes()); // ts_usec
            pcap.extend_from_slice(&9999u32.to_le_bytes()); // incl_len (lies)
            pcap.extend_from_slice(&9999u32.to_le_bytes()); // orig_len
            pcap.extend_from_slice(b"\x01\x02\x03"); // only a few bytes follow

            // capture_complete reflects the caller's outcome (truncation is
            // visible), and the flow captured before the tear survives.
            let w = summarize_pcap(&pcap, "tap-fc1", local(), false);
            assert!(!w.capture_complete, "truncation must be visible");
            assert_eq!(w.destinations.len(), 1);
            assert_eq!(w.destinations[0].port, 80);
        }

        #[test]
        fn spoofed_source_egress_is_still_counted() {
            // The threat model is a root-compromised guest. It can forge the
            // IPv4 source address, but the packet still leaves the tap toward
            // its real destination and lands in witness.pcap. The summary keys
            // on the destination, not a trusted source, so it can't be
            // suppressed by spoofing — matching the raw capture's guarantee.
            let dst = Ipv4Addr::new(203, 0, 113, 9);
            let spoofed_src = Ipv4Addr::new(10, 0, 0, 99); // not local().guest
            let mut pcap = pcap_header();
            pcap.extend(record(&frame(spoofed_src, dst, 6, 443, b"")));

            let w = summarize_pcap(&pcap, "tap-fc1", local(), true);
            assert!(
                w.destinations
                    .iter()
                    .any(|d| d.ip == "203.0.113.9" && d.port == 443),
                "egress with a spoofed source must still be recorded: {:?}",
                w.destinations
            );
        }

        #[test]
        fn non_ethernet_linktype_is_not_misparsed() {
            // Flip the linktype to raw IP (101): the parser refuses rather than
            // reading IP bytes as Ethernet frames.
            let mut header = pcap_header();
            header.truncate(20);
            header.extend_from_slice(&101u32.to_le_bytes());
            let w = summarize_pcap(&header, "tap-fc1", local(), true);
            assert!(w.destinations.is_empty());
        }
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
