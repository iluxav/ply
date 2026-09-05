//! Which kernel a microVM boots.
//!
//! The kernel is part of the RUNTIME, like the ply binary itself, not part
//! of any app: the version is a constant here, `ply self-update` brings a
//! new pin with a new binary, and no `ply.lock` ever mentions it — a
//! lockfile written on a Mac must stay byte-identical to one written on
//! Linux.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The keg this build boots. Bump it with the binary, never per app.
pub const MICROVM_KERNEL: &str = "ply/microvm-kernel@6.12.0";

/// Escape hatch for kernel development: a filesystem path (a keg's `boot/`
/// directory, or a raw arm64 `Image`), or a registry ref to fetch instead of
/// the pin. Which one it is, is decided by SHAPE — see `override_paths`.
pub const KERNEL_OVERRIDE_ENV: &str = "PLY_MICROVM_KERNEL";

/// The two files a kernel payload is made of, wherever it comes from — the
/// keg, an override directory, or a local build. Named once so the three
/// places that look for them cannot disagree.
pub const KERNEL_FILE: &str = "microvm-kernel.img";
pub const INITRAMFS_FILE: &str = "initramfs.cpio";

/// Where the kernel and its initramfs came from, for `ply check` and errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kernel {
    pub image: PathBuf,
    pub initramfs: PathBuf,
    /// What to tell the user this is: the pin, or the override that beat it.
    pub origin: String,
}

/// Is this override a filesystem path, or a registry ref? Decided by SHAPE,
/// never by existence: a value beginning `/`, `./`, `../` or `~` is a path,
/// and nothing else is.
///
/// Existence was the wrong test, and it turned a typo into a network call.
/// `PLY_MICROVM_KERNEL=/Users/me/out/keg/bot` is not a directory, so it fell
/// through to the registry arm, `split_once('/')` made a namespace out of
/// `Users`, and ply issued a real request for a package named
/// `me/out/keg/bot` — answered with a registry error and a `ply search`
/// suggestion, for a path the user had typed. A quoted `'~/out/keg/boot'`
/// the shell never expanded did exactly the same. A path that is not there
/// is a path error; it is not a package.
fn is_path_override(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
}

/// Split `PLY_MICROVM_KERNEL` into (kernel image, initramfs) when it is
/// path-shaped, or `None` when it is a registry ref.
pub fn override_paths(value: &str) -> Option<(PathBuf, PathBuf)> {
    if !is_path_override(value) {
        return None;
    }
    let path = PathBuf::from(value);
    // The filesystem is consulted for ONE thing: telling a bare `Image` from
    // a directory. A path that is not there is the directory shape, so the
    // error names `<what you typed>/microvm-kernel.img` rather than guessing.
    if path.is_file() {
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        return Some((path, dir.join(INITRAMFS_FILE)));
    }
    Some((path.join(KERNEL_FILE), path.join(INITRAMFS_FILE)))
}

/// Why this file cannot be booted from, or `None` if it can.
///
/// `Path::exists()` was not this test: it says yes to an empty file (an
/// interrupted `cp`), to a DIRECTORY named `microvm-kernel.img`, and to a
/// file this process cannot open — three ways to fail inside the VMM
/// instead of here, none of them with a message that names the real cause.
fn unbootable(path: &Path) -> Option<&'static str> {
    match std::fs::metadata(path) {
        Ok(md) if !md.is_file() => Some("is not a regular file"),
        Ok(md) if md.len() == 0 => Some("is empty"),
        Ok(_) if std::fs::File::open(path).is_err() => Some("cannot be read"),
        Ok(_) => None,
        // metadata() follows links, so a link whose target is gone lands
        // here too; symlink_metadata() is what tells those two apart, and
        // "no such file" is a lie about a symlink that is right there.
        Err(_) if path.symlink_metadata().is_ok() => Some("is a symlink to nothing"),
        Err(_) => Some("does not exist"),
    }
}

