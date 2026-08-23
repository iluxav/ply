// One module per command as they are implemented (per TASKS.md phase).

mod add;
mod build;
mod craft;
mod exec;
mod images;
mod init;
mod lb;
mod lifecycle;
mod ps;
mod run;
mod search;
mod setup;
mod stats;

use anyhow::Result;

use crate::cli::Command;

pub fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Init(args) => init::exec(args),
        Command::Build(args) => build::run(args),
        Command::Search(args) => search::exec(args),
        Command::Add(args) => add::exec(args),
        Command::Run(args) => run::exec(args),
        Command::Exec(args) => exec::exec(args),
        Command::Ps(args) => ps::exec(args),
        Command::Stats(args) => stats::exec(args),
        Command::Check(args) => lifecycle::check(args),
        Command::Import(args) => images::import(args),
        Command::Bundle(args) => images::bundle(args),
        Command::Craft(command) => craft::dispatch(command),
        Command::Deploy(args) => lifecycle::deploy(args),
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
