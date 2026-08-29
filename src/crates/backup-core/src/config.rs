use crate::platform::Platform;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Zip,
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "dir")]
    Dir,
}

impl Default for Format {
    fn default() -> Self {
        Format::Zip
    }
}

impl Format {
    pub fn extension(&self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::TarGz => "tar.gz",
            Format::Dir => "dir",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupMode {
    AfterEach,
    AtEnd,
}

impl Default for CleanupMode {
    fn default() -> Self {
        CleanupMode::AfterEach
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSettings {
    /// Base backup directory. Supports `~/`, `$HOME`, `%APPDATA%` etc.
    pub dest: String,
    #[serde(default)]
    pub format: Format,
    #[serde(default = "default_parallel")]
    pub parallel: usize,
    /// 0 = keep forever.
    #[serde(default = "default_retention")]
    pub retention: usize,
    #[serde(default)]
    pub cleanup: CleanupMode,
    #[serde(default = "default_checksum")]
    pub checksum: bool,
    #[serde(default)]
    pub excludes: Vec<String>,
}

fn default_parallel() -> usize {
    2
}
fn default_retention() -> usize {
    10
}
fn default_checksum() -> bool {
    true
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            dest: "~/backups".into(),
            format: Format::Zip,
            parallel: default_parallel(),
            retention: default_retention(),
            cleanup: CleanupMode::AfterEach,
            checksum: default_checksum(),
            excludes: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct App {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Raw path strings per platform key ("windows"|"linux"|"macos").
    #[serde(default)]
    pub paths: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default = "default_true")]
    pub compress: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub backup: BackupSettings,
    #[serde(default)]
    pub apps: Vec<App>,
    /// Absolute path of the file this config was loaded from (not serialized).
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

fn default_version() -> u32 {
    1
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}
/// Generate a stable app id from a display name.
///
/// Rules:
/// - trim and lowercase the name;
/// - every run of characters outside `[a-z0-9]` collapses to a single `-`;
/// - leading/trailing `-` are removed;
/// - an empty result falls back to `"app"`;
/// - the result is capped at 64 characters, then checked against
///   `existing_ids`, appending `-2`, `-3`, ... until unique.
///
/// Callers must not call this again when editing an existing app: pass the
/// app's existing id through unchanged so it stays stable.
pub fn generate_app_id(name: &str, existing_ids: &[&str]) -> String {
    let mut collapsed = String::new();
    let mut pending_dash = false;
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash {
                collapsed.push('-');
                pending_dash = false;
            }
            collapsed.push(c);
        } else {
            pending_dash = true;
        }
    }

    let candidate = collapsed.trim_matches('-');
    let base: String = if candidate.is_empty() {
        "app".to_string()
    } else {
        candidate.chars().take(64).collect()
    };

    let mut id = base.clone();
    let mut suffix = 2u32;
    while existing_ids.iter().any(|existing| *existing == id) {
        let suffix_text = format!("-{suffix}");
        let keep = 64usize.saturating_sub(suffix_text.len());
        let truncated: String = base.chars().take(keep).collect();
        id = format!("{truncated}{suffix_text}");
        suffix += 1;
    }
    id
}

fn absolute_of(path: &PathBuf) -> PathBuf {
    if path.is_absolute() {
        path.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

impl Config {
    /// Path strings for the given platform (unknown keys ignored).
    pub fn paths_for<'a>(&self, app: &'a App, platform: Platform) -> Vec<&'a String> {
        app.paths
            .get(platform.as_str())
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    /// Resolve the backup destination for the current platform.
    pub fn resolved_dest(&self) -> Result<PathBuf, ConfigError> {
        let base = self.config_dir();
        let state =
            crate::pathres::expand_path(&self.backup.dest, Platform::current(), base.as_deref());
        match state {
            crate::pathres::PathState::Resolved { path } => Ok(path),
            crate::pathres::PathState::UndefinedVar { var } => Err(ConfigError::DestVar { var }),
            crate::pathres::PathState::OtherPlatform { raw } => {
                Err(ConfigError::DestSyntax { raw })
            }
        }
    }

    /// Directory of the file this config was loaded from, if any.
    pub fn config_dir(&self) -> Option<PathBuf> {
        self.source
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    /// Validate and add/update an app. Only paths stored under the given
    /// platform key are read; paths for other platforms are preserved.
    /// Returns a user-facing error message on invalid input.
    pub fn upsert_app(&mut self, app: App, platform: Platform) -> Result<(), String> {
        let id = app.id.trim().to_string();
        if id.is_empty() {
            return Err("ID 不能为空".into());
        }
        if !valid_id(&id) {
            return Err("ID 只能包含小写字母、数字、- 和 _".into());
        }
        let name = app.name.trim().to_string();
        if name.is_empty() {
            return Err("名称不能为空".into());
        }
        let paths: Vec<String> = app
            .paths
            .get(platform.as_str())
            .map(|platform_paths| {
                platform_paths
                    .iter()
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if paths.is_empty() {
            return Err("至少需要填写一个备份路径".into());
        }
        let excludes: Vec<String> = app
            .excludes
            .iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();

        let mut normalized = app;
        normalized.id = id.clone();
        normalized.name = name.clone();
        normalized.excludes = excludes.clone();

        for a in self.apps.iter_mut() {
            if a.id == id {
                a.id = id;
                a.name = name;
                a.enabled = normalized.enabled;
                a.compress = normalized.compress;
                a.excludes = excludes;
                a.paths.insert(platform.as_str().to_string(), paths);
                return Ok(());
            }
        }
        normalized.paths = {
            let mut map = HashMap::new();
            map.insert(platform.as_str().to_string(), paths);
            map
        };
        self.apps.push(normalized);
        Ok(())
    }

    /// Remove an app by id. Returns true if an app was removed.
    pub fn remove_app(&mut self, id: &str) -> bool {
        let before = self.apps.len();
        self.apps.retain(|a| a.id != id);
        self.apps.len() != before
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid config JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("backup dest variable not defined: {var}")]
    DestVar { var: String },
    #[error("backup dest uses foreign-platform syntax: {raw}")]
    DestSyntax { raw: String },
    #[error("no apps defined")]
    NoApps,
}

/// Default config file location.
pub fn default_config_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        dirs::config_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
            .join("backup_tool")
            .join("config.json")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".backup_tool")
            .join("config.json")
    }
}

/// Load a config from `path`. `path` may be None → use default location.
/// Returns the config plus a list of non-fatal warnings.
pub fn load_config(path: Option<&std::path::Path>) -> Result<(Config, Vec<String>), ConfigError> {
    let path = path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_config_path);
    let text = std::fs::read_to_string(&path)?;
    let mut config: Config = serde_json::from_str(&text)?;
    config.source = Some(absolute_of(&path));

    let mut warnings = Vec::new();

    if config.apps.is_empty() {
        warnings.push("no apps defined in config".into());
    }
    for app in &config.apps {
        if app.id.trim().is_empty() {
            warnings.push(format!("app '{}' has empty id", app.name));
        }
        for (key, paths) in &app.paths {
            if !matches!(key.as_str(), "windows" | "linux" | "macos") {
                warnings.push(format!("app '{}': unknown platform key '{}'", app.id, key));
            }
            for p in paths {
                if p.trim().is_empty() {
                    warnings.push(format!("app '{}': empty path under '{}'", app.id, key));
                }
            }
        }
    }

    // Validate that the dest resolves now, so errors surface at load time.
    config.resolved_dest()?;

    Ok((config, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample_config() {
        let json = r#"{
            "version": 1,
            "backup": {
                "dest": "~/backups",
                "format": "zip",
                "parallel": 2,
                "retention": 10,
                "checksum": true,
                "excludes": ["**/.DS_Store"]
            },
            "apps": [
                {
                    "id": "vscode",
                    "name": "VSCode",
                    "enabled": true,
                    "paths": {
                        "windows": ["%APPDATA%\\Code\\User"],
                        "linux": ["$HOME/.config/Code/User"]
                    },
                    "excludes": ["Cache/**"]
                }
            ]
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.backup.format, Format::Zip);
        assert_eq!(config.backup.parallel, 2);
        assert_eq!(config.apps.len(), 1);
        let app = &config.apps[0];
        let linux_paths = config.paths_for(app, Platform::Linux);
        assert_eq!(linux_paths.len(), 1);
        assert_eq!(linux_paths[0], "$HOME/.config/Code/User");
    }

    #[test]
    fn defaults_apply() {
        let config: Config = serde_json::from_str(r#"{"apps": []}"#).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.backup.format, Format::Zip);
        assert!(config.backup.checksum);
        assert_eq!(config.backup.parallel, 2);
    }

    #[test]
    fn rejects_bad_dest_syntax() {
        let mut config: Config = serde_json::from_str(r#"{"apps": []}"#).unwrap();
        config.backup.dest = "$HOME/x".into();
        if cfg!(target_os = "windows") {
            assert!(matches!(
                config.resolved_dest(),
                Err(ConfigError::DestSyntax { .. })
            ));
        } else {
            assert!(matches!(config.resolved_dest(), Ok(_)));
        }
    }

    #[test]
    fn generate_app_id_basic_rules() {
        assert_eq!(generate_app_id("  My App  ", &[]), "my-app");
        assert_eq!(generate_app_id("My___App", &[]), "my-app");
        assert_eq!(generate_app_id("Hello, World!", &[]), "hello-world");
        assert_eq!(generate_app_id("VSCode", &[]), "vscode");
        assert_eq!(generate_app_id("已存在 App", &[]), "app");
    }

    #[test]
    fn generate_app_id_empty_falls_back() {
        assert_eq!(generate_app_id("", &[]), "app");
        assert_eq!(generate_app_id("   ", &[]), "app");
        assert_eq!(generate_app_id("!!!", &[]), "app");
        assert_eq!(generate_app_id("中文名", &[]), "app");
    }

    #[test]
    fn generate_app_id_avoids_conflicts() {
        let ids = ["app", "app-2", "my-app"];
        assert_eq!(generate_app_id("App", &ids), "app-3");
        assert_eq!(generate_app_id("My App", &ids), "my-app-2");
        assert_eq!(generate_app_id("Brand New", &ids), "brand-new");
    }

    #[test]
    fn generate_app_id_truncates_then_resolves_conflicts() {
        let long_name = "a".repeat(80);
        let id = generate_app_id(&long_name, &[]);
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c == 'a'));

        let existing = [id.as_str()];
        let conflicted = generate_app_id(&long_name, &existing);
        assert_eq!(conflicted.len(), 64);
        assert!(conflicted.ends_with("-2"));
        assert_eq!(conflicted.chars().filter(|&c| c == 'a').count(), 62);
    }
}
