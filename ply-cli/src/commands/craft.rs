#[cfg(target_os = "linux")]
mod linux {
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
                let left = &outcome.left_out;
                if left.regenerable_files > 0 {
                    println!(
                        "left out {} package-manager cache file(s), {} — apt/apk indexes and \
                         download caches, which `apt-get update` regenerates",
                        left.regenerable_files,
                        human_size(left.regenerable_bytes)
                    );
                }
                if !left.session.is_empty() {
                    // Named, not summed: a person can mean to keep something
                    // under /tmp, and a total would hide that it went.
                    const SHOWN: usize = 6;
                    let names: Vec<String> = left
                        .session
                        .iter()
                        .take(SHOWN)
                        .map(|p| p.display().to_string())
                        .collect();
                    let more = left.session.len().saturating_sub(SHOWN);
                    println!(
                        "left out {}{} — scratch and this session's own records (/tmp, \
                         package-manager logs and locks, shell history); move anything you \
                         meant to keep out of /tmp and commit again",
                        names.join(", "),
                        if more > 0 {
                            format!(" and {more} more")
                        } else {
                            String::new()
                        }
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
}

#[cfg(target_os = "linux")]
pub use linux::dispatch;

#[cfg(not(target_os = "linux"))]
pub fn dispatch(_command: crate::cli::CraftCommand) -> anyhow::Result<()> {
    anyhow::bail!("ply craft needs Linux — it builds inside a namespace sandbox")
}
