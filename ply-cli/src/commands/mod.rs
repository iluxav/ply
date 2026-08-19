// One module per command as they are implemented (per TASKS.md phase).

mod build;
mod craft;
mod exec;
mod images;
mod lb;
mod lifecycle;
mod ps;
mod run;
mod setup;

use anyhow::Result;

use crate::cli::Command;

pub fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Build(args) => build::run(args),
        Command::Run(args) => run::exec(args),
        Command::Exec(args) => exec::exec(args),
        Command::Ps(args) => ps::exec(args),
        Command::Check(args) => lifecycle::check(args),
        Command::Import(args) => images::import(args),
        Command::Bundle(args) => images::bundle(args),
        Command::Craft(command) => craft::dispatch(command),
        Command::Rebase(args) => images::rebase(args),
        Command::Systemd(args) => lifecycle::systemd(args),
        Command::Proxy(args) => lb::proxy(args),
        Command::Lb(args) => lb::exec(args),
        Command::Setup => setup::exec(),
        Command::Sync(args) => lifecycle::sync(args),
        Command::Gc(args) => lifecycle::gc(args),
        Command::Rm(args) => lifecycle::rm(args),
        Command::Audit(args) => lifecycle::audit(args),
        Command::Outdated(args) => lifecycle::outdated(args),
    }
}
