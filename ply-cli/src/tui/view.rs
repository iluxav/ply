//! Rendering. Mirrors the web dashboard's look — dark, monospace already (it's
//! a terminal), orange section heads, green for up/deploy, and the
//! `everything is a file` footer — across the apps / deploy / notify / host
//! tabs plus the app-detail view.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::data::{human_duration, human_size, now, AppRow, ServiceStatus};
use super::{App, Input, Tab};

const ORANGE: Color = Color::Rgb(0xE0, 0xA0, 0x5A);
const GREEN: Color = Color::Rgb(0x7E, 0xE0, 0x8A);
const MUTED: Color = Color::Rgb(0x88, 0x88, 0x88);
const RED: Color = Color::Rgb(0xE0, 0x6A, 0x6A);
const SEL_BG: Color = Color::Rgb(0x26, 0x26, 0x26);

pub fn render(f: &mut Frame, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(1),    // body
        Constraint::Length(1), // footer
    ])
    .split(f.area());
    header(f, rows[0], app);
    body(f, rows[1], app);
    footer(f, rows[2], app);
    if let Some(input) = &app.input {
        input_modal(f, f.area(), input);
    }
}

fn input_modal(f: &mut Frame, area: Rect, input: &Input) {
    let w = area.width.clamp(40, 76);
    let box_area = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + area.height / 3,
        width: w,
        height: 4,
    };
    f.render_widget(Clear, box_area);
    let block = Block::bordered()
        .border_style(Style::new().fg(ORANGE))
        .title(Span::styled(
            format!(" {} ", input.title),
            Style::new().fg(ORANGE).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(box_area);
    f.render_widget(block, box_area);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(input.prompt.clone(), Style::new().fg(MUTED))),
            Line::from(vec![
                Span::styled("> ", Style::new().fg(ORANGE)),
                Span::raw(input.buffer.clone()),
                Span::styled("▏", Style::new().fg(ORANGE)),
            ]),
        ]),
        inner,
    );
}

fn header(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled("ply", Style::new().fg(ORANGE).add_modifier(Modifier::BOLD)),
        Span::styled("▮  ", Style::new().fg(ORANGE)),
    ];
    for (i, (tab, label)) in TABS.iter().enumerate() {
        let on = app.tab == *tab && app.detail.is_none();
        let style = if on {
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(MUTED)
        };
        spans.push(Span::styled(format!(" {label} "), style));
        if i + 1 < TABS.len() {
            spans.push(Span::raw(" "));
        }
    }
    if let Some(idx) = app.detail {
        if let Some(a) = app.snap.apps.get(idx) {
            spans.push(Span::styled("  /  ", Style::new().fg(MUTED)));
            spans.push(Span::styled(a.name.clone(), Style::new().fg(Color::White)));
        }
    }
    let right = if app.snap.root { "root" } else { "rootless" };
    let left = Paragraph::new(Line::from(spans));
    let right = Paragraph::new(Line::from(Span::styled(
        format!("{right} · ? help · q quit "),
        Style::new().fg(MUTED),
    )))
    .right_aligned();
    f.render_widget(left, area);
    f.render_widget(right, area);
}

const TABS: [(Tab, &str); 4] = [
    (Tab::Apps, "apps"),
    (Tab::Deploy, "deploy"),
    (Tab::Notify, "notify"),
    (Tab::Host, "host"),
];

fn body(f: &mut Frame, area: Rect, app: &App) {
    if let Some(idx) = app.detail {
        if let Some(a) = app.snap.apps.get(idx) {
            if app.log_follow {
                return log_follow(f, area, a);
            }
            return detail(f, area, app, a);
        }
    }
    match app.tab {
        Tab::Apps => apps(f, area, app),
        Tab::Host => host(f, area, app),
        Tab::Deploy => deploy(f, area, app),
        Tab::Notify => placeholder(
            f,
            area,
            "notify",
            "read-only in this build — event toggles and destinations land next",
        ),
    }
}

