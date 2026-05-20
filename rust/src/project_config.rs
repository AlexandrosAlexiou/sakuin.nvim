//! Per-project config overlay read from `<project_root>/sakuin.toml`.
//!
//! The Neovim `setup()` config is the user's global default. A `sakuin.toml`
//! checked into a repo overrides whichever indexing fields it sets; unset
//! fields fall back to the Lua config, then built-in defaults. A missing or
//! invalid file is a warning, never a hard failure — a typo in the project
//! file must not stop indexing.

use std::path::Path;

use serde::Deserialize;

use crate::types::SakuinConfig;

pub const PROJECT_CONFIG_FILE: &str = "sakuin.toml";

/// Every field is optional so "absent in the TOML" is distinguishable from
/// "set to an empty value" — only `Some` values overlay the base config.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    max_file_size: Option<u64>,
    ignore_patterns: Option<Vec<String>>,
    include_patterns: Option<Vec<String>>,
    include_extensions: Option<Vec<String>>,
    respect_gitignore: Option<bool>,
}

impl ProjectConfig {
    fn overlay(self, config: &mut SakuinConfig) {
        if let Some(v) = self.max_file_size {
            config.max_file_size = v;
        }
        if let Some(v) = self.ignore_patterns {
            config.ignore_patterns = v;
        }
        if self.include_patterns.is_some() {
            config.include_patterns = self.include_patterns;
        }
        if self.include_extensions.is_some() {
            config.include_extensions = self.include_extensions;
        }
        if let Some(v) = self.respect_gitignore {
            config.respect_gitignore = v;
        }
    }
}

/// Overlay `<project_root>/sakuin.toml` onto `config` if the file exists.
pub fn apply(project_root: &Path, config: &mut SakuinConfig) {
    let path = project_root.join(PROJECT_CONFIG_FILE);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            log::warn!("Failed to read {:?}: {} — using Neovim config", path, e);
            return;
        }
    };

    match toml::from_str::<ProjectConfig>(&contents) {
        Ok(project) => {
            project.overlay(config);
            log::info!("Applied project config from {:?}", path);
        }
        Err(e) => {
            log::warn!("Failed to parse {:?}: {} — using Neovim config", path, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SakuinConfig {
        SakuinConfig::default()
    }

    #[test]
    fn absent_fields_leave_base_untouched() {
        let mut cfg = base();
        let original = cfg.ignore_patterns.clone();
        let project: ProjectConfig = toml::from_str("max_file_size = 42").unwrap();
        project.overlay(&mut cfg);
        assert_eq!(cfg.max_file_size, 42);
        assert_eq!(cfg.ignore_patterns, original);
    }

    #[test]
    fn arrays_replace_rather_than_merge() {
        let mut cfg = base();
        let project: ProjectConfig =
            toml::from_str(r#"ignore_patterns = ["build", "vendor"]"#).unwrap();
        project.overlay(&mut cfg);
        assert_eq!(cfg.ignore_patterns, vec!["build", "vendor"]);
    }

    #[test]
    fn include_extensions_can_be_set() {
        let mut cfg = base();
        assert!(cfg.include_extensions.is_none());
        let project: ProjectConfig =
            toml::from_str(r#"include_extensions = ["go", "lua"]"#).unwrap();
        project.overlay(&mut cfg);
        assert_eq!(
            cfg.include_extensions,
            Some(vec!["go".into(), "lua".into()])
        );
    }

    #[test]
    fn include_patterns_can_be_set() {
        let mut cfg = base();
        assert!(cfg.include_patterns.is_none());
        let project: ProjectConfig =
            toml::from_str(r#"include_patterns = ["*", "src/**"]"#).unwrap();
        project.overlay(&mut cfg);
        assert_eq!(
            cfg.include_patterns,
            Some(vec!["*".into(), "src/**".into()])
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<ProjectConfig>("log_file = \"/tmp/x\"").is_err());
    }

    #[test]
    fn apply_is_noop_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = base();
        let before = cfg.ignore_patterns.clone();
        apply(dir.path(), &mut cfg);
        assert_eq!(cfg.ignore_patterns, before);
    }

    #[test]
    fn apply_overlays_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(PROJECT_CONFIG_FILE),
            "respect_gitignore = false\nignore_patterns = [\"foo\"]\n",
        )
        .unwrap();
        let mut cfg = base();
        apply(dir.path(), &mut cfg);
        assert!(!cfg.respect_gitignore);
        assert_eq!(cfg.ignore_patterns, vec!["foo"]);
    }
}
