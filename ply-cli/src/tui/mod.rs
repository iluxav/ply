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
    /// The new-deployment form; when set it captures all keys.
    pub form: Option<DeployForm>,
    last_refresh: Instant,
    quit: bool,
}

pub(crate) struct Input {
    pub title: String,
    pub prompt: String,
    pub buffer: String,
    target: InputTarget,
}

/// Where a new deployment's app comes from. Two ergonomic sources, matching
/// the self-host expectations: build a repo on this box (Heroku/Coolify), or
/// follow a prebuilt image (registry / CI). Both take the same overrides and
/// both follow-latest by default.
#[derive(Clone)]
enum DeploySource {
    Repo(String),  // `repo = "<url>"` — the host clones & builds it, then follows pushes
    App(String),   // `app = "<ns/name>"` — a published image, follows the newest version
    Image(String), // `image = "<url|path>"` — a fixed image file/URL
}

enum InputTarget {
    AddDomain(String),        // the app the domain is for
    PinVersion(String),       // the deployment to pin/roll back
    SetBuild(String),         // the deployment to set a `build =` command on
    RemoveApp(String),        // stop + remove an app (typed-yes confirm)
    RemoveDeployment(String), // delete a deployment spec (typed-yes confirm)
}

/// The new-deployment form: a source selector plus the fields that apply to
/// it, all on screen at once (Tab/↑↓ move, ←→ change source, Enter deploys).
/// The repo source adds build + token fields; all sources share publish / env
/// / domain. Replaces the old field-by-field prompt.
pub(crate) struct DeployForm {
    pub source: usize, // index into SOURCES: 0 repo, 1 registry, 2 image
    pub value: String, // repo URL / app ref / image url
    pub build: String, // repo only: build command
    pub token: String, // repo only: token for a private repo
    pub publish: String,
    pub env: String,
    pub domain: String,
    /// 0 = source selector; 1..=N = the visible fields; N+1 = the Deploy button.
    pub focus: usize,
}

/// (label, value-field label, value hint) for each source, in `source` order.
pub(crate) const SOURCES: [(&str, &str, &str); 3] = [
    (
        "GitHub repo",
        "Repo URL",
        "https://github.com/you/app — cloned & built on this box, follows pushes",
    ),
    (
        "Registry app",
        "App",
        "iluxav/web — a published image, follows the newest version",
    ),
    (
        "Image URL",
        "Image",
        "https://…/app.img or a local path — a fixed image",
    ),
];

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FormField {
    Value,
    Build,
    Token,
    Publish,
    Env,
    Domain,
}

impl DeployForm {
    fn new() -> Self {
        DeployForm {
            source: 0,
            value: String::new(),
            build: String::new(),
            token: String::new(),
            publish: String::new(),
            env: String::new(),
            domain: String::new(),
            focus: 0,
        }
    }

    /// The fields shown for the current source, in display/focus order.
    pub(crate) fn fields(&self) -> Vec<FormField> {
        use FormField::*;
        if self.source == 0 {
            vec![Value, Build, Token, Publish, Env, Domain]
        } else {
            vec![Value, Publish, Env, Domain]
        }
    }

    /// Total focus stops: source selector + fields + the Deploy button.
    fn focus_count(&self) -> usize {
        self.fields().len() + 2
    }

    pub(crate) fn is_source_focus(&self) -> bool {
        self.focus == 0
    }

    pub(crate) fn is_submit_focus(&self) -> bool {
        self.focus == self.focus_count() - 1
    }

    /// The field the cursor is in, if focus is on a field (not source/submit).
    pub(crate) fn focused_field(&self) -> Option<FormField> {
        let fields = self.fields();
        (1..=fields.len())
            .contains(&self.focus)
            .then(|| fields[self.focus - 1])
    }

    pub(crate) fn value_of(&self, f: FormField) -> &str {
        match f {
            FormField::Value => &self.value,
            FormField::Build => &self.build,
            FormField::Token => &self.token,
            FormField::Publish => &self.publish,
            FormField::Env => &self.env,
            FormField::Domain => &self.domain,
        }
    }

    fn value_mut(&mut self, f: FormField) -> &mut String {
        match f {
            FormField::Value => &mut self.value,
            FormField::Build => &mut self.build,
            FormField::Token => &mut self.token,
            FormField::Publish => &mut self.publish,
            FormField::Env => &mut self.env,
            FormField::Domain => &mut self.domain,
        }
    }

    /// Label + hint for a field (the value field's label depends on source).
    pub(crate) fn field_meta(&self, f: FormField) -> (&'static str, &'static str) {
        match f {
            FormField::Value => (SOURCES[self.source].1, SOURCES[self.source].2),
            FormField::Build => (
                "Build cmd",
                "e.g. npm ci && npm run build — for a repo with no ply.toml build step (optional)",
            ),
            FormField::Token => (
                "Token",
                "GitHub token for a PRIVATE repo (stored 0600); blank = public",
            ),
            FormField::Publish => ("Publish", "8080:3000 or internal:3000 (blank = none)"),
            FormField::Env => ("Env", "KEY=VAL, comma-separated (blank = none)"),
            FormField::Domain => ("Domain", "app.example.com (blank = none)"),
        }
    }
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
            form: None,
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
    if app.form.is_some() {
        return handle_form_key(app, code);
    }
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
    app.form = Some(DeployForm::new());
}

