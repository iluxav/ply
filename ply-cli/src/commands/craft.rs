use anyhow::Result;
use ply_core::craft::{self, Change};

use crate::cli::CraftCommand;
use crate::commands::build::human_size;

pub fn dispatch(command: CraftCommand) -> Result<()> {
    match command {
        CraftCommand::New(args) => {
            let code = craft::new(
                &args.name,
                &args.from,
                args.source.as_deref(),
                &args.cmd,
                args.insecure_source,
            )?;
            std::process::exit(code);
        }
        CraftCommand::Shell(args) => {
            let code = craft::shell(&args.name, &args.cmd)?;
            std::process::exit(code);
        }
        CraftCommand::Edit(args) => {
            let name = craft::edit(&args.image, args.source.as_deref(), args.insecure_source)?;
            println!(
                "session `{name}` reconstructed from {} — continue with `ply craft shell {name}`",
                args.image.display()
            );
            Ok(())
        }
        CraftCommand::Changes(args) => {
            let changes = craft::changes(&args.name)?;
            if changes.is_empty() {
                println!("no changes yet");
                return Ok(());
            }
            for change in changes {
                match change {
                    Change::Added(p) => println!("A {}", p.display()),
                    Change::Modified(p) => println!("M {}", p.display()),
                    Change::Deleted(p) => println!("D {}", p.display()),
                }
            }
            Ok(())
        }
        CraftCommand::Commit(args) => {
            let outcome = craft::commit(&args.name, &args.version, args.output.as_deref())?;
            if outcome.skipped_deletions > 0 {
                eprintln!(
                    "warning: {} deletion(s) not packaged — packages can add and modify files, not remove them (yet)",
                    outcome.skipped_deletions
                );
            }
            println!(
                "committed {} ({})",
                outcome.image_path.display(),
                human_size(outcome.size_bytes)
            );
            println!("{}", outcome.digest);
            println!(
                "use it:  [dependencies] {} = \"{}\"  (+ a source that serves it)",
                outcome.image_name.name, outcome.image_name.version
            );
            Ok(())
        }
        CraftCommand::Ls => {
            let names = craft::list()?;
            if names.is_empty() {
                println!("no craft sessions");
            }
            for name in names {
                println!("{name}");
            }
            Ok(())
        }
        CraftCommand::Rm(args) => {
            if craft::rm(&args.name)? {
                println!("removed session {}", args.name);
            } else {
                println!("no session named {}", args.name);
            }
            Ok(())
        }
    }
}
