//! The macOS microVM suite: it boots real VMs on Hypervisor.framework, so it
//! runs only on an Apple Silicon Mac and only when asked, with `make
//! mac-test`.
//!
//! Two things must be true before any of it runs, and each one is checked
//! with a message that names the fix rather than a failure that does not:
//!
//! * `PLY_MICROVM_KERNEL` points at a built kernel (a directory holding
//!   `microvm-kernel.img` and `initramfs.cpio`, from
//!   `scripts/build-microvm-kernel.sh`). Without it these tests would fetch
//!   the pinned keg from the registry, which is not what a local bring-up
//!   run wants to be testing.
//! * The binary under test carries the `com.apple.security.hypervisor`
//!   entitlement. `hv_vm_create` checks it at the call, not at load, so an
//!   unsigned binary fails deep inside the VMM with a message that names
//!   nothing; the suite signs its own copy — see [`ply`].
//!
//! The app images are built by the `ply` binary under test, against the
//! `debian` keg. That keg is fetched once and then lives in the store, so
//! only the very first run of this suite needs the network.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// A SIGNED copy of the binary under test.
///
/// `hv_vm_create` checks `com.apple.security.hypervisor` at the call, not at
/// load, so an unsigned `ply` fails inside the VMM with a permission error
/// that names nothing. Signing `target/debug/ply` in place does not survive:
/// cargo re-uplifts that path from `target/debug/deps/` on its next
/// invocation and the signature goes with it. So the suite takes its own
/// copy, once, and signs that — which also makes
/// `cargo test -p ply-cli --test macos_vm` work on its own, without the
/// Makefile target around it.
fn ply() -> &'static Path {
    static SIGNED: OnceLock<PathBuf> = OnceLock::new();
    SIGNED.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("ply-mac-test-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory for the signed binary");
        let path = dir.join("ply");
        std::fs::copy(env!("CARGO_BIN_EXE_ply"), &path).expect("copy the ply binary");
        let plist = dir.join("hv.entitlements");
        std::fs::write(
            &plist,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\
             <key>com.apple.security.hypervisor</key><true/></dict></plist>\n",
        )
        .expect("write the entitlements");
        let out = Command::new("codesign")
            .arg("--entitlements")
            .arg(&plist)
            .args(["--force", "-s", "-"])
            .arg(&path)
            .output()
            .expect("run codesign");
        assert!(
            out.status.success(),
            "codesign failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        path
    })
}

/// The kernel to boot, or `None` when this machine has not been set up for
/// the suite. Every test returns early in that case rather than failing:
/// `cargo test --workspace` on a developer's Mac must not turn into a VM
/// bring-up requirement.
fn kernel() -> Option<String> {
    match std::env::var("PLY_MICROVM_KERNEL") {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            eprintln!(
                "SKIP: PLY_MICROVM_KERNEL is unset — build a kernel with \
                 scripts/build-microvm-kernel.sh and point it at the keg's boot/ directory"
            );
            None
        }
    }
}

