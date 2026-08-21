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
        .as_deref()
        .map(ply_core::runtime::publish::parse_publish)
        .transpose()?;

    let code = run(&RunOptions {
        image: args.image,
        cli_env,
        allow_insecure: true, // lockfile digests pin content; run never resolves
        scale: args.scale,
        links,
        publish,
    })?;
    std::process::exit(code);
}
