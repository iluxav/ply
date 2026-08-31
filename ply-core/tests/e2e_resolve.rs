//! End-to-end: build synthetic packages into a registry dir, then build an
//! app against it — resolve (MVS), fetch, verify, store, lockfile.
//! The http test is the same flow through `python3 -m http.server`.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ply_core::build::{build, BuildOptions};
use ply_core::image::read::{read_lockfile, read_manifest};

/// PLY_STORE is process-global; serialize tests that set it.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn build_dir(dir: &Path, ply_toml: &str, files: &[(&str, &str)]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("ply.toml"), ply_toml).unwrap();
    for (name, contents) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    build(&BuildOptions {
        dir: dir.to_path_buf(),
        output: None,
        allow_insecure: false,
        allow_secrets: false,
        arch: None,
    })
    .unwrap()
    .image_path
}

/// Publish alpine 3.20.0 + 3.20.5 (base) and mylib 1.2.0 into `registry`.
fn populate_registry(work: &Path, registry: &Path) {
    std::fs::create_dir_all(registry).unwrap();
    for alpine_version in ["3.20.0", "3.20.5"] {
        let img = build_dir(
            &work.join(format!("alpine-{alpine_version}")),
            &format!(
                r#"
                [package]
                name = "alpine"
                version = "{alpine_version}"
                base = true
                "#
            ),
            &[("bin/sh", "fake shell")],
        );
        std::fs::copy(&img, registry.join(img.file_name().unwrap())).unwrap();
    }
    let img = build_dir(
        &work.join("mylib"),
        r#"
        [package]
        name = "mylib"
        version = "1.2.0"

        [layer]
        path = ["/opt/mylib-1.2.0/bin"]

        [dependencies]
        alpine = "3.20"
        "#,
        &[("bin/mytool", "tool")],
    );
    std::fs::copy(&img, registry.join(img.file_name().unwrap())).unwrap();
}

fn app_toml(source: &str) -> String {
    format!(
        r#"
        [package]
        name = "myapp"
        version = "0.1.0"
        entrypoint = ["./run.sh"]
        base = "alpine@3.20"

        [dependencies]
        mylib = "1"

        [sources]
        default = "{source}"
        "#
    )
}

fn assert_resolution(app_dir: &Path, image_path: &Path, store: &Path) {
    let lock = ply_core::lockfile::Lockfile::load(&app_dir.join("ply.lock")).unwrap();
    let summary: Vec<(String, String)> = lock
        .packages
        .iter()
        .map(|p| (p.name.clone(), p.version.to_string()))
        .collect();
    // MVS: 3.20.0 selected even though 3.20.5 is published; base last.
    assert_eq!(
        summary,
        vec![
            ("mylib".to_string(), "1.2.0".to_string()),
            ("alpine".to_string(), "3.20.0".to_string()),
        ]
    );
    // Store holds every locked digest.
    for pkg in &lock.packages {
        assert!(
            store.join(&pkg.sha256).join("pkg.img").exists(),
            "store missing {}",
            pkg.sha256
        );
    }
    // The image embeds the same lockfile.
    let embedded = read_lockfile(image_path).unwrap().expect("embedded lock");
    assert_eq!(embedded, lock);
    assert!(read_manifest(image_path).unwrap().is_app());
}

#[test]
fn resolves_via_file_source() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("registry");
    populate_registry(&tmp.path().join("work"), &registry);

    let store = tmp.path().join("store");
    std::env::set_var("PLY_STORE", &store);

    let app_dir = tmp.path().join("app");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(
        app_dir.join("ply.toml"),
        app_toml(&format!("file://{}", registry.display())),
    )
    .unwrap();

    let opts = BuildOptions {
        dir: app_dir.clone(),
        output: None,
        allow_insecure: false,
        allow_secrets: false,
        arch: None,
    };
    let one = build(&opts).unwrap();
    assert_resolution(&app_dir, &one.image_path, &store);

    // Deterministic: same manifest → same lockfile, same image bytes.
    let two = build(&opts).unwrap();
    assert_eq!(one.digest, two.digest);
    std::env::remove_var("PLY_STORE");
}

#[test]
fn resolves_via_http_source() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("registry");
    populate_registry(&tmp.path().join("work"), &registry);

    // index.json — the only thing a dumb http host needs for version listing.
    let filenames: Vec<String> = std::fs::read_dir(&registry)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    std::fs::write(
        registry.join("index.json"),
        serde_json::to_string(&filenames).unwrap(),
    )
    .unwrap();

    let port = free_port();
    let mut server = std::process::Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "-d"])
        .arg(&registry)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 available");
    wait_for_port(port);

    let store = tmp.path().join("store");
    std::env::set_var("PLY_STORE", &store);

    let app_dir = tmp.path().join("app");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(
        app_dir.join("ply.toml"),
        app_toml(&format!("http://127.0.0.1:{port}")),
    )
    .unwrap();

    let outcome = build(&BuildOptions {
        dir: app_dir.clone(),
        output: None,
        allow_insecure: false,
        allow_secrets: false,
        arch: None,
    });
    server.kill().ok();
    server.wait().ok();
    let outcome = outcome.unwrap();
    assert_resolution(&app_dir, &outcome.image_path, &store);
    std::env::remove_var("PLY_STORE");
}

/// The base from `[package]` must reach resolution on its own — this app has
/// no [dependencies] at all, so only base injection can pull alpine in.
#[test]
fn resolves_base_only_app() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("registry");
    populate_registry(&tmp.path().join("work"), &registry);

    let store = tmp.path().join("store");
    std::env::set_var("PLY_STORE", &store);

    let app_dir = tmp.path().join("app");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(
        app_dir.join("ply.toml"),
        format!(
            r#"
            [package]
            name = "baseonly"
            version = "0.1.0"
            entrypoint = ["./run.sh"]
            base = "alpine@3.20"

            [sources]
            default = "file://{}"
            "#,
            registry.display()
        ),
    )
    .unwrap();

    build(&BuildOptions {
        dir: app_dir.clone(),
        output: None,
        allow_insecure: false,
        allow_secrets: false,
        arch: None,
    })
    .unwrap();

    let lock = ply_core::lockfile::Lockfile::load(&app_dir.join("ply.lock")).unwrap();
    let summary: Vec<(String, String)> = lock
        .packages
        .iter()
        .map(|p| (p.name.clone(), p.version.to_string()))
        .collect();
    assert_eq!(summary, vec![("alpine".to_string(), "3.20.0".to_string())]);
    std::env::remove_var("PLY_STORE");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_port(port: u16) {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    panic!("http.server did not start");
}
