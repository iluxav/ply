//! The wire contract between ply's VM backend (host) and its guest init.
//!
//! Two things cross the machine boundary and nothing else does:
//!
//! 1. The **spec disk** — a raw block image the VMM writes at launch and the
//!    guest reads once: `PLYSPEC1`, a little-endian u32 length, then JSON.
//!    No filesystem, because the guest must be able to read it before it has
//!    mounted anything. Env arrives here and never on the kernel cmdline,
//!    which is world-readable inside the guest and length-limited.
//! 2. The **control channel** — newline-delimited JSON over virtio-console
//!    port 1, guest→host `GuestLine` and host→guest `HostLine`.
//!
//! Both sides link this crate, so neither can drift from the other. It is
//! deliberately dependency-thin: it ends up inside a static musl init in an
//! initramfs with a size budget, so serde_json is the whole cost and nothing
//! else may be added here without a reason worth those bytes.

// This crate links into PID 1 inside a guest that cannot report a panic,
// let alone undefined behaviour. It has no need of `unsafe`, ever.
//
// **Nothing here may panic, either.** The guest inherits the workspace's
// `panic = "abort"`, and `guest_line` runs on the watcher thread of PID 1: a
// panic there is not a lost line, it is `Attempted to kill init!` with no
// diagnosis, from inside a VM. So this crate carries no `unwrap`/`expect` on
// anything a caller's data can reach.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Leads the spec disk. The guest scans every attached disk for it rather
/// than trusting a device position (ruling R0-5): the same contract, one
/// failure mode fewer, and no extra cost.
///
/// The trailing `1` is the format version, and it bumps only when an existing
/// `SpecDisk` field changes meaning or type — see that struct's compatibility
/// rule, which is what keeps every other change from needing a bump.
pub const SPEC_MAGIC: &[u8; 8] = b"PLYSPEC1";

/// Sector size the padding rounds up to — virtio-blk exposes whole sectors,
/// and a trailing partial sector is simply invisible to the guest.
pub const SECTOR: usize = 512;

/// The params tree as it crosses the wire: `(app, [(key, value)])`.
///
/// An ordered list rather than a map, so the order the host resolved things
/// in survives the trip and two runs of the same stack produce byte-identical
/// spec disks. On the wire that makes it nested arrays, not nested objects:
/// `[["db",[["state","starting"]]]]`.
pub type ParamsTree = Vec<(String, Vec<(String, String)>)>;

/// The facts under an instance's params node that ONLY the parent writes.
///
/// This is a security boundary, and it lives here because both sides of the
/// machine boundary enforce it and neither may drift from the other:
///
/// * the namespace backend bind-mounts each of these files read-only over
///   itself inside the container (`ns/container.rs`), and
/// * the guest init does the same inside the microVM over `/run/ply/self`,
///   and — the part that actually holds when the app is root — refuses to
///   forward a `publish` event for any of these names, so a file the app
///   managed to write anyway never reaches the host's params tree.
///
/// `state` is the one that matters most: it is the fact every `--after`
/// dependant gates on, so an app that could write its own would make every
/// downstream instance believe it was healthy. A name that quietly leaves
/// this list does not fail a build — it just stops sealing a key — which is
/// why the list is pinned by a test in this crate rather than repeated in
/// each consumer.
///
/// **One copy of this list remains in `ply-core`, and only one:**
/// `runtime/params_tree.rs`'s own `PARENT_OWNED`, which is this same seal
/// expressed for the namespace backend. That one should become
/// `ply_vm_proto::PARENT_OWNED`; the edit belongs to whoever owns that file
/// next, and until it happens the drift risk this constant exists to remove
/// is only half removed.
///
/// `params.rs`'s `LIVE` is **not** a third copy and must not be collapsed
/// into this one. It is a params-*declaration* concept — "live params
/// populated by the runtime, never user-declared" — which is why it sits
/// beside `RESERVED`, a list that also holds all four of these names plus
/// ten more. It happens to name the same four for a different reason, and
/// replacing it with this constant would make the manifest language depend
/// on a guest wire contract for its vocabulary.
///
/// The two must nevertheless hold the same names — one that became live
/// without becoming parent-owned is a fact an app can forge, and one that
/// went the other way is an unforgeable fact nobody can wait on — so
/// `ply-core` pins the pair with a test beside `LIVE`. Keep them equal;
/// do not make either the definition of the other.
pub const PARENT_OWNED: &[&str] = &["state", "instances", "started_at", "restarts"];

/// The user the entrypoint runs as, resolved on the host from the image's
/// `/etc/passwd` so the guest never has to parse one. Absent means root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSpec {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// One volume: where it belongs inside the guest, and which block device
/// carries it. Named explicitly rather than positionally so adding a disk
/// anywhere in the order can never silently remount a volume elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    pub path: String,
    pub dev: String,
}

/// A volume to fill from a snapshot before the app starts: the squashfs at
/// `dev` holds the tree under `subdir`; it goes to `path`, ownership and
/// modes as recorded. The tail of a `ply restore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreSpec {
    pub path: String,
    pub dev: String,
    pub subdir: String,
}