fn apps(f: &mut Frame, area: Rect, app: &App) {
    if app.snap.apps.is_empty() {
        let hint = if app.snap.root {
            "no instances running"
        } else {
            "no instances running as this user (a systemd unit's instances belong to root — try sudo ply ui)"
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, Style::new().fg(MUTED)))),
            pad(area),
        );
        return;
    }
    let n = app.snap.apps.len() as u16;
    let rows = Layout::vertical([Constraint::Length(n + 2), Constraint::Min(1)]).split(area);
    apps_table(f, rows[0], app);
    events(f, rows[1], &app.snap.events, None, "RECENT EVENTS");
}

fn apps_table(f: &mut Frame, area: Rect, app: &App) {
    let header = Row::new([
        "APP",
        "UP",
        "RST",
        "UPTIME",
        "VERSION",
        "PUBLISHED",
        "DOMAINS",
    ])
    .style(Style::new().fg(MUTED));
    let rows = app.snap.apps.iter().map(|a| {
        let (dot, color) = if a.up > 0 {
            ("● ", GREEN)
        } else {
            ("○ ", RED)
        };
        Row::new(vec![
            Cell::from(Line::from(vec![
                Span::styled(dot, Style::new().fg(color)),
                Span::raw(a.name.clone()),
            ])),
            Cell::from(format!("{}/{}", a.up, a.scale)),
            Cell::from(a.restarts.to_string()),
            Cell::from(human_duration(a.uptime)),
            Cell::from(a.version.clone()),
            Cell::from(a.published.clone()),
            Cell::from(a.domains.join(" ")),
        ])
    });
    let widths = [
        Constraint::Length(20),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(18),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD));
    let mut state = TableState::default();
    state.select(Some(
        app.apps_sel.min(app.snap.apps.len().saturating_sub(1)),
    ));
    f.render_stateful_widget(table, pad(area), &mut state);
}

fn detail(f: &mut Frame, area: Rect, app: &App, a: &AppRow) {
    let rows = Layout::vertical([
        Constraint::Length(a.instances.len() as u16 + 2), // instances
        Constraint::Length(3),                            // meta + actions
        Constraint::Min(1),                               // logs | events
    ])
    .split(area);

    // instances
    let header = Row::new(["INSTANCE", "PID", "IP", "UPTIME", "RST", "CPU", "MEM"])
        .style(Style::new().fg(MUTED));
    let irows = a.instances.iter().map(|i| {
        let (dot, color) = if i.alive {
            ("● ", GREEN)
        } else {
            ("○ ", RED)
        };
        // cpu in cores used / its limit (the [resources] cpu cap, or the whole
        // host when uncapped) with a gauge — 1.0 / 2 cores reads at once.
        let cpu_cell = match i.cpu {
            Some(c) => {
                let used = c / 100.0;
                let limit = i.cpu_max_cores.unwrap_or(app.snap.host.cpus as f64);
                Line::from(vec![
                    Span::styled(format!("{used:.1} / {limit:.0} cores "), cpu_color(i.cpu)),
                    Span::styled(bar(used, limit), Style::new().fg(ORANGE)),
                ])
            }
            None => Line::from(Span::styled("—", Style::new().fg(MUTED))),
        };
        // mem used / its limit (the [resources] mem cap, or the host total when
        // uncapped) with a gauge.
        let mem_cell = match i.mem {
            Some(m) => {
                let limit = i.mem_max.unwrap_or(app.snap.host.mem_total);
                Line::from(vec![
                    Span::raw(format!("{} / {} ", human_size(m), human_size(limit))),
                    Span::styled(bar(m as f64, limit as f64), Style::new().fg(GREEN)),
                ])
            }
            None => Line::from(Span::styled("—", Style::new().fg(MUTED))),
        };
        Row::new(vec![
            Cell::from(Line::from(vec![
                Span::styled(dot, Style::new().fg(color)),
                Span::raw(i.name.clone()),
            ])),
            Cell::from(i.pid.to_string()),
            Cell::from(i.ip.clone()),
            Cell::from(human_duration(i.uptime)),
            Cell::from(i.restarts.to_string()),
            Cell::from(cpu_cell),
            Cell::from(mem_cell),
        ])
    });
    let iwidths = [
        Constraint::Length(18),
        Constraint::Length(9),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(26),
        Constraint::Min(34),
    ];
    f.render_widget(Table::new(irows, iwidths).header(header), pad(rows[0]));

    // meta + actions
    let svc = status_spans(a.service);
    let mut meta = vec![
        Span::styled("published ", Style::new().fg(MUTED)),
        Span::raw(a.published.clone()),
        Span::styled("  ·  image ", Style::new().fg(MUTED)),
        Span::raw(a.version.clone()),
        Span::styled("  ·  service ", Style::new().fg(MUTED)),
    ];
    meta.extend(svc);
    let actions = Line::from(Span::styled(
        "[-/+ scale]  [r restart]  [R unit]  [e term]  [f logs]  [d domain]  [s snap]  [x remove]",
        Style::new().fg(MUTED),
    ));
    let h = &app.snap.host;
    let caps = Line::from(Span::styled(
        format!(
            "host: {} cores · {} RAM · load {:.2}   —   cpu/mem shown as used / limit (the app's cap, or the host when uncapped)",
            h.cpus,
            human_size(h.mem_total),
            h.load1,
        ),
        Style::new().fg(MUTED),
    ));
    f.render_widget(
        Paragraph::new(vec![Line::from(meta), actions, caps]),
        pad(rows[1]),
    );

    // logs | events
    let cols =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(rows[2]);
    logs(f, cols[0], &a.name, app.snap.root);
    let app_events: Vec<_> = app
        .snap
        .events
        .iter()
        .filter(|e| e.app == a.name)
        .cloned()
        .collect();
    events(f, cols[1], &app_events, None, "EVENTS");
}