/// Where the two files sit inside an extracted keg, relative to its rootfs.
///
/// `ply build` packs a keg's directory at `/opt/<name>-<version>` — an app
/// gets `/opt/<name>`, a keg carries its version, and that is ply's own
/// convention, not something invented here. The `boot/` level below it is
/// not decoration either: `ply build` refuses to pack any top-level `*.img`
/// (that is how it keeps a build's own output out of the next build), so a
/// `microvm-kernel.img` staged at the root of the keg directory would be
/// silently dropped from the image. One directory down it packs, and `boot/`
/// is where a Linux system keeps a kernel and an initramfs anyway.
///
/// Both facts are pinned in the crate that owns them, over a real image:
/// `build::tests::a_kegs_nested_img_ships_and_a_top_level_one_does_not`
/// builds a keg with a nested and a top-level `.img` and reads the squashfs
/// listing back. (An earlier note cited
/// `package_build_uses_keg_prefix_and_layer_toml` for the prefix; that test
/// round-trips a string through `.layer.toml` and never lists the image.)
///
/// `scripts/build-microvm-kernel.sh` stages exactly this layout, and reads it
/// back out of the built image with `unsquashfs -l`. The three must move
/// together.
fn keg_payload_dir(rootfs: &std::path::Path, image: &crate::image::name::ImageName) -> PathBuf {
    rootfs.join(format!("opt/{}-{}/boot", image.name, image.version))
}

/// The kernel this run boots: the override if there is one, else the pin,
/// fetched into the store on first use like any other keg.
pub fn resolve() -> Result<Kernel> {
    if let Some(value) = std::env::var_os(KERNEL_OVERRIDE_ENV) {
        let value = value.to_string_lossy().into_owned();
        if let Some((image, initramfs)) = override_paths(&value) {
            // A quoted `'~/...'` reaches us with the tilde still on it: the
            // shell expands ~ only unquoted. Say so, because the path looks
            // right to the person who typed it.
            let hint = if value.starts_with('~') {
                "\n(the shell expands `~` only when it is unquoted — write the path in full)"
            } else {
                ""
            };
            // BOTH halves, named. Checking only the initramfs let a
            // directory with no microvm-kernel.img resolve happily and fail
            // much later inside the VMM, where the message is about a file
            // the user never typed.
            for path in [&image, &initramfs] {
                if let Some(why) = unbootable(path) {
                    return Err(Error::Runtime(format!(
                        "{KERNEL_OVERRIDE_ENV}={value}: {} {why} — an override supplies both \
                         the kernel image and initramfs.cpio{hint}",
                        path.display()
                    )));
                }
            }
            return Ok(Kernel {
                image,
                initramfs,
                origin: format!("{KERNEL_OVERRIDE_ENV}={value}"),
            });
        }
        return fetch_keg(&value).map(|mut k| {
            // Keep what the ref RESOLVED to: `ply check` wants the version
            // that will actually boot, and `@6.12` does not say which that
            // is. The override is named too, because "why is this not the
            // pin?" is the other question this line has to answer.
            k.origin = format!("{KERNEL_OVERRIDE_ENV}={value} -> {}", k.origin);
            k
        });
    }
    match fetch_keg(MICROVM_KERNEL) {
        Ok(kernel) => Ok(kernel),
        // The keg is not published yet, so on a machine that has built one
        // locally the pin is a 404 and the only way through is an env var —
        // which lives in a shell profile, and therefore is not set in the
        // terminal the user is already sitting in. That is a confusing first
        // five minutes for a feature that works.
        //
        // So: a kernel in the conventional place is used when the pin cannot
        // be had. The pin is still tried FIRST, so publishing the keg
        // immediately takes precedence over a stale local build rather than
        // being shadowed by one — the failure mode of the other ordering is a
        // Mac that quietly keeps booting a kernel from months ago.
        Err(pin_error) => {
            let dir = local_kernel_dir();
            let (image, initramfs) = (dir.join(KERNEL_FILE), dir.join(INITRAMFS_FILE));
            if unbootable(&image).is_none() && unbootable(&initramfs).is_none() {
                eprintln!(
                    "ply: {MICROVM_KERNEL} is not published yet — booting the local kernel at {}",
                    dir.display()
                );
                return Ok(Kernel {
                    image,
                    initramfs,
                    origin: format!("{} (local; {MICROVM_KERNEL} unavailable)", dir.display()),
                });
            }
            Err(pin_error)
        }
    }
}

