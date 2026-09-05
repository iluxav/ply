//! Autoscaling policy: what `[scale]` means and what the run parent should
//! do given the last half-minute of samples. Pure — no files, no clocks of
//! its own — so every rule here is a unit test. Sampling lives in
//! `runtime::ns::probe`, acting in `runtime::run`.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// What is measured, per instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    Cpu,
    Memory,
    Net,
    /// A gauge or counter by name from the app's Prometheus-text endpoint.
    Metric(String),
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Signal::Cpu => write!(f, "cpu"),
            Signal::Memory => write!(f, "memory"),
            Signal::Net => write!(f, "net"),
            Signal::Metric(name) => write!(f, "{name}"),
        }
    }
}

/// The per-instance level the policy aims for, in the signal's unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Percent(f64),
    BytesPerSec(f64),
    Number(f64),
}

impl Target {
    fn value(self) -> f64 {
        match self {
            Target::Percent(v) | Target::BytesPerSec(v) | Target::Number(v) => v,
        }
    }
    fn show(self, v: f64) -> String {
        match self {
            Target::Percent(_) => format!("{v:.0}%"),
            Target::BytesPerSec(_) => show_rate(v),
            Target::Number(_) => format!("{v:.0}"),
        }
    }
}

fn show_rate(v: f64) -> String {
    if v >= 1e9 {
        format!("{:.1}GB/s", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.1}MB/s", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1}KB/s", v / 1e3)
    } else {
        format!("{v:.0}B/s")
    }
}

/// `[scale]`, validated.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub min: u32,
    pub max: u32,
    pub signal: Signal,
    pub target: Target,
    pub cooldown: Duration,
    pub metrics_path: String,
}

/// How far back the samples that vote reach, and how long a new instance
/// warms up before it votes.
pub const WINDOW: Duration = Duration::from_secs(30);
/// A slot needs this many samples in the window to have an opinion.
const MIN_SAMPLES: usize = 3;
/// Scale down only when the average is under this share of the target.
const DOWN_HYSTERESIS: f64 = 0.7;
/// Scale up only when the average is over this multiple of the target:
/// `ceil()` would otherwise turn a hair over target into an instance.
const UP_TOLERANCE: f64 = 1.1;

impl Policy {
    /// `has_mem_limit`: `[resources] mem` is set (memory is measured against
    /// it). `has_port`: something is published to scrape a metric from.
    pub fn parse(
        scale: &crate::manifest::Scale,
        has_mem_limit: bool,
        has_port: bool,
    ) -> Result<Policy> {
        if scale.min < 1 {
            return Err(Error::Manifest("scale.min must be at least 1".into()));
        }
        if scale.max < scale.min {
            return Err(Error::Manifest(format!(
                "scale.max ({}) must be at least scale.min ({})",
                scale.max, scale.min
            )));
        }
        let signal = match scale.signal.as_str() {
            "cpu" => Signal::Cpu,
            "memory" => Signal::Memory,
            "net" => Signal::Net,
            s => match s.strip_prefix("metric:") {
                Some(name) if !name.is_empty() => Signal::Metric(name.to_string()),
                _ => {
                    return Err(Error::Manifest(format!(
                        "scale.signal `{s}`: expected cpu, memory, net, or metric:<name>"
                    )))
                }
            },
        };
        if signal == Signal::Memory && !has_mem_limit {
            return Err(Error::Manifest(
                "scale.signal = \"memory\" measures against resources.mem — set one".into(),
            ));
        }
        if matches!(signal, Signal::Metric(_)) && !has_port {
            return Err(Error::Manifest(
                "scale.signal = \"metric:…\" is scraped on the first published port — publish one"
                    .into(),
            ));
        }
        let target = parse_target(&signal, &scale.target)?;
        let cooldown = match &scale.cooldown {
            Some(s) => crate::manifest::parse_duration(s)?,
            None => Duration::from_secs(60),
        };
        Ok(Policy {
            min: scale.min,
            max: scale.max,
            signal,
            target,
            cooldown,
            metrics_path: scale
                .metrics_path
                .clone()
                .unwrap_or_else(|| "/metrics".to_string()),
        })
    }
}