fn host(f: &mut Frame, area: Rect, app: &App) {
    let h = &app.snap.host;
    let rows = Layout::vertical([
        Constraint::Length(4),                                 // edge
        Constraint::Length(h.services.len() as u16 + 3),       // services
        Constraint::Length(h.domains.len().max(1) as u16 + 3), // domains
        Constraint::Min(1),                                    // host stats
    ])
    .split(area);

    // EDGE
    let mut edge = vec![section("EDGE (Caddy + HTTPS)")];
    if h.edge_installed {
        edge.push(status_line("caddy         ", h.caddy, "ply-edge.service"));
        edge.push(status_line("proxy watcher ", h.proxy, "ply-proxy.service"));
    } else {
        edge.push(Line::from(vec![
            Span::styled("✗ edge not installed", Style::new().fg(RED)),
            Span::styled(
                "    [i] install  (sudo ply setup --edge)",
                Style::new().fg(MUTED),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(edge), pad(rows[0]));

    // SERVICES
    let mut svc = vec![section("SERVICES")];
    svc.push(status_line(
        "deployments watcher ",
        h.reconcile,
        "timer + path, from setup",
    ));
    for (i, (unit, status)) in h.services.iter().enumerate() {
        let sel = i == app.host_sel;
        let marker = if sel { "› " } else { "  " };
        let mut line = vec![Span::styled(marker, Style::new().fg(ORANGE))];
        line.extend(status_spans(*status));
        line.push(Span::styled(
            format!("  {unit}"),
            Style::new().fg(if sel { Color::White } else { MUTED }),
        ));
        if *status == ServiceStatus::Failed {
            line.push(Span::styled("   [r] restart unit", Style::new().fg(MUTED)));
        } else if *status == ServiceStatus::NoUnit {
            line.push(Span::styled("   [u] install unit", Style::new().fg(MUTED)));
        }
        svc.push(Line::from(line));
    }
    f.render_widget(Paragraph::new(svc), pad(rows[1]));

    // DOMAINS
    let mut dom = vec![section("DOMAINS")];
    if h.domains.is_empty() {
        dom.push(Line::from(Span::styled(
            "none — [d] add a domain to an app (needs the edge installed)",
            Style::new().fg(MUTED),
        )));
    } else {
        for (d, appn) in &h.domains {
            dom.push(Line::from(vec![
                Span::raw(format!("{d:<28}")),
                Span::styled("→ ", Style::new().fg(MUTED)),
                Span::raw(appn.clone()),
            ]));
        }
    }
    f.render_widget(Paragraph::new(dom), pad(rows[2]));

    // HOST
    let disk = h
        .disk_pct
        .map(|p| format!("disk {p}%"))
        .unwrap_or_else(|| "disk —".into());
    let mem_used = h.mem_total.saturating_sub(h.mem_avail);
    let mem = if h.mem_total > 0 {
        format!("mem {} / {}", human_size(mem_used), human_size(h.mem_total))
    } else {
        "mem —".into()
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            section_span("HOST   "),
            Span::styled(
                format!(
                    "{} cores · load {:.2} · {mem} · {disk} · ply {}",
                    h.cpus, h.load1, h.version
                ),
                Style::new().fg(MUTED),
            ),
        ])),
        pad(rows[3]),
    );
}

fn logs(f: &mut Frame, area: Rect, app: &str, _root: bool) {
    let block = titled("LOGS  (f follow)");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let text = read_app_log(app);
    let lines: Vec<Line> = text
        .lines()
        .rev()
        .take(inner.height as usize)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|l| Line::from(Span::raw(l.to_string())))
        .collect();
    let body = if lines.is_empty() {
        vec![Line::from(Span::styled(
            "no output yet",
            Style::new().fg(MUTED),
        ))]
    } else {
        lines
    };
    f.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), inner);
}

