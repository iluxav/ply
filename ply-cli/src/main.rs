mod cli;
mod commands;

use clap::Parser;

fn main() {
    ply_core::restore_default_sigpipe();
    let parsed = cli::Cli::try_parse();
    let cli = match parsed {
        Ok(cli) => cli,
        Err(err) => {
            // A Docker verb? Teach the ply way instead of "unrecognized
            // subcommand" — switchers learn the model where they stumble.
            if let Some(hint) = std::env::args()
                .nth(1)
                .as_deref()
                .and_then(cli::docker_hint)
            {
                eprintln!(
                    "ply: no `{}` subcommand — {hint}",
                    std::env::args().nth(1).unwrap_or_default()
                );
                std::process::exit(2);
            }
            err.exit();
        }
    };
    if let Err(err) = commands::dispatch(cli.command) {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
