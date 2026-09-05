//! PID 1 inside a ply microVM.
//!
//! The contract with the VMM, in order:
//!   vda..vd<layer_count-1>  read-only squashfs image layers, overlay order,
//!                           top (the app) first — the same order
//!                           `runtime/ns/mount.rs` uses on Linux, from the
//!                           same lockfile.
//!   then                    one writable disk per volume, and the spec disk,
//!                           both found by content, not by position.
//!
//! Everything else — env, entrypoint, user, hostname, peers, the params
//! seed — arrives in the spec disk. Nothing arrives on the kernel cmdline:
//! it is world-readable inside the guest and length-limited, and some of
//! this is secret.
//!
//! # Nothing here may panic
//!
//! `panic = "abort"` is inherited from the workspace release profile, so a
//! panic in this process does not unwind: it aborts, the kernel sees init
//! die, and the guest ends in `Attempted to kill init!` with no diagnosis
//! whatsoever, from inside a VM. **Every `.expect()` here would be a machine
//! that dies silently.** So there are none: every failure goes through
//! `fail`, which names the problem on the PL011 console — the kernel's own
//! console, and therefore what `ply run` shows on stderr — and then powers
//! the VM off deliberately, which the host reads as exit 255.
//!
//! **That rule covers the two background threads exactly as much as `boot`.**
//! `panic = "abort"` aborts the *process*, from whichever thread panics: a
//! panicking watcher is not a lost watcher, it is a dead machine, and it
//! looks identical to a panic on the main thread — `Attempted to kill init!`
//! and nothing else. Nothing spawned here may `unwrap`, `expect`, or index a
//! slice it has not just measured.
//!
//! `ply-vm-proto` carries `#![forbid(unsafe_code)]`. This crate deliberately
//! does not: it is raw syscalls by nature.
//!
//! # Why the boot half is `cfg(target_os = "linux")`
//!
//! The pure halves — `overlay`, `spec`, `volumes`' decisions, `control`'s
//! framing — are portable and compile everywhere, so Linux CI *and* the
//! macOS darwin gate both type-check them and `cargo test` runs their
//! assertions on either platform. Only `mount(2)`, `chroot(2)` and their
//! flags are Linux-shaped, and only those live below the gate.

// Off Linux, `boot` — the only caller any of these modules will ever have —
// is compiled away, so nothing on this host calls `Control::send` or
// `overlay::lowerdir` even though both are load-bearing in the guest.
// `allow`, not `expect`: on Linux the attribute is absent entirely, so a
// function that really is orphaned is still caught where it matters.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod control;
mod net_setup;
mod overlay;
mod spec;
mod volumes;

// The syscall helpers `volumes`' Linux half calls. They live in the boot
// module because that is where their comments belong, and are re-exported
// here so `volumes` does not have to know that.
#[cfg(target_os = "linux")]
pub(crate) use boot::{mount, read_exact_head, run};