/// Where a locally built kernel goes when the keg is not published: beside
/// everything else ply keeps for itself. `scripts/build-microvm-kernel.sh`
/// prints this path, and `resolve` falls back to it.
pub fn local_kernel_dir() -> PathBuf {
    crate::paths::data_dir().join("microvm")
}

/// Fetch `<namespace>/<name>@<version>` into the store and return the two
/// files inside it. The keg is an ordinary ply image; its payload is the
/// kernel and the initramfs at fixed paths.
///
/// `fetch_keg_image`, not `fetch_app_image`: the kernel has no entrypoint,
/// so it is a keg, and the app path refuses kegs on purpose ("not a runnable
/// app"). Giving the manifest a fake entrypoint would have made
/// `ply run ply/microvm-kernel` appear to work and laundered a guard that
/// exists for a reason; the second door is narrower and says what it is for.
fn fetch_keg(reference: &str) -> Result<Kernel> {
    let (name, want) = match reference.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (reference, None),
    };
    // Always the official `ply` namespace: a runtime artifact has no manifest
    // to carry a `[sources]` entry, so there is no user-supplied source to
    // honour here. (`fetch_keg_image` takes it as a parameter so that door
    // can be tested over a `file://` source.)
    //
    // The digest comes back from the fetch already in the `sha256:<hex>`
    // form the store keys on — re-hashing the image here would re-read
    // several megabytes on every single run for an answer we were handed.
    let (image, resolved, digest) =
        crate::catalog::fetch_keg_image(name, want, crate::catalog::OFFICIAL_RUN_SOURCE)?;
    // The keg is a squashfs image like any other. Extract once into the
    // store, then reuse.
    let root = crate::store::Store::open_default()?.extracted_rootfs(&image, &digest)?;
    let dir = keg_payload_dir(&root, &resolved);
    let kernel = Kernel {
        image: dir.join(KERNEL_FILE),
        initramfs: dir.join(INITRAMFS_FILE),
        origin: resolved.to_string(),
    };
    for path in [&kernel.image, &kernel.initramfs] {
        if let Some(why) = unbootable(path) {
            return Err(Error::Runtime(format!(
                "{reference}: the kernel keg's {} {why} — rebuild it with scripts/build-microvm-kernel.sh",
                path.display()
            )));
        }
    }
    Ok(kernel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_is_a_registry_ref_with_an_exact_version() {
        let (name, version) = MICROVM_KERNEL
            .split_once('@')
            .expect("pin carries a version");
        assert_eq!(name, "ply/microvm-kernel");
        assert!(
            semver::Version::parse(version).is_ok(),
            "the pin must be exact — a range would make two Macs boot two kernels"
        );
    }

    #[test]
    fn a_directory_override_supplies_both_files() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("microvm-kernel.img"), b"x").unwrap();
        std::fs::write(td.path().join("initramfs.cpio"), b"y").unwrap();
        let (img, initrd) = override_paths(td.path().to_str().unwrap()).unwrap();
        // Equality, not `ends_with`: the two files sit AT the directory the
        // user named. `ends_with` would still pass if they moved into a
        // subdirectory of it, which is the whole thing this pins.
        assert_eq!(img, td.path().join("microvm-kernel.img"));
        assert_eq!(initrd, td.path().join("initramfs.cpio"));
    }

    #[test]
    fn an_image_override_takes_its_initramfs_from_the_same_directory() {
        let td = tempfile::tempdir().unwrap();
        let img = td.path().join("custom.img");
        std::fs::write(&img, b"x").unwrap();
        std::fs::write(td.path().join("initramfs.cpio"), b"y").unwrap();
        let (got, initrd) = override_paths(img.to_str().unwrap()).unwrap();
        assert_eq!(got, img);
        assert_eq!(initrd, td.path().join("initramfs.cpio"));
    }

    #[test]
    fn a_registry_ref_is_not_a_path() {
        assert!(override_paths("ply/microvm-kernel@6.12.1").is_none());
    }

    #[test]
    fn shape_decides_a_path_from_a_ref_before_the_filesystem_is_asked() {
        // None of these exist. All four are paths, and a path that is not
        // there is a path error — not a package name.
        for value in [
            "/no/such/keg/boot",
            "./out/keg/boot",
            "../keg/boot",
            "~/out/keg/boot",
        ] {
            assert!(is_path_override(value), "{value}");
            assert!(override_paths(value).is_some(), "{value}");
        }
        for value in [
            "ply/microvm-kernel@6.12.1",
            "ply/microvm-kernel",
            "microvm-kernel",
        ] {
            assert!(!is_path_override(value), "{value}");
            assert!(override_paths(value).is_none(), "{value}");
        }
        // The typo this rule exists for. Under `is_dir()`-then-`is_file()`,
        // this fell through to the registry arm, which split it into the
        // namespace `Users` and a package named `me/out/keg/bot`, and ply
        // made a real network request for it.
        let (image, initramfs) = override_paths("/Users/me/out/keg/bot").expect("a path");
        assert_eq!(
            image,
            PathBuf::from("/Users/me/out/keg/bot/microvm-kernel.img")
        );
        assert_eq!(
            initramfs,
            PathBuf::from("/Users/me/out/keg/bot/initramfs.cpio")
        );
    }

    #[test]
    fn a_file_that_cannot_be_booted_from_says_which_way_it_cannot() {
        let td = tempfile::tempdir().unwrap();
        let empty = td.path().join("empty.img");
        std::fs::write(&empty, b"").unwrap();
        // An interrupted `cp` of a 5 MB Image. `exists()` said yes.
        assert_eq!(unbootable(&empty), Some("is empty"));
        let dir = td.path().join("microvm-kernel.img");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(unbootable(&dir), Some("is not a regular file"));
        let dangling = td.path().join("dangling.img");
        std::os::unix::fs::symlink(td.path().join("gone"), &dangling).unwrap();
        assert_eq!(unbootable(&dangling), Some("is a symlink to nothing"));
        assert_eq!(
            unbootable(&td.path().join("nope.img")),
            Some("does not exist")
        );
        let good = td.path().join("good.img");
        std::fs::write(&good, b"Image").unwrap();
        assert_eq!(unbootable(&good), None);
        // The mode-000 case is covered by the `File::open` probe but not
        // asserted here: root reads a mode-000 file, and some CI runs as
        // root, so the assertion would pass or fail by who ran it.
    }

    /// The one version drift that is otherwise SILENT.
    ///
    /// The version lives in three places (the manifest says so itself):
    /// `kernel/microvm-kernel.toml`, `KVER` in
    /// `scripts/build-microvm-kernel.sh`, and the pin below. The script dies
    /// loudly if its `KVER` and the manifest disagree, and a pin ahead of
    /// what is published dies loudly at fetch. But bump the manifest and
    /// `KVER`, publish, and forget this constant, and BOTH versions exist in
    /// the registry: `version_matches` is prefix-based, the old one still
    /// resolves, and every Mac quietly keeps booting the old kernel forever
    /// with nothing to report. This turns the manifest's "THREE PLACES must
    /// move in one edit" comment from an instruction into an invariant.
    #[test]
    fn the_pin_matches_the_manifest_the_build_script_ships() {
        let manifest = include_str!("../../../../kernel/microvm-kernel.toml");
        let parsed: toml::Value = toml::from_str(manifest).expect("the keg manifest parses");
        let version = parsed["package"]["version"]
            .as_str()
            .expect("[package] version");
        assert_eq!(
            MICROVM_KERNEL,
            format!("ply/microvm-kernel@{version}"),
            "kernel/microvm-kernel.toml and this pin must move in one edit \
             (and so must KVER in scripts/build-microvm-kernel.sh)"
        );
        let name = parsed["package"]["name"].as_str().expect("[package] name");
        assert_eq!(name, "microvm-kernel", "the keg's name is half the pin");
    }

    #[test]
    fn the_keg_payload_lives_where_ply_build_puts_a_kegs_files() {
        use crate::image::name::{Arch, ImageName, Os};
        let image = ImageName::new(
            "microvm-kernel",
            semver::Version::parse("6.12.0").unwrap(),
            Os::Linux,
            Arch::Arm64,
        )
        .unwrap();
        // `/opt/<name>-<version>` is ply's keg prefix (build.rs), `boot/`
        // dodges `ply build`'s refusal to pack a top-level `*.img`, and
        // scripts/build-microvm-kernel.sh stages exactly this. If this
        // assertion is edited, the script's `keg/boot` must be edited with
        // it, or a published kernel resolves to two paths that do not exist.
        assert_eq!(
            keg_payload_dir(std::path::Path::new("/store/sha256:abc/rootfs"), &image),
            std::path::PathBuf::from("/store/sha256:abc/rootfs/opt/microvm-kernel-6.12.0/boot")
        );
    }

    #[test]
    fn an_override_directory_must_hold_both_files_and_says_which_is_missing() {
        // The whole round trip through the env var, because that is what a
        // developer with a locally built keg actually does: point
        // PLY_MICROVM_KERNEL at the keg's `boot/` directory and run.
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("boot");
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("microvm-kernel.img");
        let cpio = dir.join("initramfs.cpio");
        std::fs::write(&img, b"Image").unwrap();
        std::fs::write(&cpio, b"cpio").unwrap();

        // This is the ONLY test in this module that touches the process
        // environment; the rest assert against the functions directly, so
        // two of them cannot race over this variable.
        let previous = std::env::var_os(KERNEL_OVERRIDE_ENV);
        std::env::set_var(KERNEL_OVERRIDE_ENV, &dir);
        let both = resolve();
        // An initramfs with no kernel beside it used to resolve happily and
        // fail much later, inside the VMM.
        std::fs::remove_file(&img).unwrap();
        let missing_kernel = resolve();
        // …and the mirror case, which always errored, still does.
        std::fs::write(&img, b"Image").unwrap();
        std::fs::remove_file(&cpio).unwrap();
        let missing_initramfs = resolve();
        // A typo in the path: an error about the path, not a registry
        // lookup for a package called `<home>/…/bot`.
        std::env::set_var(KERNEL_OVERRIDE_ENV, td.path().join("bot"));
        let typo = resolve();
        // A zero-byte kernel image — an interrupted `cp` — which
        // `Path::exists()` accepted.
        std::fs::write(&img, b"").unwrap();
        std::fs::write(&cpio, b"cpio").unwrap();
        std::env::set_var(KERNEL_OVERRIDE_ENV, &dir);
        let truncated = resolve();
        match previous {
            Some(v) => std::env::set_var(KERNEL_OVERRIDE_ENV, v),
            None => std::env::remove_var(KERNEL_OVERRIDE_ENV),
        }

        let k = both.expect("a directory with both files resolves");
        assert_eq!(k.image, img);
        assert_eq!(k.initramfs, cpio);
        assert!(k.origin.contains(KERNEL_OVERRIDE_ENV));

        let err = missing_kernel
            .expect_err("no kernel image is an error")
            .to_string();
        assert!(err.contains("microvm-kernel.img"), "{err}");
        let err = missing_initramfs
            .expect_err("no initramfs is an error")
            .to_string();
        assert!(err.contains("initramfs.cpio"), "{err}");

        let err = typo
            .expect_err("a path that is not there is a path error")
            .to_string();
        assert!(err.contains("bot/microvm-kernel.img"), "{err}");
        assert!(err.contains("does not exist"), "{err}");
        // The failure this replaces: a registry error and a `ply search`
        // suggestion for something the user typed as a path.
        assert!(
            !err.contains("ply search") && !err.contains("published"),
            "a typo'd path must never become a registry lookup: {err}"
        );

        let err = truncated
            .expect_err("a zero-byte kernel image is not bootable")
            .to_string();
        assert!(err.contains("microvm-kernel.img is empty"), "{err}");
    }
}