/// Everything the guest needs to become the instance. Built by the VM
/// backend from `runtime::backend::InstanceSpec`; read once by the guest
/// init before it pivots.
///
/// # Compatibility rule
///
/// Host and guest are versioned independently — the kernel keg carrying the
/// init is pinned by the binary, but a user may override it — so a disk may
/// be read by an init that is older or newer than the host that wrote it.
/// The rule that keeps that safe:
///
/// - **Every field added from here on is `#[serde(default)]`**, so a disk
///   written by an older host still decodes in a newer guest. There is no
///   exception: a field that cannot be defaulted needs a magic bump instead.
/// - **Unknown fields are ignored**, so a disk written by a newer host still
///   decodes in an older guest — it just does less.
/// - **The magic's version digit (`PLYSPEC1`) bumps only when an existing
///   field changes meaning or type**, which is the one change neither of the
///   above survives. A guest that sees an unknown magic refuses to boot
///   rather than guessing at bytes it cannot interpret.
///
/// The five fields below without `#[serde(default)]` — `entrypoint`,
/// `workdir`, `env`, `hostname`, `layer_count` — are the v1 fields that have
/// no safe default, and they stay required: an instance with no entrypoint is
/// not an instance, a missing `workdir` is not the same as `/`, an empty `env`
/// would silently drop the composed secrets and `PORT`, a blank `hostname` is
/// not a host, and `layer_count: 0` would mount a rootfs with no image in it.
/// For those, refusing the disk is right and defaulting is a silent half-boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecDisk {
    pub entrypoint: Vec<String>,
    pub workdir: String,
    #[serde(default)]
    pub user: Option<UserSpec>,
    /// Fully composed on the host: manifest `[env]`, resolved params,
    /// secrets, `-e`, `HOME`, `TERM`, `PORT`. The guest adds nothing.
    pub env: Vec<(String, String)>,
    pub hostname: String,
    /// `(ip, name)` lines for `/etc/hosts` — the stack's `<name>.ply` peers.
    #[serde(default)]
    pub hosts: Vec<(String, String)>,
    /// The switch's resolver, for `/etc/resolv.conf`.
    #[serde(default)]
    pub dns: Option<String>,
    /// This instance's address on the parent's switch, and how to leave it.
    ///
    /// `None` is a guest with no network card — every instance before the
    /// switch existed, and still what an instance gets when the parent could
    /// not start one. The guest brings its own interface up from these
    /// fields: there is no DHCP on this network, because the only thing that
    /// could serve it is the same process that decided the address.
    #[serde(default)]
    pub net: Option<NetSpec>,
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
    /// Seed for `/run/ply`: the facts the parent already published before
    /// launch.
    #[serde(default)]
    pub params_seed: ParamsTree,
    /// How many of the leading disks are read-only image layers, in overlay
    /// order (top first). Everything after them is a volume or the spec disk.
    pub layer_count: usize,
    /// The host's wall clock at the moment the disk was written.
    ///
    /// A microVM has no RTC and no NTP: without this, the guest's clock
    /// starts at 1970 and every TLS handshake fails with "certificate not
    /// yet valid". The guest sets `CLOCK_REALTIME` from it before it does
    /// anything else, and the tens of milliseconds between the write and
    /// the read are the whole error. `None` — a disk from an older host —
    /// leaves the clock alone.
    #[serde(default)]
    pub clock: Option<ClockSpec>,
    /// Host directories projected into the guest over virtio-9p: a
    /// `ply.dev.toml` link, the source tree a developer edits on the Mac.
    /// Each is mounted at `path` by its `tag`. Empty for every guest that
    /// has none, and for a disk from an older host.
    #[serde(default)]
    pub shares: Vec<ShareSpec>,
    /// Volumes to populate from snapshot disks before the app starts.
    /// Absent in older disks; an older guest ignores it, which is the
    /// compatibility rule — and why the host refuses a restore when the
    /// guest cannot do it rather than launching an empty volume.
    #[serde(default)]
    pub restores: Vec<RestoreSpec>,
}

/// A point in time as `CLOCK_REALTIME` counts it: seconds and nanoseconds
/// since the Unix epoch. Two integers rather than one, because a
/// nanosecond count since 1970 overflows an i64 in 2262 and a JSON float
/// loses the nanoseconds long before that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockSpec {
    pub secs: i64,
    pub nanos: u32,
}

/// One shared host directory: the 9p mount tag the VMM announced it under,
/// and where it belongs inside the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSpec {
    pub tag: String,
    pub path: String,
}

/// The guest's own network configuration, decided by the parent.
///
/// Addresses are strings rather than `Ipv4Addr` for the same reason
/// everything else here is: this struct is JSON on a disk that two
/// independently built binaries read, and a string parses the same way in
/// both for the life of the format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetSpec {
    /// Dotted quad, e.g. `10.77.0.2`.
    pub ip: String,
    /// `16` — the switch's `10.77.0.0/16`, the same range the Linux bridge
    /// uses.
    pub prefix_len: u8,
    /// The switch itself, e.g. `10.77.0.1`: the default route, the resolver,
    /// and the only address on this network that answers for the host.
    pub gateway: String,
}

/// A capability a guest may advertise in its `ready` line. The host asks
/// before it relies on one, because a guest is versioned independently of
/// the binary that boots it: `PLY_MICROVM_KERNEL` is a documented override,
/// and an old keg with a new ply must fail with a sentence rather than
/// hang on a message nothing will ever answer.
pub const FEATURE_EXEC: &str = "exec";

/// The most bytes one `{"out":…}` carries before base64 expands them by a
/// third. Sized so an encoded line stays far under the host's per-line cap
/// even with the JSON around it: a command that prints a megabyte arrives
/// as a stream of these, not as one line nobody can buffer.
pub const EXEC_CHUNK: usize = 16 * 1024;

