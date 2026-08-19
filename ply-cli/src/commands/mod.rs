// One module per command as they are implemented (per TASKS.md phase).
// Unimplemented commands fall through to `todo`.

mod build;
mod exec;
mod lb;
mod ps;
mod run;

use anyhow::{bail, Result};

use crate::cli::Command;

pub fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Build(args) => build::run(args),
        Command::Run(args) => run::exec(args),
        Command::Exec(args) => exec::exec(args),
        Command::Ps(args) => ps::exec(args),
        Command::Check(_) => todo("check", "Phase 7"),
        Command::Import(_) => todo("import", "Phase 8"),
        Command::Bundle(_) => todo("bundle", "Phase 8"),
        Command::Systemd(_) => todo("systemd", "Phase 7"),
        Command::Proxy(args) => lb::proxy(args),
        Command::Lb(args) => lb::exec(args),
        Command::Gc(_) => todo("gc", "Phase 7"),
        Command::Rm(_) => todo("rm", "Phase 7"),
        Command::Audit(_) => todo("audit", "Phase 7"),
        Command::Outdated(_) => todo("outdated", "Phase 7"),
    }
}

fn todo(name: &str, phase: &str) -> Result<()> {
    bail!("`ply {name}` is not implemented yet (planned: {phase} — see TASKS.md)")
}