pub fn parse_target(signal: &Signal, s: &str) -> Result<Target> {
    let s = s.trim();
    match signal {
        Signal::Cpu | Signal::Memory => {
            let pct = s
                .strip_suffix('%')
                .and_then(|n| n.trim().parse::<f64>().ok())
                .filter(|v| *v > 0.0)
                .ok_or_else(|| {
                    Error::Manifest(format!(
                        "scale.target `{s}` for {signal}: expected a percent like \"70%\""
                    ))
                })?;
            Ok(Target::Percent(pct))
        }
        Signal::Net => parse_rate(s).map(Target::BytesPerSec),
        Signal::Metric(_) => s
            .parse::<f64>()
            .ok()
            .filter(|v| *v > 0.0)
            .map(Target::Number)
            .ok_or_else(|| {
                Error::Manifest(format!(
                    "scale.target `{s}` for {signal}: expected a positive number"
                ))
            }),
    }
}

/// "40MB/s", "1.5GB/s", "512KB/s", or plain bytes per second.
pub fn parse_rate(s: &str) -> Result<f64> {
    let bad = || Error::Manifest(format!("`{s}`: expected a rate like \"40MB/s\""));
    let body = s.trim().strip_suffix("/s").unwrap_or(s.trim());
    let (num, mult) = if let Some(n) = body.strip_suffix("GB") {
        (n, 1e9)
    } else if let Some(n) = body.strip_suffix("MB") {
        (n, 1e6)
    } else if let Some(n) = body.strip_suffix("KB") {
        (n, 1e3)
    } else if let Some(n) = body.strip_suffix('B') {
        (n, 1.0)
    } else {
        (body, 1.0)
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| *v > 0.0)
        .map(|v| v * mult)
        .ok_or_else(bad)
}

/// One instance's counters at one moment. `None` = not readable here
/// (no cgroup controller, no limit set, veth gone).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Raw {
    pub cpu_usec: Option<u64>,
    pub nr_throttled: Option<u64>,
    pub mem_current: Option<u64>,
    pub mem_max: Option<u64>,
    pub oom_kill: Option<u64>,
    /// Host-side veth rx + tx: everything the instance sent and received.
    pub net_bytes: Option<u64>,
    /// `cpu.max` quota in millicores; `None` when unlimited.
    pub cpu_quota_m: Option<u64>,
}

/// Rates over the interval between two snapshots.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    pub cpu_pct_of_core: Option<f64>,
    pub cpu_pct_of_quota: Option<f64>,
    pub mem_pct: Option<f64>,
    pub net_bps: Option<f64>,
    pub throttled_grew: bool,
    pub oom_grew: bool,
}

impl Reading {
    pub fn between(prev: &Raw, now: &Raw, dt: Duration) -> Reading {
        let secs = dt.as_secs_f64().max(1e-3);
        let delta = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (Some(a), Some(b)) if b >= a => Some(b - a),
            _ => None,
        };
        let cpu_pct_of_core =
            delta(prev.cpu_usec, now.cpu_usec).map(|d| d as f64 / 1e6 / secs * 100.0);
        // Against the quota in force when the interval began: a grow during
        // the interval did not let the instance use more than the old quota,
        // so dividing by the new one would call a throttled instance idle.
        let quota = prev.cpu_quota_m.or(now.cpu_quota_m);
        let cpu_pct_of_quota = match (cpu_pct_of_core, quota) {
            (Some(pct), Some(q)) if q > 0 => Some(pct * 1000.0 / q as f64),
            _ => None,
        };
        let mem_pct = match (now.mem_current, now.mem_max) {
            (Some(c), Some(m)) if m > 0 => Some(c as f64 / m as f64 * 100.0),
            _ => None,
        };
        Reading {
            cpu_pct_of_core,
            cpu_pct_of_quota,
            mem_pct,
            net_bps: delta(prev.net_bytes, now.net_bytes).map(|d| d as f64 / secs),
            throttled_grew: delta(prev.nr_throttled, now.nr_throttled).is_some_and(|d| d > 0),
            oom_grew: delta(prev.oom_kill, now.oom_kill).is_some_and(|d| d > 0),
        }
    }
}