/// Which of the command's two streams a chunk came from. Kept apart all the
/// way to the caller, because a program's diagnostics and its output are
/// different things and merging them is not recoverable.
pub const STREAM_STDOUT: u8 = 1;
pub const STREAM_STDERR: u8 = 2;

/// Run this command inside the instance.
///
/// `id` is the caller's handle on it: output and the exit status come back
/// tagged with it, and `ExecStdin`/`ExecKill` name it. Ids are the host's to
/// allocate and the guest never invents one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub id: u64,
    /// argv, the first element being the program. Resolved against the
    /// instance's own PATH by the guest, so `ls` works as well as `/bin/ls`.
    pub argv: Vec<String>,
    /// Added to the instance's environment for this command only. The
    /// instance's own env is the base, so a command sees what the app sees.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Absent = the instance's workdir, which is what `ply exec` means by
    /// "inside the instance".
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Bytes for a running command's standard input, base64 in `data`.
/// `eof` closes it, which is how `sort` or `cat` ever finish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecStdin {
    pub id: u64,
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub eof: bool,
}

/// Signal a running command by name, the same spelling `HostLine::Signal`
/// uses. This is how a caller enforces a timeout on something it started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecKill {
    pub id: u64,
    pub name: String,
}

/// A chunk of a running command's output, base64 in `data`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    pub id: u64,
    /// `STREAM_STDOUT` or `STREAM_STDERR`.
    pub stream: u8,
    pub data: String,
}

/// A command ended. The last thing said about that id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecDone {
    pub id: u64,
    /// The command's own status, or `128 + signal` if a signal ended it.
    /// 127 when it could not be started at all and 126 when it was found
    /// but could not be run — the shell's conventions, because that is what
    /// a caller's error handling already knows.
    pub code: i32,
    /// Why it could not be started, when that is what happened. Present
    /// only with 126 or 127.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Guest → host, one JSON object per line on `hvc1`.
///
/// Deliberately NOT a serde enum. `{"ready":true}` is a unit variant with a
/// payload on the wire, which no derive — tagged, untagged or otherwise —
/// expresses; the private wire structs below carry the serde work instead,
/// and this stays the shape the rest of the code matches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestLine {
    /// `{"ready":true,"features":["exec"]}` — the entrypoint has been
    /// exec'd, and this is what the guest can do. An older guest sends no
    /// `features` at all, which reads as "none of them" and is correct.
    Ready { features: Vec<String> },
    /// `{"exit":N}` — the entrypoint ended.
    Exit { code: i32 },
    /// `{"publish":{"key":"finish_boot","value":"ok"}}` — the app wrote
    /// `/run/ply/self/<key>`; forward it to the host's params tree.
    Publish { publish: Publish },
    /// `{"out":{"id":1,"stream":1,"data":"aGk="}}` — output from a command
    /// the host started with `HostLine::Exec`.
    Output { output: ExecOutput },
    /// `{"done":{"id":1,"code":0}}` — that command ended.
    Done { done: ExecDone },
}

/// One fact the instance published about itself: the guest saw the app write
/// `/run/ply/self/<key>`, and the host folds `(key, value)` into the stack's
/// params tree so peers waiting on it can proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publish {
    pub key: String,
    pub value: String,
}

/// Host → guest, one JSON object per line on `hvc1`. Not a serde enum, for
/// the same reason as `GuestLine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLine {
    /// `{"signal":"TERM"}` — forward this signal to the entrypoint.
    Signal { name: String },
    /// `{"params":[["<app>",[["<key>","<value>"]]]]}` — a live params update
    /// to apply to the read-only peer nodes under `/run/ply`.
    Params { params: ParamsTree },
    /// `{"exec":{"id":1,"argv":["ls"]}}` — run this, beside the app.
    Exec { exec: ExecRequest },
    /// `{"stdin":{"id":1,"data":"aGk=","eof":false}}`
    Stdin { stdin: ExecStdin },
    /// `{"kill":{"id":1,"name":"TERM"}}`
    Kill { kill: ExecKill },
}

/// The on-the-wire shape of a guest→host line: every variant's payload as an
/// optional field of one object. Absent fields are skipped when writing, and
/// unknown ones are dropped when reading, which is what makes an unrecognised
/// line parse into "nothing I know" rather than into an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct GuestWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    ready: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    features: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    publish: Option<Publish>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<ExecOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    done: Option<ExecDone>,
}

/// The host→guest mirror of `GuestWire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct HostWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<ParamsTree>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec: Option<ExecRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdin: Option<ExecStdin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kill: Option<ExecKill>,
}

/// Render one guest→host line, newline included. The newline is the frame,
/// so the caller writes exactly what it is handed and nothing else.
///
/// The `unwrap_or_else` below is not defensiveness about a case that can
/// happen — `GuestWire` is three `Option`s of plain types and serialising it
/// cannot fail — it is about what the *alternative* costs. This runs on the
/// watcher thread inside PID 1 under `panic = "abort"`, where an `.expect()`
/// that is wrong once is a dead machine with no message; `{}` is a line both
/// parsers already ignore, so the degraded outcome is one dropped fact. The
/// signature does not change, so nothing else has to.
pub fn guest_line(line: &GuestLine) -> String {
    let wire = match line {
        GuestLine::Ready { features } => GuestWire {
            ready: Some(true),
            features: features.clone(),
            ..Default::default()
        },
        GuestLine::Exit { code } => GuestWire {
            exit: Some(*code),
            ..Default::default()
        },
        GuestLine::Publish { publish } => GuestWire {
            publish: Some(publish.clone()),
            ..Default::default()
        },
        GuestLine::Output { output } => GuestWire {
            output: Some(output.clone()),
            ..Default::default()
        },
        GuestLine::Done { done } => GuestWire {
            done: Some(done.clone()),
            ..Default::default()
        },
    };
    let mut s = serde_json::to_string(&wire).unwrap_or_else(|_| String::from("{}"));
    s.push('\n');
    s
}

