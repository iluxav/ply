use anyhow::Result;

use crate::cli::ExecArgs;

#[cfg(target_os = "linux")]
pub fn exec(args: ExecArgs) -> Result<()> {
    let code = ply_core::runtime::ns::exec::exec(&args.app, &args.cmd)?;
    std::process::exit(code);
}

#[cfg(not(target_os = "linux"))]
pub fn exec(_args: ExecArgs) -> Result<()> {
    anyhow::bail!("ply exec is not available on this platform yet")
}
