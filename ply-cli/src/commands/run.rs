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

    // A bare name (`postgres`, `myapp@1.2`) resolves against the registry:
    // newest matching version, fetched into the store. Only when nothing by
    // that name exists locally — an existing file always wins.
    let image_arg =
        if !args.image.starts_with("docker://") && !std::path::Path::new(&args.image).exists() {
            match ply_core::catalog::parse_run_ref(&args.image) {
                Some((name, want)) => {
                    let (path, resolved) =
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
    })?;
    std::process::exit(code);
}