/// "512M", "1G", "262144K", plain bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (digits, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1024u64),
        Some('M' | 'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    digits.parse::<u64>().map(|n| n * mult).map_err(|_| {
        Error::Manifest(format!(
            "resources.mem `{s}`: expected e.g. \"512M\", \"1G\""
        ))
    })
}

/// "1.5" cores → 1500 millicores.
pub fn parse_millicores(s: &str) -> Result<u64> {
    let cores: f64 = s.trim().parse().map_err(|_| {
        Error::Manifest(format!(
            "resources.cpu `{s}`: expected a number like \"1.5\""
        ))
    })?;
    if cores <= 0.0 {
        return Err(Error::Manifest(format!("resources.cpu `{s}` must be > 0")));
    }
    Ok((cores * 1000.0).round() as u64)
}

/// What the parent should do this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Hold,
    ScaleTo { n: u32, reason: String },
}

/// The horizontal policy with its memory: the sample window per slot, the
/// last step, and the operator's pin.
pub struct Horizontal {
    policy: Policy,
    samples: BTreeMap<u32, VecDeque<(Instant, f64)>>,
    last_step: Option<Instant>,
    pinned: Option<u32>,
}

impl Horizontal {
    pub fn new(policy: Policy) -> Self {
        Horizontal {
            policy,
            samples: BTreeMap::new(),
            last_step: None,
            pinned: None,
        }
    }
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// One sample for one slot. An instance younger than the window is
    /// warming up and does not vote; nor do samples from before the last
    /// step, which changed the thing being measured.
    pub fn observe(&mut self, slot: u32, started_at: Instant, now: Instant, value: f64) {
        if now.saturating_duration_since(started_at) < WINDOW {
            return;
        }
        if let Some(step) = self.last_step {
            if now <= step {
                return;
            }
        }
        let q = self.samples.entry(slot).or_default();
        q.push_back((now, value));
        while let Some((t, _)) = q.front() {
            if now.saturating_duration_since(*t) > WINDOW {
                q.pop_front();
            } else {
                break;
            }
        }
    }

    /// A slot that is gone takes its samples with it.
    pub fn forget(&mut self, slot: u32) {
        self.samples.remove(&slot);
    }

    pub fn pin(&mut self, n: u32) {
        self.pinned = Some(n);
    }
    pub fn unpin(&mut self) {
        self.pinned = None;
    }
    pub fn pinned(&self) -> Option<u32> {
        self.pinned
    }

    /// Each voting slot's window mean, then their mean and their max;
    /// `None` until enough is known.
    fn average(&self, now: Instant) -> Option<(f64, f64)> {
        let means: Vec<f64> = self
            .samples
            .values()
            .filter_map(|q| {
                let recent: Vec<f64> = q
                    .iter()
                    .filter(|(t, _)| now.saturating_duration_since(*t) <= WINDOW)
                    .map(|(_, v)| *v)
                    .collect();
                (recent.len() >= MIN_SAMPLES)
                    .then(|| recent.iter().sum::<f64>() / recent.len() as f64)
            })
            .collect();
        (!means.is_empty()).then(|| {
            (
                means.iter().sum::<f64>() / means.len() as f64,
                means.iter().cloned().fold(f64::MIN, f64::max),
            )
        })
    }

