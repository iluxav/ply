//! Registry identity + publishing: `ply login`, `ply whoami`, `ply push`.
//!
//! GitHub is one door into the account, not the account itself: `login`
//! runs the OAuth device flow against GitHub directly (public client_id, no
//! secret anywhere), trades the GitHub token for a ply key at plybox.sh, and
//! stores only that key. Identity there is the verified email; the namespace
//! is a username chosen once on the site, so renaming on GitHub never moves
//! it and a second provider lands on the same account.

use std::io::Read;

use anyhow::{bail, Context, Result};

/// The public OAuth app client id (device flow enabled). Public by
/// design — device flow needs no secret. Bake the real one here;
/// PLY_GITHUB_CLIENT_ID overrides for testing against another app.
const GITHUB_CLIENT_ID: &str = "Ov23ctE7JOHi47WnLPVR";

fn client_id() -> Result<String> {
    if let Ok(id) = std::env::var("PLY_GITHUB_CLIENT_ID") {
        return Ok(id);
    }
    if GITHUB_CLIENT_ID.is_empty() {
        bail!("this build has no GitHub client id baked in — set PLY_GITHUB_CLIENT_ID");
    }
    Ok(GITHUB_CLIENT_ID.to_string())
}

fn api_base() -> String {
    std::env::var("PLY_REGISTRY_API").unwrap_or_else(|_| "https://plybox.sh".into())
}

fn credentials_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config")
        });
    base.join("ply").join("credentials")
}

/// The key and the namespace it publishes to. `PLY_TOKEN` wins over the
/// credentials file: a CI runner can never do the device flow, so a key in
/// the environment IS the login there. The namespace comes from `PLY_LOGIN`
/// when set, else from the registry (one GET, only for the printed lines) —
/// the server derives the real owner from the key regardless.
fn saved() -> Option<(String, String)> {
    if let Ok(token) = std::env::var("PLY_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            let login = std::env::var("PLY_LOGIN")
                .ok()
                .filter(|l| !l.trim().is_empty())
                .or_else(|| remote_login(&token))
                .unwrap_or_default();
            return Some((token, login));
        }
    }
    let raw = std::fs::read_to_string(credentials_path()).ok()?;
    let mut token = None;
    let mut login = None;
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("token = ") {
            token = Some(v.trim().trim_matches('"').to_string());
        }
        if let Some(v) = line.strip_prefix("login = ") {
            login = Some(v.trim().trim_matches('"').to_string());
        }
    }
    Some((token?, login?))
}

