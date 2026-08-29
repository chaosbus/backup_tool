use backup_core::config::{self, App, BackupSettings, CleanupMode, Config, Format};
use backup_core::events::{new_event_stream, spawn_aggregator, CancelFlag, Event};
use backup_core::history::load_history;
use backup_core::pathres::{expand_path, PathState};
use backup_core::platform::Platform;
use backup_core::{backup, generate_app_id, AppResult, BackupOptions};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Mutex, MutexGuard};
use tauri::ipc::Channel;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

pub struct AppState {
    config: Mutex<Option<Config>>,
    cancel: Mutex<CancelFlag>,
}

impl AppState {
    pub fn new() -> Self {
        let cfg = load_initial_config();
        Self {
            config: Mutex::new(cfg),
            cancel: Mutex::new(CancelFlag::new()),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    fn config(&self) -> Result<Config, String> {
        let guard = self.config.lock().map_err(|e| e.to_string())?;
        match guard.as_ref() {
            Some(c) => Ok(c.clone()),
            None => Err(format!(
                "无法加载配置文件: {}（可用 BACKUP_TOOL_CONFIG 指定路径）",
                config::default_config_path().display()
            )),
        }
    }

    fn cancel(&self) -> MutexGuard<'_, CancelFlag> {
        self.cancel.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn load_initial_config() -> Option<Config> {
    if let Ok(path) = std::env::var("BACKUP_TOOL_CONFIG") {
        return load_or_create_config(Some(std::path::Path::new(&path)));
    }
    load_or_create_config(None)
}

fn load_or_create_config(path: Option<&std::path::Path>) -> Option<Config> {
    let path = path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(config::default_config_path);
    match load_config(Some(&path)) {
        Ok((c, _w)) => Some(c),
        Err(config::ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            create_default_config(&path).ok()?;
            load_config(Some(&path)).ok().map(|(c, _w)| c)
        }
        Err(_) => None,
    }
}

fn create_default_config(path: &std::path::Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let dest = if cfg!(target_os = "windows") {
        "%USERPROFILE%\\backups"
    } else {
        "~/backups"
    };
    let text = serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "backup": {
            "dest": dest,
            "format": "zip",
            "parallel": 2,
            "retention": 10,
            "cleanup": "after_each",
            "checksum": true,
            "excludes": ["**/.DS_Store", "**/desktop.ini"]
        },
        "apps": []
    }))
    .map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn load_config(
    path: Option<&std::path::Path>,
) -> Result<(Config, Vec<String>), config::ConfigError> {
    config::load_config(path)
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
pub struct AppMeta {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub has_paths: bool,
    pub has_missing: bool,
    pub last_backup: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct SettingsView {
    pub dest: String,
    pub dest_raw: String,
    pub format: String,
    pub parallel: usize,
    pub retention: usize,
    pub cleanup: String,
    pub excludes: Vec<String>,
    pub config_path: String,
}

#[derive(Serialize, Clone)]
pub struct HistoryView {
    pub app_id: String,
    pub file: String,
    pub format: String,
    pub size: u64,
    pub files: u64,
    pub started_at: String,
    pub status: String,
}

#[derive(Serialize, Clone)]
pub struct OutcomeView {
    pub app_id: String,
    pub name: String,
    pub result: String,
    pub detail: String,
    pub size: u64,
}

#[derive(Serialize, Clone)]
pub struct BackupReportView {
    pub dest: String,
    pub ok: usize,
    pub failed: usize,
    pub skipped: usize,
    pub cancelled: usize,
    pub outcomes: Vec<OutcomeView>,
}

fn settings_view(config: &Config) -> SettingsView {
    let dest = config
        .resolved_dest()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| config.backup.dest.clone());
    let cleanup = match config.backup.cleanup {
        CleanupMode::AfterEach => "after_each",
        CleanupMode::AtEnd => "at_end",
    };
    SettingsView {
        dest,
        dest_raw: config.backup.dest.clone(),
        format: format_name(&config.backup.format).into(),
        parallel: config.backup.parallel,
        retention: config.backup.retention,
        cleanup: cleanup.into(),
        excludes: config.backup.excludes.clone(),
        config_path: config
            .source
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未知".into()),
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_apps(state: State<'_, AppState>) -> Result<Vec<AppMeta>, String> {
    let config = state.config()?;
    let platform = Platform::current();
    let history = config
        .resolved_dest()
        .ok()
        .as_deref()
        .map(load_history)
        .unwrap_or_default();

    let mut metas = Vec::new();
    for app in &config.apps {
        let mut has_paths = false;
        let mut has_missing = false;
        for raw in config.paths_for(app, platform) {
            match expand_path(raw, platform, config.config_dir().as_deref()) {
                backup_core::pathres::PathState::Resolved { path } => {
                    if path.exists() {
                        has_paths = true;
                    } else {
                        has_missing = true;
                    }
                }
                backup_core::pathres::PathState::UndefinedVar { .. } => has_missing = true,
                backup_core::pathres::PathState::OtherPlatform { .. } => {}
            }
        }
        let last_backup = history.entries_for(&app.id).first().map(|e| e.file.clone());
        metas.push(AppMeta {
            id: app.id.clone(),
            name: app.name.clone(),
            enabled: app.enabled,
            has_paths,
            has_missing,
            last_backup,
        });
    }
    Ok(metas)
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let config = state.config()?;
    Ok(settings_view(&config))
}

#[tauri::command]
pub fn get_history(
    state: State<'_, AppState>,
    app: Option<String>,
) -> Result<Vec<HistoryView>, String> {
    let config = state.config()?;
    let dest = config.resolved_dest().map_err(|e| e.to_string())?;
    let history = load_history(&dest);
    let mut v: Vec<HistoryView> = history
        .entries
        .iter()
        .filter(|e| app.as_deref().is_none_or(|a| e.app_id == a))
        .map(|e| HistoryView {
            app_id: e.app_id.clone(),
            file: e.file.clone(),
            format: e.format.clone(),
            size: e.size,
            files: e.files,
            started_at: e.started_at.clone(),
            status: e.status.clone(),
        })
        .collect();
    v.sort_by(|a, b| b.file.cmp(&a.file));
    Ok(v)
}
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiEvent {
    AppStarted {
        app_id: String,
    },
    ScanDone {
        app_id: String,
        files_total: u64,
        bytes_total: u64,
    },
    AppProgress {
        app_id: String,
        files_done: u64,
        files_total: u64,
        bytes_done: u64,
        bytes_total: u64,
    },
    OverallProgress {
        apps_done: usize,
        apps_total: usize,
        bytes_done: u64,
        bytes_total: u64,
    },
    AppFinished {
        app_id: String,
        result: String,
        detail: String,
        size: u64,
    },
    Log {
        level: String,
        msg: String,
    },
}

fn convert(ev: Event) -> Option<UiEvent> {
    match ev {
        Event::Log { level, msg } => Some(UiEvent::Log {
            level: level.as_str().into(),
            msg,
        }),
        Event::AppStarted { app_id } => Some(UiEvent::AppStarted { app_id }),
        Event::ScanDone {
            app_id,
            files_total,
            bytes_total,
        } => Some(UiEvent::ScanDone {
            app_id,
            files_total,
            bytes_total,
        }),
        Event::AppProgress {
            app_id,
            files_done,
            files_total,
            bytes_done,
            bytes_total,
            ..
        } => Some(UiEvent::AppProgress {
            app_id,
            files_done,
            files_total,
            bytes_done,
            bytes_total,
        }),
        Event::OverallProgress {
            apps_done,
            apps_total,
            bytes_done,
            bytes_total,
            ..
        } => Some(UiEvent::OverallProgress {
            apps_done,
            apps_total,
            bytes_done,
            bytes_total,
        }),
        Event::AppFinished {
            app_id,
            result,
            detail,
            size,
            ..
        } => Some(UiEvent::AppFinished {
            app_id,
            result: result_str(result).into(),
            detail,
            size,
        }),
        Event::FileDone { .. } | Event::ScanUpdate { .. } => None,
    }
}

fn result_str(r: AppResult) -> &'static str {
    match r {
        AppResult::Ok => "ok",
        AppResult::Skipped => "skipped",
        AppResult::Failed => "failed",
        AppResult::Cancelled => "cancelled",
    }
}

#[tauri::command]
pub async fn run_backup(
    state: State<'_, AppState>,
    selected: Vec<String>,
    channel: Channel<UiEvent>,
) -> Result<BackupReportView, String> {
    let config = state.config()?;
    let options = BackupOptions {
        app_ids: selected.clone(),
    };
    let apps_total = if selected.is_empty() {
        config.apps.iter().filter(|a| a.enabled).count()
    } else {
        selected.len()
    };

    let cancel = CancelFlag::new();
    *state.cancel() = cancel.clone();

    let (tx, raw_rx) = new_event_stream();
    let ch = channel.clone();
    std::thread::spawn(move || {
        let rx = spawn_aggregator(raw_rx, apps_total);
        for ev in rx.iter() {
            if let Some(ui) = convert(ev) {
                let _ = ch.send(ui);
            }
        }
    });

    let report = backup(&config, &options, tx, &cancel).map_err(|e| e.to_string())?;

    Ok(BackupReportView {
        dest: report.dest.display().to_string(),
        ok: report.ok(),
        failed: report.failed(),
        skipped: report.skipped(),
        cancelled: report.cancelled(),
        outcomes: report
            .outcomes
            .into_iter()
            .map(|o| OutcomeView {
                app_id: o.app_id,
                name: o.name,
                result: result_str(o.result).into(),
                detail: o.detail,
                size: o.size,
            })
            .collect(),
    })
}

#[tauri::command]
pub fn cancel_backup(state: State<'_, AppState>) {
    state.cancel().cancel();
}

#[tauri::command]
pub fn exit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub async fn pick_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let picked = app.dialog().file().blocking_pick_folder();
    match picked {
        Some(file_path) => file_path
            .into_path()
            .map(|path| Some(path.display().to_string()))
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

fn format_name(f: &Format) -> &'static str {
    match f {
        Format::Zip => "zip",
        Format::TarGz => "tar.gz",
        Format::Dir => "dir",
    }
}

// ---------------------------------------------------------------------------
// App CRUD
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AppInput {
    /// Empty for new apps; the backend generates a unique id from `name`.
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub compress: bool,
    /// Paths for the current platform only.
    pub paths: Vec<String>,
    pub excludes: Vec<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AppDetail {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub compress: bool,
    pub paths: Vec<String>,
    pub excludes: Vec<String>,
}

fn app_from_input(input: AppInput) -> App {
    let mut paths = HashMap::new();
    paths.insert(
        Platform::current().as_str().to_string(),
        input.paths.clone(),
    );
    App {
        id: input.id,
        name: input.name,
        enabled: input.enabled,
        compress: input.compress,
        paths,
        excludes: input.excludes,
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PathPreview {
    pub resolved: String,
    pub exists: bool,
    pub state: String,
    pub note: String,
}

fn write_config(config: &Config) -> Result<(), String> {
    let source = config
        .source
        .clone()
        .ok_or_else(|| "配置文件路径未知，无法保存".to_string())?;
    if let Some(parent) = source.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let tmp = source.with_extension(
        source
            .extension()
            .map(|ext| format!("{}.tmp", ext.to_string_lossy()))
            .unwrap_or_else(|| "tmp".into()),
    );
    let mut file = match std::fs::File::create(&tmp) {
        Ok(file) => file,
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(err.to_string());
        }
    };
    if let Err(err) = file
        .write_all(text.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    if let Err(err) = std::fs::rename(&tmp, &source) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    Ok(())
}

#[tauri::command]
pub fn get_app(state: State<'_, AppState>, id: String) -> Result<AppDetail, String> {
    let config = state.config()?;
    let platform = Platform::current();
    let app = config
        .apps
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| format!("未找到应用: {id}"))?;
    Ok(AppDetail {
        id: app.id.clone(),
        name: app.name.clone(),
        enabled: app.enabled,
        compress: app.compress,
        paths: config
            .paths_for(app, platform)
            .into_iter()
            .cloned()
            .collect(),
        excludes: app.excludes.clone(),
    })
}

fn save_app_inner(config: &mut Option<Config>, mut input: AppInput) -> Result<(), String> {
    let current = config
        .as_ref()
        .ok_or_else(|| "无法加载配置文件".to_string())?
        .clone();
    let mut candidate = current;
    if input.id.trim().is_empty() {
        let existing_ids: Vec<&str> = candidate.apps.iter().map(|a| a.id.as_str()).collect();
        input.id = generate_app_id(&input.name, &existing_ids);
    }
    let app = app_from_input(input);
    candidate.upsert_app(app, Platform::current())?;
    write_config(&candidate)?;
    *config = Some(candidate);
    Ok(())
}

#[tauri::command]
pub fn save_app(state: State<'_, AppState>, input: AppInput) -> Result<(), String> {
    let mut guard = state.config.lock().map_err(|e| e.to_string())?;
    save_app_inner(&mut guard, input)
}

fn remove_app_inner(config: &mut Option<Config>, id: &str) -> Result<(), String> {
    let current = config
        .as_ref()
        .ok_or_else(|| "无法加载配置文件".to_string())?
        .clone();
    let mut candidate = current;
    if !candidate.remove_app(id) {
        return Err(format!("未找到应用: {id}"));
    }
    write_config(&candidate)?;
    *config = Some(candidate);
    Ok(())
}

#[tauri::command]
pub fn remove_app(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let mut guard = state.config.lock().map_err(|e| e.to_string())?;
    remove_app_inner(&mut guard, &id)
}

#[tauri::command]
pub fn resolve_path(state: State<'_, AppState>, raw: String) -> Result<PathPreview, String> {
    let config = state.config()?;
    let platform = Platform::current();
    match expand_path(&raw, platform, config.config_dir().as_deref()) {
        PathState::Resolved { path } => Ok(PathPreview {
            resolved: path.display().to_string(),
            exists: path.exists(),
            state: if path.exists() {
                "ok".into()
            } else {
                "missing".into()
            },
            note: String::new(),
        }),
        PathState::UndefinedVar { var } => Ok(PathPreview {
            resolved: String::new(),
            exists: false,
            state: "undefined".into(),
            note: format!("环境变量 {var} 未定义"),
        }),
        PathState::OtherPlatform { .. } => Ok(PathPreview {
            resolved: String::new(),
            exists: false,
            state: "other_platform".into(),
            note: "该语法属于其他平台，仅在其他系统生效".into(),
        }),
    }
}
// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsInput {
    pub dest: String,
    pub format: String,
    pub parallel: usize,
    pub retention: usize,
    pub cleanup: String,
    pub excludes: Vec<String>,
}

fn save_settings_inner(config: &mut Option<Config>, input: SettingsInput) -> Result<(), String> {
    let current = config
        .as_ref()
        .ok_or_else(|| "无法加载配置文件".to_string())?
        .clone();
    let mut candidate = current;

    let dest = input.dest.trim().to_string();
    if dest.is_empty() {
        return Err("备份位置不能为空".into());
    }
    let format = match input.format.as_str() {
        "zip" => Format::Zip,
        "tar.gz" => Format::TarGz,
        "dir" => Format::Dir,
        other => return Err(format!("不支持的备份格式: {other}")),
    };
    if input.parallel == 0 {
        return Err("并行备份数必须大于 0".into());
    }
    let cleanup = match input.cleanup.as_str() {
        "after_each" => CleanupMode::AfterEach,
        "at_end" => CleanupMode::AtEnd,
        other => return Err(format!("不支持的清理时机: {other}")),
    };
    let excludes: Vec<String> = input
        .excludes
        .iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect();

    candidate.backup.dest = dest;
    candidate.backup.format = format;
    candidate.backup.parallel = input.parallel;
    candidate.backup.retention = input.retention;
    candidate.backup.cleanup = cleanup;
    candidate.backup.excludes = excludes;
    candidate.resolved_dest().map_err(|e| e.to_string())?;

    write_config(&candidate)?;
    *config = Some(candidate);
    Ok(())
}

#[tauri::command]
pub fn save_settings(state: State<'_, AppState>, input: SettingsInput) -> Result<(), String> {
    let mut guard = state.config.lock().map_err(|e| e.to_string())?;
    save_settings_inner(&mut guard, input)
}

#[tauri::command]
pub fn default_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let config = state.config()?;
    let dest = if cfg!(target_os = "windows") {
        "%USERPROFILE%\\backups"
    } else {
        "~/backups"
    };
    let defaults = BackupSettings {
        dest: dest.into(),
        excludes: vec!["**/.DS_Store".into(), "**/desktop.ini".into()],
        ..Default::default()
    };
    Ok(SettingsView {
        dest: defaults.dest.clone(),
        dest_raw: defaults.dest.clone(),
        format: format_name(&defaults.format).into(),
        parallel: defaults.parallel,
        retention: defaults.retention,
        cleanup: match defaults.cleanup {
            CleanupMode::AfterEach => "after_each",
            CleanupMode::AtEnd => "at_end",
        }
        .into(),
        excludes: defaults.excludes,
        config_path: config
            .source
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未知".into()),
    })
}

#[tauri::command]
pub fn reload_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let mut guard = state.config.lock().map_err(|e| e.to_string())?;
    let source = guard
        .as_ref()
        .and_then(|c| c.source.clone())
        .ok_or_else(|| "配置文件路径未知，无法重新加载".to_string())?;
    let (config, _warnings) = load_config(Some(&source)).map_err(|e| e.to_string())?;
    *guard = Some(config);
    match guard.as_ref() {
        Some(c) => Ok(settings_view(c)),
        None => Err("无法加载配置文件".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn app_state_with(config: Config) -> AppState {
        AppState {
            config: Mutex::new(Some(config)),
            cancel: Mutex::new(CancelFlag::new()),
        }
    }

    fn sample_app(id: &str) -> backup_core::App {
        let mut paths = HashMap::new();
        paths.insert(
            Platform::current().as_str().to_string(),
            vec!["/tmp/src".to_string()],
        );
        backup_core::App {
            id: id.into(),
            name: id.into(),
            enabled: true,
            compress: true,
            paths,
            excludes: vec![],
        }
    }

    fn sample_input(id: &str) -> AppInput {
        AppInput {
            id: id.into(),
            name: id.into(),
            enabled: true,
            compress: true,
            paths: vec!["/tmp/src".into()],
            excludes: vec![],
        }
    }

    fn settings_input(dest: &str, format: &str) -> SettingsInput {
        SettingsInput {
            dest: dest.into(),
            format: format.into(),
            parallel: 2,
            retention: 10,
            cleanup: "after_each".into(),
            excludes: vec![],
        }
    }

    #[test]
    fn app_from_input_keeps_current_platform_paths() {
        let input = AppInput {
            id: "test".into(),
            name: "Test".into(),
            enabled: true,
            compress: true,
            paths: vec!["/tmp/src".into()],
            excludes: vec!["*.log".into()],
        };
        let app = app_from_input(input);
        assert_eq!(
            app.paths.get(Platform::current().as_str()),
            Some(&vec!["/tmp/src".to_string()])
        );
    }

    #[test]
    fn missing_config_is_created_with_empty_apps() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("config.json");
        let config = load_or_create_config(Some(&path)).expect("config should be created");
        assert!(path.exists());
        assert_eq!(config.source.as_deref(), Some(path.as_path()));
        assert!(config.apps.is_empty());
    }

    #[test]
    fn write_config_replaces_file_atomically_without_temp_leftover() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        std::fs::write(&source, r#"{"version":1,"apps":[]}"#).unwrap();
        let config = Config {
            version: 2,
            backup: Default::default(),
            apps: vec![],
            source: Some(source.clone()),
        };

        write_config(&config).expect("config should be written");

        let written: Config =
            serde_json::from_str(&std::fs::read_to_string(&source).unwrap()).unwrap();
        assert_eq!(written.version, 2);
        assert!(!tmp.path().join("config.json.tmp").exists());
    }

    #[test]
    fn write_config_fails_and_cleans_temp_when_source_is_directory() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        std::fs::create_dir(&source).unwrap();
        let config = Config {
            version: 1,
            backup: Default::default(),
            apps: vec![],
            source: Some(source.clone()),
        };

        let err = write_config(&config).expect_err("directory source should fail");
        assert!(!err.is_empty());
        assert!(!tmp.path().join("config.json.tmp").exists());
    }

    #[test]
    fn save_app_write_failure_keeps_in_memory_config() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        std::fs::create_dir(&source).unwrap();
        let config = Config {
            version: 1,
            backup: Default::default(),
            apps: vec![],
            source: Some(source.clone()),
        };
        let state = app_state_with(config);
        let mut guard = state.config.lock().unwrap();

        let err =
            save_app_inner(&mut guard, sample_input("newapp")).expect_err("write should fail");
        assert!(!err.is_empty());
        assert!(guard.as_ref().unwrap().apps.is_empty());
    }

    #[test]
    fn remove_app_write_failure_keeps_in_memory_config() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        std::fs::create_dir(&source).unwrap();
        let config = Config {
            version: 1,
            backup: Default::default(),
            apps: vec![sample_app("demo")],
            source: Some(source.clone()),
        };
        let state = app_state_with(config);
        let mut guard = state.config.lock().unwrap();

        let err = remove_app_inner(&mut guard, "demo").expect_err("write should fail");
        assert!(!err.is_empty());
        assert_eq!(guard.as_ref().unwrap().apps.len(), 1);
    }

