use anyhow::Result;

use crate::cli::ExecArgs;

pub fn exec(args: ExecArgs) -> Result<()> {
    let code = run_in(&args.app, &args.cmd)?;
    std::process::exit(code);
}

/// Run `cmd` inside the instance `app` names and give back its exit code,
/// with this process's stdio attached. `ply exec` is this plus an exit;
/// `ply backup` and `ply restore` are this with a command of their own.
#[cfg(target_os = "linux")]
pub fn run_in(app: &str, cmd: &[String]) -> Result<i32> {
    Ok(ply_core::runtime::ns::exec::exec(app, cmd)?)
}

/// A microVM has no namespace to enter, so this is a client: it asks the
/// instance's worker to run the command and relays what comes back.
#[cfg(target_os = "macos")]
pub fn run_in(app: &str, cmd: &[String]) -> Result<i32> {
    Ok(ply_core::runtime::vm::exec::exec(app, cmd)?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn run_in(_app: &str, _cmd: &[String]) -> Result<i32> {
    anyhow::bail!("ply exec is not available on this platform yet")
}