    pub fn decide(&mut self, current: u32, now: Instant) -> Step {
        if self.pinned.is_some() {
            return Step::Hold;
        }
        if let Some(step) = self.last_step {
            if now.saturating_duration_since(step) < self.policy.cooldown {
                return Step::Hold;
            }
        }
        let Some((avg, peak)) = self.average(now) else {
            return Step::Hold;
        };
        let target = self.policy.target.value();
        let shown = |v: f64| self.policy.target.show(v);
        let desired = ((current as f64) * avg / target).ceil() as u32;
        let up = desired.min(self.policy.max);
        let step = if avg > UP_TOLERANCE * target && up > current {
            Some((
                up,
                format!(
                    "{} {} > {} over {}s",
                    self.policy.signal,
                    shown(avg),
                    shown(target),
                    WINDOW.as_secs()
                ),
            ))
        } else if avg < DOWN_HYSTERESIS * target && peak < target && current > self.policy.min {
            // …and no instance is above target on its own: connection-level
            // balancing can leave one saturated while the others idle, and
            // shrinking then removes the idle ones and helps nobody.
            Some((
                current - 1,
                format!(
                    "{} {} < {} over {}s",
                    self.policy.signal,
                    shown(avg),
                    shown(DOWN_HYSTERESIS * target),
                    WINDOW.as_secs()
                ),
            ))
        } else {
            None
        };
        match step {
            Some((n, reason)) => {
                self.last_step = Some(now);
                self.samples.clear();
                Step::ScaleTo { n, reason }
            }
            None => Step::Hold,
        }
    }
}

/// A live-resizable limit: bytes for memory, millicores for cpu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub min: u64,
    pub max: u64,
}

/// Vertical state per instance: how long it has been under-using each
/// resource.
#[derive(Debug, Default)]
pub struct Vertical {
    mem_low_since: Option<Instant>,
    cpu_low_since: Option<Instant>,
}

const GROW_AT: u64 = 85; // % of the limit in use
const SHRINK_BELOW: u64 = 40;

fn grow(limit: u64, range: &Range) -> Option<u64> {
    let next = (limit + limit / 2).min(range.max);
    (next != limit).then_some(next)
}
fn shrink(limit: u64, range: &Range) -> Option<u64> {
    let next = (limit - limit / 4).max(range.min);
    (next != limit).then_some(next)
}

/// The step for a resource, or `None`: pressure grows it by half at once;
/// a low streak longer than the cooldown shrinks it by a quarter.
fn resize(
    low_since: &mut Option<Instant>,
    range: &Range,
    limit: u64,
    pressure: bool,
    low: bool,
    now: Instant,
    cooldown: Duration,
) -> Option<u64> {
    if pressure {
        *low_since = None;
        return grow(limit, range);
    }
    if !low {
        *low_since = None;
        return None;
    }
    match *low_since {
        None => {
            *low_since = Some(now);
            None
        }
        Some(since) if now.saturating_duration_since(since) >= cooldown => {
            *low_since = Some(now);
            shrink(limit, range)
        }
        Some(_) => None,
    }
}

impl Vertical {
    /// `limit`/`usage` in bytes; `oom_grew`: `memory.events oom_kill` moved.
    pub fn memory(
        &mut self,
        range: &Range,
        limit: u64,
        usage: u64,
        oom_grew: bool,
        now: Instant,
        cooldown: Duration,
    ) -> Option<u64> {
        let pressure = oom_grew || usage * 100 > limit * GROW_AT;
        let low = usage * 100 < limit * SHRINK_BELOW;
        resize(
            &mut self.mem_low_since,
            range,
            limit,
            pressure,
            low,
            now,
            cooldown,
        )
    }

    /// `quota`/`usage` in millicores; `throttled_grew`: `cpu.stat
    /// nr_throttled` moved.
    pub fn cpu(
        &mut self,
        range: &Range,
        quota: u64,
        usage: u64,
        throttled_grew: bool,
        now: Instant,
        cooldown: Duration,
    ) -> Option<u64> {
        let pressure = throttled_grew && usage * 100 >= quota * 90;
        let low = usage * 100 < quota * SHRINK_BELOW;
        resize(
            &mut self.cpu_low_since,
            range,
            quota,
            pressure,
            low,
            now,
            cooldown,
        )
    }
}

