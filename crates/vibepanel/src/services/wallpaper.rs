//! Wallpaper detection and Material You color extraction.
//!
//! Handles IPC with wallpaper daemons (hyprpaper, awww/swww, wpaperd, waypaper)
//! and extracts a `material_colors::theme::Theme` from a wallpaper image for
//! use by the theming system in `vibepanel-core`.
//!
//! ## hyprpaper version support
//!
//! | hyprpaper version | Detection method | Status |
//! |-------------------|-----------------|--------|
//! | < 0.8.0 | Legacy text IPC | Supported |
//! | 0.8.0 – 0.8.3 | Neither (regression in released builds) | Soft-fail: hyprpaper detection unavailable (cascade continues) |
//! | main / >= 0.8.4 (unreleased) | hyprwire binary IPC (`hyprpaper_core@2`) | Supported, best-effort |
//!
//! The hyprwire path targets the `hyprpaper_core@2` protocol introduced in
//! <https://github.com/hyprwm/hyprpaper/pull/313>. This protocol is available
//! in hyprpaper built from `main` and is expected to ship in a future tagged
//! release. Until then, support is best-effort: if upstream changes the wire
//! contract before a stable release, detection will gracefully fall back to
//! returning `None` (no wallpaper detected), without crashing or blocking.
//!
//! Dispatch is decided per-connection by inspecting the daemon's loaded
//! libraries via `SO_PEERCRED` + `/proc/<pid>/maps`: 0.8+ links
//! `libhyprtoolkit` and uses the hyprwire path; older builds use the legacy
//! text path. Best-effort: we avoid speaking the wrong protocol by
//! classifying the daemon before sending any protocol bytes (`SO_PEERCRED`
//! is read on the protocol socket itself before anything is written to it,
//! and that same socket is then handed to the chosen protocol path), since
//! 0.7.x crashes when a polling client repeatedly sends hyprwire frames.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Duration;

use material_colors::color::Argb;
use material_colors::hct::Cam16;
use material_colors::quantize::{Quantizer, QuantizerCelebi};
use material_colors::score::Score;
use material_colors::theme::ThemeBuilder;
use tracing::{debug, warn};
use vibepanel_core::{expand_tilde, theme::relative_luminance};

/// Reject wallpaper images larger than this to avoid excessive memory use.
const MAX_WALLPAPER_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB

const HW_SUP: u8 = 0x01;
const HW_HANDSHAKE_BEGIN: u8 = 0x02;
const HW_HANDSHAKE_ACK: u8 = 0x03;
const HW_HANDSHAKE_PROTOCOLS: u8 = 0x04;
const HW_BIND_PROTOCOL: u8 = 0x0A;
const HW_NEW_OBJECT: u8 = 0x0B;
const HW_FATAL_PROTOCOL_ERROR: u8 = 0x0C;
const HW_ROUNDTRIP_REQUEST: u8 = 0x0D;
const HW_ROUNDTRIP_DONE: u8 = 0x0E;
const HW_GENERIC_PROTOCOL_MESSAGE: u8 = 0x64;

const HW_END: u8 = 0x00;
const HW_UINT: u8 = 0x10;
const HW_SEQ: u8 = 0x13;
const HW_VARCHAR: u8 = 0x20;
const HW_ARRAY: u8 = 0x21;
const HW_OBJECT: u8 = 0x22;

const HYPRWIRE_VERSION: u32 = 1;
const HYPRPAPER_PROTOCOL: &str = "hyprpaper_core";
const HYPRPAPER_STATUS_VERSION: u32 = 2;
const HYPRPAPER_CORE_GET_STATUS_OBJECT: u32 = 2;
const HYPRPAPER_STATUS_ACTIVE_WALLPAPER: u32 = 0;

// Allocation caps for hyprwire payloads. The decoded varint already rejects
// values outside u32 range, but u32-sized lengths still trivially exceed any
// realistic hyprpaper payload and would drive OOM aborts on a desynced peer
// rather than a clean fallback through `detect_wallpaper`. These bounds are
// deliberately generous (~16x PATH_MAX for strings, ~256x realistic monitor
// count for arrays) so legitimate traffic is never rejected.
const HW_MAX_VARCHAR_LEN: usize = 64 * 1024;
const HW_MAX_ARRAY_COUNT: usize = 4096;

#[derive(Debug)]
enum HyprwireValue {
    Uint(u32),
    Seq(u32),
    Varchar(String),
    ArrayUint(Vec<u32>),
    ArrayVarchar(Vec<String>),
    Object(u32),
}

#[derive(Debug)]
struct HyprwireMessage {
    code: u8,
    args: Vec<HyprwireValue>,
}

struct HyprwireClient {
    stream: UnixStream,
    next_seq: u32,
}