/// Full-screen live tail, in the TUI — re-read every frame (~8×/s), no exit.
fn log_follow(f: &mut Frame, area: Rect, a: &AppRow) {
    let block = titled(&format!("LOGS — {} · live   (esc / f back)", a.name));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let text = read_app_log(&a.name);
    let lines: Vec<Line> = text
        .lines()
        .rev()
        .take(inner.height as usize)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|l| Line::from(Span::raw(l.to_string())))
        .collect();
    let body = if lines.is_empty() {
        vec![Line::from(Span::styled(
            "no output yet",
            Style::new().fg(MUTED),
        ))]
    } else {
        lines
    };
    f.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), inner);
}

/// The freshest non-empty per-instance log, `<run_dir>/logs/<app>.<n>.log` —
/// so a scaled app (`.2`, `.3`) and a silent `.1` don't hide real output.
fn read_app_log(app: &str) -> String {
    let dir = ply_core::paths::run_dir().join("logs");
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return String::new();
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        // <app>.<n>.log, and not a rotated <app>.<n>.log.1
        let Some(stem) = name.strip_suffix(".log") else {
            continue;
        };
        let Some((a, n)) = stem.rsplit_once('.') else {
            continue;
        };
        if a != app || !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if meta.len() == 0 {
            continue;
        }
        if let Ok(mtime) = meta.modified() {
            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                best = Some((mtime, e.path()));
            }
        }
    }
    best.and_then(|(_, p)| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

fn events(
    f: &mut Frame,
    area: Rect,
    evs: &[ply_core::runtime::events::Event],
    _sel: Option<usize>,
    title: &str,
) {
    let block = titled(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let now = now();
    let lines: Vec<Line> = evs
        .iter()
        .take(inner.height as usize)
        .map(|e| {
            let age = human_duration(now.saturating_sub(e.ts));
            let color = match e.event.as_str() {
                s if s.contains("deploy") && !s.contains("fail") => GREEN,
                s if s.contains("exit")
                    || s.contains("fail")
                    || s.contains("blocked")
                    || s.contains("loop") =>
                {
                    RED
                }
                _ => ORANGE,
            };
            Line::from(vec![
                Span::styled(format!("{age:>4} "), Style::new().fg(MUTED)),
                Span::styled(format!("{:<13} ", e.event), Style::new().fg(color)),
                Span::styled(format!("{:<12} ", e.app), Style::new().fg(Color::White)),
                Span::styled(e.detail.clone(), Style::new().fg(MUTED)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn deploy(f: &mut Frame, area: Rect, app: &App) {
    let block = titled("DEPLOYMENTS ON THIS HOST");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.snap.deploys.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "none yet — press n to deploy from a GitHub URL (the host clones & builds it)",
                Style::new().fg(MUTED),
            ))),
            inner,
        );
        return;
    }
    let mut lines = Vec::new();
    for (i, d) in app.snap.deploys.iter().enumerate() {
        let sel = i == app.deploy_sel;
        let (dot, dc) = if d.ok { ("● ", GREEN) } else { ("✗ ", RED) };
        let name_style = if sel {
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Reset)
        };
        let mut head = vec![
            Span::styled(if sel { "› " } else { "  " }, Style::new().fg(ORANGE)),
            Span::styled(dot, Style::new().fg(dc)),
            Span::styled(format!("{:<16}", d.name), name_style),
        ];
        if let Some(v) = &d.version {
            head.push(Span::styled(format!("@{v} "), Style::new().fg(ORANGE)));
        }
        head.push(Span::styled(d.detail.clone(), Style::new().fg(dc)));
        lines.push(Line::from(head));
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(d.source.clone(), Style::new().fg(MUTED)),
        ]));
        // A composition expands to services — show each with its own state,
        // so the deploy tab reflects what `repo = <url>` actually brought up.
        for m in &d.members {
            let (mdot, mc) = if m.ok { ("● ", GREEN) } else { ("✗ ", RED) };
            lines.push(Line::from(vec![
                Span::raw("      "),
                Span::styled(mdot, Style::new().fg(mc)),
                Span::styled(format!("{:<14}", m.name), Style::new().fg(Color::Reset)),
                Span::styled(m.detail.clone(), Style::new().fg(MUTED)),
            ]));
        }
        lines.push(Line::from(""));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn placeholder(f: &mut Frame, area: Rect, title: &str, note: &str) {
    let block = titled(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(note, Style::new().fg(MUTED))))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn footer(f: &mut Frame, area: Rect, app: &App) {
    let hints = if app.log_follow {
        "live tail · esc / f back · q quit"
    } else if app.detail.is_some() {
        "esc back · -/+ scale · r · R unit · e · f logs · d domain · s snap · x remove"
    } else {
        match app.tab {
            Tab::Apps => "↑↓ move · enter details · r restart · 1-4 tabs · q quit",
            Tab::Host => "↑↓ move · i install edge · r restart unit · u install unit · d domain",
            Tab::Deploy => "↑↓ move · n new · b build · p pin/rollback · x delete · 1-4 tabs · q",
            _ => "1-4 tabs · q quit",
        }
    };
    let left = if app.status.is_empty() {
        Span::styled(hints, Style::new().fg(MUTED))
    } else {
        Span::styled(app.status.clone(), Style::new().fg(ORANGE))
    };
    f.render_widget(Paragraph::new(Line::from(left)), area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "everything is a file ",
            Style::new().fg(MUTED),
        )))
        .right_aligned(),
        area,
    );
}

