//! `ply ui` — a lazydocker-style terminal dashboard for a ply host. It renders
//! the same state `ply ps`, `ply why` and the events journal expose (no new
//! backend), across apps / deploy / notify / host tabs, and drives actions by
//! shelling out to the same `ply` and `systemctl` commands you would type.
//!
//! Status: apps + app-detail (live logs, cpu/mem gauges), the host tab (edge,
//! services, add-domain) and the deploy tab (list, new-from-URL, pin/rollback)
//! are wired to real data and real actions. The notify tab is still read-only.

mod data;
mod view;

use std::process::Command;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;

use data::Snapshot;

const REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Apps,
    Deploy,
    Notify,
    Host,
}

pub(crate) struct App {
    pub tab: Tab,
    pub snap: Snapshot,
    pub apps_sel: usize,
    pub host_sel: usize,
    pub deploy_sel: usize,
    pub detail: Option<usize>,
    pub log_follow: bool,
    pub status: String,
    /// A modal text prompt (add-domain, …); when set it captures all keys.
    pub input: Option<Input>,
    last_refresh: Instant,
    quit: bool,
}

pub(crate) struct Input {
    pub title: String,
    pub prompt: String,
    pub buffer: String,
    target: InputTarget,
}

enum InputTarget {
    AddDomain(String),                             // the app the domain is for
    NewDeployUrl,                                  // step 1: the GitHub URL
    NewDeployPublish { url: String },              // step 2: publish override
    NewDeployEnv { url: String, publish: String }, // step 3: env, then create
    PinVersion(String),                            // the deployment to pin/roll back
    SetBuild(String),                              // the deployment to set a `build =` command on
    RemoveApp(String),                             // stop + remove an app (typed-yes confirm)
    RemoveDeployment(String),                      // delete a deployment spec (typed-yes confirm)
}

impl App {
    fn new() -> Self {
        App {
            tab: Tab::Apps,
            snap: Snapshot::gather(),
            apps_sel: 0,
            host_sel: 0,
            deploy_sel: 0,
            detail: None,
            log_follow: false,
            status: String::new(),
            input: None,
            last_refresh: Instant::now(),
            quit: false,
        }
    }

    fn reload(&mut self) {
        self.snap = Snapshot::gather();
        self.last_refresh = Instant::now();
        self.clamp();
    }

    fn clamp(&mut self) {
        let apps = self.snap.apps.len();
        if apps == 0 {
            self.apps_sel = 0;
            self.detail = None;
        } else {
            self.apps_sel = self.apps_sel.min(apps - 1);
            if let Some(d) = self.detail {
                self.detail = Some(d.min(apps - 1));
            }
        }
        let svc = self.snap.host.services.len();
        self.host_sel = self.host_sel.min(svc.saturating_sub(1));
        self.deploy_sel = self
            .deploy_sel
            .min(self.snap.deploys.len().saturating_sub(1));
    }

    fn current_app(&self) -> Option<String> {
        let idx = self.detail.unwrap_or(self.apps_sel);
        self.snap.apps.get(idx).map(|a| a.name.clone())
    }

    fn current_scale(&self) -> usize {
        self.detail
            .and_then(|i| self.snap.apps.get(i))
            .map(|a| a.scale)
            .unwrap_or(1)
    }

    fn selected_unit(&self) -> Option<String> {
        self.snap
            .host
            .services
            .get(self.host_sel)
            .map(|(u, _)| u.clone())
    }
}

pub fn run() -> anyhow::Result<()> {
    let mut terminal = ratatui::try_init()
        .map_err(|e| anyhow::anyhow!("ply ui needs an interactive terminal ({e})"))?;
    let res = run_loop(&mut terminal);
    let _ = ratatui::try_restore();
    res
}