impl HyprwireClient {
    fn from_stream(stream: UnixStream) -> Result<Self, String> {
        // Tight handshake budget; callers widen it for the snapshot phase.
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            stream,
            next_seq: 0,
        })
    }

    fn set_read_timeout(&self, duration: Duration) -> Result<(), String> {
        self.stream
            .set_read_timeout(Some(duration))
            .map_err(|e| e.to_string())
    }

    fn next_sequence(&mut self) -> u32 {
        self.next_seq += 1;
        self.next_seq
    }

    fn perform_handshake(&mut self) -> Result<Vec<(String, u32)>, String> {
        self.send_message(HW_SUP, &[HyprwireValue::Varchar("VAX".to_string())])?;

        let begin = self.read_message()?;
        if begin.code != HW_HANDSHAKE_BEGIN {
            return Err(format!("expected handshake-begin, got {:#04x}", begin.code));
        }

        let Some(HyprwireValue::ArrayUint(versions)) = begin.args.first() else {
            return Err("invalid handshake-begin payload".to_string());
        };
        if !versions.contains(&HYPRWIRE_VERSION) {
            return Err(format!(
                "server does not support hyprwire v{}",
                HYPRWIRE_VERSION
            ));
        }

        self.send_message(HW_HANDSHAKE_ACK, &[HyprwireValue::Uint(HYPRWIRE_VERSION)])?;

        let protocols = self.read_message()?;
        if protocols.code != HW_HANDSHAKE_PROTOCOLS {
            return Err(format!(
                "expected handshake-protocols, got {:#04x}",
                protocols.code
            ));
        }

        let Some(HyprwireValue::ArrayVarchar(entries)) = protocols.args.first() else {
            return Err("invalid handshake-protocols payload".to_string());
        };

        Ok(entries
            .iter()
            .filter_map(|entry| {
                let (spec, version) = entry.split_once('@')?;
                let version = version.parse().ok()?;
                Some((spec.to_string(), version))
            })
            .collect())
    }

    fn bind_protocol(&mut self, spec: &str, version: u32) -> Result<u32, String> {
        let seq = self.next_sequence();
        self.send_message(
            HW_BIND_PROTOCOL,
            &[
                HyprwireValue::Uint(seq),
                HyprwireValue::Varchar(spec.to_string()),
                HyprwireValue::Uint(version),
            ],
        )?;
        self.await_new_object(seq)
    }

    fn create_status_object(&mut self, manager_id: u32) -> Result<u32, String> {
        let seq = self.next_sequence();
        self.send_message(
            HW_GENERIC_PROTOCOL_MESSAGE,
            &[
                HyprwireValue::Object(manager_id),
                HyprwireValue::Uint(HYPRPAPER_CORE_GET_STATUS_OBJECT),
                HyprwireValue::Seq(seq),
            ],
        )?;
        self.await_new_object(seq)
    }

    /// Block until the server replies with a `new-object` message correlated
    /// to `expected_seq`, returning the newly allocated object id.
    ///
    /// Unrelated messages are discarded. A fatal protocol error terminates
    /// the wait with the server-provided description.
    fn await_new_object(&mut self, expected_seq: u32) -> Result<u32, String> {
        loop {
            let msg = self.read_message()?;
            match msg.code {
                HW_NEW_OBJECT => {
                    let Some((id, returned_seq)) = parse_new_object(&msg) else {
                        return Err("invalid new-object payload".to_string());
                    };
                    if returned_seq == expected_seq {
                        return Ok(id);
                    }
                }
                HW_FATAL_PROTOCOL_ERROR => {
                    return Err(format_fatal_protocol_error(&msg));
                }
                _ => {}
            }
        }
    }

    fn send_roundtrip_request(&mut self) -> Result<u32, String> {
        let seq = self.next_sequence();
        self.send_message(HW_ROUNDTRIP_REQUEST, &[HyprwireValue::Uint(seq)])?;
        Ok(seq)
    }

    fn send_message(&mut self, code: u8, args: &[HyprwireValue]) -> Result<(), String> {
        let mut buffer = vec![code];
        for arg in args {
            encode_value(&mut buffer, arg);
        }
        buffer.push(HW_END);

        self.stream.write_all(&buffer).map_err(|e| e.to_string())?;
        self.stream.flush().map_err(|e| e.to_string())
    }

    fn read_message(&mut self) -> Result<HyprwireMessage, String> {
        let mut code = [0u8; 1];
        self.stream
            .read_exact(&mut code)
            .map_err(|e| e.to_string())?;

        let mut args = Vec::new();
        loop {
            let magic = self.read_byte()?;
            if magic == HW_END {
                break;
            }
            args.push(self.read_value(magic)?);
        }

        Ok(HyprwireMessage {
            code: code[0],
            args,
        })
    }

    fn read_value(&mut self, magic: u8) -> Result<HyprwireValue, String> {
        match magic {
            HW_UINT => Ok(HyprwireValue::Uint(self.read_u32()?)),
            HW_SEQ => Ok(HyprwireValue::Seq(self.read_u32()?)),
            HW_OBJECT => Ok(HyprwireValue::Object(self.read_u32()?)),
            HW_VARCHAR => {
                let len = self.read_varint()?;
                if len > HW_MAX_VARCHAR_LEN {
                    return Err(format!(
                        "hyprwire varchar length {len} exceeds cap {HW_MAX_VARCHAR_LEN}"
                    ));
                }
                let mut data = vec![0u8; len];
                self.stream
                    .read_exact(&mut data)
                    .map_err(|e| e.to_string())?;
                let text = String::from_utf8(data).map_err(|e| e.to_string())?;
                Ok(HyprwireValue::Varchar(text))
            }
            HW_ARRAY => self.read_array(),
            _ => Err(format!("unsupported hyprwire magic byte {:#04x}", magic)),
        }
    }

    fn read_array(&mut self) -> Result<HyprwireValue, String> {
        let item_type = self.read_byte()?;
        let count = self.read_varint()?;
        if count > HW_MAX_ARRAY_COUNT {
            return Err(format!(
                "hyprwire array count {count} exceeds cap {HW_MAX_ARRAY_COUNT}"
            ));
        }

        match item_type {
            HW_UINT => {
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(self.read_u32()?);
                }
                Ok(HyprwireValue::ArrayUint(items))
            }
            HW_VARCHAR => {
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    let len = self.read_varint()?;
                    if len > HW_MAX_VARCHAR_LEN {
                        return Err(format!(
                            "hyprwire varchar length {len} exceeds cap {HW_MAX_VARCHAR_LEN}"
                        ));
                    }
                    let mut data = vec![0u8; len];
                    self.stream
                        .read_exact(&mut data)
                        .map_err(|e| e.to_string())?;
                    items.push(String::from_utf8(data).map_err(|e| e.to_string())?);
                }
                Ok(HyprwireValue::ArrayVarchar(items))
            }
            _ => Err(format!(
                "unsupported hyprwire array item type {:#04x}",
                item_type
            )),
        }
    }

    fn read_byte(&mut self) -> Result<u8, String> {
        let mut byte = [0u8; 1];
        self.stream
            .read_exact(&mut byte)
            .map_err(|e| e.to_string())?;
        Ok(byte[0])
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let mut bytes = [0u8; 4];
        self.stream
            .read_exact(&mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_varint(&mut self) -> Result<usize, String> {
        let mut value = 0usize;
        let mut shift = 0usize;

        loop {
            let byte = self.read_byte()?;
            // On the 5th byte (shift == 28) only the low 4 payload bits are
            // valid for a u32-range varint; anything else would decode beyond
            // u32::MAX and drive pathological allocations downstream.
            if shift == 28 && byte & 0x70 != 0 {
                return Err("hyprwire varint exceeds u32 range".to_string());
            }
            value |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 28 {
                return Err("hyprwire varint too large".to_string());
            }
        }
    }
}

fn encode_value(buffer: &mut Vec<u8>, value: &HyprwireValue) {
    match value {
        HyprwireValue::Uint(v) => {
            buffer.push(HW_UINT);
            buffer.extend_from_slice(&v.to_le_bytes());
        }
        HyprwireValue::Seq(v) => {
            buffer.push(HW_SEQ);
            buffer.extend_from_slice(&v.to_le_bytes());
        }
        HyprwireValue::Object(v) => {
            buffer.push(HW_OBJECT);
            buffer.extend_from_slice(&v.to_le_bytes());
        }
        HyprwireValue::Varchar(v) => {
            buffer.push(HW_VARCHAR);
            buffer.extend_from_slice(&encode_varint(v.len()));
            buffer.extend_from_slice(v.as_bytes());
        }
        HyprwireValue::ArrayUint(values) => {
            buffer.push(HW_ARRAY);
            buffer.push(HW_UINT);
            buffer.extend_from_slice(&encode_varint(values.len()));
            for value in values {
                buffer.extend_from_slice(&value.to_le_bytes());
            }
        }
        HyprwireValue::ArrayVarchar(values) => {
            buffer.push(HW_ARRAY);
            buffer.push(HW_VARCHAR);
            buffer.extend_from_slice(&encode_varint(values.len()));
            for value in values {
                buffer.extend_from_slice(&encode_varint(value.len()));
                buffer.extend_from_slice(value.as_bytes());
            }
        }
    }
}

fn encode_varint(mut value: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

fn parse_new_object(message: &HyprwireMessage) -> Option<(u32, u32)> {
    match message.args.as_slice() {
        [HyprwireValue::Uint(id), HyprwireValue::Uint(seq)] => Some((*id, *seq)),
        _ => None,
    }
}

fn format_fatal_protocol_error(message: &HyprwireMessage) -> String {
    match message.args.as_slice() {
        [
            HyprwireValue::Uint(object_id),
            HyprwireValue::Uint(error_id),
            HyprwireValue::Varchar(text),
        ] => {
            format!(
                "fatal protocol error on object {} (error {}): {}",
                object_id, error_id, text
            )
        }
        _ => "fatal protocol error with invalid payload".to_string(),
    }
}

fn parse_active_wallpaper_event(
    message: &HyprwireMessage,
    status_id: u32,
) -> Option<(String, String)> {
    match message.args.as_slice() {
        [
            HyprwireValue::Object(object_id),
            HyprwireValue::Uint(method_id),
            HyprwireValue::Varchar(monitor),
            HyprwireValue::Varchar(path),
        ] if *object_id == status_id && *method_id == HYPRPAPER_STATUS_ACTIVE_WALLPAPER => {
            Some((monitor.clone(), path.clone()))
        }
        _ => None,
    }
}

fn select_monitor_wallpaper(entries: &[(String, String)], monitor: Option<&str>) -> Option<String> {
    if let Some(target) = monitor {
        if let Some((_, path)) = entries
            .iter()
            .find(|(name, path)| name == target && !path.is_empty())
        {
            debug!("Using wallpaper from target monitor '{}'", target);
            return Some(path.clone());
        }

        debug!(
            "Target monitor '{}' not found in hyprpaper, using first available",
            target
        );
    }

    entries
        .iter()
        .find_map(|(_, path)| (!path.is_empty()).then(|| path.clone()))
}

/// Returns true if the server advertises a `hyprpaper_core` version >=
/// [`HYPRPAPER_STATUS_VERSION`].
///
/// Binding always uses [`HYPRPAPER_STATUS_VERSION`] (v2) regardless of what the
/// server advertises. A newer advertised version may still honour a v2 bind if
/// the server maintains backward compatibility, avoiding breakage on upgrade. A
/// version bump here alone is not sufficient to support a changed event shape —
/// that requires code changes.
fn is_hyprpaper_status_protocol_available(protocols: &[(String, u32)]) -> bool {
    protocols
        .iter()
        .any(|(spec, version)| spec == HYPRPAPER_PROTOCOL && *version >= HYPRPAPER_STATUS_VERSION)
}

/// Collect active-wallpaper entries from a slice of hyprwire messages with
/// latest-wins dedup per monitor.
fn collect_wallpaper_entries(
    messages: &[HyprwireMessage],
    status_id: u32,
) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for msg in messages {
        if let Some((monitor, path)) = parse_active_wallpaper_event(msg, status_id) {
            if let Some(existing) = entries.iter_mut().find(|(m, _)| m == &monitor) {
                existing.1 = path;
            } else {
                entries.push((monitor, path));
            }
        }
    }
    entries
}

/// Resolve the hyprpaper IPC socket path.
///
/// Tries the instance-specific path (`hypr/$HYPRLAND_INSTANCE_SIGNATURE/.hyprpaper.sock`)
/// first, then falls back to the legacy path (`hypr/.hyprpaper.sock`).
fn resolve_hyprpaper_socket_path() -> Option<String> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;

    // Instance-specific path (Hyprland 0.40+), fall back to legacy
    let socket_path = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .ok()
        .map(|sig| format!("{}/hypr/{}/.hyprpaper.sock", runtime_dir, sig))
        .filter(|p| std::path::Path::new(p).exists())
        .unwrap_or_else(|| format!("{}/hypr/.hyprpaper.sock", runtime_dir));

    Some(socket_path)
}