fn main() {
    #[cfg(target_os = "linux")]
    boot::boot();

    #[cfg(not(target_os = "linux"))]
    {
        // Reachable only when somebody runs the host build of this binary.
        // The Linux body above is the entire program.
        eprintln!(
            "ply-guest-init is PID 1 inside a ply microVM (linux-arm64); \
             there is nothing for it to do on this host."
        );
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod boot {
    use std::ffi::CString;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicI32, Ordering};

    use ply_vm_proto::{
        GuestLine, HostLine, ParamsTree, Publish, SpecDisk, VolumeSpec, PARENT_OWNED,
    };

    use crate::control::Control;
    use crate::{net_setup, overlay, spec, volumes};

    /// How many `/dev/vdN` slots to probe before giving up. The scan stops at
    /// the first absent device anyway (they are contiguous from `vda`); this
    /// only bounds the pathological case where every slot answers.
    const MAX_DEVICES: usize = 32;

    /// How much of each device the spec-disk scan reads. Big enough that the
    /// scan's own bytes are normally the whole spec, small enough that
    /// reading it from every attached disk costs nothing.
    const SCAN_HEAD: usize = 4096;

    /// The ceiling on a re-read when the scan head turned out to be shorter
    /// than the spec. `decode_spec_disk` never allocates from the length
    /// field — it slices what it is handed — so this is the guest, not the
    /// disk, deciding how much RAM a hostile length may cost.
    const SPEC_READ_CAP: usize = 4 * 1024 * 1024;

    /// Where the image layers are mounted before the overlay is assembled.
    const LAYER_ROOT: &str = "/mnt";

    /// The assembled root, before `switch_root` makes it `/`.
    const NEWROOT: &str = "/newroot";

    // `PARENT_OWNED` is no longer defined here: it lives in `ply-vm-proto`,
    // which both sides of the machine boundary already link, so the guest and
    // the host cannot drift on which facts an app is forbidden to write.
    // `ply-core` still keeps two private copies of the same list
    // (`runtime/params_tree.rs` and `params.rs`); replacing those with the
    // shared constant is a one-line edit each, and belongs to whoever owns
    // those files next.

    /// The PL011 (`/dev/console`, `ttyAMA0`). Everything this init says goes
    /// here and nowhere else: `hvc0` belongs to the app's own stdout, so
    /// nothing ply says can be mistaken for app output.
    static CONSOLE_FD: AtomicI32 = AtomicI32::new(2);

    /// The entrypoint's pid, for the control channel's `{"signal":…}`.
    static APP_PID: AtomicI32 = AtomicI32::new(0);

    // ---------------------------------------------------------------- log

    /// Write every byte or give up; never panic, never block forever on a
    /// partial write. `println!` is not usable here: it panics if the write
    /// fails, and a panic in PID 1 aborts.
    fn write_all_fd(fd: i32, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            // SAFETY: `bytes` is a live slice and `fd` is either 2 or a
            // descriptor this process opened.
            let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
            if n > 0 {
                bytes = &bytes[n as usize..];
                continue;
            }
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
    }

    /// One diagnostic line on the kernel console.
    pub(crate) fn log(msg: &str) {
        let mut line = String::with_capacity(msg.len() + 12);
        line.push_str("ply-init: ");
        line.push_str(msg);
        line.push('\n');
        write_all_fd(CONSOLE_FD.load(Ordering::Relaxed), line.as_bytes());
    }

    /// Report and end the instance deliberately.
    ///
    /// The alternative — letting the process die — is `Attempted to kill
    /// init!` and a hung VM with no explanation, so every error path in this
    /// file that cannot continue comes here instead.
    fn fail(why: &str) -> ! {
        log(&format!("FATAL: {why}"));
        log("FATAL: the instance cannot start; powering this microVM off (the host reports 255)");
        poweroff()
    }

    fn poweroff() -> ! {
        // SAFETY: both are argument-less kernel requests.
        unsafe {
            libc::sync();
        }
        // Let the PL011 and the virtio consoles drain: a poweroff issued in
        // the same microsecond as the last write loses the one line that
        // explains the boot.
        std::thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: PSCI SYSTEM_OFF via the kernel's reboot(2).
        unsafe {
            libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
        }
        // The VMM does not implement SYSTEM_OFF. Block in the kernel rather
        // than spinning: a spin loop pins a vCPU at 100% for the life of the
        // VM, which on a laptop is a fan at full speed for nothing.
        loop {
            // SAFETY: argument-less, and it only returns on a signal.
            unsafe {
                libc::pause();
            }
        }
    }

    // ------------------------------------------------------------ syscalls

    /// `mount(2)`. Ported from the plyvm spike's guest init, with its
    /// `unwrap`s replaced: an unwrap here is a silent abort.
    pub(crate) fn mount(
        src: &str,
        target: &str,
        fstype: &str,
        flags: libc::c_ulong,
        data: &str,
    ) -> bool {
        let (Ok(s), Ok(t), Ok(f), Ok(d)) = (
            CString::new(src),
            CString::new(target),
            CString::new(fstype),
            CString::new(data),
        ) else {
            log(&format!(
                "mount {src} -> {target}: a path or option string contains a NUL byte"
            ));
            return false;
        };
        let fs_ptr = if fstype.is_empty() {
            std::ptr::null()
        } else {
            f.as_ptr()
        };
        let data_ptr: *const libc::c_void = if data.is_empty() {
            std::ptr::null()
        } else {
            d.as_ptr().cast()
        };
        // SAFETY: every pointer is a live NUL-terminated string owned above,
        // or null where the kernel accepts null.
        let r = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), fs_ptr, flags, data_ptr) };
        if r != 0 {
            log(&format!(
                "mount {src} -> {target} ({fstype}) FAILED: {}",
                std::io::Error::last_os_error()
            ));
        }
        r == 0
    }

    fn umount_detach(target: &str) {
        if let Ok(t) = CString::new(target) {
            // SAFETY: `t` is a live NUL-terminated string.
            unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) };
        }
    }

    /// Bind `path` onto itself read-only — the guest's copy of
    /// `ns/container.rs::bind_ro`, and the only thing keeping the app out of
    /// its own `state`.
    ///
    /// Undoes its own bind if the read-only remount fails, so a caller that
    /// checks the final result never mistakes a left-behind writable bind
    /// for "nothing mounted".
    fn bind_ro(path: &str) -> bool {
        if !mount(path, path, "", libc::MS_BIND, "") {
            return false;
        }
        if !mount(
            path,
            path,
            "",
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
            "",
        ) {
            umount_detach(path);
            return false;
        }
        true
    }

    /// Raw fork+execve+wait. Rust's `Command` spawns fine in ordinary
    /// worlds, but in this one (musl-static PID 1, after `switch_root`) it
    /// reports phantom ENOENTs while the raw syscall succeeds. The syscall
    /// is truth.
    ///
    /// Every allocation happens BEFORE the fork on purpose: between `fork`
    /// and `execve` only async-signal-safe functions are legal, and `malloc`
    /// is not one of them — another thread holding the allocator lock at
    /// fork time leaves the child deadlocked with no way to say so.
    ///
    /// # The child's stdin is `/dev/null`, always
    ///
    /// PID 1 is handed `/dev/console` on fds 0, 1 and 2 by the kernel, and a
    /// child that inherits fd 0 inherits a **tty nobody is attached to**. Any
    /// program that decides to ask a question then blocks in `read` forever,
    /// PID 1 blocks in `wait_for` behind it, the host never sees
    /// `{"ready":true}`, and nothing anywhere times out — the VM simply hangs
    /// with a prompt on a console no one can answer. `mke2fs` is exactly such
    /// a program — `proceed_question` does a blocking `fgets` when
    /// `proceed_delay` is `<= 0`, which it is with no `/etc/mke2fs.conf`
    /// (e2fsprogs 1.47.4, the version `E2FSVER` pins: `misc/util.c:97-126`,
    /// `misc/mke2fs.c:113` and `:2147`) — and it is the one that found this.
    ///
    /// So every child of PID 1 gets `/dev/null`: `read` returns 0, `fgets`
    /// returns NULL, and a program that wanted an answer gets EOF and decides
    /// on its own. Note what this is NOT: it is not the guard on the volume
    /// path. Clearing `isatty(0)` also clears `mke2fs`'s `CHECK_FS_EXIST`,
    /// so `mke2fs` no longer even looks for a filesystem it would be
    /// overwriting — `volumes::classify` is the only thing that decides that,
    /// and this makes sure nothing can hang while it does.
    pub(crate) fn run(prog: &str, args: &[&str]) -> i32 {
        let Ok(prog_c) = CString::new(prog) else {
            return 127;
        };
        let args_c: Result<Vec<CString>, _> = args.iter().map(|a| CString::new(*a)).collect();
        let Ok(args_c) = args_c else {
            return 127;
        };
        let mut argv: Vec<*const libc::c_char> = args_c.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        let envp = [std::ptr::null::<libc::c_char>()];
        // Opened before the fork, like every other resource here. O_CLOEXEC
        // so this descriptor does not survive into the program itself; the
        // `dup2` below clears it on fd 0, which is the copy that must.
        // SAFETY: a constant NUL-terminated path.
        let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };

        // SAFETY: the child touches only execve/_exit on pre-built pointers.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            log(&format!(
                "fork for {prog}: {}",
                std::io::Error::last_os_error()
            ));
            if null >= 0 {
                // SAFETY: opened just above and not used again.
                unsafe { libc::close(null) };
            }
            return 127;
        }
        if pid == 0 {
            // SAFETY: async-signal-safe only, on pointers built before fork.
            unsafe {
                // The `dup2` result is checked, not because it can fail with
                // two valid descriptors, but because this is the ONE path
                // that could leave fd 0 pointing at the console — the exact
                // thing the comment above swears is impossible. An unchecked
                // call makes that a claim; a checked one makes it structural,
                // for the cost of a comparison in a forked child.
                if null >= 0 && libc::dup2(null, 0) >= 0 {
                    // dup2 clears FD_CLOEXEC on the destination, except in
                    // the degenerate `null == 0` case where it is a no-op —
                    // so clear it explicitly and fd 0 is /dev/null either way.
                    libc::fcntl(0, libc::F_SETFD, 0);
                } else {
                    // No /dev/null, or a dup2 that somehow failed: a closed
                    // fd 0 gives EBADF, which is still an answer. Blocking is
                    // the one outcome that must be impossible.
                    libc::close(0);
                }
                libc::execve(prog_c.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }
        if null >= 0 {
            // SAFETY: opened above; the child has its own copy.
            unsafe { libc::close(null) };
        }
        wait_for(pid)
    }

    /// Wait for one specific pid, reaping anything else that turns up.
    fn wait_for(pid: i32) -> i32 {
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: `status` is a live local.
            let got = unsafe { libc::waitpid(-1, &mut status, 0) };
            if got < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // ECHILD: the process we wanted is already gone.
                return 255;
            }
            if got != pid {
                continue; // an orphan reparented to init
            }
            return exit_code(status);
        }
    }

    /// A wait status as the exit code the host will see. `128 + signal`
    /// matches `ns/exec.rs` and `ns/container.rs`, so a SIGKILLed app reads
    /// the same on both backends.
    fn exit_code(status: libc::c_int) -> i32 {
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            255
        }
    }

    /// Up to `max` bytes from the head of a device. `None` only when the
    /// device cannot be opened or read; a device SHORTER than `max` returns
    /// what it has, which matters because a spec disk is padded to a sector
    /// and can legitimately be 512 bytes long.
    fn read_head(dev: &str, max: usize) -> Option<Vec<u8>> {
        let file = std::fs::File::open(dev).ok()?;
        let mut buf = Vec::new();
        file.take(max as u64).read_to_end(&mut buf).ok()?;
        Some(buf)
    }

    /// Exactly `len` bytes, or `None`. The volume code needs this rather
    /// than `read_head`, and the reason is the one destructive path in this
    /// file: `volumes::classify` refuses a head shorter than `HEAD_BYTES`, so
    /// the pair is safe only while this function never returns a short
    /// answer. `a_short_device_reads_as_nothing_not_as_a_head` pins it
    /// against a real file, because the invariant is asserted in a comment in
    /// two other places and asserted by nothing.
    pub(crate) fn read_exact_head(dev: &str, len: usize) -> Option<Vec<u8>> {
        let mut file = std::fs::File::open(dev).ok()?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    fn mkdir_p(path: &str) -> bool {
        match std::fs::create_dir_all(path) {
            Ok(()) => true,
            Err(e) => {
                log(&format!("mkdir -p {path}: {e}"));
                false
            }
        }
    }

    fn write_file(path: &str, contents: &str) -> bool {
        match std::fs::write(path, contents) {
            Ok(()) => true,
            Err(e) => {
                log(&format!("write {path}: {e}"));
                false
            }
        }
    }

    // ------------------------------------------------------------- the boot

    pub fn boot() -> ! {
        // --- 0. the bare minimum to see anything at all ------------------
        open_console();
        log(&format!(
            "ply-guest-init {} is PID {}",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        ));

        if !mkdir_p("/proc") || !mount("proc", "/proc", "proc", 0, "") {
            fail("/proc would not mount — nothing below this point can report why");
        }
        let _ = mkdir_p("/sys");
        mount("sysfs", "/sys", "sysfs", 0, "");
        // CONFIG_DEVTMPFS_MOUNT is a DOCUMENTED no-op for an initramfs boot
        // (drivers/base/Kconfig says so), so init mounts /dev itself or there
        // are no disk nodes to find.
        let _ = mkdir_p("/dev");
        if !mount("devtmpfs", "/dev", "devtmpfs", 0, "") {
            fail("/dev (devtmpfs) would not mount — no block device nodes exist without it");
        }

        // --- 2. the spec disk --------------------------------------------
        // Out of the spec's numbered order on purpose, and it has to be:
        // step 1 mounts `layer_count` layers, and `layer_count` is in here.
        wait_for_first_disk();
        let spec = read_spec_disk();
        log(&format!(
            "spec: hostname={} layers={} volumes={} entrypoint={:?}",
            spec.hostname,
            spec.layer_count,
            spec.volumes.len(),
            spec.entrypoint.first().map(String::as_str).unwrap_or("")
        ));
        // Deliberately NOT the env: it carries the composed secrets.

        // --- 1a. the network ---------------------------------------------
        // Before the overlay rather than after, and deliberately: bringing
        // an interface up is instant, and doing it first means the app's own
        // first DNS query cannot race the interface into existence.
        configure_network(&spec);

        // --- 1. the overlay ----------------------------------------------
        mount_layers(&spec);
        assemble_overlay(&spec);

        // --- 3. volumes, by name from the spec disk, never by position ----
        for volume in &spec.volumes {
            prepare_volume(volume, &spec);
        }

        // --- 4. /etc/hosts, hostname, /etc/resolv.conf -------------------
        write_config(&spec);

        switch_root();
        mount_after_pivot();

        // --- 5. /run/ply --------------------------------------------------
        let params_rw = setup_params(&spec);

        // --- 6. ready, exec, forward signals, report the exit -------------
        // Opened before the fork (it only opens files, it starts no thread),
        // so the very first thing the host hears about this instance is its
        // `ready`, not a race with it.
        let (sender, inbound) = match open_control() {
            Some((sender, inbound)) => (Some(sender), Some(inbound)),
            None => (None, None),
        };
        let app = spawn_app(&spec);
        APP_PID.store(app, Ordering::SeqCst);
        if let Some(sender) = &sender {
            sender.send(&GuestLine::Ready);
        }
        start_threads(sender.clone(), inbound, params_rw);

        let code = wait_for(app);
        log(&format!("entrypoint exited {code}"));
        unmount_volumes(&spec);
        if let Some(sender) = &sender {
            sender.send(&GuestLine::Exit { code });
        }
        poweroff()
    }

    /// Point this init's diagnostics at the PL011.
    ///
    /// Through `open_raw` rather than a bare `libc::open`, so the console
    /// gets the same two mandatory flags every other descriptor here gets.
    /// `O_CLOEXEC` is the one that was missing: without it the original
    /// console descriptor — a high fd, not 1 or 2 — survived `execve` into
    /// the app, which is precisely the "spare handle on a channel it is not
    /// supposed to know about" that `open_raw`'s doc says must not happen.
    /// The `dup2`s below clear the flag on 1 and 2, which are the copies the
    /// app is meant to inherit.
    fn open_console() {
        // No console is not fatal: `CONSOLE_FD` stays at 2, which is what the
        // kernel gave PID 1, and the boot goes on saying what it is doing.
        let Some(fd) = open_raw(c"/dev/console", libc::O_WRONLY) else {
            return;
        };
        CONSOLE_FD.store(fd, Ordering::Relaxed);
        // Anything that reaches std's stdout/stderr lands here too, so a
        // stray `eprintln!` from a dependency cannot vanish. (Ruling R0-4:
        // when /dev/console cannot be opened, std points the standard
        // descriptors at /dev/null.)
        // SAFETY: `fd` is open; dup2 onto 1 and 2 is what a login shell does.
        unsafe {
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
        }
    }

    /// Give this instance its address on the parent's switch.
    ///
    /// Never fatal. A guest with no network still runs its entrypoint — it
    /// is what every instance did before the switch existed, and an app that
    /// needs no network should not be stopped by one that failed to come
    /// up. It IS loud: the line below is on the kernel console, which is
    /// `ply run`'s own stderr, so "published port never answered" has an
    /// explanation three lines above it rather than none.
    fn configure_network(spec: &SpecDisk) {
        let Some(net) = &spec.net else {
            log("no network in the spec disk: this instance has no NIC");
            return;
        };
        let (Ok(ip), Ok(gateway)) = (
            net.ip.parse::<std::net::Ipv4Addr>(),
            net.gateway.parse::<std::net::Ipv4Addr>(),
        ) else {
            log(&format!(
                "warning: the spec disk's addresses are not IPv4 ({} via {}); no network",
                net.ip, net.gateway
            ));
            return;
        };
        let Some(name) = net_setup::wait_for_interface() else {
            log(&format!(
                "warning: no network interface appeared in 2s (only {:?}) — the VMM attached \
                 no virtio-net device, or this kernel has no virtio_net driver; no network",
                net_setup::interfaces()
            ));
            return;
        };
        match net_setup::bring_up(&name, ip.octets(), net.prefix_len, gateway.octets()) {
            Ok(()) => log(&format!(
                "network: {name} {}/{} via {}",
                net.ip, net.prefix_len, net.gateway
            )),
            Err(e) => log(&format!("warning: {name}: {e}; no network")),
        }
    }

    /// virtio-mmio disks are probed during kernel init with built-in
    /// drivers, so they are normally there the instant init runs. Wait
    /// anyway, briefly: a device that is merely late must not read as a
    /// device that is missing.
    fn wait_for_first_disk() {
        let first = overlay::device_name(0);
        for _ in 0..60 {
            if std::path::Path::new(&first).exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        fail(&format!(
            "{first} never appeared after 3s — the VMM attached no virtio-blk disk, \
             or this kernel has no virtio-mmio transport"
        ));
    }

    /// Find and decode the spec disk (ruling R0-5: by magic, never by
    /// position).
    fn read_spec_disk() -> SpecDisk {
        let Some((index, head)) =
            spec::find_spec_disk(|dev| read_head(dev, SCAN_HEAD), MAX_DEVICES)
        else {
            fail(
                "no spec disk among the attached devices — every disk was scanned for the \
                 PLYSPEC1 magic and none carried it, so the VMM did not write one",
            );
        };
        let dev = overlay::device_name(index);
        log(&format!("spec disk: {dev} (device index {index})"));

        match ply_vm_proto::decode_spec_disk(&head) {
            Ok(spec) => return spec,
            // The magic matched, so this IS the spec disk. `Truncated` means
            // the scan head was not the whole of it (or it is damaged) — it
            // never means "keep scanning".
            Err(ply_vm_proto::SpecError::Truncated) => {}
            Err(e) => fail(&format!("spec disk {dev}: {e}")),
        }
        log(&format!(
            "spec disk {dev} is longer than the {SCAN_HEAD}-byte scan head; re-reading"
        ));
        let Some(full) = read_head(&dev, SPEC_READ_CAP) else {
            fail(&format!("spec disk {dev}: cannot re-read the device"));
        };
        match ply_vm_proto::decode_spec_disk(&full) {
            Ok(spec) => spec,
            Err(e) => fail(&format!(
                "spec disk {dev}: {e} (read {} bytes, cap {SPEC_READ_CAP}) — \
                 the magic matched, so this is our disk and it is damaged or oversized",
                full.len()
            )),
        }
    }

    /// Mount the image layers, after proving they ARE image layers.
    ///
    /// Nothing else verifies that `/dev/vd<i>` is `images[i]`: the mapping
    /// rests on the VMM's device-tree order and Linux's virtio-blk probe
    /// order agreeing, and Task 2's boot smoke test found qemu's `virt`
    /// machine handing transports out in REVERSE. A shifted mapping produces
    /// no error — just an older layer's binary winning, or a volume being
    /// mounted as a rootfs — so the squashfs magic is checked here. It costs
    /// one comparison per disk and catches every shift.
    ///
    /// It does NOT catch a permutation of the layers among themselves, and
    /// the cheapest fix for that needs no device-model change at all: the
    /// host already knows how large each layer is, so putting those sizes in
    /// the spec disk and comparing each device's size here (`BLKGETSIZE64`,
    /// or the squashfs superblock's own `bytes_used` at offset 40, which is
    /// already in the head this reads) separates the layers from each other
    /// for a handful of bytes on the wire. A per-layer marker or virtio-blk's
    /// serial field is the thorough version and is Task 8's decision; the
    /// size comparison is available to Task 7 for nearly nothing.
    fn mount_layers(spec: &SpecDisk) {
        if spec.layer_count == 0 {
            fail("spec disk says layer_count = 0 — there is no image to run");
        }
        if !mkdir_p(LAYER_ROOT) {
            fail("cannot create the layer mount root");
        }
        for index in 0..spec.layer_count {
            let dev = overlay::device_name(index);
            match read_head(&dev, 4) {
                Some(head) if head.len() >= 4 && &head[..4] == b"hsqs" => {}
                Some(_) => fail(&format!(
                    "{dev} should be image layer {index} of {} but carries no squashfs magic — \
                     the VMM's disk order and Linux's virtio-blk probe order disagree. Refusing \
                     to assemble a root filesystem out of the wrong disks.",
                    spec.layer_count
                )),
                None => fail(&format!(
                    "{dev} (image layer {index} of {}) cannot be read — fewer disks are attached \
                     than the spec disk says there are layers",
                    spec.layer_count
                )),
            }
            let target = format!("{LAYER_ROOT}/{index}");
            if !mkdir_p(&target)
                || !mount(
                    &dev,
                    &target,
                    "squashfs",
                    libc::MS_RDONLY | libc::MS_NODEV,
                    "",
                )
            {
                fail(&format!("image layer {index} ({dev}) would not mount"));
            }
        }
    }

    /// N read-only lowers, a tmpfs upper, and the assembled root at
    /// `NEWROOT`. The upper lives in guest RAM, exactly as the namespace
    /// backend's `instance_dir/rw` lives on the host's disk: writes an app
    /// makes outside a volume do not survive the instance, on either
    /// backend.
    fn assemble_overlay(spec: &SpecDisk) {
        if !mkdir_p("/rw") || !mount("tmpfs", "/rw", "tmpfs", 0, "mode=0755") {
            fail("the overlay's writable layer (tmpfs at /rw) would not mount");
        }
        if !mkdir_p("/rw/upper") || !mkdir_p("/rw/work") || !mkdir_p(NEWROOT) {
            fail("cannot create the overlay's upper/work directories");
        }
        let options = format!(
            "{},upperdir=/rw/upper,workdir=/rw/work",
            overlay::lowerdir(LAYER_ROOT, spec.layer_count)
        );
        if !mount("overlay", NEWROOT, "overlay", 0, &options) {
            fail("the overlay would not mount — the image layers are there but do not stack");
        }
        // Skeleton dirs a thin image may not ship. They land in the upper.
        for dir in ["proc", "sys", "dev", "dev/shm", "tmp", "etc", "run"] {
            let _ = std::fs::create_dir_all(format!("{NEWROOT}/{dir}"));
        }
    }

    /// One volume: format on first use, grow if the host made it bigger,
    /// mount it where the spec says, hand it to the app's user.
    fn prepare_volume(volume: &VolumeSpec, spec: &SpecDisk) {
        if !volume.path.starts_with('/') {
            fail(&format!(
                "volume path {:?} is not absolute — the spec disk is malformed",
                volume.path
            ));
        }
        let Some(head) = volumes::disk_head(&volume.dev) else {
            fail(&format!(
                "volume {} ({}): the device is missing, or it is smaller than the {} bytes \
                 `classify` reads and so cannot be a volume — refusing to guess whether it is \
                 formatted. A device that small in a volume slot is most likely somebody else's \
                 (the spec disk, or a tiny image layer) arriving here because the host's disk \
                 order shifted.",
                volume.path,
                volume.dev,
                volumes::HEAD_BYTES
            ));
        };
        // Fail CLOSED. `classify` formats a disk only when its head is
        // `HEAD_BYTES` zero bytes — the shape the host's own sparse file has
        // — and refuses everything it does not positively recognise, because
        // `mke2fs` is not undoable and this guest cannot know what it would
        // be destroying. The window is 68 KiB rather than a page precisely
        // so that btrfs and ZFS, whose own heads are zero for the first
        // 16-64 KiB, cannot read as "never formatted". A layer or the spec disk landing
        // in a volume slot (the disk order shifted) gets its own message,
        // since that is a host bug with a different remedy.
        let disk = volumes::classify(&head);
        if let Some(why) = volumes::refusal(&volume.path, &volume.dev, disk) {
            fail(&why);
        }
        let fresh = disk == volumes::Disk::Blank;
        if fresh {
            log(&format!(
                "volume {}: formatting {} (first use)",
                volume.path, volume.dev
            ));
            if let Err(e) = volumes::format(&volume.dev) {
                fail(&format!("volume {}: {e}", volume.path));
            }
        }
        let target = format!("{NEWROOT}{}", volume.path);
        if !mkdir_p(&target) {
            fail(&format!(
                "volume {}: cannot create its mount point",
                volume.path
            ));
        }
        if let Err(e) = volumes::mount_at(&volume.dev, &target) {
            fail(&volumes::unmountable(&volume.path, &volume.dev, &e, fresh));
        }
        // A freshly formatted ext4 is NOT an empty directory: `mke2fs` always
        // creates `lost+found`, and there is no flag to suppress it. On Linux
        // the same volume is a bind-mounted host directory, which really is
        // empty — so without this, the identical image behaves differently on
        // the two backends, which is the one thing this design promises it
        // will not do. postgres is the case that finds it: `initdb` refuses a
        // data directory that "exists but is not empty", naming `lost+found`
        // and exiting 1 before the database is ever created.
        //
        // Removing it is safe. `lost+found` is scratch space for `e2fsck`,
        // which recreates it when it needs it; nothing else reads it. Only on
        // a fresh volume, so a `lost+found` holding real recovered data on a
        // later boot is left exactly where it is.
        if fresh {
            let found = format!("{target}/lost+found");
            if let Ok(c) = CString::new(found.as_str()) {
                // SAFETY: `c` is a live NUL-terminated path.
                if unsafe { libc::rmdir(c.as_ptr()) } != 0 {
                    log(&format!(
                        "warning: volume {}: could not remove {found} ({}) — an app that \
                         requires an empty data directory may refuse to start",
                        volume.path,
                        std::io::Error::last_os_error()
                    ));
                }
            }
        }
        // Grow AFTER the mount, not before — measured, not preferred.
        //
        // `resize2fs` on an unmounted ext4 that was not cleanly unmounted
        // refuses outright ("Please run 'e2fsck -f' first"), and there is no
        // `e2fsck` in this initramfs to run. Mounting first makes the kernel
        // replay the journal, and `resize2fs` on the mounted device then does
        // an ONLINE resize, which the `resize_inode` feature in MKE2FS_ARGS
        // exists to make possible. So the awkward case — a host that killed
        // the VM last time and a manifest that raised the size since — is the
        // one this ordering handles and the other does not.
        //
        // It also buys a safety property that the offline path does not: the
        // kernel's ext4 online resize can only GROW, so `resize2fs` on a
        // mounted filesystem refuses a shrink outright ("On-line shrinking
        // not supported"). A manifest that LOWERS a volume's size therefore
        // cannot truncate a live filesystem here — the worst it can do is log
        // a warning below and run at the size it already had.
        if !fresh {
            if let Err(e) = volumes::grow(&volume.dev) {
                // Never fatal: a volume at its old size is far better than a
                // VM that will not boot.
                log(&format!(
                    "warning: volume {}: {e} — running at its current size",
                    volume.path
                ));
            }
        }
        // Volumes must belong to the app's user, or a `[package] user` app
        // cannot write its own data directory — postgres fails at initdb
        // with EPERM on a directory it appears not to own. The Linux backend
        // does the same chown, on the same one directory.
        if let Some(user) = &spec.user {
            if let Err(e) = volumes::chown_at(&target, user.uid, user.gid) {
                log(&format!(
                    "warning: volume {} could not be given to {} ({e}) — the app may not be able \
                     to write it",
                    volume.path, user.name
                ));
            }
        }
        log(&format!("volume {} <- {}", volume.path, volume.dev));
    }

    /// Unmount the volumes so their filesystems are marked clean.
    ///
    /// `sync` alone is not enough: it flushes the data but leaves ext4's
    /// journal flagged "needs recovery", so every subsequent boot replays it,
    /// and `resize2fs` refuses an unmounted filesystem in that state. The app
    /// has already exited by the time this runs, so nothing is holding the
    /// mount; a failure here is logged rather than fatal, because the
    /// instance's exit code is already decided and losing it to a busy mount
    /// would be worse than a dirty journal the next boot replays.
    fn unmount_volumes(spec: &SpecDisk) {
        for volume in &spec.volumes {
            let Ok(path) = CString::new(volume.path.as_str()) else {
                continue;
            };
            // SAFETY: a live NUL-terminated path.
            if unsafe { libc::umount2(path.as_ptr(), 0) } != 0 {
                log(&format!(
                    "warning: umount {}: {}",
                    volume.path,
                    std::io::Error::last_os_error()
                ));
            }
        }
    }

    /// `/etc/hosts`, `/etc/resolv.conf`, the run user's passwd/group/home,
    /// and the hostname. Written into `NEWROOT` while it is still reachable
    /// by that name.
    fn write_config(spec: &SpecDisk) {
        let _ = mkdir_p(&format!("{NEWROOT}/etc"));
        if !write_file(&format!("{NEWROOT}/etc/hosts"), &spec::hosts_file(spec)) {
            // Not fatal, but an app that cannot resolve its own hostname
            // stalls ~5s in DNS at every bind, which looks like a hang.
            log("warning: /etc/hosts could not be written — name resolution will be degraded");
        }
        match spec::resolv_conf(spec) {
            Some(text) => {
                write_file(&format!("{NEWROOT}/etc/resolv.conf"), &text);
            }
            // Standalone `ply run` with no switch. ply images are per-package
            // squashfs layers and neither alpine-baselayout nor Debian
            // base-files ships /etc/resolv.conf, so in practice this leaves
            // the guest with no resolver at all — which is correct, because
            // there is none.
            None => log("no resolver in the spec disk; leaving /etc/resolv.conf alone"),
        }

        if let Some(user) = &spec.user {
            // getpwuid must work — postgres, sshd and friends insist.
            append_line(
                &format!("{NEWROOT}/etc/passwd"),
                &format!(
                    "{}:x:{}:{}::/home/{}:/bin/sh\n",
                    user.name, user.uid, user.gid, user.name
                ),
            );
            append_line(
                &format!("{NEWROOT}/etc/group"),
                &format!("{}:x:{}:\n", user.name, user.gid),
            );
            let home = format!("{NEWROOT}/home/{}", user.name);
            if mkdir_p(&home) {
                let _ = volumes::chown_at(&home, user.uid, user.gid);
            }
        }

        if let Ok(name) = CString::new(spec.hostname.as_str()) {
            // SAFETY: a live NUL-terminated string and its length.
            let r = unsafe { libc::sethostname(name.as_ptr(), spec.hostname.len()) };
            if r != 0 {
                log(&format!(
                    "warning: sethostname({}): {}",
                    spec.hostname,
                    std::io::Error::last_os_error()
                ));
            }
        }
    }

    fn append_line(path: &str, line: &str) {
        use std::io::Write as _;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut f) => {
                let _ = f.write_all(line.as_bytes());
            }
            Err(e) => log(&format!("warning: append {path}: {e}")),
        }
    }

    /// The canonical `switch_root` dance: stand in the new root, move its
    /// mount over `/`, chroot to where you stand.
    ///
    /// `pivot_root` is not usable here — the initramfs rootfs cannot be
    /// unmounted — which is why this is `MS_MOVE` + `chroot`, the same thing
    /// util-linux's `switch_root(8)` does.
    fn switch_root() {
        let Ok(newroot) = CString::new(NEWROOT) else {
            fail("NEWROOT contains a NUL byte");
        };
        // SAFETY: live NUL-terminated strings throughout.
        unsafe {
            if libc::chdir(newroot.as_ptr()) != 0 {
                fail(&format!(
                    "chdir {NEWROOT}: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        if !mount(".", "/", "", libc::MS_MOVE, "") {
            fail("could not move the assembled root over / — the guest cannot become the image");
        }
        // SAFETY: "." is a constant NUL-terminated path and is the new root.
        unsafe {
            if libc::chroot(c".".as_ptr()) != 0 {
                fail(&format!("chroot .: {}", std::io::Error::last_os_error()));
            }
            if libc::chdir(c"/".as_ptr()) != 0 {
                fail(&format!("chdir /: {}", std::io::Error::last_os_error()));
            }
        }
    }

    /// The pseudo-filesystems the app expects, now inside its own root.
    fn mount_after_pivot() {
        mount(
            "proc",
            "/proc",
            "proc",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            "",
        );
        mount("sysfs", "/sys", "sysfs", 0, "");
        mount("devtmpfs", "/dev", "devtmpfs", 0, "");
        let _ = std::fs::create_dir_all("/dev/shm");
        // POSIX shared memory: PostgreSQL's default dynamic_shared_memory_type
        // is `posix`, which is shm_open under /dev/shm.
        mount(
            "tmpfs",
            "/dev/shm",
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            "mode=1777",
        );
        let _ = std::fs::create_dir_all("/tmp");
        mount(
            "tmpfs",
            "/tmp",
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            "mode=1777",
        );
        // No /dev/pts: this kernel is built without CONFIG_UNIX98_PTYS, so
        // there is no devpts to mount. That is a Task 2 kernel-config
        // decision, not an omission here; an app that needs a pty needs the
        // option added there first.
    }

    // ------------------------------------------------------------ /run/ply

    /// Seed `/run/ply` and seal it, returning a descriptor through which
    /// this init — and only this init — can still write peer nodes when the
    /// host sends a `{"params":…}` update.
    ///
    /// The mount shape, and why it has three mounts rather than one:
    ///
    /// * a tmpfs (call it A) at `/run/ply`, seeded with every peer's facts;
    /// * an O_PATH descriptor on A, taken while A is still the top mount;
    /// * a bind of A over itself (B), remounted read-only — so every PATH
    ///   lookup the app makes reaches B and cannot write, while this init's
    ///   `openat` through the descriptor still reaches A and can. That is
    ///   the guest's stand-in for the Linux backend's separation, where the
    ///   params tree lives on the host outside the container's mount
    ///   namespace entirely;
    /// * a tmpfs at `/run/ply/self`, mounted last so it lands on top of B
    ///   and stays writable: this is where the app publishes.
    fn setup_params(spec: &SpecDisk) -> Option<i32> {
        if !mkdir_p("/run/ply") {
            log("warning: params tree: /run/ply could not be created");
            return None;
        }
        if !mount("tmpfs", "/run/ply", "tmpfs", libc::MS_NOSUID, "mode=0755") {
            log("warning: params tree: /run/ply would not mount");
            return None;
        }
        // The descriptor must be taken BEFORE the read-only bind is stacked
        // on top, or it would resolve to the read-only mount instead.
        let rw = open_dir_fd("/run/ply");

        for (app, facts) in &spec.params_seed {
            if !safe_name(app) {
                log(&format!("warning: params tree: ignoring peer name {app:?}"));
                continue;
            }
            let dir = format!("/run/ply/{app}");
            if !mkdir_p(&dir) {
                continue;
            }
            for (key, value) in facts {
                if safe_name(key) {
                    write_file(&format!("{dir}/{key}"), value);
                }
            }
        }
        // `self` must exist as a directory in A before A is sealed: it is
        // the mount point the app's own writable node attaches to.
        let _ = mkdir_p("/run/ply/self");

        // Seal /run/ply. Fail CLOSED, exactly as ns/container.rs does: if
        // the read-only view cannot be established, the app gets no /run/ply
        // at all rather than a writable one.
        if !bind_ro("/run/ply") {
            umount_detach("/run/ply");
            close_fd(rw);
            log(
                "warning: params tree: /run/ply could not be made read-only, so it is not \
                 mounted at all this run",
            );
            return None;
        }

        mount_self_node(spec);
        rw
    }

    /// `/run/ply/self`: the app's own writable node, with the four
    /// parent-owned keys re-bound read-only over themselves.
    fn mount_self_node(spec: &SpecDisk) {
        if !mount(
            "tmpfs",
            "/run/ply/self",
            "tmpfs",
            libc::MS_NOSUID,
            "mode=0755",
        ) {
            log(
                "warning: params tree: the app cannot self-publish this run (/run/ply/self would \
                 not mount)",
            );
            return;
        }
        // Seed it from this instance's own node in the seed, so the app sees
        // the same facts under /run/ply/self that a Linux container sees
        // through its bind of dir(app).
        let own = spec
            .params_seed
            .iter()
            .find(|(app, _)| *app == spec.hostname)
            .map(|(_, facts)| facts.as_slice())
            .unwrap_or(&[]);
        for (key, value) in own {
            if safe_name(key) {
                write_file(&format!("/run/ply/self/{key}"), value);
            }
        }
        // The node must belong to the app or it cannot publish anything: the
        // tmpfs above is root's, and the whole point of `/run/ply/self` is
        // that a `[package] user` app writes `finish_boot` into it. The
        // DIRECTORY only — the parent-owned files stay root's, and the app
        // cannot unlink them either, because a file with a mount on it
        // refuses to be removed (EBUSY).
        if let Some(user) = &spec.user {
            if let Err(e) = volumes::chown_at("/run/ply/self", user.uid, user.gid) {
                log(&format!(
                    "warning: params tree: /run/ply/self could not be given to {} ({e}) — the app \
                     will not be able to self-publish",
                    user.name
                ));
            }
        }
        // The re-bind pins a file, so every parent-owned name must exist
        // first — an absent one is a missing seal, not a missing fact.
        for name in PARENT_OWNED {
            let path = format!("/run/ply/self/{name}");
            if !std::path::Path::new(&path).exists() {
                write_file(&path, "");
            }
        }

        // Fail CLOSED. /run/ply/self is writable because an app
        // self-publishes into it, and the ONLY thing keeping `state` — the
        // fact every `--after` dependant gates on — out of the app's reach
        // is this per-file read-only re-bind. One failure and the app could
        // write `healthy` over its own state and every downstream dependant
        // would believe it, so the whole self node comes back off and
        // self-publishing is simply unavailable this run.
        let mut sealed = true;
        for name in PARENT_OWNED {
            let path = format!("/run/ply/self/{name}");
            if !bind_ro(&path) {
                log(&format!(
                    "warning: params tree: read-only re-bind of {name} for {} failed",
                    spec.hostname
                ));
                sealed = false;
                break;
            }
        }
        if !sealed {
            umount_detach("/run/ply/self");
            log(&format!(
                "warning: params tree: {} cannot self-publish this run (/run/ply/self unmounted \
                 — a writable `state` would let it forge its own health)",
                spec.hostname
            ));
        }
    }

    /// One path segment that is safe to join: no separator, no `.`/`..`, not
    /// empty. Peer and key names come from the host, which is trusted, but a
    /// name that escaped `/run/ply` would write into the app's root.
    fn safe_name(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\0')
    }

    fn open_dir_fd(path: &str) -> Option<i32> {
        let c = CString::new(path).ok()?;
        // SAFETY: a live NUL-terminated path.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        (fd >= 0).then_some(fd)
    }

    fn close_fd(fd: Option<i32>) {
        if let Some(fd) = fd {
            // SAFETY: `fd` was opened by this process and is not used again.
            unsafe { libc::close(fd) };
        }
    }

    /// Apply a host→guest `{"params":…}` update to the peer nodes, through
    /// the writable descriptor `setup_params` kept back.
    fn apply_params(rw: i32, params: &ParamsTree) {
        for (app, facts) in params {
            // `self` is the app's own mount point in the sealed view; a host
            // update must never land underneath it.
            if !safe_name(app) || app == "self" {
                continue;
            }
            let Ok(dir_c) = CString::new(app.as_str()) else {
                continue;
            };
            // SAFETY: `rw` is an O_PATH directory fd this process owns.
            unsafe { libc::mkdirat(rw, dir_c.as_ptr(), 0o755) };
            for (key, value) in facts {
                if !safe_name(key) {
                    continue;
                }
                let Ok(rel) = CString::new(format!("{app}/{key}")) else {
                    continue;
                };
                // In place (open/truncate/write), never rename: the Linux
                // side documents why — a rename over a bind-mounted file
                // detaches every mount on it. Nothing binds these here, but
                // the two sides should not differ in a way a reader has to
                // rediscover.
                // SAFETY: `rw` is a directory fd; `rel` is a live path.
                let fd = unsafe {
                    libc::openat(
                        rw,
                        rel.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                        0o644 as libc::c_uint,
                    )
                };
                if fd < 0 {
                    continue;
                }
                write_all_fd(fd, value.as_bytes());
                // SAFETY: `fd` was just opened here.
                unsafe { libc::close(fd) };
            }
        }
    }

    // ------------------------------------------------------------- control

    fn open_control() -> Option<(Control, std::io::BufReader<std::fs::File>)> {
        match Control::open() {
            Ok((control, inbound, dev)) => {
                log(&format!("control channel: {dev}"));
                hide_control_device(dev);
                Some((control, inbound))
            }
            Err(e) => {
                // Not fatal: the app still runs. The host will see a VM that
                // powered off without an {"exit":…} and report 255.
                log(&format!(
                    "warning: no control channel ({}: {e}) — the host cannot learn this \
                     instance's exit code or published facts",
                    crate::control::CONTROL_DEV
                ));
                None
            }
        }
    }

    /// Take the control channel's name away from the app, now that both
    /// halves are open.
    ///
    /// `mount_after_pivot` mounts a full devtmpfs at `/dev` **inside the
    /// app's root**, so `/dev/hvc1` is an ordinary node in the app's own
    /// filesystem. Until this runs, an app with no `[package] user` — uid 0,
    /// full capabilities, no seccomp — can `open("/dev/hvc1", O_WRONLY)` and
    /// write `{"publish":{"key":"state","value":"healthy"}}` straight onto
    /// the wire. Or `{"exit":0}`, or `{"ready":true}`. No inotify event
    /// exists for any of that, so `watch_self`'s `PARENT_OWNED` filter never
    /// sees it: that filter is the only guest-internal *producer* of
    /// `Publish` lines, but it was never the only *writer* to the channel.
    ///
    /// This init already holds both open file descriptions and neither
    /// depends on the name, so removing the directory entry costs one
    /// syscall and takes the app's handle away.
    ///
    /// **What this is not: a boundary.** A root app here keeps `CAP_MKNOD`
    /// and `CAP_SYS_ADMIN`, so it can `mknod` a new node on the same device
    /// numbers, or mount a second devtmpfs and find the port in that. This
    /// moves the bar from "open the file that is sitting right there" to
    /// "re-create the device", which is worth one syscall and is not the same
    /// as closing the hole. The real fix is the capability drop, which needs
    /// `keep_caps`/`privileged` on the spec disk (Task 7).
    ///
    /// **The same argument applies to `/dev/vdN`, and is deliberately NOT
    /// acted on here.** The layer and volume devices are nodes in the app's
    /// root too, so a root app can write raw over its own volume's
    /// filesystem, or over an image layer, with no filter in the way.
    /// Unlinking them would not disturb the mounts — a mount holds its block
    /// device, not its name — but a volume an operator may need to identify
    /// from inside a running guest is a real cost, and it buys nothing
    /// against an app that can `mknod` its way back. That one belongs with
    /// the capability drop, not with a name.
    fn hide_control_device(dev: &str) {
        let Ok(c) = CString::new(dev) else { return };
        // SAFETY: a live NUL-terminated path.
        if unsafe { libc::unlink(c.as_ptr()) } == 0 {
            log(&format!(
                "control channel: unlinked {dev} (this init keeps it open; the app can no longer \
                 open it by name)"
            ));
            return;
        }
        // Not fatal, and deliberately loud: the channel still works, but the
        // app can reach it, and that is a fact worth having in the log of a
        // boot somebody is debugging later.
        log(&format!(
            "warning: could not unlink {dev} ({}) — the app can open the control channel \
             directly and forge any line on it",
            std::io::Error::last_os_error()
        ));
    }

    /// The two background loops, started only after the fork: a process that
    /// forks while other threads hold locks hands the child a deadlock.
    fn start_threads(
        control: Option<Control>,
        inbound: Option<std::io::BufReader<std::fs::File>>,
        params_rw: Option<i32>,
    ) {
        let (Some(control), Some(inbound)) = (control, inbound) else {
            close_fd(params_rw);
            return;
        };
        let watcher = control.clone();
        std::thread::spawn(move || watch_self(watcher));
        std::thread::spawn(move || {
            let end = crate::control::pump(inbound, |line| match line {
                HostLine::Signal { name } => forward_signal(&name),
                HostLine::Params { params } => match params_rw {
                    Some(fd) => apply_params(fd, &params),
                    None => log("params update ignored: /run/ply is not mounted"),
                },
            });
            // Say so. This thread ending means the host can no longer signal
            // this instance — `ply stop` will not reach it, and no params
            // update will — and a silent return makes that indistinguishable
            // from an app that is merely slow to exit.
            log(&format!(
                "control channel closed ({end}); no further host signals or params updates \
                 will reach this instance"
            ));
        });
    }

    fn forward_signal(name: &str) {
        let pid = APP_PID.load(Ordering::SeqCst);
        let Some(sig) = signal_number(name) else {
            log(&format!("control: unknown signal {name:?}, ignored"));
            return;
        };
        if pid > 0 {
            // SAFETY: kill(2) on a pid this process forked.
            unsafe { libc::kill(pid, sig) };
        }
    }

    fn signal_number(name: &str) -> Option<libc::c_int> {
        let bare = name.strip_prefix("SIG").unwrap_or(name);
        Some(match bare {
            "TERM" => libc::SIGTERM,
            "INT" => libc::SIGINT,
            "HUP" => libc::SIGHUP,
            "QUIT" => libc::SIGQUIT,
            "KILL" => libc::SIGKILL,
            "USR1" => libc::SIGUSR1,
            "USR2" => libc::SIGUSR2,
            // httpd's graceful-stop signal, forwarded on Linux for the same
            // reason (ns/container.rs's init_loop).
            "WINCH" => libc::SIGWINCH,
            _ => return None,
        })
    }

    /// Watch `/run/ply/self` and forward every fact the app publishes.
    ///
    /// `IN_CLOSE_WRITE` rather than `IN_MODIFY`: an app that writes a value
    /// in two `write` calls would otherwise publish a half value.
    ///
    /// # The `PARENT_OWNED` filter below is the security boundary
    ///
    /// It reads like housekeeping — "skip the names the app cannot write
    /// anyway" — and that reason is **wrong**, which matters because a future
    /// reader who believes it will delete the filter as redundant.
    ///
    /// An app with no `[package] user` runs in this guest as uid 0 with full
    /// capabilities in the initial user namespace. Nothing here does what
    /// `ns/container.rs` does before `execve` on Linux: there is no
    /// capability drop and no seccomp policy, and `ns/security.rs` is what
    /// makes `umount2` return EPERM for a root app *there*. Here it does not:
    /// a root app can `umount2("/run/ply/self/state", MNT_DETACH)` and write
    /// whatever it likes underneath the read-only re-bind.
    ///
    /// What this filter buys, stated exactly: it is the only guest-internal
    /// *producer* of `Publish` lines, so a `state` the app forged underneath
    /// the read-only re-bind is never forwarded. The app lies to itself and
    /// the host's params tree does not hear it.
    ///
    /// What it does NOT buy — and an earlier version of this comment claimed
    /// it did, which is worse than saying nothing: "a forged `state` never
    /// leaves the guest" is false on the filter alone. **`watch_self` is not
    /// the only writer to the wire.** The control channel is a device node in
    /// the app's own `/dev` (`mount_after_pivot` mounts a full devtmpfs
    /// there), and a root app that opens it can write any `GuestLine` it
    /// likes — `publish`, `exit`, `ready` — with no inotify event anywhere.
    /// `open_control` now unlinks that node for exactly this reason; see
    /// `hide_control_device` for what the unlink does and does not close.
    ///
    /// So, honestly: the filter closes the file-writing path, the unlink
    /// closes the easy device path, and **neither is a boundary against a
    /// root app that can `mknod`.** That is the capability drop, and it needs
    /// `keep_caps`/`privileged` on the wire (Task 7). Delete neither of the
    /// two before then.
    fn watch_self(control: Control) {
        // SAFETY: argument-only flags.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            log(&format!(
                "warning: inotify_init1: {} — published facts will not reach the host",
                std::io::Error::last_os_error()
            ));
            return;
        }
        let Ok(path) = CString::new("/run/ply/self") else {
            log("warning: /run/ply/self contains a NUL byte — published facts will not reach the host");
            return;
        };
        // SAFETY: `fd` is an inotify descriptor and `path` is live.
        let wd = unsafe {
            libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO)
        };
        if wd < 0 {
            log(&format!(
                "warning: cannot watch /run/ply/self: {} — published facts will not reach the host",
                std::io::Error::last_os_error()
            ));
            return;
        }
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: `buf` is a live local buffer.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                // Never silently: this thread ending means the app can still
                // write /run/ply/self and nothing will ever forward it, which
                // from the host looks exactly like an app that never finished
                // booting. One line here is the difference between a
                // diagnosis and a mystery.
                log(&format!(
                    "warning: the /run/ply/self watch ended ({}) — nothing this app publishes \
                     from now on will reach the host",
                    if n == 0 {
                        "end of file on the inotify descriptor".to_string()
                    } else {
                        std::io::Error::last_os_error().to_string()
                    }
                ));
                return;
            }
            for name in inotify_names(&buf[..n as usize]) {
                if PARENT_OWNED.contains(&name.as_str()) || !safe_name(&name) {
                    continue;
                }
                let Ok(value) = std::fs::read_to_string(format!("/run/ply/self/{name}")) else {
                    continue;
                };
                control.send(&GuestLine::Publish {
                    publish: Publish {
                        key: name,
                        value: value.trim().to_string(),
                    },
                });
            }
        }
    }

    /// Pull the file names out of one inotify read.
    ///
    /// The struct is `{ wd: i32, mask: u32, cookie: u32, len: u32 }` then
    /// `len` bytes of NUL-padded name — a packed 16-byte header this parses
    /// by offset rather than by casting to a `libc` struct, because the
    /// events in a single read are variable-length and NOT aligned to the
    /// struct's alignment on every architecture.
    fn inotify_names(mut bytes: &[u8]) -> Vec<String> {
        const HEADER: usize = 16;
        let mut names = Vec::new();
        while bytes.len() >= HEADER {
            let len = u32::from_ne_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
            let end = HEADER + len;
            if end > bytes.len() {
                break;
            }
            let raw = &bytes[HEADER..end];
            let raw = match raw.iter().position(|b| *b == 0) {
                Some(nul) => &raw[..nul],
                None => raw,
            };
            if let Ok(name) = std::str::from_utf8(raw) {
                if !name.is_empty() {
                    names.push(name.to_string());
                }
            }
            bytes = &bytes[end..];
        }
        names
    }

    // ----------------------------------------------------------- the app

    /// Fork and exec the entrypoint. Returns its pid; never returns in the
    /// child.
    fn spawn_app(spec: &SpecDisk) -> i32 {
        let Some(program) = spec.entrypoint.first() else {
            fail("the spec disk carries an empty entrypoint — there is nothing to run");
        };
        let path_env = spec
            .env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        let resolved = spec::resolve_program(program, path_env, is_executable_file);

        // Everything that allocates happens before the fork; see `run`.
        let Ok(prog_c) = CString::new(resolved.as_str()) else {
            fail("the entrypoint contains a NUL byte");
        };
        let argv_c: Result<Vec<CString>, _> = spec
            .entrypoint
            .iter()
            .map(|a| CString::new(a.as_str()))
            .collect();
        let Ok(argv_c) = argv_c else {
            fail("an entrypoint argument contains a NUL byte");
        };
        let env_c: Result<Vec<CString>, _> = spec
            .env
            .iter()
            .map(|(k, v)| CString::new(format!("{k}={v}")))
            .collect();
        let Ok(env_c) = env_c else {
            fail("an environment entry contains a NUL byte");
        };
        let Ok(workdir_c) = CString::new(spec.workdir.as_str()) else {
            fail("the workdir contains a NUL byte");
        };
        let mut argv: Vec<*const libc::c_char> = argv_c.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<*const libc::c_char> = env_c.iter().map(|e| e.as_ptr()).collect();
        envp.push(std::ptr::null());
        let user = spec.user.as_ref().map(|u| (u.uid, u.gid));

        // hvc0 carries the app's stdout and stderr and nothing else; stdin is
        // /dev/null, because nothing on the host is wired to write to it yet
        // (Task 8/9 own that decision) and a blocking read on the kernel's
        // own console would be worse than EOF.
        let out = open_raw(c"/dev/hvc0", libc::O_WRONLY);
        let null = open_raw(c"/dev/null", libc::O_RDONLY);
        match out {
            // `hvc0` is a tty, and N_TTY's ONLCR would turn every `\n` the
            // app writes into `\r\n` — so the log ring the host tees into
            // would hold CRLF where the namespace backend's pipe holds LF,
            // for every line of every app.
            Some(fd) => crate::control::set_raw(fd),
            None => {
                log("warning: /dev/hvc0 is absent — the app's output goes to the kernel console")
            }
        }

        // SAFETY: the child calls only async-signal-safe functions on
        // pointers built above.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            fail(&format!(
                "fork for the entrypoint: {}",
                std::io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            // SAFETY: every call below is async-signal-safe.
            unsafe {
                if let Some(fd) = null {
                    libc::dup2(fd, 0);
                }
                if let Some(fd) = out {
                    libc::dup2(fd, 1);
                    libc::dup2(fd, 2);
                }
                if let Some((uid, gid)) = user {
                    let gids = [gid];
                    if libc::setgroups(1, gids.as_ptr()) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0
                    {
                        write_all_fd(2, b"ply-init: cannot switch to the app's user\n");
                        libc::_exit(126);
                    }
                }
                // PR_SET_NO_NEW_PRIVS, the one clamp of `ns/security.rs`'s
                // set that costs a single syscall and needs nothing on the
                // wire. It closes the setuid-binary path: a `[package] user`
                // app that finds a setuid-root binary in its own image cannot
                // use it to become root. The flag is inherited across execve
                // and can never be turned off, which is the point.
                //
                // Set AFTER the uid switch, matching `ns/container.rs`'s
                // order, and unconditionally, because `SpecDisk` carries
                // neither `privileged` nor `keep_caps` — when it does (Task
                // 7), this must honour `privileged` the way the namespace
                // backend does.
                //
                // THE GAP THIS DOES NOT CLOSE, stated plainly so it is not
                // mistaken for parity: there is still no capability drop and
                // no seccomp filter here. An app with no `[package] user`
                // runs as uid 0 with the full bounding set in the initial
                // user namespace, so it keeps CAP_SYS_ADMIN and can unmount
                // its own read-only bindings — `watch_self`'s PARENT_OWNED
                // filter, not the mount, is what keeps a forged `state` off
                // the wire, and only together with `hide_control_device`'s
                // unlink of the control node, since a root app that can open
                // that node bypasses the filter entirely. Neither survives an
                // app that re-creates the node with CAP_MKNOD. Closing it
                // properly needs `keep_caps` and `privileged` on the spec
                // disk (Task 7) plus this crate growing the equivalent of
                // `drop_capabilities`.
                if !set_no_new_privs() {
                    write_all_fd(
                        2,
                        b"ply-init: PR_SET_NO_NEW_PRIVS failed; a setuid binary in this image could still gain privileges\n",
                    );
                }
                if libc::chdir(workdir_c.as_ptr()) != 0 {
                    // Legal to continue from `/` — a thin image may have no
                    // prefix dir — but silently wrong for anything that
                    // operates on `.`, so say so.
                    write_all_fd(2, b"ply-init: cannot enter the workdir; running from /\n");
                }
                libc::execve(prog_c.as_ptr(), argv.as_ptr(), envp.as_ptr());
                write_all_fd(2, b"ply-init: exec failed - the entrypoint is not on the image's PATH, or its interpreter/libc is missing from the layers\n");
                libc::_exit(127);
            }
        }
        log(&format!("exec {resolved} (pid {pid})"));
        pid
    }

    /// `open(2)` for the two descriptors the app inherits, with the two
    /// flags that are not optional here.
    ///
    /// `O_CLOEXEC` because these are dup2'd onto 0/1/2 in the child and dup2
    /// clears the flag on the destination — so the app gets exactly three
    /// standard descriptors and NOT a spare writable `/dev/hvc0` and a spare
    /// `/dev/null` at some higher number, which is what it inherited before.
    /// A second handle on the log channel is a small thing to hand a process
    /// that is not supposed to know the channel exists.
    ///
    /// `O_NOCTTY` because `/dev/hvc0` is a tty and PID 1 is a session leader:
    /// without it, opening it would make it this session's controlling
    /// terminal.
    fn open_raw(path: &std::ffi::CStr, flags: libc::c_int) -> Option<i32> {
        // SAFETY: `path` is a NUL-terminated C string literal.
        let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_NOCTTY | libc::O_CLOEXEC) };
        (fd >= 0).then_some(fd)
    }

    /// `PR_SET_NO_NEW_PRIVS`: from here on, no `execve` by this process or
    /// any of its descendants may grant privileges it does not already have.
    ///
    /// One syscall, no argument marshalling, nothing on the wire, and it is
    /// the half of `ns/security.rs`'s clamp set that needs no `keep_caps`. It
    /// is irreversible by design — a process may set the flag and can never
    /// clear it — which is why calling it in a test is safe for the rest of
    /// that test binary and why calling it before `execve` is enough.
    ///
    /// Safe to call between `fork` and `execve`: one syscall, no allocation,
    /// no locks.
    fn set_no_new_privs() -> bool {
        // SAFETY: prctl with an option that takes one scalar and ignores the
        // remaining arguments.
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 }
    }

    fn is_executable_file(path: &str) -> bool {
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn one_inotify_read_yields_every_name_in_it() {
            fn event(name: &str) -> Vec<u8> {
                // Names are NUL-padded to a multiple of the struct's
                // alignment, which is exactly the case a naive parser gets
                // wrong.
                let mut padded = name.as_bytes().to_vec();
                padded.push(0);
                while !padded.len().is_multiple_of(8) {
                    padded.push(0);
                }
                let mut out = Vec::new();
                out.extend_from_slice(&1i32.to_ne_bytes());
                out.extend_from_slice(&libc::IN_CLOSE_WRITE.to_ne_bytes());
                out.extend_from_slice(&0u32.to_ne_bytes());
                out.extend_from_slice(&(padded.len() as u32).to_ne_bytes());
                out.extend_from_slice(&padded);
                out
            }
            let mut buf = event("finish_boot");
            buf.extend_from_slice(&event("port"));
            assert_eq!(inotify_names(&buf), vec!["finish_boot", "port"]);
            // A short tail is dropped, not read past.
            buf.truncate(buf.len() - 4);
            assert_eq!(inotify_names(&buf), vec!["finish_boot"]);
            assert!(inotify_names(&[]).is_empty());
        }

        #[test]
        fn a_key_that_would_escape_run_ply_is_refused() {
            assert!(safe_name("finish_boot"));
            assert!(!safe_name(""));
            assert!(!safe_name(".."));
            assert!(!safe_name("."));
            assert!(!safe_name("a/b"));
            assert!(!safe_name("../../etc/passwd"));
        }

        #[test]
        fn a_signal_name_maps_with_or_without_its_prefix() {
            assert_eq!(signal_number("TERM"), Some(libc::SIGTERM));
            assert_eq!(signal_number("SIGTERM"), Some(libc::SIGTERM));
            assert_eq!(signal_number("WINCH"), Some(libc::SIGWINCH));
            assert_eq!(signal_number("NOPE"), None);
        }

        #[test]
        fn a_short_device_reads_as_nothing_not_as_a_head() {
            // The invariant the whole volume path rests on, asserted against
            // a real file for the first time. `volumes::classify` refuses a
            // head shorter than HEAD_BYTES, and that is only *safe* rather
            // than merely *defensive* because this reader never produces one
            // — a partial read that reached a fail-open classifier is how a
            // live database gets reformatted.
            let path = std::env::temp_dir()
                .join(format!("ply-guest-init-head-{}", unsafe { libc::getpid() }));
            let dev = path.to_string_lossy().to_string();
            assert!(std::fs::write(&path, vec![0u8; 100]).is_ok());
            assert!(
                read_exact_head(&dev, 4096).is_none(),
                "a 100-byte device must read as nothing, never as a short head"
            );
            // And it is a real read, not a blanket None: the same file at the
            // asked-for length comes back whole.
            assert_eq!(read_exact_head(&dev, 100).map(|b| b.len()), Some(100));
            assert!(std::fs::write(&path, vec![0u8; 8192]).is_ok());
            assert_eq!(read_exact_head(&dev, 4096).map(|b| b.len()), Some(4096));
            let _ = std::fs::remove_file(&path);
            // A device that is not there at all is the other half of `None`.
            assert!(read_exact_head(&dev, 4096).is_none());
        }

        #[test]
        fn every_child_of_pid_1_gets_dev_null_on_stdin_and_can_never_block_on_a_prompt() {
            // What this pins: `run`'s child used to inherit PID 1's fd 0,
            // which the kernel hands init as /dev/console. mke2fs saw a tty
            // on both ends, decided to ask `Proceed anyway? (y,N)`, and
            // blocked in fgets forever — PID 1 stuck behind it in `wait_for`,
            // no {"ready":true} for the host, and nothing that times out.
            //
            // fd 0 is pointed at a pipe with a line already in it, so "the
            // child saw EOF" cannot be satisfied by this test binary's own
            // stdin happening to be /dev/null.
            let mut fds = [0i32; 2];
            // SAFETY: `fds` is a live array of two ints.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
            // SAFETY: fd 0 exists in any process this test runs in.
            let saved = unsafe { libc::dup(0) };
            assert!(saved >= 0, "dup(0)");
            // SAFETY: both descriptors are open.
            assert!(unsafe { libc::dup2(fds[0], 0) } >= 0, "dup2 onto stdin");
            // SAFETY: a live buffer on an open pipe.
            unsafe { libc::write(fds[1], b"x\n".as_ptr().cast(), 2) };

            // First prove the probe itself works, or a missing /bin/sh would
            // make the real assertion below pass for the wrong reason.
            let sanity = run("/bin/sh", &["sh", "-c", "exit 0"]);
            // `read` is a shell builtin: it returns 0 when it read a line
            // (the pipe) and 1 at end of file (/dev/null). No PATH, no
            // external binary, and it cannot block either way.
            //
            // What it pins is "not this process's stdin", which is the whole
            // property: both /dev/null and the closed-fd fallback answer
            // immediately, and blocking is the single outcome that must be
            // impossible. Verified by mutation — deleting the redirect makes
            // this read the line and the assertion fail.
            let saw_stdin = run("/bin/sh", &["sh", "-c", "read line"]);

            // SAFETY: every descriptor here was opened above.
            unsafe {
                libc::dup2(saved, 0);
                libc::close(saved);
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            assert_eq!(sanity, 0, "/bin/sh must run for this test to mean anything");
            // `!= 0`, not `== 1`: what must be true is "the child did not
            // read the line", and `read`'s exact status at EOF is the
            // shell's business — dash and bash both answer 1 today, but the
            // assertion should not depend on that. Equally mutation-
            // sensitive: deleting the redirect makes `read` succeed and
            // return 0, and this fails.
            assert_ne!(
                saw_stdin, 0,
                "the child read a line, so it inherited this process's stdin instead of /dev/null"
            );
        }

        #[test]
        fn the_app_is_exec_d_with_no_new_privs_so_a_setuid_binary_cannot_regain_root() {
            // Deliberately set on the test process itself rather than in a
            // forked child: the flag is irreversible and inherited, and there
            // is nothing in this binary it can break — it only forbids
            // GAINING privileges across an execve, which no test does. A
            // forked probe would also race `run`'s waitpid(-1).
            assert!(set_no_new_privs(), "prctl(PR_SET_NO_NEW_PRIVS)");
            // The four trailing zeros are NOT optional: the kernel returns
            // EINVAL for PR_GET_NO_NEW_PRIVS unless arg2..arg5 are all zero,
            // and `prctl` is variadic, so omitting them passes whatever
            // happens to be in those registers. Measured, not assumed — the
            // first version of this assertion read -1 from a flag that was
            // in fact set.
            // SAFETY: prctl with a query option and no output pointer.
            let got = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
            assert_eq!(
                got, 1,
                "the flag must actually be ON afterwards; passing 0, or the wrong option, would \
                 leave a [package] user app able to use a setuid binary in its own image"
            );
        }

        #[test]
        fn the_wire_cap_is_never_below_the_spec_disk_cap() {
            // Both numbers bound the SAME params tree by two routes: the
            // seed arrives on the spec disk, updates arrive as
            // `{"params":…}` lines. A wire cap below the disk cap is a tree
            // the host can write and the guest cannot read — and because
            // `pump` RETURNS on `LineTooLong`, the instance would lose every
            // later signal too, not just that one line.
            // At COMPILE time: a wire cap below the disk cap should not
            // build, because the damage it does — an instance that silently
            // stops receiving signals — appears long after the commit that
            // causes it.
            const { assert!(crate::control::MAX_LINE >= SPEC_READ_CAP) };
        }

        #[test]
        fn no_descriptor_this_init_opens_survives_into_the_app() {
            // `open_raw`'s promise: the app gets exactly three standard
            // descriptors and no spare handle on the log channel or the
            // console. dup2 clears FD_CLOEXEC on 0/1/2, which are the copies
            // it is meant to have; the originals must not follow it through
            // execve. /dev/null stands in for the real devices, which do not
            // exist on a build host — the flags are the whole subject.
            let fd = open_raw(c"/dev/null", libc::O_RDONLY).expect("/dev/null opens");
            // SAFETY: `fd` was just opened by this process.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            // SAFETY: same, and it is not used again.
            unsafe { libc::close(fd) };
            assert!(flags >= 0, "F_GETFD");
            assert!(
                flags & libc::FD_CLOEXEC != 0,
                "open_raw must set O_CLOEXEC — `open_console` goes through it for exactly this \
                 reason, and without it the app inherits a spare writable console"
            );
        }

        #[test]
        fn unlinking_the_control_node_removes_the_name_and_keeps_the_channel() {
            // What the app loses and what this init keeps, over a real file:
            // the app's route to the wire is `open(path)`, and PID 1's is a
            // file description it already holds. Removing the name ends the
            // first without touching the second.
            use std::io::Write as _;
            let path = std::env::temp_dir()
                .join(format!("ply-guest-init-ctl-{}", unsafe { libc::getpid() }));
            let dev = path.to_string_lossy().to_string();
            assert!(std::fs::write(&path, b"").is_ok());
            let mut held = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("the init opens the channel first");

            hide_control_device(&dev);

            assert!(
                !path.exists(),
                "the name must be gone, or the app can still open the control channel"
            );
            assert!(
                std::fs::OpenOptions::new().write(true).open(&path).is_err(),
                "and opening it by name must fail, which is the app's only easy route"
            );
            // The half that must NOT break: this init still writes lines.
            assert!(
                held.write_all(b"{\"ready\":true}\n").is_ok(),
                "the open description outlives the name — otherwise the unlink costs the host \
                 every message from this instance"
            );
            let _ = std::fs::remove_file(&path);
            // (A device that is not there is not an error either — it logs a
            // warning and returns. Not exercised here on purpose: the log
            // line it prints would land in every CI run's output saying the
            // app can forge lines, which is exactly the sentence nobody
            // should have to decide is spurious.)
        }

        #[test]
        fn a_signalled_app_reports_the_same_code_as_it_would_on_linux() {
            // 128 + signal, matching ns/exec.rs and ns/container.rs, so an
            // app killed by SIGKILL reads 137 on both backends.
            assert_eq!(exit_code(0), 0);
            assert_eq!(exit_code(3 << 8), 3);
            assert_eq!(exit_code(libc::SIGKILL), 128 + libc::SIGKILL);
        }
    }
}