/// Drive the new-deployment form: Tab/↑↓ move focus, ←→ change source, typing
/// edits the focused field, Enter deploys, Esc cancels.
fn handle_form_key(app: &mut App, code: KeyCode) {
    // Tabbing off the repo URL (or token) inspects the repo and prefills.
    let inspect_after;
    {
        let Some(form) = app.form.as_mut() else {
            return;
        };
        let count = form.focus_count();
        let leaving_url = form.source == 0
            && matches!(
                form.focused_field(),
                Some(FormField::Value) | Some(FormField::Token)
            );
        let mut moved = false;
        match code {
            KeyCode::Esc => {
                app.form = None;
                app.status = "cancelled".into();
                return;
            }
            KeyCode::Tab | KeyCode::Down => {
                form.focus = (form.focus + 1) % count;
                moved = true;
            }
            KeyCode::BackTab | KeyCode::Up => {
                form.focus = (form.focus + count - 1) % count;
                moved = true;
            }
            KeyCode::Left if form.is_source_focus() => {
                form.source = (form.source + SOURCES.len() - 1) % SOURCES.len();
            }
            KeyCode::Right if form.is_source_focus() => {
                form.source = (form.source + 1) % SOURCES.len();
            }
            KeyCode::Enter => {
                if form.is_source_focus() {
                    form.focus = 1; // picked a source → jump to its first field
                } else {
                    submit_form(app);
                    return;
                }
            }
            KeyCode::Backspace => {
                if let Some(f) = form.focused_field() {
                    form.value_mut(f).pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(f) = form.focused_field() {
                    form.value_mut(f).push(c);
                }
            }
            _ => {}
        }
        inspect_after = moved && leaving_url && !form.value.trim().is_empty();
    }
    if inspect_after {
        inspect_repo(app);
    }
}

/// Fetch the repo's ply.toml and prefill/annotate the form: publish from a
/// declared port, a note for a composition, or a hint when there's no
/// ply.toml. Best-effort — a failure just leaves the fields for the user.
fn inspect_repo(app: &mut App) {
    let (url, token) = match app.form.as_ref() {
        Some(f) if f.source == 0 => (f.value.trim().to_string(), f.token.trim().to_string()),
        _ => return,
    };
    let Some((owner, repo)) = parse_github(&url) else {
        return;
    };
    match fetch_ply_toml(&owner, &repo, &token) {
        Ok(Some(text)) => {
            if let Ok(Some(stack)) = ply_core::stack::parse(&text, std::path::Path::new("ply.toml"))
            {
                app.status = format!(
                    "✓ {repo}: composition · {} services (deployed as a set)",
                    stack.members.len()
                );
                return;
            }
            match ply_core::manifest::Manifest::parse(&text) {
                Ok(m) => {
                    let port = m.ports.values().next().copied();
                    if let (Some(p), Some(form)) = (port, app.form.as_mut()) {
                        if form.publish.trim().is_empty() {
                            form.publish = format!("internal:{p}");
                        }
                    }
                    app.status = match port {
                        Some(p) => format!(
                            "✓ {repo}: app · port {p} — publish prefilled internal:{p} (edit for a public port like 8080:{p})"
                        ),
                        None => format!("✓ {repo}: app (no declared port)"),
                    };
                }
                Err(e) => app.status = format!("✗ {repo}: ply.toml didn't parse — {e}"),
            }
        }
        Ok(None) => {
            app.status =
                format!("{repo}: no ply.toml — set a Build command, or it must be ply-native")
        }
        Err(e) => {
            app.status = format!("couldn't read {repo}/ply.toml — {e} (private? add a Token)")
        }
    }
}

/// `(owner, repo)` from a github URL or ssh shorthand; None if not github.
fn parse_github(url: &str) -> Option<(String, String)> {
    let s = url.trim().trim_end_matches('/');
    let rest = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .or_else(|| s.strip_prefix("git+https://github.com/"))
        .or_else(|| s.strip_prefix("git@github.com:"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut it = rest.split('/');
    let owner = it.next()?.to_string();
    let repo = it.next()?.to_string();
    (!owner.is_empty() && !repo.is_empty()).then_some((owner, repo))
}

/// GET the repo's ply.toml via the GitHub contents API (works for public and,
/// with a token, private). `Ok(None)` = no ply.toml (404).
fn fetch_ply_toml(owner: &str, repo: &str, token: &str) -> Result<Option<String>, String> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/contents/ply.toml");
    let mut req = ureq::get(&url)
        .header("User-Agent", "ply")
        .header("Accept", "application/vnd.github.raw+json");
    if !token.is_empty() {
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    match req.call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
            .map(Some)
            .map_err(|e| e.to_string()),
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(ureq::Error::StatusCode(c)) => Err(format!("HTTP {c}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Build the order from the form's fields and create it.
fn submit_form(app: &mut App) {
    let Some(form) = app.form.as_ref() else {
        return;
    };
    let value = form.value.trim().to_string();
    if value.is_empty() {
        app.status = format!("enter a {}", SOURCES[form.source].1.to_lowercase());
        return;
    }
    let source = match form.source {
        0 => DeploySource::Repo(value),
        1 => DeploySource::App(value),
        _ => DeploySource::Image(value),
    };
    let (build, token) = (form.build.trim().to_string(), form.token.trim().to_string());
    let (publish, env, domain) = (
        form.publish.trim().to_string(),
        form.env.trim().to_string(),
        form.domain.trim().to_string(),
    );
    match create_deployment(&source, &build, &token, &publish, &env, &domain) {
        // The deployments watcher (ply-deployments.path → ply-reconcile.service)
        // picks up the new order within seconds and builds it — serialized by
        // systemd. We deliberately do NOT kick `ply reconcile` here: a direct
        // run races the watcher on the same checkout ("shallow file has changed").
        // The deploy row shows a "deploying" spinner until reconcile writes a
        // real status.
        Ok(msg) => app.status = format!("⟳ {msg}"),
        Err(e) => app.status = format!("✗ {e}"),
    }
    app.form = None;
    app.reload();
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
/// Write a deployment order from a chosen source + overrides. One shape for
/// all three sources: a source line (`repo`/`app`/`image`), then the shared
/// overrides (publish, env, domain). Every source follows-latest by default
/// (the Spec default `auto = true`), so nothing extra is written for it.
fn create_deployment(
    source: &DeploySource,
    build: &str,
    token: &str,
    publish: &str,
    env: &str,
    domain: &str,
) -> anyhow::Result<String> {
    let (source_line, name, is_repo) = match source {
        DeploySource::Repo(url) => (
            format!("repo = \"{url}\"\n"),
            deploy_name_from_url(url),
            true,
        ),
        DeploySource::App(reference) => (
            format!("app = \"{reference}\"\n"),
            deploy_name_from_ref(reference),
            false,
        ),
        DeploySource::Image(url) => (
            format!("image = \"{url}\"\n"),
            deploy_name_from_url(url),
            false,
        ),
    };
    if name.is_empty() {
        anyhow::bail!("could not derive a name from the source");
    }
    let dir = ply_core::deployments::dir();
    let path = dir.join(format!("{name}.toml"));
    if path.exists() {
        anyhow::bail!("a deployment named {name} already exists");
    }
    let mut spec = source_line;
    // repo-only: a build command, and a token for a private repo (written to a
    // 0600 file the spec references, never inlined into the order).
    if is_repo && !build.is_empty() {
        spec.push_str(&format!("build = {}\n", toml_string(build)));
    }
    if is_repo && !token.is_empty() {
        let rel = write_token_file(&dir, &name, token)?;
        spec.push_str(&format!("token_file = \"{rel}\"\n"));
    }
    if !publish.is_empty() {
        spec.push_str(&format!("publish = [\"{publish}\"]\n"));
    }
    if !domain.is_empty() {
        spec.push_str(&format!("domain = [\"{domain}\"]\n"));
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
    Ok(format!("created {name} — reconcile is deploying it"))
}

/// A TOML basic string with quotes/backslashes escaped — for a build command.
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Write a private-repo token to `<deployments>/.secrets/<name>.token` (dir
/// 0700, file 0600) and return the relative path for `token_file =`
/// (`resolve_secret` resolves it against the deployments dir).
fn write_token_file(dir: &std::path::Path, name: &str, token: &str) -> anyhow::Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let secrets = dir.join(".secrets");
    std::fs::create_dir_all(&secrets)?;
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o700))?;
    let rel = format!(".secrets/{name}.token");
    let path = dir.join(&rel);
    std::fs::write(&path, format!("{}\n", token.trim()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(rel)
}

/// A deployment name from a registry ref: the package tail (`iluxav/web` →
/// `web`), sanitized to the deployment-name grammar.
fn deploy_name_from_ref(reference: &str) -> String {
    reference
        .split('@')
        .next()
        .unwrap_or(reference)
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("")
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
    use super::{data, deploy_name_from_ref, deploy_name_from_url, view, App, Tab};

    #[test]
    fn deployment_name_derivation() {
        // registry refs → the package tail, version stripped
        assert_eq!(deploy_name_from_ref("iluxav/web"), "web");
        assert_eq!(deploy_name_from_ref("iluxav/web@1.2.0"), "web");
        assert_eq!(deploy_name_from_ref("postgres@17"), "postgres");
        // repo / image URLs → the basename, .git stripped, sanitized
        assert_eq!(
            deploy_name_from_url("https://github.com/iluxav/rm-web"),
            "rm-web"
        );
        assert_eq!(
            deploy_name_from_url("https://github.com/iluxav/rm-web.git"),
            "rm-web"
        );
        assert_eq!(
            deploy_name_from_url("https://cdn.example.com/app.img"),
            "app-img"
        );
    }
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
            form: None,
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