/// Discriminate hyprpaper 0.8+ (hyprwire) from < 0.8 (legacy text) without
/// sending any bytes to the daemon.
///
/// Empirical findings on a live 0.7.6 daemon:
/// - Sending the hyprwire SUP framing (`01 20 03 'V' 'A' 'X' 00`) once is
///   handled (replies "invalid command"), but a second SUP from the same
///   client crashes the daemon. So we cannot use any byte-level probe.
/// - Sending legacy `listactive` to a 0.8 hyprwire server makes its binary
///   parser log "malformed message" per byte every poll cycle.
///
/// Instead, use `SO_PEERCRED` to discover the daemon's pid and inspect
/// `/proc/<pid>/maps` for a `libhyprtoolkit` mapping (0.8.0+ is a complete
/// rewrite onto hyprtoolkit; 0.7.x does not link it).
fn hyprpaper_uses_hyprwire(stream: &UnixStream) -> Option<bool> {
    use std::os::fd::AsRawFd;

    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        debug!("hyprpaper: SO_PEERCRED failed: {err}; skipping classification");
        return None;
    }
    let pid = cred.pid;

    let maps_path = format!("/proc/{pid}/maps");
    let maps = match std::fs::read_to_string(&maps_path) {
        Ok(m) => m,
        Err(e) => {
            debug!("hyprpaper: cannot read {maps_path}: {e}; skipping classification");
            return None;
        }
    };
    Some(maps_indicate_hyprwire(&maps))
}

/// Returns `true` if `maps` (contents of `/proc/<pid>/maps`) contains a
/// `libhyprtoolkit` mapping, which indicates hyprpaper 0.8+ (hyprwire
/// protocol). This is a best-effort heuristic — a false positive would route
/// hyprwire bytes to a 0.7.x daemon and crash it. The match is on the mapped
/// library basename to avoid matching unrelated paths that happen to contain
/// "libhyprtoolkit" as a directory or binary name component.
fn maps_indicate_hyprwire(maps: &str) -> bool {
    maps.lines().any(|line| {
        line.rsplit_once('/')
            .map(|(_, name)| name.starts_with("libhyprtoolkit.so"))
            .unwrap_or(false)
    })
}

/// Detect the current wallpaper path from hyprpaper via its IPC socket.
///
/// Dispatches to either the hyprwire binary protocol (0.8+) or the legacy
/// text protocol (< 0.8) based on `hyprpaper_uses_hyprwire`. Sending the
/// wrong probe to a given version either crashes 0.7 or log-spams 0.8, so
/// we never fall back from one to the other.
///
/// If `monitor` is provided, returns that monitor's wallpaper. Falls back to the
/// first listed monitor if the target isn't found (e.g. unplugged, name mismatch).
fn detect_hyprpaper_wallpaper(monitor: Option<&str>) -> Option<String> {
    let socket_path = resolve_hyprpaper_socket_path()?;
    let stream = UnixStream::connect(&socket_path).ok()?;
    let uses_hyprwire = hyprpaper_uses_hyprwire(&stream)?;

    if uses_hyprwire {
        detect_hyprpaper_hyprwire(stream, monitor)
    } else {
        detect_hyprpaper_legacy(stream, monitor)
    }
}

