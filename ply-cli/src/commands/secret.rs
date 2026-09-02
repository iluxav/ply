//! `ply secret ls|set` — a thin CLI over [`SecretStore`] (Task 3). Nothing
//! here re-implements the store: `ls`/`set` below are one-line wrappers so
//! tests can drive them without clap, and the `exec_*` functions parse args
//! and print. Secret values never appear in output, logs, or error strings.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use ply_core::secrets::SecretStore;

use crate::cli::{SecretLsArgs, SecretSetArgs};

/// Pick the store the `-C DIR` / `--deployments STACK` selector names.
/// Clap's `conflicts_with` keeps them mutually exclusive; `deployments`
/// wins here only because `dir` always carries its `.` default.
fn select_store(dir: &std::path::Path, deployments: Option<&str>) -> SecretStore {
    match deployments {
        Some(stack) => SecretStore::for_deployments(stack),
        None => SecretStore::for_stack(dir),
    }
}

/// The directory a store keeps its files in. `SecretStore` has no `dir`
/// getter (Task 3 didn't need one), but `path()` always joins exactly one
/// component onto it, so its parent is the store's root regardless of which
/// member/param we ask about.
fn store_dir(store: &SecretStore) -> PathBuf {
    store
        .path("x", "x")
        .parent()
        .expect("path() joins one component onto the store dir")
        .to_path_buf()
}

/// Split `NAME` into `(member, param)`, requiring exactly one dot and both
/// halves to match the identifier shape Task 1's template engine accepts
/// (`[A-Za-z0-9_-]+` — mirrors `parse_ref`'s `ident` closure in
/// `ply-core/src/params.rs`).
fn parse_name(name: &str) -> Result<(String, String)> {
    let ident = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    match name.split_once('.') {
        Some((member, param)) if ident(member) && ident(param) => {
            Ok((member.to_string(), param.to_string()))
        }
        _ => bail!("invalid secret name `{name}` — expected MEMBER.PARAM, e.g. db.password"),
    }
}

/// List secret names, sorted, never values.
fn ls(store: &SecretStore) -> Result<Vec<String>> {
    Ok(store.list()?)
}

/// Validate `name`, write `value`, and return the path written.
fn set(store: &SecretStore, name: &str, value: &str) -> Result<PathBuf> {
    let (member, param) = parse_name(name)?;
    store.set(&member, &param, value)?;
    Ok(store.path(&member, &param))
}

/// Trim exactly one trailing newline — `"\n"` or `"\r\n"` — and nothing
/// else, so a secret with meaningful internal or leading whitespace round-trips.
fn trim_trailing_newline(s: &str) -> &str {
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.strip_suffix('\r').unwrap_or(s)
}

/// Read one line for `name`'s value from stdin. Prompts to stderr first when
/// stdin is a terminal (no hidden input — that needs a dependency this task
/// doesn't add); refuses an empty result so a stray Enter can't wipe a
/// secret to "".
fn read_value_from_stdin(name: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        eprint!("value for {name} (input hidden is not supported; use a pipe for scripts): ");
        let _ = std::io::stderr().flush();
    }
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading secret value from stdin")?;
    let value = trim_trailing_newline(&line);
    if value.is_empty() {
        bail!("refusing to store an empty secret");
    }
    Ok(value.to_string())
}

pub fn exec_ls(args: &SecretLsArgs) -> Result<()> {
    let store = select_store(&args.dir, args.deployments.as_deref());
    let names = ls(&store)?;
    if names.is_empty() {
        eprintln!("no secrets under {}", store_dir(&store).display());
        return Ok(());
    }
    for name in names {
        println!("{name}");
    }
    Ok(())
}

pub fn exec_set(args: &SecretSetArgs) -> Result<()> {
    let store = select_store(&args.dir, args.deployments.as_deref());
    let value = match &args.value {
        Some(v) => v.clone(),
        None => read_value_from_stdin(&args.name)?,
    };
    let path = set(&store, &args.name, &value)?;
    println!("wrote {} (0600)", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn member_dot_param_is_the_only_shape_accepted() {
        assert_eq!(
            parse_name("db.password").unwrap(),
            ("db".to_string(), "password".to_string())
        );
        let no_dot = parse_name("db").unwrap_err().to_string();
        assert!(
            no_dot.contains("expected MEMBER.PARAM, e.g. db.password"),
            "{no_dot}"
        );
        let two_dots = parse_name("a.b.c").unwrap_err().to_string();
        assert!(
            two_dots.contains("expected MEMBER.PARAM, e.g. db.password"),
            "{two_dots}"
        );
    }

    #[test]
    fn empty_halves_and_bad_characters_are_rejected() {
        assert!(parse_name(".password").is_err(), "empty member");
        assert!(parse_name("db.").is_err(), "empty param");
        assert!(parse_name("").is_err());
        assert!(
            parse_name("db pw.password").is_err(),
            "space not in the ident shape"
        );
        assert!(parse_name("db.pass word").is_err());
        // the ident shape ply-core/src/params.rs accepts: letters, digits, _, -
        assert_eq!(
            parse_name("my-db.api_key").unwrap(),
            ("my-db".to_string(), "api_key".to_string())
        );
    }

    #[test]
    fn set_then_ls_round_trips_and_the_file_is_0600() {
        let td = tempfile::tempdir().unwrap();
        let store = SecretStore::for_stack(td.path());

        let path = set(&store, "db.password", "hunter2").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hunter2\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        assert_eq!(ls(&store).unwrap(), vec!["db.password".to_string()]);
    }

    #[test]
    fn set_rejects_a_malformed_name_without_touching_the_store() {
        let td = tempfile::tempdir().unwrap();
        let store = SecretStore::for_stack(td.path());
        assert!(set(&store, "db", "hunter2").is_err());
        assert!(ls(&store).unwrap().is_empty());
    }

    #[test]
    fn ls_on_an_empty_store_is_empty_not_an_error() {
        let td = tempfile::tempdir().unwrap();
        let store = SecretStore::for_stack(td.path());
        assert_eq!(ls(&store).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn store_dir_recovers_the_stacks_secrets_directory() {
        let td = tempfile::tempdir().unwrap();
        let store = SecretStore::for_stack(td.path());
        assert_eq!(store_dir(&store), td.path().join(".ply").join("secrets"));
    }

    #[test]
    fn stdin_trim_takes_exactly_one_trailing_newline_and_keeps_internal_whitespace() {
        assert_eq!(trim_trailing_newline("hunter2\n"), "hunter2");
        assert_eq!(trim_trailing_newline("hunter2\r\n"), "hunter2");
        assert_eq!(
            trim_trailing_newline("hunter2"),
            "hunter2",
            "EOF without a newline"
        );
        assert_eq!(
            trim_trailing_newline("  spaced out value  \n"),
            "  spaced out value  ",
            "only the trailing newline goes, not surrounding whitespace"
        );
    }
}
