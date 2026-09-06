//! `ply why APP`: collect the files, let `ply_core::why` decide what they say.
use anyhow::Result;
use ply_core::runtime::{events, logring, state};

use crate::cli::WhyArgs;

pub fn exec(args: WhyArgs) -> Result<()> {
    let states: Vec<_> = state::list()?
        .into_iter()
        .filter(|s| s.app == args.app && s.alive())
        .collect();
    let events = events::read();
    let egress = ply_core::egress::log::read_app(&args.app);
    let manifest = states
        .first()
        .and_then(|s| ply_core::image::read::read_manifest(std::path::Path::new(&s.image)).ok());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let report = ply_core::why::build(
        &args.app,
        now,
        &states,
        &events,
        &egress,
        manifest.as_ref(),
        |slot| logring::tail(&args.app, slot, ply_core::why::LOG_TAIL_LINES),
    );
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render());
    }
    Ok(())
}
