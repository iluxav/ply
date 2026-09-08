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

    // A package's own variables, base first so that a dependent's setting
    // of the same name wins — the same precedence PATH has above.
    for layer in layers.iter().rev() {
        for (k, v) in &layer.env {
            env.insert(k.clone(), v.clone());
        }
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
            env: BTreeMap::new(),
        };
        let ffmpeg = Layer {
            path: vec!["/opt/ffmpeg-6.1.0/bin".into()],
            ld_library_path: vec!["/opt/ffmpeg-6.1.0/lib".into()],
            env: BTreeMap::new(),
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

    #[test]
    fn a_packages_own_variables_come_before_the_manifests_and_dependents_win() {
        let ruby = Layer {
            path: vec!["/opt/ruby-3.3.8/usr/bin".into()],
            ld_library_path: vec![],
            env: [(
                "RUBYLIB".to_string(),
                "/opt/ruby-3.3.8/usr/lib/ruby/3.3.0".to_string(),
            )]
            .into_iter()
            .collect(),
        };
        let gem = Layer {
            path: vec![],
            ld_library_path: vec![],
            env: [
                ("RUBYLIB".to_string(), "/opt/gem-1.0.0/lib".to_string()),
                ("GEM_HOME".to_string(), "/opt/gem-1.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        // Overlay order: the dependent (gem) first, its dependency (ruby) last.
        let env = compose_env(&[&gem, &ruby], &BTreeMap::new(), &[]);
        assert_eq!(env["RUBYLIB"], "/opt/gem-1.0.0/lib", "the dependent's wins");
        assert_eq!(env["GEM_HOME"], "/opt/gem-1.0.0");
        // …and the app's own [env] beats every package.
        let mut manifest_env = BTreeMap::new();
        manifest_env.insert("GEM_HOME".to_string(), "/srv/gems".to_string());
        let env = compose_env(&[&gem, &ruby], &manifest_env, &[]);
        assert_eq!(env["GEM_HOME"], "/srv/gems");
    }
}
