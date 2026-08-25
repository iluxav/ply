use anyhow::{bail, Result};
use ply_core::runtime::run::{parse_env_file, run, RunOptions};

use crate::cli::RunArgs;

pub fn exec(args: RunArgs) -> Result<()> {
    let mut links: Vec<(std::path::PathBuf, String)> = Vec::new();
    for link in &args.link {
        let Some((host, container)) = link.split_once(':') else {
            bail!("--link `{link}`: expected HOST:CONTAINER, e.g. ./src:/opt/myapp");
        };
        if !container.starts_with('/') {
            bail!("--link `{link}`: container path must be absolute");
        }
        links.push((host.into(), container.to_string()));
    }

    let mut cli_env: Vec<(String, String)> = Vec::new();
    if let Some(file) = &args.env_file {
        cli_env.extend(parse_env_file(file)?);
    }
    for pair in &args.env {
        let Some((k, v)) = pair.split_once('=') else {
            bail!("--env `{pair}`: expected KEY=VALUE");
        };
        cli_env.push((k.to_string(), v.to_string()));
    }

    let publish = args
        .publish
        .iter()
        .map(|s| ply_core::runtime::publish::parse_publish(s))
        .collect::<Result<Vec<_>, _>>()?;
    // Two listeners on one host port is a config error, not a race to lose
    // at bind time with a confusing "address in use".
    for (i, spec) in publish.iter().enumerate() {
        if let Some(dup) = publish[..i].iter().find(|p| p.host_port == spec.host_port) {
            bail!(
                "--publish {}: host port {} is already published by an earlier --publish",
                spec.host_port,
                dup.host_port
            );
        }
    }

    // Three run forms besides a plain .img path:
    //  - a directory with ply.toml: build it, run the result (cargo run)
    //  - a bare name (`postgres`, `myapp@1.2`): resolve against the registry,
    //    newest matching version, fetched into the store
    //  - docker://: OCI import (below)
    // An existing file always wins over a name lookup.
    let mut dev_entrypoint: Option<Vec<String>> = None;
    let image_path = std::path::Path::new(&args.image);
    let image_arg = if !args.image.starts_with("docker://") && image_path.is_dir() {
        let dir = image_path.to_path_buf();
        if ply_core::stack::load(&dir)?.is_some() {
            bail!(
                "{} is a [stack] — `ply up` starts stacks; `ply run` runs one app",
                dir.join("ply.toml").display()
            );
        }
        if !dir.join("ply.toml").exists() {
            bail!(
                "{} has no ply.toml — `ply run DIR` builds and runs an app directory",
                dir.display()
            );
        }
        let image = match ply_core::build::up_to_date_image(&dir, None)? {
            Some(image) => {
                eprintln!(
                    "ply: {} up to date",
                    image.file_name().unwrap_or_default().to_string_lossy()
                );
                image
            }
            None => {
                let outcome = ply_core::build::build(&ply_core::build::BuildOptions {
                    dir: dir.clone(),
                    output: None,
                    allow_insecure: false,
                    arch: None,
                })?;
                eprintln!("ply: built {}", outcome.image_name);
                outcome.image_path
            }
        };
        // The dev overlay applies only on the DIR form: it belongs to the
        // working tree, not the artifact — plain image runs stay pristine.
        let app = ply_core::image::read::read_manifest(&image)?.package.name;
        if let Some(overlay) = ply_core::dev::load(&dir, &app)? {
            eprintln!("ply: applying ply.dev.toml ({})", overlay.describe());
            dev_entrypoint = overlay.entrypoint;
            // before the explicit -e flags, so those still win
            let mut merged = overlay.env;
            merged.append(&mut cli_env);
            cli_env = merged;
            links.extend(overlay.links);
        }
        image.to_string_lossy().into_owned()
    } else if !args.image.starts_with("docker://") && !image_path.exists() {
        match ply_core::catalog::parse_run_ref(&args.image) {
            Some((name, want)) => {
                let (path, resolved, _digest) =
                    ply_core::catalog::fetch_app_image(&name, want.as_deref(), &args.source)?;
                eprintln!("ply: resolved {} -> {resolved}", args.image);
                path.to_string_lossy().into_owned()
            }
            None => args.image.clone(),
        }
    } else {
        args.image.clone()
    };

    // `docker://…` becomes a local image before anything else looks at it, so
    // every downstream path (manifest read, lockfile, deploy) sees a plain file.
    let image = ply_core::oci::ensure_local(&image_arg, args.pull)?;

    let code = run(&RunOptions {
        image,
        cli_env,
        allow_insecure: true, // lockfile digests pin content; run never resolves
        scale: args.scale,
        links,
        publish,
        after: args.after.clone(),
        after_timeout: ply_core::manifest::parse_duration(&args.after_timeout)
            .map_err(|e| anyhow::anyhow!("--after-timeout: {e}"))?,
        privileged: args.privileged,
        entrypoint: dev_entrypoint,
    })?;
    std::process::exit(code);
}