/// Parse one guest→host line. `None` for anything this build does not
/// understand: the two sides are versioned independently (the kernel keg is
/// pinned by the binary, but a user may override it), and an unknown line
/// must never end an instance.
///
/// A line carrying two known fields yields the first in `GuestWire`'s
/// declaration order — `{"ready":true,"exit":7}` is `Ready`, and the `exit`
/// is dropped. Senders never emit two; this is only so both sides agree on
/// what a reader does if one ever does.
pub fn parse_guest_line(text: &str) -> Option<GuestLine> {
    // A control line is a JSON *object*. serde's derive would also accept a
    // positional array of the same arity, which would let a future line shape
    // be misread as a command rather than ignored.
    let text = text.trim();
    if !text.starts_with('{') {
        return None;
    }
    let wire: GuestWire = serde_json::from_str(text).ok()?;
    if wire.ready == Some(true) {
        return Some(GuestLine::Ready {
            features: wire.features,
        });
    }
    if let Some(code) = wire.exit {
        return Some(GuestLine::Exit { code });
    }
    if let Some(publish) = wire.publish {
        return Some(GuestLine::Publish { publish });
    }
    if let Some(output) = wire.output {
        return Some(GuestLine::Output { output });
    }
    wire.done.map(|done| GuestLine::Done { done })
}

/// Render one host→guest line, newline included. Degrades to `{}` for the
/// same reason `guest_line` does — the host is not PID 1, but the two
/// renderers should not differ in a way a reader has to rediscover.
pub fn host_line(line: &HostLine) -> String {
    let wire = match line {
        HostLine::Signal { name } => HostWire {
            signal: Some(name.clone()),
            ..Default::default()
        },
        HostLine::Params { params } => HostWire {
            params: Some(params.clone()),
            ..Default::default()
        },
        HostLine::Exec { exec } => HostWire {
            exec: Some(exec.clone()),
            ..Default::default()
        },
        HostLine::Stdin { stdin } => HostWire {
            stdin: Some(stdin.clone()),
            ..Default::default()
        },
        HostLine::Kill { kill } => HostWire {
            kill: Some(kill.clone()),
            ..Default::default()
        },
    };
    let mut s = serde_json::to_string(&wire).unwrap_or_else(|_| String::from("{}"));
    s.push('\n');
    s
}

/// Parse one host→guest line; `None` for anything unknown, and the first
/// known field in declaration order when a line carries two — both for the
/// reasons `parse_guest_line` gives.
pub fn parse_host_line(text: &str) -> Option<HostLine> {
    // A control line is a JSON *object*. serde's derive would also accept a
    // positional array of the same arity, which would let a future line shape
    // be misread as a command rather than ignored.
    let text = text.trim();
    if !text.starts_with('{') {
        return None;
    }
    let wire: HostWire = serde_json::from_str(text).ok()?;
    if let Some(name) = wire.signal {
        return Some(HostLine::Signal { name });
    }
    if let Some(params) = wire.params {
        return Some(HostLine::Params { params });
    }
    if let Some(exec) = wire.exec {
        return Some(HostLine::Exec { exec });
    }
    if let Some(stdin) = wire.stdin {
        return Some(HostLine::Stdin { stdin });
    }
    wire.kill.map(|kill| HostLine::Kill { kill })
}

// --------------------------------------------------------------- base64
//
// A command's output is not text — it is whatever the program wrote — and a
// JSON string holds only valid UTF-8. So output crosses base64-encoded.
//
// Hand-rolled rather than taken as a dependency, for the reason the header
// gives: this crate links into a static init inside an initramfs with a size
// budget, and the whole codec is thirty lines. It is also the only code here
// that touches attacker-shaped bytes in bulk, so it indexes nothing and
// returns `None` rather than failing.

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn b64_encode(bytes: &[u8]) -> String {
    let symbol = |n: u32| -> char { B64_ALPHABET[(n & 63) as usize] as char };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = |i: usize| -> u32 { chunk.get(i).copied().unwrap_or(0) as u32 };
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        out.push(symbol(n >> 18));
        out.push(symbol(n >> 12));
        out.push(if chunk.len() > 1 { symbol(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { symbol(n) } else { '=' });
    }
    out
}

/// The inverse. `None` for anything that is not base64 — padding and
/// newlines are skipped, every other stray byte is a refusal, because a
/// chunk that decoded "mostly" would corrupt a caller's output silently.
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for byte in text.bytes() {
        let six = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        acc = (acc << 6) | six as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Why a byte range refused to be a spec disk.
#[derive(Debug)]
pub enum SpecError {
    /// This is not our disk: the leading bytes are not `SPEC_MAGIC`. Never
    /// returned for a disk `is_spec_disk` accepted — see `decode_spec_disk`.
    Magic,
    /// It is our disk and it is cut short: the magic matched but the length
    /// field or the body it points at runs past the end of the bytes.
    Truncated,
    /// The JSON body would not encode or decode.
    Json(serde_json::Error),
    /// The encoded body does not fit the format's u32 length field.
    TooLarge(usize),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::Magic => write!(f, "not a ply spec disk (bad magic)"),
            SpecError::Truncated => write!(f, "spec disk truncated"),
            SpecError::Json(e) => write!(f, "spec disk JSON: {e}"),
            SpecError::TooLarge(n) => {
                write!(f, "spec disk body is {n} bytes, over the u32 length field")
            }
        }
    }
}

