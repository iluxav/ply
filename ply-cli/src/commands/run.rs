use anyhow::{bail, Result};
use ply_core::runtime::run::{parse_env_file, run, RunOptions};

use crate::cli::RunArgs;

/// A domain lands verbatim in proxy config — refuse anything that could
/// smuggle config syntax. Hostname labels, dots, one leading wildcard.
pub fn validate_domain(domain: &str) -> Result<()> {
    let host = domain.strip_prefix("*.").unwrap_or(domain);
    let ok = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    if !ok {
        bail!("--domain `{domain}`: not a valid hostname (labels of [a-z0-9-], dots between, optional leading `*.`)");
    }
    Ok(())
}

/// Parse `-e` pairs into launch-ready `(KEY, VALUE)` env for the child.
///
/// `KEY=VALUE` sets the value outright, unchanged. A bare `KEY` (no `=`)
/// inherits the CALLER's own value for that name — the escape hatch a
/// secret-tainted `ply up` spawn uses to hand a child a value without ever
/// writing it into argv (and so `/proc/*/cmdline`): the parent sets it on
/// the child's environment via `Command::env`, then passes only the bare
/// name here. A bare name absent from the caller's own environment is a
/// hard error — nothing to inherit, and silently dropping it would start
/// the child with an unresolved hole instead of the value it was promised.
fn parse_cli_env(pairs: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for pair in pairs {
        match pair.split_once('=') {
            Some((k, v)) => out.push((k.to_string(), v.to_string())),
            None => match std::env::var(pair) {
                Ok(v) => out.push((pair.clone(), v)),
                Err(_) => bail!(
                    "-e {pair}: not set in the environment (bare -e KEY inherits the caller's value)"
                ),
            },
        }
    }
    Ok(out)
}