/// Detect the wallpaper via the legacy text-based IPC protocol (hyprpaper < 0.8.0).
///
/// Sends the `listactive` command and parses lines of `MONITOR = /path/to/image`.
/// Only called after `hyprpaper_uses_hyprwire` confirms a legacy daemon, so no
/// fast-fail against a hyprwire server is needed.
fn detect_hyprpaper_legacy(stream: UnixStream, monitor: Option<&str>) -> Option<String> {
    let mut stream = stream;
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .ok();
    stream.write_all(b"listactive").ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;

    parse_hyprpaper_active_response(&response, monitor)
}

/// Parse a hyprpaper `listactive` response into a wallpaper path.
///
/// Response format: `"eDP-1 = /path/to/image\nMONITOR2 = /path/to/image2\n"`
fn parse_hyprpaper_active_response(response: &str, monitor: Option<&str>) -> Option<String> {
    let entries: Vec<(String, String)> = response
        .lines()
        .filter_map(|line| {
            let (name, path) = line.split_once('=')?;
            Some((name.trim().to_string(), path.trim().to_string()))
        })
        .collect();

    select_monitor_wallpaper(&entries, monitor)
}

/// Detect the wallpaper via the hyprwire binary protocol (`hyprpaper_core@2`).
///
/// hyprpaper 0.8.x migrated away from the legacy text IPC to hyprwire. Released
/// `hyprpaper_core@1` (0.8.0–0.8.3) is write-only; upstream `hyprpaper_core@2`
/// (main / future >= 0.8.4) adds a status object that emits
/// `active_wallpaper(monitor, path)` events. Detection soft-fails to `None` if
/// the server does not advertise `hyprpaper_core@2`.
fn detect_hyprpaper_hyprwire(stream: UnixStream, monitor: Option<&str>) -> Option<String> {
    let mut client = HyprwireClient::from_stream(stream)
        .inspect_err(|e| debug!("hyprwire: failed to connect: {}", e))
        .ok()?;

    let protocols = client
        .perform_handshake()
        .inspect_err(|e| debug!("hyprwire: handshake failed: {}", e))
        .ok()?;

    debug!(
        "hyprwire: server protocols = {:?}",
        protocols
            .iter()
            .map(|(spec, version)| format!("{}@{}", spec, version))
            .collect::<Vec<_>>()
    );

    if !is_hyprpaper_status_protocol_available(&protocols) {
        let v = protocols
            .iter()
            .find(|(spec, _)| spec == HYPRPAPER_PROTOCOL)
            .map(|(_, version)| *version);
        match v {
            Some(ver) => debug!(
                "hyprwire: {}@{} exposes no readable wallpaper state yet",
                HYPRPAPER_PROTOCOL, ver
            ),
            None => debug!("hyprwire: server does not advertise {}", HYPRPAPER_PROTOCOL),
        }
        return None;
    }

    let manager_id = client
        .bind_protocol(HYPRPAPER_PROTOCOL, HYPRPAPER_STATUS_VERSION)
        .inspect_err(|e| debug!("hyprwire: failed to bind {}: {}", HYPRPAPER_PROTOCOL, e))
        .ok()?;

    let status_id = client
        .create_status_object(manager_id)
        .inspect_err(|e| debug!("hyprwire: failed to create status object: {}", e))
        .ok()?;

    // The initial wallpaper snapshot is delivered asynchronously after binding;
    // a busy daemon may need more headroom than the handshake budget allows.
    client
        .set_read_timeout(Duration::from_millis(500))
        .inspect_err(|e| debug!("hyprwire: failed to extend read timeout: {}", e))
        .ok()?;

    let roundtrip_seq = client
        .send_roundtrip_request()
        .inspect_err(|e| debug!("hyprwire: failed to request roundtrip: {}", e))
        .ok()?;

    let mut pending: Vec<HyprwireMessage> = Vec::new();

    loop {
        let msg = client
            .read_message()
            .inspect_err(|e| debug!("hyprwire: failed while reading status events: {}", e))
            .ok()?;

        match msg.code {
            HW_GENERIC_PROTOCOL_MESSAGE => {
                pending.push(msg);
            }
            HW_ROUNDTRIP_DONE => {
                if let Some(HyprwireValue::Uint(done_seq)) = msg.args.first()
                    && *done_seq == roundtrip_seq
                {
                    break;
                }
            }
            HW_FATAL_PROTOCOL_ERROR => {
                debug!("hyprwire: {}", format_fatal_protocol_error(&msg));
                return None;
            }
            _ => {}
        }
    }

    let wallpapers = collect_wallpaper_entries(&pending, status_id);

    if wallpapers.is_empty() {
        debug!("hyprwire: status object returned no active wallpapers");
        return None;
    }

    select_monitor_wallpaper(&wallpapers, monitor)
}

/// Detect the current wallpaper path from awww (or legacy swww) via CLI.
///
/// Output format: `{namespace}: {output}: {w}x{h}, scale: {s}, currently displaying: image: {path}`
fn detect_awww_wallpaper(monitor: Option<&str>) -> Option<String> {
    let output = Command::new("awww")
        .arg("query")
        .output()
        .or_else(|_| Command::new("swww").arg("query").output())
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Try to match the target monitor first
    if let Some(target) = monitor {
        for line in stdout.lines() {
            let Some(after_ns) = line.split_once(": ").map(|(_, rest)| rest) else {
                continue;
            };
            let Some((output_name, _)) = after_ns.split_once(':') else {
                continue;
            };
            if output_name.trim() == target {
                return extract_awww_image_path(line);
            }
        }
    }

    // Fall back to first line with an image path
    stdout.lines().find_map(extract_awww_image_path)
}

