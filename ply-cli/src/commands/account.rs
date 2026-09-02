//! Registry identity + publishing: `ply login`, `ply whoami`, `ply push`.
//!
//! GitHub is one door into the account, not the account itself: `login`
//! runs the OAuth device flow against GitHub directly (public client_id, no
//! secret anywhere), trades the GitHub token for a ply key at plybox.sh, and
//! stores only that key. Identity there is the verified email; the namespace
//! is a username chosen once on the site, so renaming on GitHub never moves
//! it and a second provider lands on the same account.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ply_core::image::name::Arch;
use ply_core::record::{Artifact, Record};

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

/// What a push will do, worked out before a byte leaves the machine.
///
/// `record` is what `POST /api/publish/` receives (minus `pushed_at` /
/// `published_by`, which the server stamps); `image` is the file whose bytes
/// back it, when there is one; `upload` says whether those bytes still have
/// to be sent — a `--src` push and a stack both answer no.
#[derive(Debug)]
pub struct PushPlan {
    pub record: Record,
    pub image: Option<PathBuf>,
    pub upload: bool,
}

/// Everything a push decides on its own: what the target IS, which manifest
/// is the record, which artifact the version gains, and under whose name.
/// Reads (and may build) files; never touches the network, so `--dry-run` is
/// exactly this function plus a `println!`.
fn plan_push(
    target: &Path,
    src: Option<&str>,
    arch: Option<&str>,
    as_namespace: Option<&str>,
) -> Result<PushPlan> {
    let arch_flag = crate::commands::build::parse_arch(arch)?;
    let arch = arch_flag.unwrap_or_else(Arch::host);

    // A stack first: `stack.toml`, or a directory whose ply.toml is one —
    // it has no image, so nothing is built and nothing is uploaded.
    let mut plan = if let Some((stack, toml_path)) = load_stack_for_push(target)? {
        if src.is_some() || arch_flag.is_some() {
            eprintln!("note: a stack has no artifact — --src/--arch ignored");
        }
        plan_stack(&stack, &toml_path)?
    } else if target.is_dir() {
        // An app/keg directory builds first, exactly as `ply build` would:
        // the record must describe the bytes, and the bytes do not exist yet.
        let image = crate::commands::build::build_for_push(target, Some(arch))?;
        plan_image(&image, src, arch)?
    } else if target.extension().and_then(|e| e.to_str()) == Some("toml") {
        // A `.toml` that `load_stack_for_push` did not claim is an app or keg
        // manifest — the source of an image, not a thing to publish. What
        // gets published is the manifest INSIDE the built image.
        bail!(
            "{} is a manifest, not an artifact — push what it builds: \
             `ply push {}` (builds it first), or the built .img",
            target.display(),
            target
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .display()
        );
    } else {
        // An existing `.img` already knows its arch — its NAME says so.
        plan_image(target, src, image_arch(target, arch_flag, arch)?)?
    };

    // Owner: the manifest wins, `--as` fills a manifest that names none, and
    // a disagreement is a mistake worth stopping for — publishing under the
    // wrong namespace is not undoable.
    match (plan.record.owner.as_deref(), as_namespace) {
        (Some(a), Some(b)) if a != b => {
            bail!("manifest says owner = \"{a}\" but --as {b} was given — drop one of them")
        }
        (None, Some(b)) => plan.record.owner = Some(b.to_string()),
        _ => {}
    }
    Ok(plan)
}

/// The arch of an artifact that already exists: the FILE says so, via the
/// canonical `<name>-<version>-linux-<arch>.img` grammar. A cross-built
/// image pushed from the other kind of box would otherwise be stamped with
/// the HOST's arch — bytes that download, verify, and cannot run, published
/// under a lie no later push can correct (the registry is append-only).
/// `--arch` may confirm the filename, never contradict it; a name outside
/// the grammar claims no arch, so `--arch` (else the host) decides as
/// before. A directory target has no image yet — it is BUILT for `--arch`,
/// so this never applies there.
fn image_arch(image: &Path, flag: Option<Arch>, fallback: Arch) -> Result<Arch> {
    let named = image
        .file_name()
        .and_then(|f| f.to_str())
        .and_then(|f| ply_core::image::name::ImageName::parse(f).ok())
        .map(|n| n.arch);
    match (named, flag) {
        (Some(named), Some(flag)) if named != flag => bail!(
            "{} is a {} image, but --arch {} was given — drop --arch, or push the {} build",
            image.display(),
            named.as_str(),
            flag.as_str(),
            flag.as_str()
        ),
        (Some(named), _) => Ok(named),
        (None, _) => Ok(fallback),
    }
}