impl std::error::Error for SpecError {}

/// `PLYSPEC1` + u32 LE length + JSON, zero-padded to a whole sector.
pub fn encode_spec_disk(spec: &SpecDisk) -> Result<Vec<u8>, SpecError> {
    let json = serde_json::to_vec(spec).map_err(SpecError::Json)?;
    let mut out = Vec::with_capacity(SPEC_MAGIC.len() + 4 + json.len() + SECTOR);
    out.extend_from_slice(SPEC_MAGIC);
    let len = u32::try_from(json.len()).map_err(|_| SpecError::TooLarge(json.len()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&json);
    let pad = (SECTOR - out.len() % SECTOR) % SECTOR;
    out.resize(out.len() + pad, 0);
    Ok(out)
}

/// Read a spec disk back out of a device's bytes.
///
/// Three things worth knowing, because this runs in a guest where a failure
/// is invisible:
///
/// - **It never allocates from the length field.** The length only slices the
///   bytes it was handed, so a corrupt or hostile field is a `Truncated`, not
///   a multi-gigabyte reservation inside a VM with a memory budget.
/// - **It ignores the sector padding.** The image is zero-filled to a whole
///   sector and virtio-blk hands the guest whole sectors, so `bytes` is
///   normally longer than the body; only the length field decides where the
///   JSON ends.
/// - **It distinguishes truncation from a foreign disk.** `Magic` means
///   "this is not our disk" and nothing else; `Truncated` means "it is ours
///   and it is cut short". The invariant that makes the pair usable, pinned
///   by `every_disk_the_scan_accepts_never_fails_to_decode_with_bad_magic`:
///   **anything `is_spec_disk` accepts never fails here with `Magic`.** Were
///   that not so, a truncated read of the right device would report bad magic
///   and send a reader hunting a device-ordering bug that does not exist.
pub fn decode_spec_disk(bytes: &[u8]) -> Result<SpecDisk, SpecError> {
    if bytes.len() < SPEC_MAGIC.len() || &bytes[..SPEC_MAGIC.len()] != SPEC_MAGIC {
        return Err(SpecError::Magic);
    }
    if bytes.len() < SPEC_MAGIC.len() + 4 {
        return Err(SpecError::Truncated);
    }
    let body_at = SPEC_MAGIC.len() + 4;
    // The length check above already proves these four bytes are there, so
    // the `else` is unreachable — but it is written as a branch rather than
    // an `.expect()` because this decodes attacker-shaped bytes inside PID 1
    // under `panic = "abort"`, and the crate header's rule is that no input
    // can reach a panic here. `Truncated` is also the honest answer for a
    // disk too short to hold its own length field.
    let Ok(len_bytes) = <[u8; 4]>::try_from(&bytes[SPEC_MAGIC.len()..body_at]) else {
        return Err(SpecError::Truncated);
    };
    let len = u32::from_le_bytes(len_bytes) as usize;
    let body = bytes
        .get(body_at..body_at + len)
        .ok_or(SpecError::Truncated)?;
    serde_json::from_slice(body).map_err(SpecError::Json)
}

/// Does this disk's head carry the spec-disk magic? The guest calls this on
/// every attached device (ruling R0-5).
///
/// `head` needs only the first `SPEC_MAGIC.len()` (8) bytes of the device;
/// a caller reading a sector or a page passes the whole thing and the extra
/// is ignored. A shorter head — including an empty one, which a first read of
/// a device can legitimately return — is `false`, never a panic.
///
/// A `true` here promises only that `decode_spec_disk` will not answer
/// `SpecError::Magic`; the disk may still be truncated or carry bad JSON.
pub fn is_spec_disk(head: &[u8]) -> bool {
    head.len() >= SPEC_MAGIC.len() && &head[..SPEC_MAGIC.len()] == SPEC_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SpecDisk {
        SpecDisk {
            restores: vec![],
            entrypoint: vec![
                "/opt/db/bin/postgres".into(),
                "-D".into(),
                "/var/lib/pg".into(),
            ],
            workdir: "/opt/db".into(),
            user: Some(UserSpec {
                name: "postgres".into(),
                uid: 70,
                gid: 70,
            }),
            env: vec![("POSTGRES_PASSWORD".into(), "hunter2".into())],
            hostname: "db".into(),
            hosts: vec![("10.77.0.3".into(), "web.ply".into())],
            dns: Some("10.77.0.1".into()),
            net: Some(NetSpec {
                ip: "10.77.0.2".into(),
                prefix_len: 16,
                gateway: "10.77.0.1".into(),
            }),
            volumes: vec![VolumeSpec {
                path: "/var/lib/pg".into(),
                dev: "/dev/vdc".into(),
            }],
            params_seed: vec![("db".into(), vec![("state".into(), "starting".into())])],
            layer_count: 2,
            clock: Some(ClockSpec {
                secs: 1_757_000_000,
                nanos: 5,
            }),
            shares: vec![ShareSpec {
                tag: "share0".into(),
                path: "/opt/app/src".into(),
            }],
        }
    }

    #[test]
    fn a_spec_disk_round_trips_through_its_own_bytes() {
        let image = encode_spec_disk(&sample()).unwrap();
        assert_eq!(
            &image[..SPEC_MAGIC.len()],
            SPEC_MAGIC,
            "the magic leads, so a scan can find this disk"
        );
        assert!(
            is_spec_disk(&image),
            "the scan the guest runs must recognise it"
        );
        let back = decode_spec_disk(&image).unwrap();
        assert_eq!(back.entrypoint, sample().entrypoint);
        assert_eq!(back.env, sample().env);
        assert_eq!(back.volumes[0].dev, "/dev/vdc");
        assert_eq!(
            back.net.as_ref().map(|n| n.ip.as_str()),
            Some("10.77.0.2"),
            "the guest configures eth0 from the disk, not from DHCP"
        );
    }

    /// The compatibility rule, exercised on the field this task added: a
    /// disk written before the switch existed must still decode, and must
    /// mean "this instance has no network card" rather than failing.
    #[test]
    fn a_disk_written_before_the_network_existed_still_decodes() {
        let mut older = sample();
        older.net = None;
        let json = serde_json::to_string(&older).expect("encode");
        assert!(!json.contains("\"prefix_len\""), "nothing to carry");
        let mut without: serde_json::Value = serde_json::from_str(&json).expect("value");
        without
            .as_object_mut()
            .expect("an object")
            .remove("net")
            .expect("the field is present as null");
        let back: SpecDisk =
            serde_json::from_value(without).expect("a disk with no `net` field at all");
        assert_eq!(back.net, None);
        assert_eq!(back.entrypoint, older.entrypoint);
    }

    #[test]
    fn the_image_is_padded_to_a_whole_number_of_sectors() {
        let image = encode_spec_disk(&sample()).unwrap();
        assert_eq!(
            image.len() % 512,
            0,
            "virtio-blk hands the guest whole sectors"
        );
    }

    #[test]
    fn a_disk_from_a_host_without_a_clock_or_shares_still_decodes() {
        // The compatibility rule in `SpecDisk`'s doc: every field added
        // after v1 defaults, so an older host's disk boots in a newer guest.
        // The guest then leaves the clock alone and mounts nothing extra.
        let json =
            r#"{"entrypoint":["/bin/true"],"workdir":"/","env":[],"hostname":"h","layer_count":1}"#;
        let mut bytes = SPEC_MAGIC.to_vec();
        bytes.extend_from_slice(&(json.len() as u32).to_le_bytes());
        bytes.extend_from_slice(json.as_bytes());
        let spec = decode_spec_disk(&bytes).expect("a v1 disk decodes");
        assert_eq!(spec.clock, None);
        assert!(spec.shares.is_empty());
        // …and the new fields round-trip when present.
        let mut spec = spec;
        spec.clock = Some(ClockSpec {
            secs: 1_800_000_000,
            nanos: 42,
        });
        spec.shares = vec![ShareSpec {
            tag: "share0".into(),
            path: "/opt/app/src".into(),
        }];
        let again = decode_spec_disk(&encode_spec_disk(&spec).unwrap()).unwrap();
        assert_eq!(again.clock, spec.clock);
        assert_eq!(again.shares, spec.shares);
    }

    #[test]
    fn a_disk_that_is_not_a_spec_disk_is_refused_not_guessed() {
        assert!(decode_spec_disk(b"hsqs\0\0\0\0nonsense").is_err());
        assert!(
            !is_spec_disk(b"hsqs\0\0\0\0nonsense"),
            "a squashfs layer is not the spec disk"
        );
        // A short head is the one branch with a panic behind it, and the
        // guest's first read of a device may well be short.
        assert!(!is_spec_disk(b"PLY"));
        assert!(!is_spec_disk(b""));
        // Right magic, length longer than the buffer: still a refusal.
        let mut truncated = SPEC_MAGIC.to_vec();
        truncated.extend_from_slice(&9999u32.to_le_bytes());
        truncated.extend_from_slice(b"{}");
        assert!(decode_spec_disk(&truncated).is_err());
    }

    #[test]
    fn guest_lines_are_newline_delimited_json_in_both_directions() {
        // A guest with nothing to advertise renders exactly the line it
        // always did, so an older host reads it unchanged.
        let line = guest_line(&GuestLine::Ready { features: vec![] });
        assert_eq!(line, "{\"ready\":true}\n");
        // …and one that can run commands says so, in a field an older host
        // ignores.
        let with = guest_line(&GuestLine::Ready {
            features: vec![FEATURE_EXEC.to_string()],
        });
        assert_eq!(with, "{\"ready\":true,\"features\":[\"exec\"]}\n");
        assert_eq!(
            parse_guest_line(&with),
            Some(GuestLine::Ready {
                features: vec!["exec".to_string()]
            })
        );
        // An older guest sends no features at all, which reads as none.
        assert_eq!(
            parse_guest_line("{\"ready\":true}"),
            Some(GuestLine::Ready { features: vec![] })
        );
        assert!(
            !line
                .strip_suffix('\n')
                .expect("exactly one trailing newline")
                .contains('\n'),
            "a control line must never contain its own delimiter"
        );
        assert!(matches!(
            parse_guest_line(r#"{"exit":3}"#).unwrap(),
            GuestLine::Exit { code: 3 }
        ));
        assert!(matches!(
            parse_host_line(r#"{"signal":"TERM"}"#).unwrap(),
            HostLine::Signal { name } if name == "TERM"
        ));
        assert_eq!(
            host_line(&HostLine::Signal {
                name: "TERM".into()
            }),
            "{\"signal\":\"TERM\"}\n"
        );
    }

    #[test]
    fn neither_renderer_can_panic_and_its_fallback_line_is_one_both_parsers_drop() {
        // `guest_line` runs on the watcher thread of PID 1 under
        // `panic = "abort"`: a panic there is not a lost line, it is
        // `Attempted to kill init!` from inside a VM. Serialising these
        // types cannot fail, so what this pins is the SHAPE OF THE FALLBACK
        // rather than a reachable case — `{}` must be a line the other side
        // ignores, or a degraded render would be worse than a dropped one.
        assert!(
            parse_host_line("{}\n").is_none(),
            "the guest's fallback line must be inert on the host's parser"
        );
        assert!(
            parse_guest_line("{}\n").is_none(),
            "and the host's fallback line inert on the guest's"
        );
        // Every variant still renders its real line, so the fallback is not
        // quietly the normal path.
        for line in [
            GuestLine::Ready { features: vec![] },
            GuestLine::Exit { code: 7 },
            GuestLine::Publish {
                publish: Publish {
                    key: "finish_boot".into(),
                    value: "ok".into(),
                },
            },
        ] {
            let rendered = guest_line(&line);
            assert_ne!(rendered, "{}\n", "{line:?} must not render as the fallback");
            assert_eq!(parse_guest_line(&rendered), Some(line));
        }
        for line in [
            HostLine::Signal {
                name: "TERM".into(),
            },
            HostLine::Params {
                params: vec![("web".into(), vec![("state".into(), "healthy".into())])],
            },
        ] {
            let rendered = host_line(&line);
            assert_ne!(rendered, "{}\n", "{line:?} must not render as the fallback");
            assert_eq!(parse_host_line(&rendered), Some(line));
        }
    }

    #[test]
    fn base64_round_trips_every_byte_and_every_length() {
        // Every byte value, so no alphabet entry is wrong; every length mod
        // 3, so both padding cases are covered.
        let all: Vec<u8> = (0..=255u8).collect();
        for len in 0..=all.len() {
            let slice = &all[..len];
            let encoded = b64_encode(slice);
            assert_eq!(encoded.len() % 4, 0, "len {len} is not padded to a quantum");
            assert_eq!(b64_decode(&encoded).as_deref(), Some(slice), "len {len}");
        }
        // The canonical vectors, so this is standard base64 and not a
        // private dialect that only round-trips with itself.
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_refuses_what_is_not_base64() {
        assert_eq!(b64_decode("Zm9v*"), None);
        assert_eq!(b64_decode("Zm 9v"), None);
        // Padding and newlines are noise, not data.
        assert_eq!(b64_decode("Zm9v\n").as_deref(), Some(&b"foo"[..]));
    }

    #[test]
    fn an_exec_request_and_its_answer_cross_the_wire() {
        // The whole loop, as the host and guest actually speak it.
        let request = HostLine::Exec {
            exec: ExecRequest {
                id: 7,
                argv: vec!["sh".into(), "-c".into(), "echo hi".into()],
                env: vec![("X".into(), "1".into())],
                cwd: Some("/opt/app".into()),
            },
        };
        let text = host_line(&request);
        assert!(text.ends_with('\n'), "the newline is the frame");
        assert_eq!(parse_host_line(&text), Some(request));

        // Output is arbitrary bytes, including invalid UTF-8, which is the
        // reason it is base64 and not a JSON string.
        let raw = vec![0x00, 0xff, b'h', b'i', 0x80];
        let out = GuestLine::Output {
            output: ExecOutput {
                id: 7,
                stream: STREAM_STDERR,
                data: b64_encode(&raw),
            },
        };
        let parsed = parse_guest_line(&guest_line(&out)).expect("output parses");
        let GuestLine::Output { output } = &parsed else {
            panic!("expected output, got {parsed:?}");
        };
        assert_eq!(b64_decode(&output.data), Some(raw));

        let done = GuestLine::Done {
            done: ExecDone {
                id: 7,
                code: 127,
                error: Some("not on PATH".into()),
            },
        };
        assert_eq!(parse_guest_line(&guest_line(&done)), Some(done));
        // A plain success carries no error field at all.
        let ok = guest_line(&GuestLine::Done {
            done: ExecDone {
                id: 7,
                code: 0,
                error: None,
            },
        });
        assert!(!ok.contains("error"), "{ok}");
    }

    #[test]
    fn stdin_and_kill_reach_a_running_command() {
        for line in [
            HostLine::Stdin {
                stdin: ExecStdin {
                    id: 1,
                    data: b64_encode(b"data\n"),
                    eof: false,
                },
            },
            HostLine::Stdin {
                stdin: ExecStdin {
                    id: 1,
                    data: String::new(),
                    eof: true,
                },
            },
            HostLine::Kill {
                kill: ExecKill {
                    id: 1,
                    name: "TERM".into(),
                },
            },
        ] {
            assert_eq!(parse_host_line(&host_line(&line)), Some(line));
        }
    }

    #[test]
    fn an_older_guest_ignores_exec_instead_of_dying() {
        // The compatibility rule: a line this build does not know is
        // nothing, never an error. An init from before exec existed keeps
        // running the app when a newer host asks it to run a command.
        assert_eq!(parse_host_line("{\"neverheardofit\":{\"id\":1}}"), None);
        assert_eq!(parse_guest_line("{\"neverheardofit\":1}"), None);
    }

    #[test]
    fn an_unknown_line_is_ignored_not_fatal() {
        // Either side may be newer than the other; a line it does not know
        // must not end the instance.
        assert!(parse_host_line(r#"{"future":{"x":1}}"#).is_none());
        assert!(parse_guest_line(r#"{"future":{"x":1}}"#).is_none());
        assert!(parse_host_line("not json at all").is_none());
        assert!(parse_host_line(r#"{"signal":"TERM"}"#).is_some());

        // A control line is a JSON object. serde's derive for a struct also
        // accepts a positional array of the same arity, which would let a
        // future line shape be read as a command nobody sent. Every variant
        // is reachable that way, so every variant's array shape is checked.
        assert!(parse_guest_line("[true,null,null]").is_none());
        assert!(parse_guest_line("[null,3,null]").is_none());
        assert!(parse_guest_line(r#"[null,null,{"key":"k","value":"v"}]"#).is_none());
        assert!(parse_guest_line(r#"[null,null,["k","v"]]"#).is_none());
        assert!(parse_host_line(r#"["TERM",null]"#).is_none());
        assert!(parse_host_line(r#"[null,[["a",[["k","v"]]]]]"#).is_none());
        // Leading whitespace does not smuggle one past the guard.
        assert!(parse_guest_line("  [true,null,null]  ").is_none());
        assert!(parse_host_line("  [\"TERM\",null]  ").is_none());
        // And the guard does not cost us a well-formed line.
        assert!(parse_guest_line("  {\"ready\":true}  ").is_some());
    }

    #[test]
    fn a_line_with_two_known_fields_yields_the_first_in_declaration_order() {
        // Senders never emit two; this pins what a reader does if one ever
        // does, so the two sides cannot disagree about it.
        assert!(matches!(
            parse_guest_line(r#"{"ready":true,"exit":7}"#).unwrap(),
            GuestLine::Ready { .. }
        ));
        assert!(matches!(
            parse_host_line(r#"{"signal":"TERM","params":[]}"#).unwrap(),
            HostLine::Signal { name } if name == "TERM"
        ));
    }

    #[test]
    fn every_disk_the_scan_accepts_never_fails_to_decode_with_bad_magic() {
        // `is_spec_disk` reads the magic alone, so a device carrying the
        // magic and nothing after it is one the scan accepts. Decoding it
        // must then say "truncated": "bad magic" would send whoever reads
        // that message hunting a device-ordering bug that does not exist.
        for tail in [b"".as_slice(), b"\x01", b"\x01\x02", b"\x01\x02\x03"] {
            let head = [SPEC_MAGIC.as_slice(), tail].concat();
            assert!(
                is_spec_disk(&head),
                "the magic leads, so the scan accepts this device"
            );
            let err = decode_spec_disk(&head).expect_err("no length field, no body");
            assert!(
                matches!(err, SpecError::Truncated),
                "the magic was right and the disk was cut short, got: {err}"
            );
        }
        // The converse still holds: a foreign disk is `Magic`, not truncation.
        assert!(matches!(
            decode_spec_disk(b"hsqs\0\0\0\0nonsense").unwrap_err(),
            SpecError::Magic
        ));
        assert!(matches!(
            decode_spec_disk(b"PLY").unwrap_err(),
            SpecError::Magic
        ));
    }

    #[test]
    fn a_disk_written_by_an_older_host_still_decodes() {
        // The compatibility rule on `SpecDisk`: every field added after the
        // magic's version digit is `#[serde(default)]`, so a disk carrying
        // only the five that have no safe default still decodes; and an
        // unknown field from a newer host is dropped, not fatal.
        let json = br#"{"entrypoint":["/bin/sh"],"workdir":"/","env":[],"hostname":"db","layer_count":1,"future_field":{"x":1}}"#;
        let mut image = SPEC_MAGIC.to_vec();
        image.extend_from_slice(&(json.len() as u32).to_le_bytes());
        image.extend_from_slice(json);
        let back = decode_spec_disk(&image).expect("older and newer disks both decode");
        assert_eq!(back.entrypoint, vec!["/bin/sh".to_string()]);
        assert_eq!(back.user, None);
        assert_eq!(back.dns, None);
        assert!(back.hosts.is_empty());
        assert!(back.volumes.is_empty());
        assert!(back.params_seed.is_empty());
    }

    #[test]
    fn the_parent_owned_names_are_pinned_because_a_dropped_one_is_a_forgeable_fact() {
        // This list is a security boundary, not a convenience: every name in
        // it is a file the app must not be able to write, and a name that
        // silently leaves the list is a fact an app can forge about itself.
        // Spelled out here rather than derived, so a drop is a test failure
        // and not a behaviour change nobody notices.
        assert_eq!(
            PARENT_OWNED,
            ["state", "instances", "started_at", "restarts"]
        );
        assert!(
            PARENT_OWNED.contains(&"state"),
            "`state` is the fact every --after dependant gates on"
        );
    }
}
