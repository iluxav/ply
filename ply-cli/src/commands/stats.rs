use anyhow::Result;

use crate::cli::StatsArgs;
use crate::commands::build::human_size;

pub fn exec(args: StatsArgs) -> Result<()> {
    let stats = ply_core::stats::collect(args.app.as_deref(), args.sample_ms)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
        return Ok(());
    }
    if stats.is_empty() {
        println!("no running instances");
        return Ok(());
    }

    let header = format!(
        "{:<24} {:>7} {:>10} {:>10} {:>10} {:>6} {:>10} {:>10} THR",
        "NAME", "CPU%", "MEM", "PEAK", "LIMIT", "PIDS", "NET RX", "NET TX"
    );
    println!("{header}");
    for s in &stats {
        let opt_size = |v: Option<u64>| v.map(human_size).unwrap_or_else(|| "-".into());
        println!(
            "{:<24} {:>7} {:>10} {:>10} {:>10} {:>6} {:>10} {:>10} {}",
            format!("{}.{}", s.app, s.n),
            s.cpu_percent
                .map(|p| format!("{p:.1}"))
                .unwrap_or_else(|| "-".into()),
            opt_size(s.mem_current),
            opt_size(s.mem_peak),
            opt_size(s.mem_max),
            s.pids_current
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            opt_size(s.net_rx),
            opt_size(s.net_tx),
            s.throttled
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".into()),
        );
    }
    Ok(())
}