/// Extract the image path from a single awww/swww query output line.
fn extract_awww_image_path(line: &str) -> Option<String> {
    let marker = "currently displaying: image: ";
    let idx = line.find(marker)?;
    let path = line[idx + marker.len()..].trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// Detect the current wallpaper path from wpaperd via its state symlinks.
///
/// wpaperd creates symlinks at `$XDG_STATE_HOME/wpaperd/wallpapers/<OUTPUT>`
/// pointing to the current wallpaper image.
fn detect_wpaperd_wallpaper(monitor: Option<&str>) -> Option<String> {
    let state_dir = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
        std::env::var("HOME")
            .map(|h| format!("{}/.local/state", h))
            .unwrap_or_default()
    });
    if state_dir.is_empty() {
        return None;
    }

    let wallpapers_dir = std::path::PathBuf::from(&state_dir).join("wpaperd/wallpapers");

    // Try the target monitor symlink first
    if let Some(target) = monitor {
        let symlink = wallpapers_dir.join(target);
        if let Ok(path) = std::fs::read_link(&symlink) {
            return Some(path.to_string_lossy().into_owned());
        }
    }

    // Fall back to first symlink in the directory
    let entries = std::fs::read_dir(&wallpapers_dir).ok()?;
    for entry in entries.flatten() {
        if let Ok(path) = std::fs::read_link(entry.path()) {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    None
}

/// Detect the current wallpaper path from waypaper's INI config.
///
/// Parses `wallpaper = ` under `[Settings]` in `~/.config/waypaper/config.ini`.
/// Handles per-monitor parallel lists and `~` path expansion.
fn detect_waypaper_wallpaper(monitor: Option<&str>) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let config_path =
        std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", home));
    let ini_path = std::path::PathBuf::from(&config_path).join("waypaper/config.ini");

    let content = std::fs::read_to_string(&ini_path).ok()?;

    let mut in_settings = false;
    let mut wallpaper = None;
    let mut monitors_line = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_settings = trimmed.eq_ignore_ascii_case("[settings]");
            continue;
        }
        if !in_settings {
            continue;
        }

        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "wallpaper" => wallpaper = Some(value.to_string()),
                "monitors" => monitors_line = Some(value.to_string()),
                _ => {}
            }
        }
    }

    let wallpaper = wallpaper?;

    // waypaper supports per-monitor parallel lists: monitors = eDP-1,DP-1 / wallpaper = path1,path2
    if let Some(target) = monitor
        && let Some(ref monitors_csv) = monitors_line
    {
        let monitors: Vec<&str> = monitors_csv.split(',').map(|s| s.trim()).collect();
        let paths: Vec<&str> = wallpaper.split(',').map(|s| s.trim()).collect();
        if let Some(idx) = monitors.iter().position(|m| *m == target)
            && let Some(path) = paths.get(idx)
        {
            return Some(expand_tilde(path, &home));
        }
    }

    // Single wallpaper or no monitor match — use the first (or only) path
    let first_path = wallpaper.split(',').next()?.trim();
    Some(expand_tilde(first_path, &home))
}

/// Detect the current wallpaper from any supported daemon.
///
/// Cascade order: hyprpaper → wpaperd → waypaper → awww/swww.
/// Lightweight checks (socket, filesystem) run first; subprocess-based
/// detection (awww/swww CLI) is last to avoid unnecessary fork+exec.
pub fn detect_wallpaper(monitor: Option<&str>) -> Option<String> {
    if let Some(path) = detect_hyprpaper_wallpaper(monitor) {
        debug!("Wallpaper detected via hyprpaper: {}", path);
        return Some(path);
    }
    if let Some(path) = detect_wpaperd_wallpaper(monitor) {
        debug!("Wallpaper detected via wpaperd: {}", path);
        return Some(path);
    }
    if let Some(path) = detect_waypaper_wallpaper(monitor) {
        debug!("Wallpaper detected via waypaper: {}", path);
        return Some(path);
    }
    if let Some(path) = detect_awww_wallpaper(monitor) {
        debug!("Wallpaper detected via awww/swww: {}", path);
        return Some(path);
    }

    debug!("No wallpaper daemon detected");
    None
}

/// Rebuild a Material You theme from a previously extracted source color.
///
/// This is cheap (pure math, no I/O) and used when only the light/dark preference
/// changes without the wallpaper itself changing.
pub fn theme_from_source_color(source: Argb) -> material_colors::theme::Theme {
    ThemeBuilder::with_source(source).build()
}