/// An image: the record is the manifest INSIDE it (what the artifact really
/// contains — never the working-copy ply.toml), plus the one artifact this
/// push adds to the version.
fn plan_image(image: &Path, src: Option<&str>, arch: Arch) -> Result<PushPlan> {
    let mut record = ply_core::record::record_for_image(image)
        .with_context(|| format!("reading {}", image.display()))?;
    // The wire form is bare 64-hex: the registry hashes the bytes it receives
    // (`createHash().digest("hex")`) and validates records against
    // `/^[0-9a-f]{64}$/`. `sha256_file` returns ply's `sha256:<hex>` display
    // form, so it is stripped HERE, at the one place an artifact is made.
    let digest = ply_core::digest::sha256_file(image)?;
    let sha256 = bare_sha256(&digest).to_string();
    let bytes = std::fs::metadata(image)
        .with_context(|| format!("reading {}", image.display()))?
        .len();
    // `--src` means the bytes live at the publisher's host: we hash them here
    // (so the record still pins content) and upload nothing. Unverified until
    // something fetches and re-hashes them, which is not this release.
    let (src, upload) = match src {
        Some(template) => (
            ply_core::record::expand_src(template, &record.version, arch.as_str()),
            false,
        ),
        None => (String::new(), true),
    };
    record.artifacts = vec![Artifact {
        arch: arch.as_str().to_string(),
        src,
        sha256,
        bytes,
        verified: false,
    }];
    Ok(PushPlan {
        record,
        image: Some(image.to_path_buf()),
        upload,
    })
}