/// Resolve a key's login at the registry. Best-effort: a network hiccup
/// must not fail a push whose authority is the key, not the name.
fn remote_login(token: &str) -> Option<String> {
    let mut resp = ureq::get(format!("{}/api/cli/whoami/", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()?;
    let body: serde_json::Value =
        serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()?;
    body["login"].as_str().map(str::to_string)
}

pub fn login() -> Result<()> {
    // 1 · device code
    let mut resp = ureq::post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .send_form([("client_id", client_id()?.as_str())])
        .context("reaching github")?;
    let body: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    let device_code = body["device_code"].as_str().context("no device_code")?;
    let user_code = body["user_code"].as_str().context("no user_code")?;
    let verify_url = body["verification_uri"]
        .as_str()
        .unwrap_or("https://github.com/login/device");
    let interval = body["interval"].as_u64().unwrap_or(5).max(5);

    println!("open   {verify_url}");
    println!("enter  {user_code}");
    println!("waiting for you to authorize …");

    // 2 · poll for the GitHub token
    let github_token = loop {
        std::thread::sleep(std::time::Duration::from_secs(interval));
        let mut poll = ureq::post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .send_form([
                ("client_id", client_id()?.as_str()),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .context("polling github")?;
        let poll: serde_json::Value = serde_json::from_str(&poll.body_mut().read_to_string()?)?;
        if let Some(token) = poll["access_token"].as_str() {
            break token.to_string();
        }
        match poll["error"].as_str() {
            Some("authorization_pending") | Some("slow_down") => continue,
            Some(e) => bail!("github: {e}"),
            None => bail!("github: unexpected answer"),
        }
    };

    // 3 · trade it for a ply token (the GitHub token is then discarded)
    let mut resp = ureq::post(&format!("{}/api/cli/auth/", api_base()))
        .header("Content-Type", "application/json")
        .send(format!("{{\"github_token\":{github_token:?}}}"))
        .context("reaching the registry")?;
    let body: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    let token = body["token"].as_str().context("registry issued no token")?;
    // The namespace is the username the person chose on the site — null
    // until they have. The key is still valid; only publishing waits.
    let namespace = body["login"].as_str();

    let path = credentials_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(
        &path,
        format!("token = {token:?}\nlogin = {:?}\n", namespace.unwrap_or("")),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    match namespace {
        Some(ns) => println!("logged in — your registry namespace is `{ns}/`"),
        None => println!(
            "logged in — now choose your username at {}/account/ ; it becomes your namespace",
            api_base()
        ),
    }
    Ok(())
}

pub fn whoami() -> Result<()> {
    let Some((token, login)) = saved() else {
        println!("not logged in — run `ply login`, or set PLY_TOKEN (CI)");
        return Ok(());
    };
    if login.is_empty() {
        println!(
            "logged in, but no username yet — choose one at {}/account/",
            api_base()
        );
        return Ok(());
    }
    println!("{login}");
    // Grants beyond your own login are worth naming: they are what makes
    // `ply push --as ply` legal, and they are invisible otherwise.
    if let Some(extra) = granted_namespaces(&token) {
        let others: Vec<String> = extra.into_iter().filter(|n| n != &login).collect();
        if !others.is_empty() {
            println!("also publishes to: {}", others.join(", "));
        }
    }
    Ok(())
}

/// Namespaces this key may publish to, straight from the registry.
fn granted_namespaces(token: &str) -> Option<Vec<String>> {
    let mut resp = ureq::get(format!("{}/api/cli/whoami/", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()?;
    let body: serde_json::Value =
        serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()?;
    Some(
        body["namespaces"]
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
    )
}

/// `ply key new` — mint another key from the one this machine already
/// holds. The point is CI: a runner cannot do a device flow, so you mint a
/// key here and paste it into a repository secret as PLY_TOKEN.
pub fn key_new(note: Option<&str>) -> Result<()> {
    let (token, _) = saved().context("not logged in — run `ply login` first")?;
    let note = note.unwrap_or("");
    let mut resp = ureq::post(format!("{}/api/auth/tokens/", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(format!("{{\"note\":{note:?}}}"))
        .map_err(|e| anyhow::anyhow!("minting a key: {e}"))?;
    let body: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    let fresh = body["token"]
        .as_str()
        .context("the registry issued no key")?;
    println!("{fresh}");
    eprintln!("ply: shown once — store it now (CI: a repository secret named PLY_TOKEN)");
    Ok(())
}

/// `ply key ls` — what keys exist, when each was last used. Never the keys
/// themselves: the registry keeps only hashes.
pub fn key_ls() -> Result<()> {
    let (token, _) = saved().context("not logged in — run `ply login` first")?;
    let mut resp = ureq::get(format!("{}/api/auth/tokens/", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| anyhow::anyhow!("listing keys: {e}"))?;
    let body: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    let keys = body["keys"].as_array().cloned().unwrap_or_default();
    if keys.is_empty() {
        println!("no keys");
        return Ok(());
    }
    println!("{:<6} {:<24} {:<26} LAST USED", "ID", "NOTE", "CREATED");
    for k in keys {
        let note = k["note"].as_str().unwrap_or("");
        println!(
            "{:<6} {:<24} {:<26} {}",
            k["id"].as_i64().unwrap_or(0),
            if note.is_empty() { "-" } else { note },
            k["created_at"].as_str().unwrap_or("-"),
            k["last_used_at"].as_str().unwrap_or("never"),
        );
    }
    Ok(())
}

/// `ply key rm <id>` — revoke immediately; anything using it stops.
pub fn key_rm(id: i64) -> Result<()> {
    let (token, _) = saved().context("not logged in — run `ply login` first")?;
    ureq::post(format!("{}/api/auth/tokens/revoke/", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(format!("{{\"id\":{id}}}"))
        .map_err(|e| anyhow::anyhow!("revoking key {id}: {e}"))?;
    println!("key #{id} revoked");
    Ok(())
}

/// Where a push goes. Without `--as`, the bare endpoint: the server owns
/// the default namespace, so a CI key whose login we could not resolve
/// still lands in the right place. With it, the path form the REST API
/// advertises — `/api/push/<namespace>/<filename>`.
///
/// The trailing slashes are load-bearing: the site normalizes to them and
/// answers a slashless POST with a 308, which a request carrying a body
/// must not chase. A last segment that looks like a file is the exception
/// — those keep no slash, or the normalizer redirects the other way.
fn push_endpoint(as_namespace: Option<&str>, filename: Option<&str>) -> String {
    match (as_namespace, filename) {
        (Some(ns), Some(file)) => format!("{}/api/push/{ns}/{file}", api_base()),
        (Some(ns), None) => format!("{}/api/push/{ns}/", api_base()),
        _ => format!("{}/api/push/", api_base()),
    }
}

pub fn push(image: &str, as_namespace: Option<&str>) -> Result<()> {
    let Some((token, login)) = saved() else {
        bail!("not logged in — run `ply login`, or set PLY_TOKEN (CI)");
    };
    // `--as` publishes to a granted namespace (the official shelves, a
    // shared org) via the path form; the server still checks the grant.
    // Without it we post to the bare endpoint and let the server default to
    // the key's own login — never to a locally guessed name.
    let shown = as_namespace.unwrap_or(&login).to_string();
    if shown.is_empty() {
        bail!(
            "no namespace yet — choose your username at {}/account/ (it becomes your namespace)",
            api_base()
        );
    }
    if image.starts_with("https://") {
        return push_url(image, &token, &shown, as_namespace);
    }
    let path = std::path::Path::new(image);
    // Stack push: a .toml file, or a directory whose ply.toml is a stack.
    if let Some((stack, toml_path)) = load_stack_for_push(path)? {
        return push_stack(&stack, &toml_path, &token, &shown, as_namespace);
    }
    let login = shown;
    let image = path;
    let filename = image
        .file_name()
        .and_then(|f| f.to_str())
        .context("image path has no filename")?
        .to_string();
    let mut file =
        std::fs::File::open(image).with_context(|| format!("opening {}", image.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    // Derive the catalog metadata from the image itself and send it alongside
    // the bytes — the server stores it verbatim (client-derives, server-
    // stores). Compact JSON is a valid single-line header value.
    let meta = ply_core::catalog::derive_push_meta(image)
        .with_context(|| format!("reading {} — is it a ply image?", image.display()))?;
    let meta_json = serde_json::to_string(&meta).context("encoding push metadata")?;
    println!(
        "pushing {filename} ({:.1} MiB, {}) as {login}/…",
        bytes.len() as f64 / (1024.0 * 1024.0),
        format!("{:?}", meta.kind).to_lowercase(),
    );

    let result = ureq::post(&push_endpoint(as_namespace, Some(&filename)))
        .header("Authorization", &format!("Bearer {token}"))
        .header("X-Ply-Filename", &filename)
        .header("X-Ply-Meta", &meta_json)
        .header("Content-Type", "application/octet-stream")
        .send(&bytes[..]);
    let mut resp = match result {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) => {
            bail!("registry answered {code} — bump the version if it is already published");
        }
        Err(e) => return Err(e).context("reaching the registry"),
    };
    let body: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    match body["published"].as_str() {
        Some(published) => {
            println!("published {published}");
            println!("  {}", body["url"].as_str().unwrap_or(""));
            if let Some(use_snippet) = body["use"].as_str() {
                println!("deploy it with:");
                for line in use_snippet.lines() {
                    println!("  {line}");
                }
            }
        }
        None => bail!(
            "push failed: {}",
            body["error"].as_str().unwrap_or("unknown error")
        ),
    }
    Ok(())
}

/// URL mode: the registry records where the bytes live and verifies them
/// server-side (fetch + hash) — it never stores them. The catalog stays
/// a catalog; the publisher's host stays the host.
fn push_url(url: &str, token: &str, login: &str, as_namespace: Option<&str>) -> Result<()> {
    println!("registering {url} under {login}/… (the registry fetches and hashes it — no upload)");
    let body = serde_json::json!({ "url": url }).to_string();
    let result = ureq::post(&push_endpoint(as_namespace, None))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(body.as_bytes());
    let mut resp = match result {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) => {
            bail!("registry answered {code} — check the URL (https, no query string, canonical <name>-<x.y.z>-linux-<arch>.img basename)");
        }
        Err(e) => return Err(e).context("reaching the registry"),
    };
    let body: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    match body["published"].as_str() {
        Some(published) => {
            println!("published {published} (bytes stay at the origin)");
            println!("  sha256: {}", body["sha256"].as_str().unwrap_or(""));
            println!("  origin: {}", body["url"].as_str().unwrap_or(""));
        }
        None => bail!(
            "push failed: {}",
            body["error"].as_str().unwrap_or("unknown error")
        ),
    }
    Ok(())
}

/// If `path` is a stack — a directory whose ply.toml has `[[app]]`, or a
/// `.toml` file with `[[app]]` — load it. Returns (stack, the toml path).
fn load_stack_for_push(
    path: &std::path::Path,
) -> Result<Option<(ply_core::stack::Stack, std::path::PathBuf)>> {
    if path.is_dir() {
        return Ok(ply_core::stack::load(path)?.map(|s| (s, path.join("ply.toml"))));
    }
    if path.extension().and_then(|e| e.to_str()) == Some("toml") {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if let Some(stack) = ply_core::stack::parse(&text, path)? {
            return Ok(Some((stack, path.to_path_buf())));
        }
    }
    Ok(None)
}

/// Publish a stack: upload the stack toml (the template, `$VAR` holes intact)
/// and send its derived `apps[]`. The registry stores the template; consumers
/// fill the holes at deploy time.
fn push_stack(
    stack: &ply_core::stack::Stack,
    toml_path: &std::path::Path,
    token: &str,
    login: &str,
    as_namespace: Option<&str>,
) -> Result<()> {
    let name = stack
        .name
        .as_deref()
        .context("a stack push needs `[stack] name`")?;
    let version = stack
        .version
        .as_deref()
        .context("a stack push needs `[stack] version` (semver x.y.z)")?;
    if ply_core::catalog::parse_run_ref(name) != Some((name.to_string(), None)) {
        bail!("stack name `{name}` must be lowercase [a-z0-9-], starting with a letter or digit");
    }
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3
        || !parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        bail!("stack version `{version}` must be x.y.z");
    }
    let filename = format!("{name}-{version}.stack.toml");
    let bytes =
        std::fs::read(toml_path).with_context(|| format!("reading {}", toml_path.display()))?;
    let meta = ply_core::catalog::derive_stack_meta(stack);
    let meta_json = serde_json::to_string(&meta).context("encoding stack metadata")?;
    println!(
        "pushing {filename} (stack, {} members) as {login}/…",
        stack.members.len()
    );

    let result = ureq::post(&push_endpoint(as_namespace, Some(&filename)))
        .header("Authorization", &format!("Bearer {token}"))
        .header("X-Ply-Filename", &filename)
        .header("X-Ply-Meta", &meta_json)
        .header("Content-Type", "application/octet-stream")
        .send(&bytes[..]);
    let mut resp = match result {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) => {
            bail!("registry answered {code} — bump [stack] version if it is already published");
        }
        Err(e) => return Err(e).context("reaching the registry"),
    };
    let body: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    match body["published"].as_str() {
        Some(published) => {
            println!("published {published} (stack)");
            println!("  {}", body["url"].as_str().unwrap_or(""));
        }
        None => bail!(
            "push failed: {}",
            body["error"].as_str().unwrap_or("unknown error")
        ),
    }
    Ok(())
}