/// Extract a Material You theme and average relative luminance from a wallpaper image.
///
/// Returns `(Theme, luminance)` where:
/// - `Theme` has light/dark schemes, tonal palettes, and source color
/// - `luminance` is the average relative luminance of the image (0.0 = black, 1.0 = white),
///   used to auto-derive the light/dark scheme polarity when `scheme` is not set in config.
///
/// Returns `None` on failure (file too large, I/O error, decode error).
/// On success, the inner `Option<Theme>` is `None` when no high-chroma colour
/// survived quantisation (e.g. a fully-greyscale wallpaper), but luminance is
/// still returned so auto-scheme derivation works correctly in that case.
pub fn extract_theme_from_image(
    path: &str,
) -> Option<(Option<material_colors::theme::Theme>, f64)> {
    let file_size = std::fs::metadata(path)
        .inspect_err(|e| warn!("Failed to stat wallpaper '{}': {}", path, e))
        .ok()?
        .len();
    if file_size > MAX_WALLPAPER_FILE_SIZE {
        warn!(
            "Wallpaper '{}' too large ({} MB, max {} MB)",
            path,
            file_size / (1024 * 1024),
            MAX_WALLPAPER_FILE_SIZE / (1024 * 1024)
        );
        return None;
    }

    let image_bytes = std::fs::read(path)
        .inspect_err(|e| warn!("Failed to read wallpaper image '{}': {}", path, e))
        .ok()?;

    let img = image::load_from_memory(&image_bytes)
        .inspect_err(|e| warn!("Failed to decode wallpaper image '{}': {}", path, e))
        .ok()?;

    // Match matugen's preprocessing more closely so quantization sees a similar
    // pixel distribution for wide and tall wallpapers.
    let resized = img.resize_exact(112, 112, image::imageops::FilterType::Triangle);
    let rgba = resized.to_rgba8();

    let pixels: Vec<Argb> = rgba
        .pixels()
        .map(|p| Argb::new(p[3], p[0], p[1], p[2]))
        .filter(|argb| argb.alpha == 255)
        .collect();

    // Compute average relative luminance from all opaque pixels for scheme auto-derivation.
    // Uses the same pixel buffer already loaded for color extraction — no extra I/O.
    let luminance = if pixels.is_empty() {
        0.0
    } else {
        let total: f64 = pixels
            .iter()
            .map(|argb| relative_luminance(argb.red, argb.green, argb.blue))
            .sum();
        total / pixels.len() as f64
    };

    let mut result = QuantizerCelebi::quantize(&pixels, 128);
    result
        .color_to_count
        .retain(|&argb, _| Cam16::from(argb).chroma >= 5.0);

    let ranked = Score::score(&result.color_to_count, None, None, None);
    let theme = ranked
        .first()
        .map(|&source_color| ThemeBuilder::with_source(source_color).build());

    Some((theme, luminance))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test: a fully-greyscale wallpaper has no high-chroma colours,
    /// so after the chroma retain filter `color_to_count` is empty. Before the
    /// fix in `09a36d7`, the `?` on `ranked.first()` caused the whole function
    /// to return `None`, silently discarding the luminance that had already been
    /// computed and forcing auto mode to dark regardless of actual brightness.
    ///
    /// After the fix the outer `Option` reflects decode success/failure only;
    /// luminance is always returned when image decode succeeds.
    #[test]
    fn test_extract_theme_grayscale_preserves_luminance() {
        use image::{GrayImage, ImageFormat};
        use std::io::Cursor;

        // 4×4 mid-gray image — all pixels have chroma ≈ 0, well below the 5.0
        // retain threshold, so the quantiser colour map will be empty or
        // produce only Score's built-in fallback colour.
        let img = GrayImage::from_pixel(4, 4, image::Luma([128u8]));
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png)
            .expect("failed to encode test PNG");

        let path = std::env::temp_dir().join("vibepanel_test_grayscale.png");
        std::fs::write(&path, buf.into_inner()).expect("write tempfile");

        let result = extract_theme_from_image(path.to_str().expect("valid utf-8 path"));
        let _ = std::fs::remove_file(&path);

        // The outer Option must be Some — a valid image is not a decode failure.
        // Before the fix this returned None, discarding luminance entirely.
        let (_theme, luminance) = result.expect("extraction must succeed for a valid image");

        // Luminance must be in (0, 1) — mid-gray luma 128 gives ~0.216.
        assert!(
            luminance > 0.0 && luminance < 1.0,
            "luminance should be in (0, 1), got {luminance}"
        );
    }

    #[test]
    fn test_extract_awww_image_path_standard() {
        let line =
            "default: eDP-1: 1920x1080, scale: 1, currently displaying: image: /home/user/wall.png";
        assert_eq!(
            extract_awww_image_path(line),
            Some("/home/user/wall.png".to_string())
        );
    }

    #[test]
    fn test_extract_awww_image_path_spaces_in_path() {
        let line = "default: DP-2: 2560x1440, scale: 1, currently displaying: image: /home/user/My Wallpapers/wall.jpg";
        assert_eq!(
            extract_awww_image_path(line),
            Some("/home/user/My Wallpapers/wall.jpg".to_string())
        );
    }

    #[test]
    fn test_extract_awww_image_path_no_marker() {
        assert_eq!(extract_awww_image_path("some random line"), None);
    }

    #[test]
    fn test_extract_awww_image_path_empty_after_marker() {
        let line = "default: eDP-1: 1920x1080, scale: 1, currently displaying: image:   ";
        assert_eq!(extract_awww_image_path(line), None);
    }

    // ----- varint codec -----

    #[test]
    fn test_encode_varint_byte_layout() {
        assert_eq!(encode_varint(0), vec![0x00]);
        assert_eq!(encode_varint(127), vec![0x7F]);
        assert_eq!(encode_varint(128), vec![0x80, 0x01]);
        assert_eq!(encode_varint(16383), vec![0xFF, 0x7F]);
        assert_eq!(encode_varint(16384), vec![0x80, 0x80, 0x01]);
    }

    #[test]
    fn test_varint_decode_truncated() {
        // Continuation bit set but no more bytes
        assert!(read_varint_from_bytes(&[0x80]).is_err());
        assert!(read_varint_from_bytes(&[]).is_err());
    }

    #[test]
    fn test_varint_decode_u32_boundary() {
        // 0xFF,0xFF,0xFF,0xFF,0x0F -> u32::MAX (5th byte payload 0x0F, max valid)
        assert_eq!(
            read_varint_from_bytes(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).expect("u32::MAX decodes"),
            u32::MAX as usize
        );
    }

    #[test]
    fn test_varint_decode_rejects_above_u32() {
        // 5th byte payload 0x10 would set bit 32 -> exceeds u32 range
        assert!(read_varint_from_bytes(&[0xFF, 0xFF, 0xFF, 0xFF, 0x10]).is_err());
        // Highest 5th-byte payload 0x7F is also out of range
        assert!(read_varint_from_bytes(&[0x80, 0x80, 0x80, 0x80, 0x7F]).is_err());
        // Continuation bit set on a 5th byte with oversized payload: rejected early
        assert!(read_varint_from_bytes(&[0x80, 0x80, 0x80, 0x80, 0x90, 0x01]).is_err());
    }

    // ----- allocation caps on hyprwire payload sizes -----

    fn read_varint_from_bytes(bytes: &[u8]) -> Result<usize, String> {
        let (a, mut b) = UnixStream::pair().expect("socketpair");
        b.write_all(bytes).expect("write payload");
        drop(b);
        HyprwireClient::from_stream(a).unwrap().read_varint()
    }

    fn read_value_from_bytes(magic: u8, bytes: &[u8]) -> Result<HyprwireValue, String> {
        let (a, mut b) = UnixStream::pair().expect("socketpair");
        b.write_all(bytes).expect("write payload");
        drop(b);
        let mut client = HyprwireClient::from_stream(a).unwrap();
        client.read_value(magic)
    }

    #[test]
    fn test_read_value_rejects_oversized_varchar() {
        // Length beyond HW_MAX_VARCHAR_LEN must be rejected before allocating.
        let mut payload = encode_varint(HW_MAX_VARCHAR_LEN + 1);
        // A few body bytes so the socket has something to offer; the cap check
        // must fire before any read_exact on the body.
        payload.extend_from_slice(&[0u8; 16]);
        let err = read_value_from_bytes(HW_VARCHAR, &payload).expect_err("cap rejects");
        assert!(err.contains("exceeds cap"), "unexpected error: {err}");
    }

    #[test]
    fn test_read_value_accepts_varchar_at_cap() {
        // Exactly HW_MAX_VARCHAR_LEN must still be accepted.
        let mut payload = encode_varint(HW_MAX_VARCHAR_LEN);
        payload.extend(std::iter::repeat_n(b'a', HW_MAX_VARCHAR_LEN));
        let value = read_value_from_bytes(HW_VARCHAR, &payload).expect("cap-sized accepted");
        match value {
            HyprwireValue::Varchar(s) => assert_eq!(s.len(), HW_MAX_VARCHAR_LEN),
            other => panic!("expected Varchar, got {other:?}"),
        }
    }

    #[test]
    fn test_read_array_rejects_oversized_count() {
        // Item type + varint count above HW_MAX_ARRAY_COUNT.
        let mut payload = vec![HW_UINT];
        payload.extend_from_slice(&encode_varint(HW_MAX_ARRAY_COUNT + 1));
        let err = read_value_from_bytes(HW_ARRAY, &payload).expect_err("cap rejects");
        assert!(err.contains("exceeds cap"), "unexpected error: {err}");
    }

    #[test]
    fn test_read_array_rejects_oversized_inner_varchar() {
        // Count of 1, then oversized varchar length inside.
        let mut payload = vec![HW_VARCHAR];
        payload.extend_from_slice(&encode_varint(1));
        payload.extend_from_slice(&encode_varint(HW_MAX_VARCHAR_LEN + 1));
        payload.extend_from_slice(&[0u8; 16]);
        let err = read_value_from_bytes(HW_ARRAY, &payload).expect_err("cap rejects");
        assert!(err.contains("exceeds cap"), "unexpected error: {err}");
    }

    // ----- parse_hyprpaper_active_response (legacy text protocol) -----

    #[test]
    fn test_parse_legacy_single_monitor_no_target() {
        let response = "eDP-1 = /home/user/wall.png\n";
        assert_eq!(
            parse_hyprpaper_active_response(response, None),
            Some("/home/user/wall.png".to_string())
        );
    }

    #[test]
    fn test_parse_legacy_multi_monitor_target_match() {
        let response = "eDP-1 = /home/user/first.png\nDP-1 = /home/user/second.png\n";
        assert_eq!(
            parse_hyprpaper_active_response(response, Some("DP-1")),
            Some("/home/user/second.png".to_string())
        );
    }

    #[test]
    fn test_parse_legacy_multi_monitor_target_miss_falls_back() {
        let response = "eDP-1 = /home/user/first.png\nDP-1 = /home/user/second.png\n";
        assert_eq!(
            parse_hyprpaper_active_response(response, Some("HDMI-A-1")),
            Some("/home/user/first.png".to_string())
        );
    }

    #[test]
    fn test_parse_legacy_empty_path_filtered() {
        let response = "eDP-1 = \nDP-1 = /home/user/second.png\n";
        assert_eq!(
            parse_hyprpaper_active_response(response, None),
            Some("/home/user/second.png".to_string())
        );
    }

    #[test]
    fn test_parse_legacy_target_empty_path_falls_back() {
        let response = "eDP-1 = /home/user/first.png\nDP-1 = \n";
        assert_eq!(
            parse_hyprpaper_active_response(response, Some("DP-1")),
            Some("/home/user/first.png".to_string())
        );
    }

    #[test]
    fn test_parse_legacy_non_ascii_path() {
        // Regression guard: legacy hyprpaper can return UTF-8 paths with
        // non-ASCII characters (CJK, accented, etc.). The old is_ascii()
        // guard rejected these incorrectly.
        let response = "eDP-1 = /home/user/壁纸.png\n";
        assert_eq!(
            parse_hyprpaper_active_response(response, None),
            Some("/home/user/壁纸.png".to_string())
        );

        let response = "DP-1 = /home/david/Bilder/Café.jpg\n";
        assert_eq!(
            parse_hyprpaper_active_response(response, Some("DP-1")),
            Some("/home/david/Bilder/Café.jpg".to_string())
        );
    }

    #[test]
    fn test_parse_legacy_empty_response() {
        assert_eq!(parse_hyprpaper_active_response("", None), None);
    }

    #[test]
    fn test_parse_legacy_no_equals_lines() {
        assert_eq!(
            parse_hyprpaper_active_response("garbage without separators\n", None),
            None
        );
    }

    // ----- parse_new_object -----

    #[test]
    fn test_parse_new_object_valid() {
        let msg = HyprwireMessage {
            code: HW_NEW_OBJECT,
            args: vec![HyprwireValue::Uint(42), HyprwireValue::Uint(7)],
        };
        assert_eq!(parse_new_object(&msg), Some((42, 7)));
    }

    #[test]
    fn test_parse_new_object_wrong_types() {
        let msg = HyprwireMessage {
            code: HW_NEW_OBJECT,
            args: vec![HyprwireValue::Uint(42), HyprwireValue::Seq(7)],
        };
        assert_eq!(parse_new_object(&msg), None);
    }

    // ----- parse_active_wallpaper_event -----

    fn active_wallpaper_msg(
        object_id: u32,
        method_id: u32,
        monitor: &str,
        path: &str,
    ) -> HyprwireMessage {
        HyprwireMessage {
            code: HW_GENERIC_PROTOCOL_MESSAGE,
            args: vec![
                HyprwireValue::Object(object_id),
                HyprwireValue::Uint(method_id),
                HyprwireValue::Varchar(monitor.to_string()),
                HyprwireValue::Varchar(path.to_string()),
            ],
        }
    }

    #[test]
    fn test_active_wallpaper_event_matching() {
        let msg = active_wallpaper_msg(
            5,
            HYPRPAPER_STATUS_ACTIVE_WALLPAPER,
            "eDP-1",
            "/home/user/wall.png",
        );
        assert_eq!(
            parse_active_wallpaper_event(&msg, 5),
            Some(("eDP-1".to_string(), "/home/user/wall.png".to_string()))
        );
    }

    #[test]
    fn test_active_wallpaper_event_wrong_object_id() {
        let msg = active_wallpaper_msg(
            5,
            HYPRPAPER_STATUS_ACTIVE_WALLPAPER,
            "eDP-1",
            "/home/user/wall.png",
        );
        assert_eq!(parse_active_wallpaper_event(&msg, 99), None);
    }

    // ----- select_monitor_wallpaper -----

    fn entry(name: &str, path: &str) -> (String, String) {
        (name.to_string(), path.to_string())
    }

    #[test]
    fn test_select_no_target_returns_first_nonempty() {
        let entries = vec![
            entry("eDP-1", ""),
            entry("DP-1", "/home/user/wall.png"),
            entry("HDMI-A-1", "/home/user/other.png"),
        ];
        assert_eq!(
            select_monitor_wallpaper(&entries, None),
            Some("/home/user/wall.png".to_string())
        );
    }

    #[test]
    fn test_select_target_found() {
        let entries = vec![
            entry("eDP-1", "/home/user/first.png"),
            entry("DP-1", "/home/user/second.png"),
        ];
        assert_eq!(
            select_monitor_wallpaper(&entries, Some("DP-1")),
            Some("/home/user/second.png".to_string())
        );
    }

    #[test]
    fn test_select_all_empty_returns_none() {
        let entries = vec![entry("eDP-1", ""), entry("DP-1", "")];
        assert_eq!(select_monitor_wallpaper(&entries, None), None);
        assert_eq!(select_monitor_wallpaper(&entries, Some("DP-1")), None);
    }

    // ----- maps_indicate_hyprwire -----

    #[test]
    fn test_maps_indicate_hyprwire_matches_library_mapping() {
        let maps = "\
7f0000000000-7f0000010000 r-xp 00000000 fd:00 1 /usr/lib/libhyprtoolkit.so.0\n\
7f0000020000-7f0000030000 r-xp 00000000 fd:00 2 /usr/lib/libhyprlang.so\n";
        assert!(maps_indicate_hyprwire(maps));
    }

    #[test]
    fn test_maps_indicate_hyprwire_rejects_hyprpaper_07_maps() {
        let maps = "\
55d000000000-55d000040000 r-xp 00000000 fd:00 10 /usr/bin/hyprpaper\n\
7f0000000000-7f0000020000 r-xp 00000000 fd:00 20 /usr/lib/libhyprlang.so\n\
7f0000030000-7f0000040000 r-xp 00000000 fd:00 21 /usr/lib/libwayland-client.so.0\n";
        assert!(!maps_indicate_hyprwire(maps));
    }

    #[test]
    fn test_maps_indicate_hyprwire_rejects_bare_hyprtoolkit_substring() {
        let maps = "\
55d000000000-55d000040000 r-xp 00000000 fd:00 10 /opt/hyprtoolkit-demo/bin/demo\n";
        assert!(!maps_indicate_hyprwire(maps));
    }

    #[test]
    fn test_maps_indicate_hyprwire_rejects_hyprtoolkit_helper_library() {
        let maps = "\
7f0000000000-7f0000010000 r-xp 00000000 fd:00 1 /usr/lib/libhyprtoolkit-helper.so.0\n";
        assert!(!maps_indicate_hyprwire(maps));
    }

    #[test]
    fn test_maps_indicate_hyprwire_rejects_hyprtoolkit_plugin_library() {
        let maps = "\
7f0000000000-7f0000010000 r-xp 00000000 fd:00 1 /usr/lib/libhyprtoolkit-plugin.so.1\n";
        assert!(!maps_indicate_hyprwire(maps));
    }

    // ----- is_hyprpaper_status_protocol_available -----

    #[test]
    fn test_is_hyprpaper_status_protocol_available_rejects_old_version() {
        let protocols = vec![(HYPRPAPER_PROTOCOL.to_string(), 1u32)];
        assert!(!is_hyprpaper_status_protocol_available(&protocols));
    }

    #[test]
    fn test_is_hyprpaper_status_protocol_available_accepts_v2_and_above() {
        let protocols = vec![
            (HYPRPAPER_PROTOCOL.to_string(), 1u32),
            (HYPRPAPER_PROTOCOL.to_string(), 2u32),
            ("other_protocol".to_string(), 5u32),
        ];
        assert!(is_hyprpaper_status_protocol_available(&protocols));
    }

    // ----- collect_wallpaper_entries -----

    #[test]
    fn test_collect_wallpaper_entries_latest_wins() {
        let status_id = 20;
        let msgs = vec![
            active_wallpaper_msg(
                status_id,
                HYPRPAPER_STATUS_ACTIVE_WALLPAPER,
                "eDP-1",
                "/old.png",
            ),
            active_wallpaper_msg(
                status_id,
                HYPRPAPER_STATUS_ACTIVE_WALLPAPER,
                "eDP-1",
                "/new.png",
            ),
        ];
        let entries = collect_wallpaper_entries(&msgs, status_id);
        assert_eq!(entries, vec![("eDP-1".to_string(), "/new.png".to_string())]);
    }

    fn enc(value: &HyprwireValue) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_value(&mut buf, value);
        buf
    }

    fn mock_send(stream: &mut UnixStream, code: u8, args: &[u8]) {
        let mut buf = vec![code];
        buf.extend_from_slice(args);
        buf.push(HW_END);
        stream.write_all(&buf).expect("write mock message");
    }

    /// Exercises the local hyprwire state machine end-to-end. This guards
    /// internal sequencing regressions, not upstream protocol compatibility.
    #[test]
    fn test_detect_hyprpaper_hyprwire_happy_path() {
        use std::os::unix::net::UnixListener;

        let socket_path = std::env::temp_dir().join(format!(
            "vibepanel_test_hyprwire_happy_{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).expect("bind temp socket");
        let server_thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut srv = HyprwireClient::from_stream(stream).unwrap();

            let sup = srv.read_message().expect("read sup");
            assert_eq!(sup.code, HW_SUP);
            assert!(
                matches!(sup.args.first(), Some(HyprwireValue::Varchar(s)) if s == "VAX"),
                "sup payload must be Varchar(\"VAX\"), got {:?}",
                sup.args
            );

            let begin_args = enc(&HyprwireValue::ArrayUint(vec![HYPRWIRE_VERSION]));
            mock_send(&mut srv.stream, HW_HANDSHAKE_BEGIN, &begin_args);

            let ack = srv.read_message().expect("read ack");
            assert_eq!(ack.code, HW_HANDSHAKE_ACK);
            assert!(
                matches!(ack.args.first(), Some(HyprwireValue::Uint(v)) if *v == HYPRWIRE_VERSION),
                "ack payload must be Uint({}), got {:?}",
                HYPRWIRE_VERSION,
                ack.args
            );

            let protocols = enc(&HyprwireValue::ArrayVarchar(vec![format!(
                "{}@{}",
                HYPRPAPER_PROTOCOL, HYPRPAPER_STATUS_VERSION
            )]));
            mock_send(&mut srv.stream, HW_HANDSHAKE_PROTOCOLS, &protocols);

            let bind = srv.read_message().expect("read bind");
            assert_eq!(bind.code, HW_BIND_PROTOCOL);
            let bind_seq = match bind.args.first() {
                Some(HyprwireValue::Uint(seq)) => *seq,
                _ => panic!("bind must start with Uint seq, got {:?}", bind.args),
            };
            assert!(
                matches!(bind.args.get(1), Some(HyprwireValue::Varchar(s)) if s == HYPRPAPER_PROTOCOL),
                "bind spec must be {:?}, got {:?}",
                HYPRPAPER_PROTOCOL,
                bind.args
            );
            assert!(
                matches!(bind.args.get(2), Some(HyprwireValue::Uint(v)) if *v == HYPRPAPER_STATUS_VERSION),
                "bind version must be {}, got {:?}",
                HYPRPAPER_STATUS_VERSION,
                bind.args
            );

            let mut manager = enc(&HyprwireValue::Uint(10));
            manager.extend_from_slice(&enc(&HyprwireValue::Uint(bind_seq)));
            mock_send(&mut srv.stream, HW_NEW_OBJECT, &manager);

            let create_status = srv.read_message().expect("read create status");
            assert_eq!(create_status.code, HW_GENERIC_PROTOCOL_MESSAGE);
            assert!(
                matches!(create_status.args.first(), Some(HyprwireValue::Object(id)) if *id == 10),
                "create-status target must be Object(10), got {:?}",
                create_status.args
            );
            assert!(
                matches!(create_status.args.get(1), Some(HyprwireValue::Uint(m)) if *m == HYPRPAPER_CORE_GET_STATUS_OBJECT),
                "create-status method must be {}, got {:?}",
                HYPRPAPER_CORE_GET_STATUS_OBJECT,
                create_status.args
            );
            let status_seq = match create_status.args.get(2) {
                Some(HyprwireValue::Seq(seq)) => *seq,
                _ => panic!(
                    "create-status must carry Seq in arg[2], got {:?}",
                    create_status.args
                ),
            };

            let mut status = enc(&HyprwireValue::Uint(20));
            status.extend_from_slice(&enc(&HyprwireValue::Uint(status_seq)));
            mock_send(&mut srv.stream, HW_NEW_OBJECT, &status);

            let roundtrip = srv.read_message().expect("read roundtrip request");
            assert_eq!(roundtrip.code, HW_ROUNDTRIP_REQUEST);
            let roundtrip_seq = match roundtrip.args.first() {
                Some(HyprwireValue::Uint(seq)) => *seq,
                _ => panic!("roundtrip request must start with Uint seq"),
            };

            let mut wallpaper = enc(&HyprwireValue::Object(20));
            wallpaper.extend_from_slice(&enc(&HyprwireValue::Uint(
                HYPRPAPER_STATUS_ACTIVE_WALLPAPER,
            )));
            wallpaper.extend_from_slice(&enc(&HyprwireValue::Varchar("eDP-1".to_string())));
            wallpaper.extend_from_slice(&enc(&HyprwireValue::Varchar(
                "/home/user/wall.png".to_string(),
            )));
            mock_send(&mut srv.stream, HW_GENERIC_PROTOCOL_MESSAGE, &wallpaper);

            let done = enc(&HyprwireValue::Uint(roundtrip_seq));
            mock_send(&mut srv.stream, HW_ROUNDTRIP_DONE, &done);
        });

        let stream = UnixStream::connect(&socket_path).expect("connect to mock server");
        let result = detect_hyprpaper_hyprwire(stream, Some("eDP-1"));

        server_thread.join().expect("server thread");
        let _ = std::fs::remove_file(&socket_path);

        assert_eq!(result, Some("/home/user/wall.png".to_string()));
    }
}
