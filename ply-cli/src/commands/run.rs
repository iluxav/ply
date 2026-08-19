use anyhow::{bail, Result};
use ply_core::runtime::run::{parse_env_file, run, RunOptions};

use crate::cli::RunArgs;

pub fn exec(args: RunArgs) -> Result<()> {
    if !args.link.is_empty() {
        bail!("--link is not implemented yet (planned: Phase 6 — see TASKS.md)");
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

    let code = run(&RunOptions {
        image: args.image,
        cli_env,
        allow_insecure: true, // lockfile digests pin content; run never resolves
        scale: args.scale,
    })?;
    std::process::exit(code);
}
