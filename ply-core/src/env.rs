//! Env composition: package fragments (topological order) → manifest [env]
//! → CLI overrides. Last wins.

use std::collections::BTreeMap;

use crate::manifest::Layer;

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// `layers` in overlay order (dependents first, base last) — PATH entries of
/// dependents take precedence over their dependencies'.
pub fn compose_env(
    layers: &[&Layer],
    manifest_env: &BTreeMap<String, String>,
    cli_env: &[(String, String)],
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    let path_entries: Vec<&str> = layers
        .iter()
        .flat_map(|l| l.path.iter().map(String::as_str))
        .chain(std::iter::once(DEFAULT_PATH))
        .collect();
    env.insert("PATH".to_string(), path_entries.join(":"));

    let ld_entries: Vec<&str> = layers
        .iter()
        .flat_map(|l| l.ld_library_path.iter().map(String::as_str))
        .collect();
    if !ld_entries.is_empty() {
        env.insert("LD_LIBRARY_PATH".to_string(), ld_entries.join(":"));
    }

    for (k, v) in manifest_env {
        env.insert(k.clone(), v.clone());
    }
    for (k, v) in cli_env {
        env.insert(k.clone(), v.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composition_order() {
        let node = Layer {
            path: vec!["/opt/node-22.0.0/bin".into()],
            ld_library_path: vec![],
        };
        let ffmpeg = Layer {
            path: vec!["/opt/ffmpeg-6.1.0/bin".into()],
            ld_library_path: vec!["/opt/ffmpeg-6.1.0/lib".into()],
        };
        let mut manifest_env = BTreeMap::new();
        manifest_env.insert("NODE_ENV".to_string(), "production".to_string());
        let cli = vec![("NODE_ENV".to_string(), "debug".to_string())];

        let env = compose_env(&[&ffmpeg, &node], &manifest_env, &cli);
        assert!(env["PATH"].starts_with("/opt/ffmpeg-6.1.0/bin:/opt/node-22.0.0/bin:"));
        assert!(env["PATH"].ends_with(DEFAULT_PATH));
        assert_eq!(env["LD_LIBRARY_PATH"], "/opt/ffmpeg-6.1.0/lib");
        assert_eq!(env["NODE_ENV"], "debug", "CLI wins over manifest");
    }
}
