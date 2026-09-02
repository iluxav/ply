// One module per command as they are implemented (per TASKS.md phase).

mod account;
mod add;
mod build;
mod control;
mod craft;
mod exec;
mod images;
mod init;
pub mod lb;
mod lifecycle;
mod logs;
mod ps;
mod reconcile;
mod run;
mod search;
mod secret;
mod self_update;
mod setup;
mod stats;
mod up;
mod volume;

use anyhow::Result;

use crate::cli::Command;

pub fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Init(args) => init::exec(args),
        Command::Build(args) => build::run(args),
        Command::Search(args) => search::exec(args),
        Command::Add(args) => add::exec(args),
        Command::Run(args) => run::exec(args),
        Command::Up(args) => up::exec(args),
        Command::Exec(args) => exec::exec(args),
        Command::Logs(args) => logs::exec(args),
        Command::Scale(args) => control::scale(args),
        Command::Restart(args) => control::restart(args),
        Command::Reconcile(args) => reconcile::exec(args),
        Command::Ps(args) => ps::exec(args),
        Command::Stats(args) => stats::exec(args),
        Command::Check(args) => lifecycle::check(args),
        Command::Inspect(args) => images::inspect(args),
        Command::Import(args) => images::import(args),
        Command::Bundle(args) => images::bundle(args),
        Command::Craft(command) => craft::dispatch(command),
        Command::Deploy(args) => lifecycle::deploy(args),
        Command::Rebase(args) => images::rebase(args),
        Command::Systemd(args) => lifecycle::systemd(args),
        Command::Proxy(args) => lb::proxy(args),
        Command::Setup(args) => setup::exec(args),
        Command::SelfUpdate(args) => self_update::exec(args),
        Command::Login => account::login(),
        Command::Whoami => account::whoami(),
        Command::Key(cmd) => match cmd {
            crate::cli::KeyCommand::New(args) => account::key_new(args.note.as_deref()),
            crate::cli::KeyCommand::Ls => account::key_ls(),
            crate::cli::KeyCommand::Rm(args) => account::key_rm(args.id),
        },
        Command::Push(args) => account::push(args),
        Command::Sync(args) => lifecycle::sync(args),
        Command::Gc(args) => lifecycle::gc(args),
        Command::Volume(cmd) => match cmd {
            crate::cli::VolumeCommand::Ls(args) => volume::ls(&args),
            crate::cli::VolumeCommand::Rm(args) => volume::rm(&args),
        },
        Command::Rm(args) => lifecycle::rm(args),
        Command::Secret(cmd) => match cmd {
            crate::cli::SecretCommand::Ls(args) => secret::exec_ls(&args),
            crate::cli::SecretCommand::Set(args) => secret::exec_set(&args),
        },
        Command::Audit(args) => lifecycle::audit(args),
        Command::Outdated(args) => lifecycle::outdated(args),
    }
}