/// A scratch root for one test: its own `PLY_DATA_DIR` (so volumes never
/// leak between tests or between a test and the developer's own instances)
/// and its own app directory. Removed on drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("ply-mac-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).expect("scratch dir");
        std::fs::create_dir_all(dir.join("data")).expect("scratch data dir");
        Scratch { dir }
    }

    fn app_dir(&self) -> PathBuf {
        self.dir.join("app")
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.join("data")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Write a manifest and build it into an arm64 image. Returns the image path.
fn build_app(scratch: &Scratch, manifest: &str) -> PathBuf {
    let dir = scratch.app_dir();
    std::fs::write(dir.join("ply.toml"), manifest).expect("write ply.toml");
    let out = Command::new(ply())
        .args(["build", ".", "--arch", "arm64"])
        .current_dir(&dir)
        .output()
        .expect("run ply build");
    assert!(
        out.status.success(),
        "ply build failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let image = std::fs::read_dir(&dir)
        .expect("read app dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "img"))
        .expect("ply build wrote an .img");
    image
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// `ply run <image>` against the scratch data directory, with stdout and
/// stderr kept apart — the split is itself under test.
fn run_image(scratch: &Scratch, kernel: &str, image: &Path) -> Run {
    let out = Command::new(ply())
        .arg("run")
        .arg(image)
        .env("PLY_MICROVM_KERNEL", kernel)
        .env("PLY_DATA_DIR", scratch.data_dir())
        .output()
        .expect("run ply run");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// One `DISK <name> <sectors> <first 8 bytes, hex>` line per attached disk,
/// printed by the guest itself. `/sys/block` is enumerated rather than
/// `/dev`, so the size comes from the kernel's own view of the device.
const DUMP_DISKS: &str = "for d in /sys/block/vd*; do n=${d#/sys/block/}; \
     printf \"DISK %s %s \" \"$n\" \"$(cat $d/size)\"; \
     head -c 8 /dev/$n | od -An -tx1 | tr -d \"[:space:]\"; echo; done; \
     echo MOUNTS; grep /data /proc/mounts";

fn disk_line<'a>(stdout: &'a str, name: &str) -> (&'a str, &'a str) {
    let prefix = format!("DISK {name} ");
    let line = stdout
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("the guest reported no {name}; it said:\n{stdout}"));
    let mut fields = line.split_whitespace().skip(2);
    let sectors = fields.next().expect("a sector count");
    let head = fields.next().expect("a head");
    (sectors, head)
}

/// squashfs: `hsqs`.
const SQUASHFS_MAGIC: &str = "68737173";
/// `ply_vm_proto::SPEC_MAGIC`, `PLYSPEC1`.
const SPEC_MAGIC: &str = "504c595350454331";
/// 8 GiB in 512-byte sectors — the size `vm::volume_disk` creates.
const VOLUME_SECTORS: &str = "16777216";

/// **The single load-bearing property of the disk model.**
///
/// The VMM's device tree decides the order Linux probes virtio-mmio
/// transports in, and therefore which disk is `vda`, which is `vdb`, and so
/// on. `spec_disk::volume_devs` tells the guest a volume is at a particular
/// `/dev/vdX` and the guest mounts it without a second opinion, so a
/// mismatch here has no error path at all — just a database mounting an
/// empty disk, or an older layer's binary winning inside the overlay.
///
/// Milestone 0 proved this is not hypothetical: qemu's `virt` machine hands
/// out these transports in **reverse**, and the layer passed first came up
/// as `/dev/vdb`.
///
/// So: four disks with four distinguishable heads — two squashfs layers of
/// very different sizes, one 8 GiB volume, one spec disk — asserted position
/// by position.
#[test]
fn disks_reach_the_guest_in_attach_order() {
    let Some(kernel) = kernel() else { return };
    let scratch = Scratch::new("order");
    let image = build_app(
        &scratch,
        &format!(
            r#"
[package]
name = "plytest-order"
version = "0.1.0"
entrypoint = ['/bin/sh', '-c', '{DUMP_DISKS}']

[dependencies]
debian = "13.6"

[volumes.data]
path = "/data"
"#
        ),
    );
    let run = run_image(&scratch, &kernel, &image);
    assert_eq!(
        run.code, 0,
        "the guest did not run:\nstdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );

    // 0: the app's own layer — squashfs, and the smaller of the two.
    let (app_sectors, app_head) = disk_line(&run.stdout, "vda");
    // 1: the `debian` keg — squashfs, and much larger.
    let (base_sectors, base_head) = disk_line(&run.stdout, "vdb");
    // 2: the volume — writable, 8 GiB, and neither of the two magics.
    let (vol_sectors, vol_head) = disk_line(&run.stdout, "vdc");
    // 3: the spec disk, last.
    let (_spec_sectors, spec_head) = disk_line(&run.stdout, "vdd");

    assert!(
        app_head.starts_with(SQUASHFS_MAGIC),
        "vda must be image layer 0, and it read {app_head}"
    );
    assert!(
        base_head.starts_with(SQUASHFS_MAGIC),
        "vdb must be image layer 1, and it read {base_head}"
    );
    let app: u64 = app_sectors.parse().expect("vda's size is a number");
    let base: u64 = base_sectors.parse().expect("vdb's size is a number");
    assert!(
        app < base,
        "the app's own layer comes FIRST, before the packages it depends on — the same order \
         `runtime/ns/mount.rs` overlays them in on Linux, from the same lockfile. vda was \
         {app} sectors and vdb {base}"
    );

    assert_eq!(
        vol_sectors, VOLUME_SECTORS,
        "vdc must be the 8 GiB volume disk, not a layer or the spec disk"
    );
    assert!(
        !vol_head.starts_with(SQUASHFS_MAGIC) && !vol_head.starts_with(SPEC_MAGIC),
        "vdc must be the volume; it read {vol_head}"
    );
    assert!(
        run.stdout.contains("/dev/vdc /data ext4"),
        "the guest must mount the volume at the path the spec disk named:\n{}",
        run.stdout
    );

    assert!(
        spec_head.starts_with(SPEC_MAGIC),
        "the spec disk is attached LAST, after every volume — a spec disk that landed before \
         one would shift every volume's device name. vdd read {spec_head}"
    );
    // The guest agrees, independently: it finds the spec disk by scanning
    // for the magic (ruling R0-5), and says which device answered.
    assert!(
        run.stderr.contains("spec disk: /dev/vdd (device index 3)"),
        "the guest's own scan must land on the same device:\n{}",
        run.stderr
    );
}

/// The app's exit status is the app's, and it crosses the machine boundary
/// on the control channel — not inferred from the VM stopping, which is what
/// 255 means.
#[test]
fn a_guest_that_powers_off_returns_its_exit_code_over_the_control_channel() {
    let Some(kernel) = kernel() else { return };
    let scratch = Scratch::new("exit");
    let image = build_app(
        &scratch,
        r#"
[package]
name = "plytest-exit"
version = "0.1.0"
entrypoint = ["/bin/sh", "-c", "exit 7"]

[dependencies]
debian = "13.6"
"#,
    );
    let run = run_image(&scratch, &kernel, &image);
    assert_eq!(
        run.code, 7,
        "`ply run` exits with the APP's code, not the VMM's:\nstderr:\n{}",
        run.stderr
    );
}

/// `hvc0` carries the app's stdout and stderr and nothing else; `hvc1`
/// carries the JSON control channel and never reaches a stream a user reads.
/// If the two were one device, `ply logs` would hold `{"ready":true}` and
/// nothing downstream could tell an app's output from ply's own.
#[test]
fn the_apps_stdout_arrives_on_port_zero_and_control_json_never_does() {
    let Some(kernel) = kernel() else { return };
    let scratch = Scratch::new("streams");
    let image = build_app(
        &scratch,
        r#"
[package]
name = "plytest-streams"
version = "0.1.0"
entrypoint = ["/bin/sh", "-c", "echo MARKER-STDOUT; echo MARKER-STDERR >&2"]

[dependencies]
debian = "13.6"
"#,
    );
    let run = run_image(&scratch, &kernel, &image);
    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);

    // Both of the app's streams are `hvc0`, exactly as the namespace
    // backend's single log pipe carries both.
    assert!(
        run.stdout.contains("MARKER-STDOUT") && run.stdout.contains("MARKER-STDERR"),
        "the app's output must reach the parent's stdout:\n{}",
        run.stdout
    );
    for line in ["{\"ready\":true}", "{\"exit\":", "{\"publish\":"] {
        assert!(
            !run.stdout.contains(line),
            "a control line reached the app's stdout — the two consoles are not separate:\n{}",
            run.stdout
        );
    }
    // The kernel log is the parent's stderr, so a boot failure is visible
    // without ever touching what the app said.
    assert!(
        run.stderr.contains("Run /init as init process"),
        "the kernel log belongs on stderr:\n{}",
        run.stderr
    );
}

// ---------------------------------------------------------------- network

/// A run left in the background, with its output collected on the way past.
///
/// The network tests need an instance that is still RUNNING while the test
/// talks to it, which `run_image` cannot give: it waits for the app to end.
struct Background {
    child: std::process::Child,
    log: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Background {
    /// `ply run <image> <args…>`, with stdout and stderr merged into one
    /// buffer the test can read at any point (a failure's explanation is
    /// almost always on the guest's console, not in the assertion).
    fn start(scratch: &Scratch, kernel: &str, image: &Path, args: &[&str]) -> Background {
        use std::io::Read;
        let mut child = Command::new(ply())
            .arg("run")
            .arg(image)
            .args(args)
            .env("PLY_MICROVM_KERNEL", kernel)
            .env("PLY_DATA_DIR", scratch.data_dir())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn ply run");
        let log = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        for stream in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let log = log.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut log) = log.lock() {
                        log.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            });
        }
        Background { child, log }
    }

    fn output(&self) -> String {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// Signal the run the way a shell does and wait for it to exit, giving
    /// back its code and how long the stop took. `None` means it was still
    /// alive at the deadline.
    fn signal_and_wait(
        &mut self,
        signal: &str,
        within: std::time::Duration,
    ) -> Option<(i32, std::time::Duration)> {
        let began = std::time::Instant::now();
        let _ = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(self.child.id().to_string())
            .status();
        while began.elapsed() < within {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some((status.code().unwrap_or(-1), began.elapsed()));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        // A run the test already stopped and reaped is done; signalling its
        // pid again would, on a busy machine, reach whoever inherited it.
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        // SIGTERM first, so the run tears its own VM down and removes the
        // instance directory; SIGKILL would leave both behind for the next
        // test in the same scratch.
        let _ = Command::new("kill")
            .arg(self.child.id().to_string())
            .status();
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A free TCP port on loopback, for a `--publish` the developer's own
/// machine is not already using. (5432 on a Mac with VS Code is famously
/// not free.)
fn free_host_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = listener.local_addr().expect("its number").port();
    drop(listener);
    port
}

/// Retry `f` until it answers or the deadline passes. Instance startup is a
/// kernel boot plus an overlay plus an exec, and asserting on a fixed sleep
/// is how a suite becomes flaky on a busy laptop.
fn within<T>(
    what: &str,
    seconds: u64,
    log: impl Fn() -> String,
    mut f: impl FnMut() -> Option<T>,
) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        if let Some(value) = f() {
            return value;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "{what} did not happen within {seconds}s. The run said:\n{}",
                log()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// **The acceptance test of the whole networking task, in miniature.**
///
/// A listener inside the guest, a `--publish` on the host, and a connection
/// from the Mac that reaches it. Nothing on the host can dial `10.77.0.2`,
/// so this passes only if the published pool goes through the switch —
/// which is exactly what `Instance::connector` was added for. Before it, the
/// parent bound the host port, accepted the connection, found no reachable
/// backend and reset it.
#[test]
fn a_published_port_reaches_a_listener_inside_the_guest() {
    use std::io::Read;
    let Some(kernel) = kernel() else { return };
    let scratch = Scratch::new("publish");
    let image = build_app(
        &scratch,
        r#"
[package]
name = "plytest-publish"
version = "0.1.0"
entrypoint = [
  "/usr/bin/perl", "-MIO::Socket::INET", "-e",
  "$|=1; my $s = IO::Socket::INET->new(LocalAddr=>'0.0.0.0', LocalPort=>7777, Listen=>8, ReuseAddr=>1) or die $!; print \"listening\n\"; while (my $c = $s->accept) { print $c \"hello from the guest\n\"; close $c }",
]

[dependencies]
debian = "13.6"
"#,
    );
    let port = free_host_port();
    let run = Background::start(
        &scratch,
        &kernel,
        &image,
        &["--publish", &format!("127.0.0.1:{port}:7777")],
    );

    let greeting = within(
        "the published port answered",
        60,
        || run.output(),
        || {
            let mut conn = std::net::TcpStream::connect_timeout(
                &([127, 0, 0, 1], port).into(),
                std::time::Duration::from_secs(1),
            )
            .ok()?;
            conn.set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .ok()?;
            let mut got = String::new();
            conn.read_to_string(&mut got).ok()?;
            (!got.is_empty()).then_some(got)
        },
    );
    assert_eq!(greeting.trim(), "hello from the guest");
    assert!(
        run.output()
            .contains("network: eth0 10.77.0.2/16 via 10.77.0.1"),
        "the guest must have configured itself from the spec disk:\n{}",
        run.output()
    );
}

/// The guest's own view of the network: an address the switch allocated, a
/// resolver that is the switch, and a `<name>.ply` answer for itself.
///
/// One test rather than three because a single `getent` proves all of it:
/// the answer can only come back if the NIC is up, the default route
/// reaches the gateway, UDP works, and the switch's resolver has this
/// instance in its name table.
#[test]
fn a_guest_resolves_its_own_ply_name_through_the_switch() {
    let Some(kernel) = kernel() else { return };
    let scratch = Scratch::new("dns");
    let image = build_app(
        &scratch,
        r#"
[package]
name = "plytest-dns"
version = "0.1.0"
entrypoint = [
  "/bin/sh", "-c",
  "echo RESOLV $(cat /etc/resolv.conf); echo SELF $(getent hosts plytest-dns.ply); echo SLOT $(getent hosts plytest-dns.1.ply); echo MISSING $(getent hosts nosuch.ply; echo rc=$?)",
]

[dependencies]
debian = "13.6"
"#,
    );
    let run = run_image(&scratch, &kernel, &image);
    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    assert!(
        run.stdout.contains("RESOLV nameserver 10.77.0.1"),
        "the spec disk must point the guest at the switch:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("SELF 10.77.0.2 plytest-dns.ply"),
        "the switch must answer <name>.ply from its own table:\n{}",
        run.stdout
    );
    // Instances are addressed by SLOT and `<app>` is an alias onto the
    // first of them, so both spellings answer and both name one machine.
    assert!(
        run.stdout.contains("SLOT 10.77.0.2 plytest-dns.1.ply"),
        "the per-instance name must resolve too:\n{}",
        run.stdout
    );
    // A name in ply's own suffix that nobody joined under is NXDOMAIN and is
    // never forwarded — forwarding it would leak a stack's member names to
    // the host's resolver, which could not answer it anyway.
    assert!(
        run.stdout.contains("MISSING rc=2"),
        "an unknown .ply name must be a clean NXDOMAIN:\n{}",
        run.stdout
    );
}

/// **Egress**: a guest resolves a public name and fetches over TCP. This is
/// what makes a microVM useful for builds, and it is the half the spike's
/// hand-written TCP could not have carried — the far end is the real
/// internet, with retransmits and windows and an MSS of its own.
///
/// Skipped when this Mac cannot reach the internet itself, so a test on a
/// train fails for the reason it should: not at all.
#[test]
fn a_guest_reaches_the_internet_through_the_switch() {
    let Some(kernel) = kernel() else { return };
    if std::net::TcpStream::connect_timeout(
        &"1.1.1.1:80".parse().expect("a literal address"),
        std::time::Duration::from_secs(3),
    )
    .is_err()
    {
        eprintln!("SKIP: this host has no internet, so the guest cannot be expected to");
        return;
    }
    let scratch = Scratch::new("egress");
    let image = build_app(
        &scratch,
        r#"
[package]
name = "plytest-egress"
version = "0.1.0"
entrypoint = [
  "/bin/bash", "-c",
  "getent hosts deb.debian.org > /dev/null || { echo NODNS; exit 1; }; echo DNS ok; exec 3<>/dev/tcp/deb.debian.org/80 || { echo NOTCP; exit 1; }; printf 'HEAD / HTTP/1.0\\r\\nHost: deb.debian.org\\r\\n\\r\\n' >&3; echo FETCH $(head -1 <&3)",
]

[dependencies]
debian = "13.6"
"#,
    );
    let run = run_image(&scratch, &kernel, &image);
    assert_eq!(
        run.code, 0,
        "stdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
    assert!(
        run.stdout.contains("DNS ok"),
        "the switch must forward a public name to the host's resolver:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("FETCH HTTP/1."),
        "the guest's TCP must terminate in the switch and be bridged to a real host \
         socket:\n{}",
        run.stdout
    );
}

// ------------------------------------------------------------------- stop

/// **`^C` on `ply run` must reach the app INSIDE the guest.**
///
/// The supervisor's signal handler stops an instance by `kill`ing
/// `child_pid()`. A microVM has none — it is threads in this very process,
/// not a child — so the handler reaches nothing and the main loop has to
/// send the stop signal itself, down the `hvc1` control channel, where the
/// guest init turns `{"signal":"TERM"}` back into a `kill(app_pid, SIGTERM)`.
///
/// When that step was missing the failure was silent and looked like
/// patience: the guest never saw the signal, `SHUTDOWN_GRACE` expired ten
/// seconds later, the VM was torn down, and `ply run` returned **255** — the
/// code for "it stopped without telling us why" — instead of the app's own.
/// A database in that guest never ran its shutdown checkpoint.
///
/// **Every unit test in this repo is blind to it.** The hole is not in a
/// function; it is a call that was not made, across a supervisor, a virtio
/// console, a kernel and a shell's trap handler. So this test asserts the
/// property end to end and on all three counts: the guest printed the trap's
/// own line, `ply run` returned the trap's own code, and it did so long
/// before the grace window could have produced the same exit by accident.
#[test]
fn an_interrupt_reaches_the_app_in_the_guest_and_its_own_exit_code_comes_back() {
    let Some(kernel) = kernel() else { return };
    let scratch = Scratch::new("stop");
    // The trap prints before it exits, so a pass cannot be faked by any
    // path that merely ends the VM: only the app itself can write GOT-TERM
    // on `hvc0`, and only after receiving the signal. `sleep 0.2` because a
    // POSIX shell runs a trap after the current command finishes, so the
    // sleep's length is the app's own reaction time and nothing else's.
    let image = build_app(
        &scratch,
        r#"
[package]
name = "plytest-stop"
version = "0.1.0"
entrypoint = ["/bin/sh", "-c", "trap 'echo GOT-TERM; exit 7' TERM; echo READY; while true; do sleep 0.2; done"]

[dependencies]
debian = "13.6"
"#,
    );
    let mut run = Background::start(&scratch, &kernel, &image, &[]);
    within(
        "the app never reported READY",
        90,
        || run.output(),
        || run.output().contains("READY").then_some(()),
    );

    let stop = std::time::Duration::from_secs(30);
    let (code, took) = run.signal_and_wait("INT", stop).unwrap_or_else(|| {
        panic!(
            "`ply run` never exited after ^C. It said:\n{}",
            run.output()
        )
    });
    let output = run.output();

    assert!(
        output.contains("GOT-TERM"),
        "the guest's app never received the stop signal — the supervisor's \
         request_stop step is the only thing that can deliver it to a VM. \
         The run said:\n{output}"
    );
    assert_eq!(
        code, 7,
        "`ply run` must exit with the APP's code; 255 is the tell that the \
         grace window expired and the VM was killed. The run said:\n{output}"
    );
    // SHUTDOWN_GRACE is 10s and SIGKILL follows it. Half of that is a wide
    // margin for a laptop under load and still nowhere near the escalation.
    assert!(
        took < std::time::Duration::from_secs(5),
        "the stop took {took:?} — a polite stop that reaches the guest is \
         bounded by the app's own reaction time, not by the grace window"
    );
}

// ------------------------------------------------------------------ stacks

/// `ply up` in `dir`, backgrounded, with its output collected like
/// [`Background::start`]'s.
///
/// A second constructor rather than a parameter on the first, because a
/// stack is a different shape of run: `ply up` spawns one `ply run` per
/// member and owns the switch they share, so what this holds is the PARENT
/// of the processes that own the VMs.
impl Background {
    fn up(dir: &Path, data_dir: &Path, kernel: &str) -> Background {
        use std::io::Read;
        let mut child = Command::new(ply())
            .arg("up")
            .arg("-C")
            .arg(dir)
            .env("PLY_MICROVM_KERNEL", kernel)
            .env("PLY_DATA_DIR", data_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn ply up");
        let log = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        for stream in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let log = log.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut log) = log.lock() {
                        log.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            });
        }
        Background { child, log }
    }
}

/// Write one member's `ply.toml` into `<scratch>/<name>/` and build it, so
/// `ply up` finds an image beside the manifest instead of rebuilding.
fn build_member(scratch: &Scratch, name: &str, manifest: &str) {
    let dir = scratch.dir.join(name);
    std::fs::create_dir_all(&dir).expect("a member directory");
    std::fs::write(dir.join("ply.toml"), manifest).expect("write ply.toml");
    let out = Command::new(ply())
        .args(["build", ".", "--arch", "arm64"])
        .current_dir(&dir)
        .output()
        .expect("run ply build");
    assert!(
        out.status.success(),
        "ply build failed for {name}:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// **The acceptance test for multi-member stacks: one switch, two machines,
/// a connection BY NAME.**
///
/// Everything below is only possible if `ply up` ran a switch of its own and
/// both members joined it over `--vswitch`. Each `ply run` is a separate
/// process with a separate microVM, so nothing here can be satisfied by a
/// member talking to itself:
///
/// * `--after alpha` gates on alpha's `[health] port`, and that probe runs
///   in BETA's `ply run`, which reaches alpha only by dialling the switch
///   socket alpha's state file names. Before this, the gate read every
///   macOS dependency as "not answering" and timed out.
/// * `alpha.ply` resolves inside beta's guest to alpha's address on the
///   switch — `10.77.0.2`, not `127.0.0.1`. A loopback answer is the exact
///   failure this test exists to catch: it would look wired up and talk
///   only to itself.
/// * The greeting is bytes alpha wrote, so the connection is real TCP across
///   two guests and a userspace L2, not a name that merely resolved.
#[test]
fn two_stack_members_reach_each_other_by_name() {
    let Some(kernel) = kernel() else { return };
    let scratch = Scratch::new("stack");

    // alpha listens on 7777 and greets whoever connects. `[health] port`
    // makes that listener what `--after` waits for.
    build_member(
        &scratch,
        "alpha",
        r#"
[package]
name = "plystack-alpha"
version = "0.1.0"
entrypoint = [
  "/usr/bin/perl", "-MIO::Socket::INET", "-e",
  "$|=1; my $s = IO::Socket::INET->new(LocalAddr=>'0.0.0.0', LocalPort=>7777, Listen=>8, ReuseAddr=>1) or die $!; print \"ALPHA listening\n\"; while (my $c = $s->accept) { print $c \"hello from alpha\n\"; close $c }",
]

[health]
port = 7777

[dependencies]
debian = "13.6"
"#,
    );

    // beta resolves the name, dials it, and prints what came back.
    build_member(
        &scratch,
        "beta",
        r#"
[package]
name = "plystack-beta"
version = "0.1.0"
entrypoint = [
  "/bin/bash", "-c",
  "echo BETA-RESOLVED $(getent hosts alpha.ply); exec 3<>/dev/tcp/alpha.ply/7777 || { echo BETA-NOCONNECT; exit 1; }; echo BETA-GREETING $(head -1 <&3); echo BETA-DISCOVERY $ALPHA_ADDR; sleep 120",
]

[dependencies]
debian = "13.6"
"#,
    );

    // alpha is published so that `--after` has an address to hand beta at
    // all: `discovery_env` only speaks for a dependency that declared one.
    // The host port is picked free, because a fixed one is how a suite
    // starts failing on whichever laptop already runs something there.
    let published = free_host_port();
    std::fs::write(
        scratch.dir.join("stack.toml"),
        format!(
            r#"
[stack]
name = "plystack"
version = "0.1.0"

[[app]]
run = "./alpha"
name = "alpha"
publish = ["internal:{published}:7777"]

[[app]]
run = "./beta"
name = "beta"
after = ["alpha"]
"#
        ),
    )
    .expect("write stack.toml");

    let up = Background::up(&scratch.dir, &scratch.data_dir(), &kernel);
    within(
        "beta never printed its greeting",
        180,
        || up.output(),
        || up.output().contains("BETA-GREETING").then_some(()),
    );
    let output = up.output();

    // One switch, and `ply up` owns it.
    assert!(
        output.contains("stack network (userspace switch"),
        "`ply up` must run one switch for the stack:\n{output}"
    );
    // The gate was satisfied by a probe that went through the switch. Its
    // failure text is the SWITCH's own ("nothing is listening on 10.77…"),
    // which no host-side `connect` to that address could ever produce.
    assert!(
        output.contains("alpha is healthy"),
        "`--after alpha` must gate on alpha's health port through the switch:\n{output}"
    );
    // The name resolved to a switch address, not to loopback.
    let resolved = output
        .lines()
        .find(|l| l.starts_with("BETA-RESOLVED "))
        .unwrap_or_else(|| panic!("beta never resolved alpha.ply:\n{output}"));
    assert!(
        resolved.contains("10.77.0.") && !resolved.contains("127.0.0.1"),
        "alpha.ply must be alpha's address on the switch, never loopback — a \
         loopback line in /etc/hosts beats DNS and points every cross-member \
         connection back into the caller's own guest. Beta said: {resolved}"
    );
    // And bytes only alpha could have written came back over it.
    assert!(
        output.contains("BETA-GREETING hello from alpha"),
        "beta must have opened a real TCP connection to alpha:\n{output}"
    );
    // `--after` also answered "and where is it?", and inside a stack's own
    // network the honest answer is the peer's name and its own port — not
    // the host side of a published proxy.
    assert!(
        output.contains("BETA-DISCOVERY alpha.ply:7777"),
        "discovery_env must hand a stack member its peer's in-network address:\n{output}"
    );
}