fn run_loop(terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
    let mut app = App::new();
    loop {
        if app.last_refresh.elapsed() >= REFRESH {
            app.reload();
        }
        terminal.draw(|f| view::render(f, &app))?;
        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    handle_key(&mut app, terminal, k.code);
                }
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn handle_key(app: &mut App, terminal: &mut DefaultTerminal, code: KeyCode) {
    if app.input.is_some() {
        return handle_input_key(app, code);
    }
    app.status.clear();
    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Esc => {
            if app.log_follow {
                app.log_follow = false;
            } else if app.detail.is_some() {
                app.detail = None;
            } else {
                app.quit = true;
            }
        }
        KeyCode::Char('1') => switch(app, Tab::Apps),
        KeyCode::Char('2') => switch(app, Tab::Deploy),
        KeyCode::Char('3') => switch(app, Tab::Notify),
        KeyCode::Char('4') => switch(app, Tab::Host),
        KeyCode::Tab => cycle(app),
        KeyCode::Down | KeyCode::Char('j') => move_sel(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_sel(app, -1),
        KeyCode::Enter if app.tab != Tab::Host => {
            if app.detail.is_none() && app.tab == Tab::Apps && !app.snap.apps.is_empty() {
                app.detail = Some(app.apps_sel);
            }
        }
        // app actions (detail or apps selection)
        KeyCode::Char('r') if app.tab != Tab::Host => {
            if let Some(name) = app.current_app() {
                capture(app, &["ply", "restart", &name]);
            }
        }
        KeyCode::Char('+') | KeyCode::Char('=') if app.detail.is_some() => {
            if let Some(name) = app.current_app() {
                let n = (app.current_scale() + 1).to_string();
                capture(app, &["ply", "scale", &name, &n]);
            }
        }
        KeyCode::Char('-') if app.detail.is_some() => {
            if let Some(name) = app.current_app() {
                let n = app.current_scale().saturating_sub(1).max(1).to_string();
                capture(app, &["ply", "scale", &name, &n]);
            }
        }
        KeyCode::Char('s') if app.detail.is_some() => {
            if let Some(name) = app.current_app() {
                capture(app, &["ply", "snapshot", "take", &name]);
            }
        }
        KeyCode::Char('e') if app.detail.is_some() => {
            if let Some(name) = app.current_app() {
                interactive(
                    terminal,
                    app,
                    vec!["ply".into(), "exec".into(), name, "sh".into()],
                );
            }
        }
        KeyCode::Char('f') if app.detail.is_some() => {
            // In-TUI full-screen live tail — no leaving the dashboard.
            app.log_follow = !app.log_follow;
        }
        KeyCode::Char('R') if app.detail.is_some() => {
            if let Some(name) = app.current_app() {
                unit_action(terminal, app, "restart", &format!("ply-{name}"));
            }
        }
        KeyCode::Char('x') if app.detail.is_some() => {
            if let Some(name) = app.current_app() {
                open_remove_app(app, name);
            }
        }
        // host tab actions
        KeyCode::Char('i') if app.tab == Tab::Host => {
            let mut argv = Vec::new();
            if !app.snap.root {
                argv.push("sudo".into());
            }
            argv.extend(["ply".into(), "setup".into(), "--edge".into()]);
            interactive(terminal, app, argv);
        }
        KeyCode::Char('r') if app.tab == Tab::Host => {
            if let Some(unit) = app.selected_unit() {
                unit_action(terminal, app, "restart", &unit);
            }
        }
        KeyCode::Enter if app.tab == Tab::Host => {
            if let Some(unit) = app.selected_unit() {
                journal(terminal, app, &unit);
            }
        }
        KeyCode::Char('d') if app.detail.is_some() => {
            if let Some(name) = app.current_app() {
                open_add_domain(app, name);
            }
        }
        KeyCode::Char('d') if app.tab == Tab::Host => {
            if let Some(unit) = app.selected_unit() {
                let name = unit.strip_prefix("ply-").unwrap_or(&unit).to_string();
                open_add_domain(app, name);
            }
        }
        // deploy tab actions
        KeyCode::Char('n') if app.tab == Tab::Deploy => open_new_deployment(app),
        KeyCode::Char('p') if app.tab == Tab::Deploy => {
            if let Some(d) = app.snap.deploys.get(app.deploy_sel) {
                open_pin_version(app, d.name.clone());
            }
        }
        KeyCode::Char('b') if app.tab == Tab::Deploy => {
            if let Some(d) = app.snap.deploys.get(app.deploy_sel) {
                open_set_build(app, d.name.clone());
            }
        }
        KeyCode::Char('x') if app.tab == Tab::Deploy => {
            if let Some(d) = app.snap.deploys.get(app.deploy_sel) {
                open_remove_deployment(app, d.name.clone());
            }
        }
        _ => {}
    }
}

fn open_add_domain(app: &mut App, name: String) {
    app.input = Some(Input {
        title: format!("add domain to {name}"),
        prompt: "domain (e.g. app.example.com)".into(),
        buffer: String::new(),
        target: InputTarget::AddDomain(name),
    });
}

fn open_new_deployment(app: &mut App) {
    app.input = Some(Input {
        title: "new deployment (1/3)".into(),
        prompt: "GitHub repo URL — the host clones & builds this repo's ply.toml".into(),
        buffer: String::new(),
        target: InputTarget::NewDeployUrl,
    });
}

fn open_pin_version(app: &mut App, name: String) {
    app.input = Some(Input {
        title: format!("pin version of {name}"),
        prompt: "version to pin, or blank to follow the latest (rollback)".into(),
        buffer: String::new(),
        target: InputTarget::PinVersion(name),
    });
}

fn open_set_build(app: &mut App, name: String) {
    app.input = Some(Input {
        title: format!("build command for {name}"),
        prompt: "e.g. npm ci && npm run build   (blank clears it)".into(),
        buffer: String::new(),
        target: InputTarget::SetBuild(name),
    });
}

fn open_remove_app(app: &mut App, name: String) {
    app.input = Some(Input {
        title: format!("remove {name}"),
        prompt: "stop & remove this app (volumes kept). type 'yes' to confirm".into(),
        buffer: String::new(),
        target: InputTarget::RemoveApp(name),
    });
}

fn open_remove_deployment(app: &mut App, name: String) {
    app.input = Some(Input {
        title: format!("delete deployment {name}"),
        prompt: "delete the spec & stop it (volumes kept). type 'yes' to confirm".into(),
        buffer: String::new(),
        target: InputTarget::RemoveDeployment(name),
    });
}

fn handle_input_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            app.input = None;
            app.status = "cancelled".into();
        }
        KeyCode::Enter => {
            if let Some(input) = app.input.take() {
                apply_input(app, input);
            }
        }
        KeyCode::Backspace => {
            if let Some(i) = app.input.as_mut() {
                i.buffer.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some(i) = app.input.as_mut() {
                i.buffer.push(c);
            }
        }
        _ => {}
    }
}