// -- helpers --------------------------------------------------------------

/// A `used / total` gauge — 10 cells of █ (used) then ░ (free). Empty when
/// `total` is 0; clamps if `used` somehow exceeds it.
fn bar(used: f64, total: f64) -> String {
    const W: usize = 10;
    if total <= 0.0 {
        return String::new();
    }
    let filled = ((used / total) * W as f64).round().clamp(0.0, W as f64) as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(W - filled))
}

fn cpu_color(cpu: Option<f64>) -> Style {
    match cpu {
        Some(c) if c >= 80.0 => Style::new().fg(RED),
        Some(c) if c >= 40.0 => Style::new().fg(ORANGE),
        Some(_) => Style::new().fg(GREEN),
        None => Style::new().fg(MUTED),
    }
}

fn status_spans(s: ServiceStatus) -> Vec<Span<'static>> {
    let (dot, color) = match s {
        ServiceStatus::Active => ("● ", GREEN),
        ServiceStatus::Failed => ("✗ ", RED),
        ServiceStatus::Inactive => ("○ ", MUTED),
        ServiceStatus::NoUnit => ("— ", MUTED),
    };
    vec![
        Span::styled(dot, Style::new().fg(color)),
        Span::styled(s.label(), Style::new().fg(color)),
    ]
}

fn status_line(label: &'static str, s: ServiceStatus, note: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(label, Style::new().fg(Color::White))];
    spans.extend(status_spans(s));
    spans.push(Span::styled(format!("   {note}"), Style::new().fg(MUTED)));
    Line::from(spans)
}

fn section(title: &str) -> Line<'static> {
    Line::from(section_span(title))
}

fn section_span(title: &str) -> Span<'static> {
    Span::styled(
        title.to_string(),
        Style::new().fg(ORANGE).add_modifier(Modifier::BOLD),
    )
}

fn titled(title: &str) -> Block<'static> {
    Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(ORANGE).add_modifier(Modifier::BOLD),
        ))
}

/// One-column left padding so tables/text don't hug the edge.
fn pad(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    }
}
