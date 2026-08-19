// One module per command as they are implemented (per TASKS.md phase).
// Unimplemented commands fall through to `todo`.

use anyhow::{bail, Result};

use crate::cli::Command;

pub fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Build(_) => todo("build", "Phase 1"),
        Command::Run(_) => todo("run", "Phase 2"),
        Command::Exec(_) => todo("exec", "Phase 6"),
        Command::Ps(_) => todo("ps", "Phase 5"),
        Command::Check(_) => todo("check", "Phase 7"),
        Command::Import(_) => todo("import", "Phase 8"),
        Command::Bundle(_) => todo("bundle", "Phase 8"),
        Command::Systemd(_) => todo("systemd", "Phase 7"),
        Command::Proxy(_) => todo("proxy", "Phase 5"),
        Command::Lb(_) => todo("lb", "Phase 5"),
        Command::Gc(_) => todo("gc", "Phase 7"),
        Command::Rm(_) => todo("rm", "Phase 7"),
        Command::Audit(_) => todo("audit", "Phase 7"),
        Command::Outdated(_) => todo("outdated", "Phase 7"),
    }
}

fn todo(name: &str, phase: &str) -> Result<()> {
    bail!("`ply {name}` is not implemented yet (planned: {phase} — see TASKS.md)")
}