fn apply_input(app: &mut App, input: Input) {
    match input.target {
        InputTarget::AddDomain(name) => {
            let domain = input.buffer.trim().to_string();
            if domain.is_empty() {
                app.status = "cancelled — no domain entered".into();
                return;
            }
            app.status = match add_domain(&name, &domain) {
                Ok(msg) => format!("✓ {msg}"),
                Err(e) => format!("✗ {e}"),
            };
            app.reload();
        }
        InputTarget::NewDeployUrl => {
            let url = input.buffer.trim().to_string();
            if url.is_empty() {
                app.status = "cancelled — no URL entered".into();
                return;
            }
            app.input = Some(Input {
                title: "new deployment (2/3)".into(),
                prompt: "publish, e.g. 8080:3000 or internal:3000  (blank = none)".into(),
                buffer: String::new(),
                target: InputTarget::NewDeployPublish { url },
            });
        }
        InputTarget::NewDeployPublish { url } => {
            let publish = input.buffer.trim().to_string();
            app.input = Some(Input {
                title: "new deployment (3/3)".into(),
                prompt: "env, KEY=VAL comma-separated  (blank = none)".into(),
                buffer: String::new(),
                target: InputTarget::NewDeployEnv { url, publish },
            });
        }
        InputTarget::NewDeployEnv { url, publish } => {
            let env = input.buffer.trim().to_string();
            app.status = match new_deployment(&url, &publish, &env) {
                Ok(msg) => format!("✓ {msg}"),
                Err(e) => format!("✗ {e}"),
            };
            app.reload();
        }
        InputTarget::PinVersion(name) => {
            let v = input.buffer.trim().to_string();
            app.status = match pin_version(&name, &v) {
                Ok(msg) => format!("✓ {msg}"),
                Err(e) => format!("✗ {e}"),
            };
            app.reload();
        }
        InputTarget::SetBuild(name) => {
            let cmd = input.buffer.trim().to_string();
            app.status = match set_build(&name, &cmd) {
                Ok(msg) => format!("✓ {msg}"),
                Err(e) => format!("✗ {e}"),
            };
            app.reload();
        }
        InputTarget::RemoveApp(name) => {
            if input.buffer.trim() != "yes" {
                app.status = "cancelled (type 'yes' to remove)".into();
                return;
            }
            app.status = match remove_app(&name) {
                Ok(msg) => format!("✓ {msg}"),
                Err(e) => format!("✗ {e}"),
            };
            app.detail = None;
            app.reload();
        }
        InputTarget::RemoveDeployment(name) => {
            if input.buffer.trim() != "yes" {
                app.status = "cancelled (type 'yes' to delete)".into();
                return;
            }
            app.status = match remove_deployment(&name) {
                Ok(msg) => format!("✓ {msg}"),
                Err(e) => format!("✗ {e}"),
            };
            app.reload();
        }
    }
}

