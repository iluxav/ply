use std::io::Read;

use anyhow::{bail, Result};
use ply_core::runtime::logring;

use crate::cli::LogsArgs;

pub fn exec(args: LogsArgs) -> Result<()> {
    let rings = logring::list();
    let Some(target) = &args.app else {
        if rings.is_empty() {
            bail!("no logs yet — rings appear once an app runs");
        }
        for (app, n) in &rings {
            println!("{app}.{n}");
        }
        return Ok(());
    };

    // `app` = every instance of the app; `app.N` = that instance
    let selected: Vec<(String, u32)> = match target.rsplit_once('.') {
        Some((app, n)) if n.parse::<u32>().is_ok() => {
            let n: u32 = n.parse().unwrap();
            rings
                .iter()
                .filter(|(a, i)| a == app && *i == n)
                .cloned()
                .collect()
        }
        _ => rings.iter().filter(|(a, _)| a == target).cloned().collect(),
    };
    if selected.is_empty() {
        bail!("no logs for `{target}` — `ply logs` lists what exists, `ply ps` what runs");
    }
    let prefix = selected.len() > 1;

    for (app, n) in &selected {
        for line in logring::tail(app, *n, args.lines) {
            if prefix {
                println!("{app}.{n} | {line}");
            } else {
                println!("{line}");
            }
        }
    }
    if !args.follow {
        return Ok(());
    }

    // Follow by polling size deltas — the ring is a plain file, and 300ms is
    // indistinguishable from tail -f at human speed.
    let mut offsets: Vec<(String, u32, u64)> = selected
        .iter()
        .map(|(app, n)| {
            let size = std::fs::metadata(logring::path(app, *n))
                .map(|m| m.len())
                .unwrap_or(0);
            (app.clone(), *n, size)
        })
        .collect();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(300));
        for (app, n, offset) in &mut offsets {
            let path = logring::path(app, *n);
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let size = meta.len();
            if size < *offset {
                *offset = 0; // rotated under us: catch the fresh file from its start
            }
            if size == *offset {
                continue;
            }
            if let Ok(mut file) = std::fs::File::open(&path) {
                use std::io::Seek;
                let _ = file.seek(std::io::SeekFrom::Start(*offset));
                let mut chunk = String::new();
                if file.read_to_string(&mut chunk).is_ok() {
                    for line in chunk.lines() {
                        if prefix {
                            println!("{app}.{n} | {line}");
                        } else {
                            println!("{line}");
                        }
                    }
                    *offset = size;
                }
            }
        }
    }
}