/// `sha256:<hex>` → `<hex>`. ply prints digests prefixed; the registry stores
/// and compares bare hex, on both endpoints.
fn bare_sha256(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

/// A stack: the toml IS the artifact, so there is nothing to upload and the
/// record carries no artifacts at all. Its members must be things a consumer
/// can resolve — a `./dir` member means nothing on someone else's machine.
fn plan_stack(stack: &ply_core::stack::Stack, toml_path: &Path) -> Result<PushPlan> {
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
    for member in &stack.members {
        if let ply_core::stack::MemberSource::Path(path) = &member.source {
            bail!(
                "member `{}` runs {} — a published stack's members must be registry refs \
                 (`postgres@17`) or URLs; publish that app first, then reference it by name",
                member.name,
                path.display()
            );
        }
    }
    let text = std::fs::read_to_string(toml_path)
        .with_context(|| format!("reading {}", toml_path.display()))?;
    let record = ply_core::record::record_for_toml(&text, toml_path)?;
    Ok(PushPlan {
        record,
        image: None,
        upload: false,
    })
}

/// `ply push` — two requests at most: the bytes (mechanical), then the record
/// (the publish). The manifest travels inside the record, so the registry
/// never has to read squashfs and there is no second, derived truth to keep
/// in sync.
pub fn push(args: crate::cli::PushArgs) -> Result<()> {
    // A bare URL carries no manifest, and the manifest IS the record now.
    if args.target.starts_with("https://") || args.target.starts_with("http://") {
        bail!("to publish an image hosted elsewhere: ply push ./the.img --src https://…");
    }
    let mut plan = plan_push(
        Path::new(&args.target),
        args.src.as_deref(),
        args.arch.as_deref(),
        args.as_namespace.as_deref(),
    )?;
    // `--dry-run` is the plan and nothing else — it must work on a box with
    // no credentials file, no PLY_TOKEN and no network, because that is
    // exactly when you want to see what a push WOULD send. So it prints
    // before `saved()` is ever consulted: the record as the manifest (and
    // `--as`) define it, with no owner filled in from a key the server would
    // derive the same value from anyway.
    if args.dry_run {
        println!("{}", serde_json::to_string_pretty(&plan.record)?);
        return Ok(());
    }
    let Some((token, login)) = saved() else {
        bail!("not logged in — run `ply login`, or set PLY_TOKEN (CI)");
    };
    // The namespace the bytes must land in: whatever the plan resolved —
    // a manifest `owner`, else `--as`. None means "the key's own login", which
    // is exactly the server's default, so the header is then omitted. Getting
    // this wrong uploads under one namespace and publishes under another, and
    // the artifact records as external.
    let declared_owner = plan.record.owner.clone();
    // Neither the manifest nor `--as` named an owner: it is the key's own
    // namespace (the server derives the same value from the key; naming it
    // here keeps the record self-contained, and the printed lines truthful).
    if plan.record.owner.is_none() && !login.is_empty() {
        plan.record.owner = Some(login.clone());
    }
    let Some(owner) = plan.record.owner.clone().filter(|o| !o.is_empty()) else {
        bail!(
            "no namespace yet — choose your username at {}/account/ (it becomes your namespace)",
            api_base()
        );
    };

    if plan.upload {
        upload_artifact(&mut plan, &token, declared_owner.as_deref(), &owner)?;
    }

    // The publish. `backfill` is the migration flag, and absent means false —
    // only the backfill script ever sends it.
    let payload = serde_json::to_value(&plan.record).context("encoding the record")?;
    let mut resp = ureq::post(format!("{}/api/publish/", api_base()))
        .config()
        .http_status_as_error(false)
        .build()
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(payload.to_string())
        .context("reaching the registry")?;
    let status = resp.status().as_u16();
    let body = json_body(&mut resp);
    if !(200..300).contains(&status) {
        let mut msg = format!(
            "publish refused ({status}): {}",
            body["error"].as_str().unwrap_or("no reason given")
        );
        // 409 carries the difference between what is on record and what you
        // sent — the only thing that makes it actionable.
        if let Some(diff) = body["diff"].as_str() {
            msg.push('\n');
            msg.push_str(diff);
        }
        bail!(msg);
    }

    let (name, version) = (&plan.record.name, &plan.record.version);
    println!("published {owner}/{name}@{version}");
    println!(
        "  {}/{owner}/{name}/{name}-{version}.toml",
        registry_base(&body, &owner, name, plan.upload)
    );
    if plan.record.kind == ply_core::catalog::PackageKind::App {
        println!("use:");
        match body["use"].as_str() {
            Some(snippet) => {
                for line in snippet.lines() {
                    println!("  {line}");
                }
            }
            None => println!("  ply run {owner}/{name}@{version}"),
        }
    }
    Ok(())
}

/// Send the bytes and let the response say where they landed. The sha256 is
/// computed here and sent with them: the registry verifies rather than
/// trusts, and an identical re-upload is idempotent.
fn upload_artifact(
    plan: &mut PushPlan,
    token: &str,
    namespace: Option<&str>,
    owner: &str,
) -> Result<()> {
    let image = plan
        .image
        .clone()
        .context("nothing to upload — a stack has no image")?;
    let filename = image
        .file_name()
        .and_then(|f| f.to_str())
        .context("image path has no filename")?
        .to_string();
    let bytes = std::fs::read(&image).with_context(|| format!("reading {}", image.display()))?;
    let artifact = plan
        .record
        .artifacts
        .first()
        .context("an image push plans exactly one artifact")?
        .clone();
    println!(
        "pushing {filename} ({}) as {owner}/…",
        crate::commands::build::human_size(bytes.len() as u64)
    );

    let mut request = ureq::post(format!("{}/api/upload/", api_base()))
        .config()
        .http_status_as_error(false)
        .build()
        .header("Authorization", &format!("Bearer {token}"))
        .header("X-Ply-Filename", &filename)
        .header("X-Ply-Sha256", &artifact.sha256)
        .header("Content-Type", "application/octet-stream");
    if let Some(ns) = namespace {
        request = request.header("X-Ply-Namespace", ns);
    }
    let mut resp = request
        .send(&bytes[..])
        .context("uploading to the registry")?;
    let status = resp.status().as_u16();
    let body = json_body(&mut resp);
    if !(200..300).contains(&status) {
        bail!(
            "upload refused ({status}): {}",
            body["error"].as_str().unwrap_or("no reason given")
        );
    }
    // Where the bytes now live, and whether the registry vouches for them.
    let artifact = &mut plan.record.artifacts[0];
    if let Some(src) = body["src"].as_str() {
        artifact.src = src.to_string();
    }
    if let Some(verified) = body["verified"].as_bool() {
        artifact.verified = verified;
    }
    Ok(())
}

/// A response body as JSON, or `null` when the server sent something else —
/// a status code is still worth reporting when the body is a proxy's HTML.
fn json_body(resp: &mut ureq::http::Response<ureq::Body>) -> serde_json::Value {
    resp.body_mut()
        .read_to_string()
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Where the registry serves files from — the `.toml` sits beside the bytes.
/// Read off the artifact THIS run uploaded (it landed under
/// `<origin>/{owner}/{name}/`), so a self-hosted registry prints its own URLs.
/// Without an upload the srcs on record are the publisher's own hosts — a
/// release asset URL says nothing about where the `.toml` lives — so a stack
/// and a `--src` push both fall back to the public registry.
fn registry_base(stored: &serde_json::Value, owner: &str, name: &str, uploaded: bool) -> String {
    const PUBLIC: &str = "https://registry.plybox.sh";
    if !uploaded {
        return PUBLIC.to_string();
    }
    let prefix = format!("/{owner}/{name}/");
    stored["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["src"].as_str())
        .find(|src| src.contains(&prefix))
        .and_then(|src| {
            let (scheme, rest) = src.split_once("://")?;
            Some(format!("{scheme}://{}", rest.split('/').next()?))
        })
        .unwrap_or_else(|| PUBLIC.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stack_file_plans_a_record_with_no_artifacts_and_no_upload() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("stack.toml");
        std::fs::write(
            &f,
            "[stack]\nname = \"todos\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"postgres@17\"\n",
        )
        .unwrap();
        let p = plan_push(&f, None, None, Some("iluxav")).unwrap();
        assert_eq!(p.record.kind, ply_core::catalog::PackageKind::Stack);
        assert_eq!(p.record.owner.as_deref(), Some("iluxav"));
        assert!(p.record.artifacts.is_empty() && p.image.is_none() && !p.upload);
    }

    #[test]
    fn a_stack_with_a_local_member_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("stack.toml");
        std::fs::write(
            &f,
            "[stack]\nname = \"t\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"./server\"\n",
        )
        .unwrap();
        let e = plan_push(&f, None, None, None).unwrap_err().to_string();
        assert!(e.contains("./server"), "{e}");
    }

    #[test]
    fn an_image_with_src_plans_an_unverified_external_artifact_and_no_upload() {
        let td = tempfile::tempdir().unwrap();
        let img = ply_core::image::squashfs::test_image_with_manifest(
            td.path(),
            "[package]\nname = \"a\"\nversion = \"1.0.0\"\nentrypoint = [\"x\"]\n",
        );
        let p = plan_push(
            &img,
            Some("https://h/a-{version}-linux-{arch}.img"),
            Some("x64"),
            None,
        )
        .unwrap();
        let a = &p.record.artifacts[0];
        assert_eq!(
            (a.arch.as_str(), a.src.as_str(), a.verified),
            ("x64", "https://h/a-1.0.0-linux-x64.img", false)
        );
        assert_eq!(
            a.sha256,
            ply_core::digest::sha256_file(&img)
                .unwrap()
                .trim_start_matches("sha256:")
        );
        // bare 64-hex on the wire: the registry hashes and validates that form
        assert!(
            a.sha256.len() == 64 && a.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "{}",
            a.sha256
        );
        assert!(!p.upload);
    }

    /// A canonically-named copy of a built test image, so `ImageName::parse`
    /// has something to read.
    fn named_image(td: &Path, filename: &str) -> PathBuf {
        let built = ply_core::image::squashfs::test_image_with_manifest(
            td,
            "[package]\nname = \"a\"\nversion = \"1.0.0\"\nentrypoint = [\"x\"]\n",
        );
        let named = td.join(filename);
        std::fs::rename(&built, &named).unwrap();
        named
    }

    #[test]
    fn an_existing_img_takes_its_arch_from_its_filename() {
        let td = tempfile::tempdir().unwrap();
        let img = named_image(td.path(), "a-1.0.0-linux-arm64.img");
        // No --arch: the FILE says arm64, whatever this host is. Stamping
        // the host arch here publishes arm64 bytes as x64 — an image that
        // downloads, verifies, and cannot run.
        let p = plan_push(&img, None, None, None).unwrap();
        assert_eq!(p.record.artifacts[0].arch, "arm64");
        // --src templates expand with the file's arch too
        let p = plan_push(
            &img,
            Some("https://h/a-{version}-linux-{arch}.img"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            p.record.artifacts[0].src,
            "https://h/a-1.0.0-linux-arm64.img"
        );
        // --arch agreeing is fine; --arch contradicting names both and stops
        assert_eq!(
            plan_push(&img, None, Some("arm64"), None)
                .unwrap()
                .record
                .artifacts[0]
                .arch,
            "arm64"
        );
        let e = plan_push(&img, None, Some("x64"), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("arm64") && e.contains("x64"), "{e}");
    }

    #[test]
    fn a_non_canonical_img_name_still_honors_arch() {
        // `test.img` claims no arch, so --arch (else the host) decides.
        let td = tempfile::tempdir().unwrap();
        let img = ply_core::image::squashfs::test_image_with_manifest(
            td.path(),
            "[package]\nname = \"a\"\nversion = \"1.0.0\"\nentrypoint = [\"x\"]\n",
        );
        let p = plan_push(&img, None, Some("arm64"), None).unwrap();
        assert_eq!(p.record.artifacts[0].arch, "arm64");
    }

    #[test]
    fn as_conflicts_with_a_manifest_owner() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("stack.toml");
        std::fs::write(&f, "[stack]\nname = \"t\"\nowner = \"ply\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"postgres@17\"\n").unwrap();
        assert!(plan_push(&f, None, None, Some("iluxav"))
            .unwrap_err()
            .to_string()
            .contains("--as"));
    }
}
