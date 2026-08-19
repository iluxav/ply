use anyhow::Result;

use crate::cli::ExecArgs;

pub fn exec(args: ExecArgs) -> Result<()> {
    let code = ply_core::runtime::exec::exec(&args.app, &args.cmd)?;
    std::process::exit(code);
}
