//! Registry identity + publishing: `ply login`, `ply whoami`, `ply push`.
//!
//! GitHub is the account system. `login` runs the OAuth device flow
//! against GitHub directly (public client_id, no secret anywhere), trades
//! the GitHub token for a ply token at plybox.sh, and stores only the ply
//! token. Your registry namespace IS your GitHub login.

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

fn saved() -> Option<(String, String)> {
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
    let login = body["login"].as_str().unwrap_or("?");

    let path = credentials_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, format!("token = {token:?}\nlogin = {login:?}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    println!("logged in as {login} — your registry namespace is `{login}/`");
    Ok(())
}

pub fn whoami() -> Result<()> {
    match saved() {
        Some((_, login)) => println!("{login}"),
        None => println!("not logged in — run `ply login`"),
    }
    Ok(())
}

pub fn push(image: &std::path::Path) -> Result<()> {
    let Some((token, login)) = saved() else {
        bail!("not logged in — run `ply login` first");
    };
    let filename = image
        .file_name()
        .and_then(|f| f.to_str())
        .context("image path has no filename")?
        .to_string();
    let mut file =
        std::fs::File::open(image).with_context(|| format!("opening {}", image.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    println!(
        "pushing {filename} ({:.1} MiB) as {login}/…",
        bytes.len() as f64 / (1024.0 * 1024.0)
    );

    let result = ureq::post(&format!("{}/api/push/", api_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .header("X-Ply-Filename", &filename)
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