    #[test]
    fn remove_app_missing_id_returns_error() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        let config = Config {
            version: 1,
            backup: Default::default(),
            apps: vec![sample_app("demo")],
            source: Some(source.clone()),
        };
        let state = app_state_with(config);
        let mut guard = state.config.lock().unwrap();

        let err = remove_app_inner(&mut guard, "missing").expect_err("missing id should fail");
        assert!(err.contains("未找到应用"));
        assert_eq!(guard.as_ref().unwrap().apps.len(), 1);
    }

    #[test]
    fn save_app_empty_id_auto_generates() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        let config = Config {
            version: 1,
            backup: BackupSettings::default(),
            apps: vec![],
            source: Some(source),
        };
        let state = app_state_with(config);
        let mut guard = state.config.lock().unwrap();

        let input = AppInput {
            id: String::new(),
            name: "My App".into(),
            enabled: true,
            compress: true,
            paths: vec!["/tmp/src".into()],
            excludes: vec![],
        };
        save_app_inner(&mut guard, input).expect("save should succeed");

        let apps = &guard.as_ref().unwrap().apps;
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].id, "my-app");
        assert_eq!(apps[0].name, "My App");
    }

    #[test]
    fn save_app_existing_id_stays_stable() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        let config = Config {
            version: 1,
            backup: BackupSettings::default(),
            apps: vec![sample_app("vscode")],
            source: Some(source),
        };
        let state = app_state_with(config);
        let mut guard = state.config.lock().unwrap();

        let input = AppInput {
            id: "vscode".into(),
            name: "VSCode 改名".into(),
            enabled: false,
            compress: true,
            paths: vec!["/tmp/vscode".into()],
            excludes: vec![],
        };
        save_app_inner(&mut guard, input).expect("save should succeed");

        let apps = &guard.as_ref().unwrap().apps;
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].id, "vscode");
        assert_eq!(apps[0].name, "VSCode 改名");
        assert!(!apps[0].enabled);
    }

    #[test]
    fn save_app_generated_id_avoids_conflicts_with_suffix() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        let config = Config {
            version: 1,
            backup: BackupSettings::default(),
            apps: vec![sample_app("my-app"), sample_app("my-app-2")],
            source: Some(source),
        };
        let state = app_state_with(config);
        let mut guard = state.config.lock().unwrap();

        let input = AppInput {
            id: String::new(),
            name: "My App".into(),
            enabled: true,
            compress: true,
            paths: vec!["/tmp/src".into()],
            excludes: vec![],
        };
        save_app_inner(&mut guard, input).expect("save should succeed");

        let apps = &guard.as_ref().unwrap().apps;
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[2].id, "my-app-3");
    }

    #[test]
    fn save_settings_invalid_dest_keeps_memory_unchanged() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        let config = Config {
            version: 1,
            backup: BackupSettings {
                dest: tmp.path().display().to_string(),
                ..Default::default()
            },
            apps: vec![],
            source: Some(source),
        };
        let state = app_state_with(config);
        let mut guard = state.config.lock().unwrap();

        let err =
            save_settings_inner(&mut guard, settings_input("   ", "zip")).expect_err("should fail");
        assert!(!err.is_empty());
        assert_eq!(
            guard.as_ref().unwrap().backup.dest,
            tmp.path().display().to_string()
        );
    }

    #[test]
    fn save_settings_invalid_format_keeps_memory_unchanged() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        let dest = tmp.path().join("backups").display().to_string();
        let config = Config {
            version: 1,
            backup: BackupSettings {
                dest: dest.clone(),
                ..Default::default()
            },
            apps: vec![],
            source: Some(source),
        };
        let state = app_state_with(config);
        let mut guard = state.config.lock().unwrap();

        let err = save_settings_inner(&mut guard, settings_input(&dest, "rar"))
            .expect_err("unsupported format should fail");
        assert!(err.contains("rar"));
        assert_eq!(guard.as_ref().unwrap().backup.format, Format::Zip);
    }

    #[test]
    fn save_settings_valid_commits_to_memory_and_disk() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("config.json");
        let dest = tmp.path().join("backups").display().to_string();
        let config = Config {
            version: 1,
            backup: BackupSettings::default(),
            apps: vec![],
            source: Some(source.clone()),
        };
        let state = app_state_with(config);
        let mut guard = state.config.lock().unwrap();

        save_settings_inner(
            &mut guard,
            SettingsInput {
                dest: dest.clone(),
                format: "tar.gz".into(),
                parallel: 4,
                retention: 3,
                cleanup: "at_end".into(),
                excludes: vec!["*.tmp".into(), "  ".into()],
            },
        )
        .expect("save should succeed");

        let backup = &guard.as_ref().unwrap().backup;
        assert_eq!(backup.dest, dest);
        assert_eq!(backup.format, Format::TarGz);
        assert_eq!(backup.parallel, 4);
        assert_eq!(backup.retention, 3);
        assert_eq!(backup.cleanup, CleanupMode::AtEnd);
        assert_eq!(backup.excludes, vec!["*.tmp".to_string()]);

        let on_disk: Config =
            serde_json::from_str(&std::fs::read_to_string(&source).unwrap()).unwrap();
        assert_eq!(on_disk.backup.format, Format::TarGz);
        assert_eq!(on_disk.backup.parallel, 4);
    }
}