/// The value of metric `name` in Prometheus text exposition: samples of
/// that name summed across label sets; `None` when absent.
pub fn prometheus_value(text: &str, name: &str) -> Option<f64> {
    let mut sum = 0.0;
    let mut seen = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (head, rest) = match line.find(|c: char| c.is_whitespace()) {
            Some(i) => (&line[..i], line[i..].trim_start()),
            None => continue,
        };
        let metric = head.split('{').next().unwrap_or(head);
        if metric != name {
            continue;
        }
        let value = rest.split_whitespace().next().unwrap_or("");
        if let Ok(v) = value.parse::<f64>() {
            sum += v;
            seen = true;
        }
    }
    seen.then_some(sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn scale(signal: &str, target: &str) -> crate::manifest::Scale {
        crate::manifest::Scale {
            min: 2,
            max: 8,
            signal: signal.into(),
            target: target.into(),
            cooldown: None,
            metrics_path: None,
        }
    }

    #[test]
    fn a_policy_parses_each_signal_with_its_own_target_unit() {
        let p = Policy::parse(&scale("cpu", "70%"), true, true).unwrap();
        assert_eq!(p.signal, Signal::Cpu);
        assert_eq!(p.target, Target::Percent(70.0));
        assert_eq!(p.cooldown, Duration::from_secs(60));
        assert_eq!(
            Policy::parse(&scale("memory", "80%"), true, true)
                .unwrap()
                .target,
            Target::Percent(80.0)
        );
        assert_eq!(
            Policy::parse(&scale("net", "40MB/s"), true, true)
                .unwrap()
                .target,
            Target::BytesPerSec(40.0 * 1e6)
        );
        let m = Policy::parse(&scale("metric:queue_depth", "100"), true, true).unwrap();
        assert_eq!(m.signal, Signal::Metric("queue_depth".into()));
        assert_eq!(m.target, Target::Number(100.0));
        assert_eq!(m.metrics_path, "/metrics");
    }

    #[test]
    fn a_policy_refuses_what_it_cannot_measure_or_mean() {
        let err = |s: &crate::manifest::Scale, mem, port| {
            Policy::parse(s, mem, port).unwrap_err().to_string()
        };
        assert!(
            err(&scale("memory", "80%"), false, true).contains("resources.mem"),
            "memory needs a limit"
        );
        assert!(
            err(&scale("metric:q", "5"), true, false).contains("port"),
            "metric needs a port to scrape"
        );
        assert!(
            err(&scale("cpu", "70"), true, true).contains('%'),
            "cpu target is a percent"
        );
        assert!(
            err(&scale("net", "40%"), true, true).contains("/s"),
            "net target is a rate"
        );
        assert!(err(&scale("disk", "1"), true, true).contains("signal"));
        let mut s = scale("cpu", "70%");
        s.max = 1;
        assert!(err(&s, true, true).contains("max"));
        let mut s = scale("cpu", "70%");
        s.min = 0;
        assert!(err(&s, true, true).contains("min"));
    }

    #[test]
    fn rates_read_like_people_write_them() {
        assert_eq!(parse_rate("40MB/s").unwrap(), 40.0 * 1e6);
        assert_eq!(parse_rate("1.5GB/s").unwrap(), 1.5e9);
        assert_eq!(parse_rate("512KB/s").unwrap(), 512e3);
        assert_eq!(parse_rate("1000").unwrap(), 1000.0);
        assert!(parse_rate("fast").is_err());
    }

    fn horizontal(target: &str) -> Horizontal {
        Horizontal::new(Policy::parse(&scale("cpu", target), true, true).unwrap())
    }
    const T0: fn() -> Instant = Instant::now;
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Two instances at 84 % against a 70 % target want ceil(2 × 84/70) = 3.
    #[test]
    fn scale_up_goes_straight_to_the_desired_count() {
        let mut h = horizontal("70%");
        let t0 = T0();
        let started = t0 - secs(120); // long past warm-up
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            h.observe(1, started, now, 84.0);
            h.observe(2, started, now, 84.0);
        }
        match h.decide(2, t0 + secs(30)) {
            Step::ScaleTo { n: 3, reason } => {
                assert!(reason.contains("84%") && reason.contains("70%"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn scale_down_is_one_step_and_needs_clear_headroom() {
        let mut h = horizontal("70%");
        let t0 = T0();
        let started = t0 - secs(120);
        // 60 % of target: below the 70 %-of-target line → one down.
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            for slot in 1..=6 {
                h.observe(slot, started, now, 42.0);
            }
        }
        assert!(matches!(
            h.decide(6, t0 + secs(30)),
            Step::ScaleTo { n: 5, .. }
        ));
        // Just under target but above the hysteresis line: hold.
        let mut h = horizontal("70%");
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            h.observe(1, started, now, 60.0);
            h.observe(2, started, now, 60.0);
            h.observe(3, started, now, 60.0);
        }
        assert!(matches!(h.decide(3, t0 + secs(30)), Step::Hold));
    }

    /// Keep-alive clients stay on the instance they connected to, so new
    /// instances can sit idle while one is saturated. A low MEAN then says
    /// "shrink", which would remove the idle ones and leave the hot one
    /// hot — so no instance may be above target for a scale-down.
    #[test]
    fn a_hot_instance_among_idle_ones_blocks_scale_down() {
        let mut h = horizontal("70%");
        let t0 = T0();
        let started = t0 - secs(120);
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            h.observe(1, started, now, 100.0);
            h.observe(2, started, now, 0.0);
            h.observe(3, started, now, 0.0);
        }
        assert!(
            matches!(h.decide(3, t0 + secs(30)), Step::Hold),
            "mean 33% but slot 1 is saturated"
        );
        // Nobody above target: the mean decides.
        let mut h = horizontal("70%");
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            h.observe(1, started, now, 60.0);
            h.observe(2, started, now, 0.0);
            h.observe(3, started, now, 0.0);
        }
        assert!(matches!(
            h.decide(3, t0 + secs(30)),
            Step::ScaleTo { n: 2, .. }
        ));
    }

    /// `ceil(3 × 50.4/50)` is 4: without a tolerance band a hair over target
    /// buys an instance (seen live: "cpu 50% > 50%"). Within 10 % of target
    /// the count holds, as Kubernetes' HPA does.
    #[test]
    fn a_hair_over_target_is_not_a_scale_up() {
        let t0 = T0();
        let started = t0 - secs(120);
        let mut h = horizontal("50%");
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            for slot in 1..=3 {
                h.observe(slot, started, now, 50.4);
            }
        }
        assert!(matches!(h.decide(3, t0 + secs(30)), Step::Hold));
        let mut h = horizontal("50%");
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            for slot in 1..=3 {
                h.observe(slot, started, now, 56.0); // 12 % over: acts
            }
        }
        assert!(matches!(
            h.decide(3, t0 + secs(30)),
            Step::ScaleTo { n: 4, .. }
        ));
    }

    #[test]
    fn steps_are_clamped_to_min_and_max() {
        let mut h = horizontal("70%");
        let t0 = T0();
        let started = t0 - secs(120);
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            for slot in 1..=8 {
                h.observe(slot, started, now, 700.0);
            }
        }
        assert!(
            matches!(h.decide(8, t0 + secs(30)), Step::Hold),
            "already at max"
        );
        let mut h = horizontal("70%");
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            h.observe(1, started, now, 1.0);
            h.observe(2, started, now, 1.0);
        }
        assert!(
            matches!(h.decide(2, t0 + secs(30)), Step::Hold),
            "already at min"
        );
    }

    #[test]
    fn one_step_per_cooldown_and_the_window_restarts_after_a_step() {
        let mut h = horizontal("70%");
        let t0 = T0();
        let started = t0 - secs(120);
        for i in 0..6 {
            h.observe(1, started, t0 + secs(5 * i), 90.0);
        }
        assert!(matches!(h.decide(2, t0 + secs(30)), Step::ScaleTo { .. }));
        // Same hot samples 10 s later: cooldown holds it.
        h.observe(1, started, t0 + secs(40), 90.0);
        assert!(matches!(h.decide(3, t0 + secs(40)), Step::Hold));
        // After the cooldown, only post-step samples count: one 90 is not a window.
        assert!(matches!(h.decide(3, t0 + secs(100)), Step::Hold));
    }

    #[test]
    fn warming_instances_and_empty_windows_do_not_vote() {
        let mut h = horizontal("70%");
        let t0 = T0();
        let old = t0 - secs(120);
        let young = t0 - secs(5);
        for i in 0..6 {
            let now = t0 + secs(5 * i);
            h.observe(1, old, now, 60.0);
            h.observe(2, young, now, 500.0); // would force a scale-up if counted
        }
        assert!(matches!(h.decide(2, t0 + secs(30)), Step::Hold));
        let mut h = horizontal("70%");
        assert!(
            matches!(h.decide(2, t0), Step::Hold),
            "nothing observed yet"
        );
    }

    #[test]
    fn a_pin_by_the_operator_pauses_the_policy_until_auto() {
        let mut h = horizontal("70%");
        let t0 = T0();
        let started = t0 - secs(120);
        h.pin(5);
        for i in 0..6 {
            h.observe(1, started, t0 + secs(5 * i), 99.0);
        }
        assert!(matches!(h.decide(5, t0 + secs(30)), Step::Hold));
        assert_eq!(h.pinned(), Some(5));
        h.unpin();
        assert!(matches!(h.decide(5, t0 + secs(30)), Step::ScaleTo { .. }));
    }

    #[test]
    fn memory_grows_by_half_under_pressure_and_shrinks_by_a_quarter_when_idle() {
        let range = Range {
            min: 256 << 20,
            max: 2 << 30,
        };
        let t0 = T0();
        let mut v = Vertical::default();
        // 91 % used → ×1.5
        assert_eq!(
            v.memory(&range, 512 << 20, 470 << 20, false, t0, secs(60)),
            Some(768 << 20)
        );
        // OOM kill → grows even at low usage
        assert_eq!(
            v.memory(&range, 512 << 20, 10 << 20, true, t0, secs(60)),
            Some(768 << 20)
        );
        // capped at max
        assert_eq!(
            v.memory(&range, 1536 << 20, 1500 << 20, false, t0, secs(60)),
            Some(2 << 30)
        );
        // 30 % used: not yet — the low streak has to outlast the cooldown
        assert_eq!(
            v.memory(&range, 1 << 30, 300 << 20, false, t0, secs(60)),
            None
        );
        assert_eq!(
            v.memory(&range, 1 << 30, 300 << 20, false, t0 + secs(61), secs(60)),
            Some(768 << 20)
        );
        // floor at min
        assert_eq!(
            v.memory(&range, 300 << 20, 10 << 20, false, t0 + secs(200), secs(60)),
            Some(256 << 20)
        );
    }

    #[test]
    fn cpu_grows_when_throttled_near_its_quota() {
        let range = Range {
            min: 500,
            max: 4000,
        }; // millicores
        let t0 = T0();
        let mut v = Vertical::default();
        assert_eq!(v.cpu(&range, 1000, 950, true, t0, secs(60)), Some(1500));
        assert_eq!(
            v.cpu(&range, 1000, 950, false, t0, secs(60)),
            None,
            "busy but not throttled: fine"
        );
        assert_eq!(
            v.cpu(&range, 3000, 2950, true, t0, secs(60)),
            Some(4000),
            "capped"
        );
        assert_eq!(v.cpu(&range, 2000, 300, false, t0, secs(60)), None);
        assert_eq!(
            v.cpu(&range, 2000, 300, false, t0 + secs(61), secs(60)),
            Some(1500)
        );
    }

    #[test]
    fn prometheus_text_sums_a_metrics_samples_and_skips_the_rest() {
        let text = "# HELP queue_depth Jobs waiting\n# TYPE queue_depth gauge\nqueue_depth{shard=\"a\"} 40\nqueue_depth{shard=\"b\"} 65\nqueue_depth_total 9\nother 1\n";
        assert_eq!(prometheus_value(text, "queue_depth"), Some(105.0));
        assert_eq!(prometheus_value(text, "other"), Some(1.0));
        assert_eq!(prometheus_value(text, "missing"), None);
        assert_eq!(prometheus_value("inflight 3.5\n", "inflight"), Some(3.5));
    }
}