/// Stop and remove an app, whatever manages it: delete its deployment spec if
/// any (so reconcile won't restart it), disable+remove its systemd unit if any
/// (ditto), then `ply rm` to stop it now. Volumes are kept.
fn remove_app(name: &str) -> anyhow::Result<String> {
    let mut removed = Vec::new();
    let spec = ply_core::deployments::dir().join(format!("{name}.toml"));
    if spec.exists() {
        std::fs::remove_file(&spec)?;
        let _ = std::fs::remove_file(ply_core::deployments::status_path(name));
        removed.push("deployment");
    }
    let unit = format!("ply-{name}");
    let has_unit = Command::new("systemctl")
        .args(["cat", &unit])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_unit {
        let _ = Command::new("systemctl")
            .args(["disable", "--now", &unit])
            .output();
        let _ = std::fs::remove_file(format!("/etc/systemd/system/{unit}.service"));
        let _ = Command::new("systemctl").arg("daemon-reload").output();
        removed.push("unit");
    }
    let _ = Command::new("ply").args(["rm", name]).output();
    if removed.is_empty() {
        Ok(format!("removed {name} (stopped; volumes kept)"))
    } else {
        Ok(format!(
            "removed {name} — deleted {} + stopped (volumes kept)",
            removed.join(" + ")
        ))
    }
}

/// Delete a deployment spec so reconcile stops and removes its app.
fn remove_deployment(name: &str) -> anyhow::Result<String> {
    let spec = ply_core::deployments::dir().join(format!("{name}.toml"));
    if !spec.exists() {
        anyhow::bail!("no deployment {name}");
    }
    std::fs::remove_file(&spec)?;
    let _ = std::fs::remove_file(ply_core::deployments::status_path(name));
    let _ = Command::new("ply").args(["rm", name]).output();
    Ok(format!("deleted deployment {name} (reconcile stops it)"))
}

