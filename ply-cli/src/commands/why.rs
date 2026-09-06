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
    // Asleep, there is no instance to name the image: the marker does.
    let asleep = ply_core::runtime::after::AsleepMarker::find(&args.app);
    let image = states
        .first()
        .map(|s| s.image.clone())
        .or_else(|| asleep.as_ref().map(|m| m.image.clone()));
    let manifest = image
        .and_then(|i| ply_core::image::read::read_manifest(std::path::Path::new(&i)).ok());
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
        asleep.as_ref(),
        |slot| logring::tail(&args.app, slot, ply_core::why::LOG_TAIL_LINES),
    );
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render());
    }
    Ok(())
}
