use anyhow::Result;

use crate::cli::ExecArgs;

#[cfg(target_os = "linux")]
pub fn exec(args: ExecArgs) -> Result<()> {
    let code = ply_core::runtime::ns::exec::exec(&args.app, &args.cmd)?;
    std::process::exit(code);
}

/// A microVM has no namespace to enter, so this is a client: it asks the
/// instance's worker to run the command and relays what comes back.
#[cfg(target_os = "macos")]
pub fn exec(args: ExecArgs) -> Result<()> {
    let code = ply_core::runtime::vm::exec::exec(&args.app, &args.cmd)?;
    std::process::exit(code);
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn exec(_args: ExecArgs) -> Result<()> {
    anyhow::bail!("ply exec is not available on this platform yet")
}