/// Set (or, blank, clear) a deployment's `build =` command — the on-host build
/// step. reconcile re-clones and rebuilds. This is the fix for a repo whose
/// image `include`s build output (e.g. Next.js `.next/standalone`).
fn set_build(name: &str, cmd: &str) -> anyhow::Result<String> {
    use toml_edit::{value, DocumentMut};
    let path = ply_core::deployments::dir().join(format!("{name}.toml"));
    if !path.exists() {
        anyhow::bail!("no deployment {name}");
    }
    let mut doc: DocumentMut = std::fs::read_to_string(&path)?.parse()?;
    if cmd.is_empty() {
        doc.remove("build");
        std::fs::write(&path, doc.to_string())?;
        Ok(format!("{name}: build command cleared"))
    } else {
        doc["build"] = value(cmd);
        std::fs::write(&path, doc.to_string())?;
        Ok(format!("{name}: build set — reconcile rebuilds"))
    }
}

/// Create a build-on-host deployment from a GitHub URL. Writing the spec into
/// the deployments dir is enough — the `ply-deployments.path` unit reconciles
/// it (clone + build + run), so this does not block on the build.
fn new_deployment(url: &str, publish: &str, env: &str) -> anyhow::Result<String> {
    let name = deploy_name_from_url(url);
    if name.is_empty() {
        anyhow::bail!("could not derive a name from {url}");
    }
    let dir = ply_core::deployments::dir();
    let path = dir.join(format!("{name}.toml"));
    if path.exists() {
        anyhow::bail!("a deployment named {name} already exists");
    }
    let mut spec = format!("repo = \"{url}\"\n");
    if !publish.is_empty() {
        spec.push_str(&format!("publish = [\"{publish}\"]\n"));
    }
    let pairs: Vec<(&str, &str)> = env
        .split(',')
        .filter_map(|kv| kv.trim().split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .filter(|(k, _)| !k.is_empty())
        .collect();
    if !pairs.is_empty() {
        spec.push_str("\n[env]\n");
        for (k, v) in pairs {
            spec.push_str(&format!("{k} = \"{v}\"\n"));
        }
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, spec)?;
    Ok(format!(
        "created {name} — press b to set a build command if it needs one (Next.js/TS)"
    ))
}

fn deploy_name_from_url(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_end_matches(".git")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Pin (or, with a blank version, un-pin → follow latest) a deployment's
/// `version`; reconcile then rolls to it.
fn pin_version(name: &str, version: &str) -> anyhow::Result<String> {
    use toml_edit::{value, DocumentMut};
    let path = ply_core::deployments::dir().join(format!("{name}.toml"));
    if !path.exists() {
        anyhow::bail!("no deployment {name}");
    }
    let mut doc: DocumentMut = std::fs::read_to_string(&path)?.parse()?;
    if version.is_empty() {
        doc.remove("version");
        std::fs::write(&path, doc.to_string())?;
        Ok(format!("{name} now follows the latest"))
    } else {
        doc["version"] = value(version);
        std::fs::write(&path, doc.to_string())?;
        Ok(format!(
            "{name} pinned to {version} — reconcile rolls to it"
        ))
    }
}

/// Add a domain to an app's deployment spec (`deployments/<app>.toml`).
/// Domains live on deployments — reconcile turns them into `--domain` flags,
/// and ply-proxy renders them into Caddy. Requires the deployment to exist.
fn add_domain(app: &str, domain: &str) -> anyhow::Result<String> {
    use toml_edit::{Array, DocumentMut, Item, Value};
    let path = ply_core::paths::data_dir()
        .join("deployments")
        .join(format!("{app}.toml"));
    if !path.exists() {
        anyhow::bail!(
            "{app} has no deployment spec — a domain attaches via a deployment (deployments/{app}.toml)"
        );
    }
    let mut doc: DocumentMut = std::fs::read_to_string(&path)?.parse()?;
    let entry = doc
        .entry("domain")
        .or_insert(Item::Value(Value::Array(Array::new())));
    let arr = entry
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("`domain` in {app}.toml is not a list"))?;
    if arr.iter().any(|v| v.as_str() == Some(domain)) {
        return Ok(format!("{domain} already on {app}"));
    }
    arr.push(domain);
    std::fs::write(&path, doc.to_string())?;
    Ok(format!("added {domain} to {app} — reconcile applies it"))
}

fn switch(app: &mut App, tab: Tab) {
    app.tab = tab;
    app.detail = None;
    app.log_follow = false;
}

fn cycle(app: &mut App) {
    app.tab = match app.tab {
        Tab::Apps => Tab::Deploy,
        Tab::Deploy => Tab::Notify,
        Tab::Notify => Tab::Host,
        Tab::Host => Tab::Apps,
    };
    app.detail = None;
    app.log_follow = false;
}

fn move_sel(app: &mut App, delta: isize) {
    let (sel, len) = match app.tab {
        Tab::Host => (&mut app.host_sel, app.snap.host.services.len()),
        Tab::Deploy => (&mut app.deploy_sel, app.snap.deploys.len()),
        _ => (&mut app.apps_sel, app.snap.apps.len()),
    };
    if len == 0 {
        return;
    }
    let next = (*sel as isize + delta).rem_euclid(len as isize);
    *sel = next as usize;
}

/// A quick, non-interactive `ply …` — result goes to the status line.
fn capture(app: &mut App, argv: &[&str]) {
    app.status = match Command::new(argv[0]).args(&argv[1..]).output() {
        Ok(o) if o.status.success() => format!("✓ {}", argv.join(" ")),
        Ok(o) => {
            let why = String::from_utf8_lossy(&o.stderr);
            let why = why.trim().lines().next().unwrap_or("failed");
            format!("✗ {} — {why}", argv.join(" "))
        }
        Err(e) => format!("✗ {} — {e}", argv.join(" ")),
    };
    app.reload();
}

/// Leave the TUI, run something that owns the terminal (a shell, `setup
/// --edge`, `journalctl -f`), then re-enter.
fn interactive(terminal: &mut DefaultTerminal, app: &mut App, argv: Vec<String>) {
    ratatui::restore();
    println!("\n$ {}\n", argv.join(" "));
    let status = Command::new(&argv[0]).args(&argv[1..]).status();
    *terminal = ratatui::init();
    app.status = match status {
        Ok(s) if s.success() => format!("✓ {}", argv.join(" ")),
        Ok(_) => format!("· {} exited", argv.join(" ")),
        Err(e) => format!("✗ {} — {e}", argv.join(" ")),
    };
    app.reload();
}

/// `systemctl [--user] <verb> <unit>` — root runs it directly, otherwise via
/// sudo (interactive, so the password prompt lands in the terminal).
fn unit_action(terminal: &mut DefaultTerminal, app: &mut App, verb: &str, unit: &str) {
    let mut argv: Vec<String> = Vec::new();
    if app.snap.root {
        argv.push("systemctl".into());
    } else {
        // rootless per-app units are --user; system units need sudo. Assume
        // the common case (a system unit) and let sudo prompt.
        argv.extend(["sudo".into(), "systemctl".into()]);
    }
    argv.push(verb.into());
    argv.push(unit.into());
    interactive(terminal, app, argv);
}

fn journal(terminal: &mut DefaultTerminal, app: &mut App, unit: &str) {
    let mut argv: Vec<String> = Vec::new();
    if !app.snap.root {
        argv.push("sudo".into());
    }
    argv.extend([
        "journalctl".into(),
        "-u".into(),
        unit.into(),
        "-n".into(),
        "200".into(),
        "-f".into(),
    ]);
    interactive(terminal, app, argv);
}

#[cfg(test)]
mod tests {
    use super::data::{AppRow, Host, InstanceRow, ServiceStatus, Snapshot};
    use super::{data, view, App, Tab};
    use ply_core::runtime::events::Event;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::Instant;

    fn sample() -> App {
        let apps = vec![
            AppRow {
                name: "dashboard".into(),
                up: 1,
                scale: 1,
                restarts: 0,
                uptime: 3 * 86400,
                version: "0.1.41".into(),
                published: "10.77.0.1:7070".into(),
                domains: vec!["admin.plybox.sh".into()],
                instances: vec![InstanceRow {
                    name: "dashboard.1".into(),
                    pid: 2166045,
                    ip: "10.77.0.3".into(),
                    uptime: 3 * 86400,
                    restarts: 0,
                    alive: true,
                    cpu: Some(0.2),
                    mem: Some(19_088_998),
                    mem_max: Some(268_435_456),
                    cpu_max_cores: Some(2.0),
                }],
                service: ServiceStatus::Active,
            },
            AppRow {
                name: "plybox-web".into(),
                up: 1,
                scale: 1,
                restarts: 0,
                uptime: 3 * 3600,
                version: "0.4.50".into(),
                published: "10.77.0.1:3000".into(),
                domains: vec!["plybox.sh".into()],
                instances: vec![],
                service: ServiceStatus::Failed,
            },
        ];
        let events = vec![Event {
            ts: data::now().saturating_sub(3 * 3600),
            app: "plybox-web".into(),
            event: "deploy".into(),
            detail: "deployed plybox-web-0.4.50-linux-x64.img".into(),
        }];
        let host = Host {
            edge_installed: true,
            caddy: ServiceStatus::Active,
            proxy: ServiceStatus::Active,
            reconcile: ServiceStatus::Active,
            services: vec![
                ("ply-dashboard".into(), ServiceStatus::Active),
                ("ply-plybox-web".into(), ServiceStatus::Failed),
            ],
            domains: vec![("admin.plybox.sh".into(), "dashboard".into())],
            disk_pct: Some(27),
            version: "0.1.95".into(),
            cpus: 4,
            mem_total: 8 * 1024 * 1024 * 1024,
            mem_avail: 6 * 1024 * 1024 * 1024,
            load1: 1.03,
        };
        App {
            tab: Tab::Apps,
            snap: Snapshot {
                apps,
                events,
                host,
                deploys: vec![super::data::DeployRow {
                    name: "site".into(),
                    source: "repo https://github.com/iluxav/rm-web".into(),
                    version: None,
                    ok: true,
                    detail: "unchanged (site-0.1.0-linux-arm64.img)".into(),
                    members: vec![],
                }],
                root: true,
            },
            apps_sel: 0,
            host_sel: 0,
            deploy_sel: 0,
            detail: None,
            log_follow: false,
            status: String::new(),
            input: None,
            last_refresh: Instant::now(),
            quit: false,
        }
    }

    fn rendered(app: &App) -> String {
        let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
        t.draw(|f| view::render(f, app)).unwrap();
        let buf = t.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
        }
        s
    }

    #[test]
    fn apps_tab_renders_chrome_and_rows() {
        let s = rendered(&sample());
        for needle in [
            "ply",
            "apps",
            "host",
            "dashboard",
            "plybox-web",
            "RECENT EVENTS",
        ] {
            assert!(s.contains(needle), "missing {needle:?}");
        }
    }

    #[test]
    fn host_tab_shows_edge_and_services() {
        let mut app = sample();
        app.tab = Tab::Host;
        let s = rendered(&app);
        for needle in ["EDGE", "SERVICES", "ply-plybox-web", "DOMAINS"] {
            assert!(s.contains(needle), "missing {needle:?}");
        }
    }

    #[test]
    fn deploy_tab_lists_deployments() {
        let mut app = sample();
        app.tab = Tab::Deploy;
        let s = rendered(&app);
        for needle in ["DEPLOYMENTS", "site", "github.com/iluxav/rm-web"] {
            assert!(s.contains(needle), "missing {needle:?}");
        }
    }

    #[test]
    fn detail_shows_instance_and_actions() {
        let mut app = sample();
        app.detail = Some(0);
        let s = rendered(&app);
        assert!(s.contains("dashboard.1"), "instance row");
        assert!(s.contains("restart"), "actions hint");
    }
}
