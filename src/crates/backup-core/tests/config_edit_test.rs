use backup_core::config::Config;
use backup_core::platform::Platform;
use std::collections::HashMap;

fn base_config() -> Config {
    serde_json::from_str(
        r#"{
            "version": 1,
            "backup": { "dest": "~/backups", "format": "zip", "parallel": 2, "retention": 5 },
            "apps": [
                { "id": "demo", "name": "Demo", "enabled": true,
                  "paths": { "windows": ["C:/exists/demo"], "linux": ["/tmp/demo"] },
                  "excludes": ["*.log"] }
            ]
        }"#,
    )
    .unwrap()
}

fn app(id: &str, name: &str, paths: Vec<&str>) -> backup_core::App {
    app_for_platform(id, name, Platform::current(), paths)
}

fn app_for_platform(
    id: &str,
    name: &str,
    platform: Platform,
    paths: Vec<&str>,
) -> backup_core::App {
    let mut map = HashMap::new();
    map.insert(
        platform.as_str().to_string(),
        paths.into_iter().map(String::from).collect(),
    );
    app_from_paths(id, name, map)
}

fn app_from_paths(id: &str, name: &str, paths: HashMap<String, Vec<String>>) -> backup_core::App {
    backup_core::App {
        id: id.into(),
        name: name.into(),
        enabled: true,
        compress: true,
        paths,
        excludes: vec![],
    }
}

#[test]
fn upsert_adds_new_app() {
    let mut cfg = base_config();
    let platform = Platform::current();
    cfg.upsert_app(app("newapp", "New App", vec!["/tmp/src"]), platform)
        .unwrap();

    assert_eq!(cfg.apps.len(), 2);
    let a = cfg.apps.iter().find(|a| a.id == "newapp").unwrap();
    assert_eq!(a.name, "New App");
    // paths normalized to the current platform key only
    assert_eq!(a.paths.len(), 1);
    assert!(a.paths.contains_key(platform.as_str()));
    assert_eq!(a.paths[platform.as_str()], vec!["/tmp/src".to_string()]);
}

#[test]
fn upsert_updates_existing_app() {
    let mut cfg = base_config();
    let platform = Platform::current();
    let mut existing = app("demo", "Demo Renamed", vec!["%APPDATA%\\Demo"]);
    existing.enabled = false;
    cfg.upsert_app(existing, platform).unwrap();

    assert_eq!(cfg.apps.len(), 1, "should not duplicate");
    let a = &cfg.apps[0];
    assert_eq!(a.name, "Demo Renamed");
    assert!(!a.enabled);
    assert_eq!(
        a.paths[platform.as_str()],
        vec!["%APPDATA%\\Demo".to_string()]
    );
    // other platform paths preserved
    let other = if platform == Platform::Windows {
        Platform::Linux
    } else {
        Platform::Windows
    };
    let expected = base_config().apps[0].paths[other.as_str()].clone();
    assert_eq!(a.paths[other.as_str()], expected);
}

#[test]
fn upsert_preserves_other_platform_paths() {
    let mut cfg = base_config();
    let mut existing = app_for_platform(
        "demo",
        "Demo Linux Update",
        Platform::Linux,
        vec!["/srv/demo"],
    );
    existing.enabled = false;
    existing.compress = false;
    existing.excludes = vec!["tmp/**".to_string()];
    cfg.upsert_app(existing, Platform::Linux).unwrap();

    assert_eq!(cfg.apps.len(), 1, "should not duplicate");
    let a = &cfg.apps[0];
    assert_eq!(a.name, "Demo Linux Update");
    assert!(!a.enabled);
    assert!(!a.compress);
    assert_eq!(a.excludes, vec!["tmp/**".to_string()]);
    assert_eq!(a.paths["linux"], vec!["/srv/demo".to_string()]);
    assert_eq!(a.paths["windows"], vec!["C:/exists/demo".to_string()]);
}

#[test]
fn upsert_uses_only_current_platform_input_paths() {
    let mut cfg = base_config();
    let platform = Platform::current();
    let other = if platform == Platform::Windows {
        "linux"
    } else {
        "windows"
    };

    let mut paths = HashMap::new();
    paths.insert(
        platform.as_str().to_string(),
        vec!["/from/current".to_string()],
    );
    paths.insert(other.to_string(), vec!["/from/other".to_string()]);
    cfg.upsert_app(app_from_paths("multi", "Multi", paths), platform)
        .unwrap();

    let a = cfg.apps.iter().find(|a| a.id == "multi").unwrap();
    assert_eq!(a.paths.len(), 1);
    assert_eq!(
        a.paths.get(platform.as_str()),
        Some(&vec!["/from/current".to_string()])
    );
}

#[test]
fn upsert_keeps_other_platforms_when_current_key_missing() {
    let mut cfg = base_config();
    let platform = Platform::current();
    let other = if platform == Platform::Windows {
        "linux"
    } else {
        "windows"
    };
    cfg.apps[0].paths.remove(platform.as_str());
    let other_paths = cfg.apps[0].paths[other].clone();

    cfg.upsert_app(app("demo", "Demo", vec!["/current/new"]), platform)
        .unwrap();

    assert_eq!(cfg.apps.len(), 1);
    let a = &cfg.apps[0];
    assert!(a.paths.contains_key(platform.as_str()));
    assert_eq!(a.paths[platform.as_str()], vec!["/current/new".to_string()]);
    assert_eq!(a.paths[other], other_paths);
}

#[test]
fn upsert_rejects_invalid_input() {
    let mut cfg = base_config();
    let platform = Platform::current();
    assert!(cfg
        .upsert_app(app("", "X", vec!["/tmp"]), platform)
        .is_err());
    assert!(cfg
        .upsert_app(app("Bad ID!", "X", vec!["/tmp"]), platform)
        .is_err());
    assert!(cfg
        .upsert_app(app("ok", "", vec!["/tmp"]), platform)
        .is_err());
    assert!(cfg
        .upsert_app(app("ok", "X", vec!["  ", ""]), platform)
        .is_err());
    // empty excludes/paths filtered but still valid if at least one path present
    assert!(cfg
        .upsert_app(app("ok", "X", vec!["/tmp", "  "]), platform)
        .is_ok());
    assert_eq!(cfg.apps.len(), 2);
}

#[test]
fn remove_app_deletes() {
    let mut cfg = base_config();
    assert!(cfg.remove_app("demo"));
    assert_eq!(cfg.apps.len(), 0);
    assert!(!cfg.remove_app("demo"));
}