pub fn exec(args: RunArgs) -> Result<()> {
    for domain in &args.domain {
        validate_domain(domain)?;
    }
    let mut links: Vec<(std::path::PathBuf, String)> = Vec::new();
    for link in &args.link {
        let Some((host, container)) = link.split_once(':') else {
            bail!("--link `{link}`: expected HOST:CONTAINER, e.g. ./src:/opt/myapp");
        };
        if !container.starts_with('/') {
            bail!("--link `{link}`: container path must be absolute");
        }
        links.push((host.into(), container.to_string()));
    }

    let mut cli_env: Vec<(String, String)> = Vec::new();
    if let Some(file) = &args.env_file {
        cli_env.extend(parse_env_file(file)?);
    }
    cli_env.extend(parse_cli_env(&args.env)?);

    let publish = args
        .publish
        .iter()
        .map(|s| ply_core::runtime::publish::parse_publish(s))
        .collect::<Result<Vec<_>, _>>()?;
    // Two listeners on one host port is a config error, not a race to lose
    // at bind time with a confusing "address in use".
    for (i, spec) in publish.iter().enumerate() {
        if let Some(dup) = publish[..i].iter().find(|p| p.host_port == spec.host_port) {
            bail!(
                "--publish {}: host port {} is already published by an earlier --publish",
                spec.host_port,
                dup.host_port
            );
        }
    }

    // Four run forms besides a plain .img path:
    //  - an http(s) URL of an .img: fetched into the store, once per URL
    //  - a directory with ply.toml: build it, run the result (cargo run)
    //  - a bare name (`postgres`, `myapp@1.2`): resolve against the registry,
    //    newest matching version, fetched into the store
    //  - docker://: OCI import (below)
    // An existing file always wins over a name lookup.
    let mut dev_entrypoint: Option<Vec<String>> = None;
    let image_path = std::path::Path::new(&args.image);
    let image_arg = if args.image.starts_with("http://") || args.image.starts_with("https://") {
        let (path, resolved) = ply_core::source::fetch_url_image(&args.image)?;
        eprintln!("ply: {resolved} (url)");
        path.to_string_lossy().into_owned()
    } else if !args.image.starts_with("docker://") && image_path.is_dir() {
        let dir = image_path.to_path_buf();
        if ply_core::stack::load(&dir)?.is_some() {
            bail!(
                "{} is a stack (it has [[app]]) — `ply up` starts stacks; `ply run` runs one app",
                dir.join("ply.toml").display()
            );
        }
        if !dir.join("ply.toml").exists() {
            bail!(
                "{} has no ply.toml — `ply run DIR` builds and runs an app directory",
                dir.display()
            );
        }
        let image = match ply_core::build::up_to_date_image(&dir, None)? {
            Some(image) => {
                eprintln!(
                    "ply: {} up to date",
                    image.file_name().unwrap_or_default().to_string_lossy()
                );
                image
            }
            None => {
                let outcome = ply_core::build::build(&ply_core::build::BuildOptions {
                    dir: dir.clone(),
                    output: None,
                    allow_insecure: false,
                    arch: None,
                    // CD lanes are non-interactive: a repo that carries a .env
                    // must fail loudly, never ship it.
                    allow_secrets: false,
                })?;
                eprintln!("ply: built {}", outcome.image_name);
                outcome.image_path
            }
        };
        // The dev overlay applies only on the DIR form: it belongs to the
        // working tree, not the artifact — plain image runs stay pristine.
        let app = ply_core::image::read::read_manifest(&image)?.package.name;
        if let Some(overlay) = ply_core::dev::load(&dir, &app)? {
            eprintln!("ply: applying ply.dev.toml ({})", overlay.describe());
            dev_entrypoint = overlay.entrypoint;
            // before the explicit -e flags, so those still win
            let mut merged = overlay.env;
            merged.append(&mut cli_env);
            cli_env = merged;
            links.extend(overlay.links);
        }
        image.to_string_lossy().into_owned()
    } else if !args.image.starts_with("docker://") && !image_path.exists() {
        match ply_core::catalog::parse_namespaced_ref(&args.image) {
            Some((name, want)) => {
                match ply_core::catalog::fetch_app_image(&name, want.as_deref(), &args.source) {
                    Ok((path, resolved, _digest)) => {
                        eprintln!("ply: resolved {} -> {resolved}", args.image);
                        path.to_string_lossy().into_owned()
                    }
                    // Only once the app lookup has failed is it worth a
                    // second round trip to ask whether it's a stack.
                    Err(e) => {
                        if args.image.contains('/')
                            && ply_core::catalog::fetch_stack(&args.image, &args.source).is_ok()
                        {
                            bail!("{0} is a stack — run it with `ply up {0}`", args.image);
                        }
                        return Err(e.into());
                    }
                }
            }
            None => {
                // A `namespace/name` ref that resolves to a stack → `ply up`.
                if args.image.contains('/')
                    && !args.image.contains("://")
                    && ply_core::catalog::fetch_stack(&args.image, &args.source).is_ok()
                {
                    bail!("{0} is a stack — run it with `ply up {0}`", args.image);
                }
                args.image.clone()
            }
        }
    } else {
        args.image.clone()
    };

    // `docker://…` becomes a local image before anything else looks at it, so
    // every downstream path (manifest read, lockfile, deploy) sees a plain file.
    let image = ply_core::oci::ensure_local(&image_arg, args.pull)?;

    // [requests] links: the image asks, the operator answers. --grant-links
    // mounts them; otherwise they are listed and NOT mounted — never silent
    // host access for image authors.
    if let Some(requests) = ply_core::image::read::read_manifest(&image)?.requests {
        if !requests.links.is_empty() {
            if args.grant_links {
                for spec in &requests.links {
                    let (host, container) = spec.split_once(':').expect("validated at build");
                    links.push((host.into(), container.to_string()));
                }
                eprintln!(
                    "ply: granted {} requested link(s): {}",
                    requests.links.len(),
                    requests.links.join(", ")
                );
            } else {
                eprintln!(
                    "ply: image requests host links (NOT granted — --grant-links mounts them):"
                );
                for spec in &requests.links {
                    eprintln!("ply:   {spec}");
                }
            }
        }
    }

    let code = run(&RunOptions {
        image,
        name: args.name.clone(),
        cli_env,
        allow_insecure: true, // lockfile digests pin content; run never resolves
        scale: args.scale,
        links,
        publish,
        netns: args.netns.clone(),
        netns_peers: args.netns_peer.clone(),
        netns_dns: args.netns_dns.clone(),
        after: args.after.clone(),
        after_timeout: ply_core::manifest::parse_duration(&args.after_timeout)
            .map_err(|e| anyhow::anyhow!("--after-timeout: {e}"))?,
        privileged: args.privileged,
        entrypoint: dev_entrypoint,
        domains: args.domain.clone(),
        volumes: args.volume.clone(),
    })?;
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::{parse_cli_env, validate_domain};

    // --- env parsing --------------------------------------------------------

    #[test]
    fn key_value_pair_is_set_outright() {
        let out = parse_cli_env(&["FOO=bar".to_string()]).unwrap();
        assert_eq!(out, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn bare_key_inherits_the_callers_value() {
        // SAFETY: test-only, single-threaded within this test's scope.
        unsafe { std::env::set_var("PLY_TEST_BARE_ENV_VAR", "inherited") };
        let out = parse_cli_env(&["PLY_TEST_BARE_ENV_VAR".to_string()]).unwrap();
        // SAFETY: test-only cleanup, same rationale as above.
        unsafe { std::env::remove_var("PLY_TEST_BARE_ENV_VAR") };
        assert_eq!(
            out,
            vec![("PLY_TEST_BARE_ENV_VAR".to_string(), "inherited".to_string())]
        );
    }

    #[test]
    fn bare_key_missing_from_the_environment_is_an_error() {
        // Guard against test pollution: this name should never be set.
        // SAFETY: test-only.
        unsafe { std::env::remove_var("PLY_TEST_DEFINITELY_UNSET_VAR") };
        let e = parse_cli_env(&["PLY_TEST_DEFINITELY_UNSET_VAR".to_string()])
            .unwrap_err()
            .to_string();
        assert!(e.contains("not set in the environment"), "{e}");
        assert!(e.contains("bare -e KEY inherits the caller's value"), "{e}");
    }

    #[test]
    fn domain_grammar() {
        for good in [
            "api.example.com",
            "example.com",
            "*.example.com",
            "a-b.x-1.io",
            "localhost",
        ] {
            assert!(validate_domain(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "-x.com",
            "x-.com",
            "a b.com",
            "a{b}.com",
            "*.",
            "a..b",
            "*.*.x.com",
        ] {
            assert!(validate_domain(bad).is_err(), "{bad}");
        }
    }
}
