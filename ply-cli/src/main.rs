mod cli;
mod commands;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    if let Err(err) = commands::dispatch(cli.command) {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
