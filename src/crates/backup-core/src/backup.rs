use crate::config::{App, CleanupMode, Config, Format};
use crate::events::{AppResult, CancelFlag, Event, EventSender, LogLevel};
use crate::history::{HistoryEntry, HistoryWriter};
use crate::pathres::{expand_path, PathState};
use crate::platform::Platform;
use crate::summary::{
    summary_path_for, write_summary, BackupPathInfo, BackupSummary, ChecksumInfo,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct AppOutcome {
    pub app_id: String,
    pub name: String,
    pub result: AppResult,
    pub detail: String,
    pub size: u64,
    pub checksum: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BackupReport {
    pub dest: PathBuf,
    pub started_at: String,
    pub finished_at: String,
    pub outcomes: Vec<AppOutcome>,
}

impl BackupReport {
    pub fn count(&self, result: AppResult) -> usize {
        self.outcomes.iter().filter(|o| o.result == result).count()
    }
    pub fn ok(&self) -> usize {
        self.count(AppResult::Ok)
    }
    pub fn failed(&self) -> usize {
        self.count(AppResult::Failed)
    }
    pub fn skipped(&self) -> usize {
        self.count(AppResult::Skipped)
    }
    pub fn cancelled(&self) -> usize {
        self.count(AppResult::Cancelled)
    }
}

/// Options for a backup run.
#[derive(Debug, Clone, Default)]
pub struct BackupOptions {
    /// Explicitly selected app ids. Empty = all enabled apps.
    pub app_ids: Vec<String>,
}

/// Run a backup for the selected apps. Publish progress on `tx`.
pub fn backup(
    config: &Config,
    options: &BackupOptions,
    tx: EventSender,
    cancel: &CancelFlag,
) -> Result<BackupReport, crate::config::ConfigError> {
    let dest = config.resolved_dest()?;

    let selected: Vec<&App> = if options.app_ids.is_empty() {
        config.apps.iter().filter(|a| a.enabled).collect()
    } else {
        config
            .apps
            .iter()
            .filter(|a| options.app_ids.iter().any(|id| id == &a.id))
            .collect()
    };

    if selected.is_empty() {
        return Err(crate::config::ConfigError::NoApps);
    }

    let apps_total = selected.len();
    let parallel = config.backup.parallel.max(1).min(apps_total);
    let platform = Platform::current();

    let started_at = now_string();
    let _ = tx.send(Event::Log {
        level: LogLevel::Info,
        msg: format!("backup started: {apps_total} app(s) -> {}", dest.display()),
    });

    let history = HistoryWriter::new();
    let outcomes: std::sync::Arc<std::sync::Mutex<Vec<AppOutcome>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let removed_retention: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    // Worker pool: pull owned app clones from a shared queue so threads don't
    // borrow past this function.
    let selected_owned: Vec<App> = selected.into_iter().cloned().collect();
    let (queue_tx, queue_rx) = crossbeam_channel::unbounded::<App>();
    for app in &selected_owned {
        let _ = queue_tx.send(app.clone());
    }
    drop(queue_tx);

    let mut handles = Vec::new();
    for _ in 0..parallel {
        let queue_rx = queue_rx.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        let history = history.clone();
        let dest = dest.clone();
        let config = config.clone();
        let outcomes = outcomes.clone();
        let removed_retention = removed_retention.clone();

        handles.push(std::thread::spawn(move || {
            while let Ok(app) = queue_rx.recv() {
                let outcome = process_app(&app, &config, &dest, &platform, &tx, &cancel, &history);
                if config.backup.cleanup == CleanupMode::AfterEach && config.backup.retention > 0 {
                    if let Ok(removed) = history.apply_retention(&dest, config.backup.retention) {
                        removed_retention.lock().unwrap().extend(removed);
                    }
                }
                outcomes.lock().unwrap().push(outcome);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }

    // At-end retention cleanup.
    if config.backup.cleanup == CleanupMode::AtEnd && config.backup.retention > 0 {
        if let Ok(removed) = history.apply_retention(&dest, config.backup.retention) {
            removed_retention.lock().unwrap().extend(removed);
        }
    }

    // Delete retained-away archives from disk together with their summary sidecars.
    for (app_id, file) in removed_retention.lock().unwrap().iter() {
        let archive = dest.join(app_id).join(file);
        let _ = remove_archive(&archive);
        let _ = std::fs::remove_file(summary_path_for(&archive));
    }

    let report = BackupReport {
        dest,
        started_at,
        finished_at: now_string(),
        outcomes: outcomes.lock().unwrap().clone(),
    };

    Ok(report)
}

fn now_string() -> String {
    chrono::Local::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Per-app processing
// ---------------------------------------------------------------------------

enum RootClass {
    Backup(PathBuf),
    Missing(PathBuf),
    Undefined(String),
    OtherPlatform,
}

fn classify_root(raw: &str, config: &Config, platform: Platform) -> RootClass {
    match expand_path(raw, platform, config.config_dir().as_deref()) {
        PathState::Resolved { path } => match std::fs::metadata(&path) {
            Ok(_) => RootClass::Backup(path),
            Err(_) => RootClass::Missing(path),
        },
        PathState::UndefinedVar { var } => RootClass::Undefined(var),
        PathState::OtherPlatform { .. } => RootClass::OtherPlatform,
    }
}

fn process_app(
    app: &App,
    config: &Config,
    dest: &Path,
    platform: &Platform,
    tx: &EventSender,
    cancel: &CancelFlag,
    history: &HistoryWriter,
) -> AppOutcome {
    let app_id = app.id.clone();
    let started_at = now_string();
    let _ = tx.send(Event::AppStarted {
        app_id: app_id.clone(),
    });

    /// A configured root plus its resolution status (used for the summary).
    struct RootInfo {
        raw: String,
        resolved: PathBuf,
        existed: bool,
    }

    // 1. Resolve paths.
    let raws = config.paths_for(app, *platform);
    let mut valid_roots: Vec<PathBuf> = Vec::new();
    let mut root_infos: Vec<RootInfo> = Vec::new();
    let mut skipped_reason: Vec<String> = Vec::new();
    for raw in raws {
        match classify_root(raw, config, *platform) {
            RootClass::Backup(p) => {
                valid_roots.push(p.clone());
                root_infos.push(RootInfo {
                    raw: raw.to_string(),
                    resolved: p,
                    existed: true,
                });
            }
            RootClass::Missing(p) => {
                let _ = tx.send(Event::Log {
                    level: LogLevel::Warn,
                    msg: format!("app {app_id}: path missing, skipped: {}", p.display()),
                });
                skipped_reason.push(format!("path missing: {}", p.display()));
                root_infos.push(RootInfo {
                    raw: raw.to_string(),
                    resolved: p,
                    existed: false,
                });
            }
            RootClass::Undefined(var) => {
                let _ = tx.send(Event::Log {
                    level: LogLevel::Warn,
                    msg: format!("app {app_id}: env var not defined: {var}"),
                });
                skipped_reason.push(format!("env var not defined: {var}"));
                root_infos.push(RootInfo {
                    raw: raw.to_string(),
                    resolved: PathBuf::new(),
                    existed: false,
                });
            }
            RootClass::OtherPlatform => {
                let _ = tx.send(Event::Log {
                    level: LogLevel::Debug,
                    msg: format!("app {app_id}: path targets another platform, ignored"),
                });
            }
        }
    }

    if valid_roots.is_empty() {
        let detail = if skipped_reason.is_empty() {
            "not configured for this platform".to_string()
        } else {
            skipped_reason.join("; ")
        };
        let _ = tx.send(Event::AppFinished {
            app_id: app_id.clone(),
            result: AppResult::Skipped,
            detail: detail.clone(),
            size: 0,
            checksum: None,
        });
        return AppOutcome {
            app_id,
            name: app.name.clone(),
            result: AppResult::Skipped,
            detail,
            size: 0,
            checksum: None,
        };
    }

    // Build archive prefixes: every root is archived under its basename;
    // colliding basenames are disambiguated with parent path components.
    let resolved_roots: Vec<PathBuf> = root_infos
        .iter()
        .filter(|info| !info.resolved.as_os_str().is_empty())
        .map(|info| info.resolved.clone())
        .collect();
    let root_prefixes = build_archive_prefixes(&resolved_roots);
    let pack_roots: Vec<(PathBuf, String)> = root_prefixes
        .iter()
        .filter(|(root, _)| root.exists())
        .cloned()
        .collect();

    // 2. Exclude rules.
    let excludes = match build_excludes(app, config) {
        Ok(set) => set,
        Err(e) => {
            let _ = tx.send(Event::Log {
                level: LogLevel::Error,
                msg: format!("app {app_id}: invalid exclude rule: {e}"),
            });
            let _ = tx.send(Event::AppFinished {
                app_id: app_id.clone(),
                result: AppResult::Failed,
                detail: format!("invalid exclude rule: {e}"),
                size: 0,
                checksum: None,
            });
            return AppOutcome {
                app_id,
                name: app.name.clone(),
                result: AppResult::Failed,
                detail: format!("invalid exclude rule: {e}"),
                size: 0,
                checksum: None,
            };
        }
    };

    // 3. Scan.
    let mut files: Vec<PathBuf> = Vec::new();
    let mut empty_dirs: Vec<PathBuf> = Vec::new();
    let mut bytes_total: u64 = 0;
    let mut scanned: u64 = 0;
    let mut last_scan_emit = Instant::now();
    let scan_err = scan_roots(
        &valid_roots,
        &excludes,
        tx,
        cancel,
        &mut files,
        &mut empty_dirs,
        &mut bytes_total,
        &mut scanned,
        &mut last_scan_emit,
        &app_id,
    );

    if let Some(err) = scan_err {
        let _ = tx.send(Event::AppFinished {
            app_id: app_id.clone(),
            result: AppResult::Failed,
            detail: format!("scan failed: {err}"),
            size: 0,
            checksum: None,
        });
        return AppOutcome {
            app_id,
            name: app.name.clone(),
            result: AppResult::Failed,
            detail: format!("scan failed: {err}"),
            size: 0,
            checksum: None,
        };
    }

    let files_total = files.len() as u64;
    let _ = tx.send(Event::ScanDone {
        app_id: app_id.clone(),
        files_total,
        bytes_total,
    });
    let _ = tx.send(Event::Log {
        level: LogLevel::Info,
        msg: format!("app {app_id}: scanned {files_total} file(s), {bytes_total} bytes"),
    });

    if cancel.is_cancelled() {
        let _ = tx.send(Event::AppFinished {
            app_id: app_id.clone(),
            result: AppResult::Cancelled,
            detail: "cancelled during scan".into(),
            size: 0,
            checksum: None,
        });
        return AppOutcome {
            app_id,
            name: app.name.clone(),
            result: AppResult::Cancelled,
            detail: "cancelled during scan".into(),
            size: 0,
            checksum: None,
        };
    }

    // 4. Package.
    let app_dir = dest.join(&app_id);
    if let Err(e) = std::fs::create_dir_all(&app_dir) {
        let _ = tx.send(Event::AppFinished {
            app_id: app_id.clone(),
            result: AppResult::Failed,
            detail: format!("cannot create dest dir: {e}"),
            size: 0,
            checksum: None,
        });
        return AppOutcome {
            app_id,
            name: app.name.clone(),
            result: AppResult::Failed,
            detail: format!("cannot create dest dir: {e}"),
            size: 0,
            checksum: None,
        };
    }

    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let base_name = format!("{app_id}_{timestamp}");
    let format = if app.compress {
        config.backup.format
    } else {
        Format::Dir
    };

    let pack_result = if cancel.is_cancelled() {
        Err("cancelled before pack".into())
    } else {
        let ctx = PackCtx {
            app_id: app_id.clone(),
            files_total,
            bytes_total,
        };
        pack_app(
            &pack_roots,
            &empty_dirs,
            &files,
            &app_dir,
            &base_name,
            format,
            &ctx,
            tx,
            cancel,
        )
    };

    match pack_result {
        Ok((final_path, size)) => {
            let checksum = if config.backup.checksum && format != Format::Dir {
                sha256_hex(&final_path).ok()
            } else {
                None
            };
            let finished_at = now_string();

            let summary = BackupSummary {
                version: 1,
                app_id: app_id.clone(),
                app_name: app.name.clone(),
                platform: platform.as_str().into(),
                archive: final_path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                format: format.extension().into(),
                size,
                files: files_total,
                bytes_total,
                started_at: started_at.clone(),
                finished_at: finished_at.clone(),
                checksum: checksum.clone().map(|digest| ChecksumInfo {
                    algorithm: "sha256".into(),
                    digest,
                }),
                status: "ok".into(),
                excludes: combined_excludes(app, config),
                paths: root_infos
                    .iter()
                    .map(|info| BackupPathInfo {
                        raw: info.raw.clone(),
                        resolved: info.resolved.display().to_string(),
                        existed: info.existed,
                        archive_root: root_prefixes
                            .iter()
                            .find(|(root, _)| *root == info.resolved)
                            .map(|(_, prefix)| prefix.clone())
                            .unwrap_or_default(),
                        restore_to: info.resolved.display().to_string(),
                    })
                    .collect(),
            };

            if let Err(err) = write_summary(&final_path, &summary) {
                let _ = remove_archive(&final_path);
                let detail = format!("summary write failed: {err}");
                let _ = tx.send(Event::Log {
                    level: LogLevel::Error,
                    msg: format!("app {app_id}: {detail}"),
                });
                let _ = tx.send(Event::AppFinished {
                    app_id: app_id.clone(),
                    result: AppResult::Failed,
                    detail: detail.clone(),
                    size: 0,
                    checksum: None,
                });
                return AppOutcome {
                    app_id,
                    name: app.name.clone(),
                    result: AppResult::Failed,
                    detail,
                    size: 0,
                    checksum: None,
                };
            }

            let _ = tx.send(Event::Log {
                level: LogLevel::Info,
                msg: format!(
                    "app {app_id}: finished {}, {size} bytes",
                    final_path.display()
                ),
            });
            let _ = tx.send(Event::AppFinished {
                app_id: app_id.clone(),
                result: AppResult::Ok,
                detail: format!("{size} bytes"),
                size,
                checksum: checksum.clone(),
            });

            let entry = HistoryEntry {
                app_id: app_id.clone(),
                file: final_path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                format: format.extension().into(),
                size,
                files: files_total,
                bytes_total,
                started_at,
                finished_at,
                checksum: checksum.clone(),
                status: "ok".into(),
            };
            let _ = history.append(dest, entry);

            AppOutcome {
                app_id,
                name: app.name.clone(),
                result: AppResult::Ok,
                detail: format!("{size} bytes"),
                size,
                checksum,
            }
        }
        Err(err) if cancel.is_cancelled() => {
            let _ = tx.send(Event::AppFinished {
                app_id: app_id.clone(),
                result: AppResult::Cancelled,
                detail: format!("cancelled: {err}"),
                size: 0,
                checksum: None,
            });
            AppOutcome {
                app_id,
                name: app.name.clone(),
                result: AppResult::Cancelled,
                detail: format!("cancelled: {err}"),
                size: 0,
                checksum: None,
            }
        }
        Err(err) => {
            let _ = tx.send(Event::AppFinished {
                app_id: app_id.clone(),
                result: AppResult::Failed,
                detail: format!("pack failed: {err}"),
                size: 0,
                checksum: None,
            });
            AppOutcome {
                app_id,
                name: app.name.clone(),
                result: AppResult::Failed,
                detail: format!("pack failed: {err}"),
                size: 0,
                checksum: None,
            }
        }
    }
}

/// Effective exclude rules: global settings first, then app-specific ones.
fn combined_excludes(app: &App, config: &Config) -> Vec<String> {
    config
        .backup
        .excludes
        .iter()
        .chain(app.excludes.iter())
        .map(|pattern| pattern.trim().to_string())
        .filter(|pattern| !pattern.is_empty())
        .collect()
}

fn build_excludes(app: &App, config: &Config) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in combined_excludes(app, config) {
        let glob = Glob::new(&pattern).map_err(|e| e.to_string())?;
        builder.add(glob);
    }
    builder.build().map_err(|e| e.to_string())
}

fn is_excluded(excludes: &GlobSet, abs: &Path, rel: &Path) -> bool {
    let rel_slash = rel.to_string_lossy().replace('\\', "/");
    excludes.is_match(rel_slash.as_str()) || excludes.is_match(abs)
}

fn scan_roots(
    roots: &[PathBuf],
    excludes: &GlobSet,
    tx: &EventSender,
    cancel: &CancelFlag,
    files: &mut Vec<PathBuf>,
    empty_dirs: &mut Vec<PathBuf>,
    bytes_total: &mut u64,
    scanned: &mut u64,
    last_emit: &mut Instant,
    app_id: &str,
) -> Option<std::io::Error> {
    let mut dirs_seen: Vec<PathBuf> = Vec::new();
    let mut file_ancestors: HashSet<PathBuf> = HashSet::new();

    for root in roots {
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                let rel = e.path().strip_prefix(root).unwrap_or(e.path());
                !is_excluded(excludes, e.path(), rel)
            })
        {
            if cancel.is_cancelled() {
                return None;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    let _ = tx.send(Event::Log {
                        level: LogLevel::Warn,
                        msg: format!("app {app_id}: walk error: {err}"),
                    });
                    continue;
                }
            };
            let path = entry.path().to_path_buf();
            let ft = entry.file_type();
            if ft.is_dir() {
                dirs_seen.push(path.clone());
            } else {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                if is_excluded(excludes, &path, &rel) {
                    continue;
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                files.push(path.clone());
                *bytes_total += size;
                *scanned += 1;
                let mut cur = rel.parent();
                while let Some(p) = cur {
                    if p.as_os_str().is_empty() {
                        break;
                    }
                    file_ancestors.insert(p.to_path_buf());
                    cur = p.parent();
                }
                if last_emit.elapsed() >= Duration::from_millis(100) {
                    let _ = tx.send(Event::ScanUpdate {
                        app_id: app_id.to_string(),
                        files_scanned: *scanned,
                    });
                    *last_emit = Instant::now();
                }
            }
        }
    }

    for d in dirs_seen {
        if !file_ancestors.contains(&d) {
            empty_dirs.push(d);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Packing
// ---------------------------------------------------------------------------

type PackResult = Result<(PathBuf, u64), String>;

struct PackCtx {
    app_id: String,
    files_total: u64,
    bytes_total: u64,
}

/// Archive prefix for every root. Each root is archived under its basename;
/// colliding basenames get parent path components prepended until every prefix
/// is unique. Identical roots fall back to numeric suffixes.
fn build_archive_prefixes(roots: &[PathBuf]) -> Vec<(PathBuf, String)> {
    fn base_of(root: &Path) -> String {
        root.file_name()
            .map(|name| name.to_string_lossy().replace('\\', "/"))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "root".to_string())
    }
    fn components_of(root: &Path) -> Vec<String> {
        root.components()
            .filter_map(|component| match component {
                std::path::Component::Normal(part) => {
                    Some(part.to_string_lossy().replace('\\', "/"))
                }
                _ => None,
            })
            .collect()
    }

    // (root, prefix, path components, components consumed)
    let mut entries: Vec<(PathBuf, String, Vec<String>, usize)> = roots
        .iter()
        .map(|root| {
            let components = components_of(root);
            (root.clone(), base_of(root), components, 1)
        })
        .collect();

    loop {
        let mut counts: std::collections::HashMap<String, usize> = Default::default();
        for entry in &entries {
            *counts.entry(entry.1.clone()).or_insert(0) += 1;
        }
        let mut changed = false;
        for (root, prefix, components, used) in entries.iter_mut() {
            if counts.get(prefix.as_str()).copied().unwrap_or(0) <= 1 {
                continue;
            }
            changed = true;
            if *used < components.len() {
                let parent = components[components.len() - 1 - *used].clone();
                *prefix = format!("{parent}/{prefix}");
                *used += 1;
            } else {
                let base = base_of(root);
                let mut suffix = 2u32;
                loop {
                    let candidate = format!("{base}-{suffix}");
                    if !counts.contains_key(&candidate) {
                        counts.insert(candidate.clone(), 1);
                        *prefix = candidate;
                        break;
                    }
                    suffix += 1;
                }
            }
        }
        if !changed {
            break;
        }
    }

    entries
        .into_iter()
        .map(|(root, prefix, _, _)| (root, prefix))
        .collect()
}

fn pack_app(
    roots: &[(PathBuf, String)],
    empty_dirs: &[PathBuf],
    files: &[PathBuf],
    app_dir: &Path,
    base_name: &str,
    format: Format,
    ctx: &PackCtx,
    tx: &EventSender,
    cancel: &CancelFlag,
) -> PackResult {
    match format {
        Format::Zip => pack_zip(
            roots, empty_dirs, files, app_dir, base_name, ctx, tx, cancel,
        ),
        Format::TarGz => pack_targz(
            roots, empty_dirs, files, app_dir, base_name, ctx, tx, cancel,
        ),
        Format::Dir => pack_dir(
            roots, empty_dirs, files, app_dir, base_name, ctx, tx, cancel,
        ),
    }
}

/// Archive name for `path` inside `root`, prefixed with the root's archive
/// prefix. The root itself maps to the bare prefix.
fn archive_name(prefix: &str, root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => prefix.to_string(),
        Ok(relative) => format!("{prefix}/{}", relative.to_string_lossy().replace('\\', "/")),
        Err(_) => prefix.to_string(),
    }
}

/// Target path inside a directory backup, mirroring `archive_name`.
fn archive_target(tmp: &Path, prefix: &str, root: &Path, path: &Path) -> PathBuf {
    match path.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => tmp.join(prefix),
        Ok(relative) => tmp.join(prefix).join(relative),
        Err(_) => tmp.join(prefix),
    }
}

/// Remove an archive produced by this tool (file or directory).
fn remove_archive(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(err) => Err(err),
    }
}

fn unique_dest_file(app_dir: &Path, base_name: &str, format: Format) -> PathBuf {
    let mut candidate = app_dir.join(format!("{}.{}", base_name, format.extension()));
    let mut n = 1;
    while candidate.exists() {
        candidate = app_dir.join(format!("{base_name}_{n}.{}", format.extension()));
        n += 1;
    }
    candidate
}

fn pack_zip(
    roots: &[(PathBuf, String)],
    empty_dirs: &[PathBuf],
    files: &[PathBuf],
    app_dir: &Path,
    base_name: &str,
    ctx: &PackCtx,
    tx: &EventSender,
    cancel: &CancelFlag,
) -> PackResult {
    let final_path = unique_dest_file(app_dir, base_name, Format::Zip);
    let tmp_path = app_dir.join(format!(
        "{}.zip.tmp",
        final_path.file_name().unwrap().to_string_lossy()
    ));

    let f = std::fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut done: u64 = 0;
    let mut bytes_done: u64 = 0;

    macro_rules! emit_progress {
        () => {{
            let _ = tx.send(Event::AppProgress {
                app_id: ctx.app_id.clone(),
                files_done: done,
                files_total: ctx.files_total,
                bytes_done,
                bytes_total: ctx.bytes_total,
                eta: None,
            });
        }};
    }

    for dir in empty_dirs {
        if cancel.is_cancelled() {
            let _ = zw.finish();
            let _ = std::fs::remove_file(&tmp_path);
            return Err("cancelled".into());
        }
        for (root, prefix) in roots {
            if dir.starts_with(root) {
                let name = archive_name(prefix, root, dir);
                let _ = zw.add_directory(&name, opts);
            }
        }
    }

    for (root, prefix) in roots {
        for path in files {
            if cancel.is_cancelled() {
                let _ = zw.finish();
                let _ = std::fs::remove_file(&tmp_path);
                return Err("cancelled".into());
            }
            if !path.starts_with(root) {
                continue;
            }
            let name = archive_name(prefix, root, path);
            let mut f = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Event::Log {
                        level: LogLevel::Warn,
                        msg: format!("cannot open {}, skipped: {e}", path.display()),
                    });
                    continue;
                }
            };
            let written = match zw.start_file(name.clone(), opts) {
                Err(e) => {
                    let _ = tx.send(Event::Log {
                        level: LogLevel::Warn,
                        msg: format!("failed to add {} to archive: {e}", path.display()),
                    });
                    continue;
                }
                Ok(()) => match std::io::copy(&mut f, &mut zw) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = tx.send(Event::Log {
                            level: LogLevel::Warn,
                            msg: format!("failed to pack {}, skipped: {e}", path.display()),
                        });
                        continue;
                    }
                },
            };
            done += 1;
            bytes_done += written;
            let _ = tx.send(Event::FileDone {
                app_id: ctx.app_id.clone(),
                path: name,
                bytes_written: written,
            });
            emit_progress!();
        }
    }

    let f = zw.finish().map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp_path, &final_path).map_err(|e| e.to_string())?;
    let size = std::fs::metadata(&final_path)
        .map(|m| m.len())
        .map_err(|e| e.to_string())?;
    Ok((final_path, size))
}

fn pack_targz(
    roots: &[(PathBuf, String)],
    empty_dirs: &[PathBuf],
    files: &[PathBuf],
    app_dir: &Path,
    base_name: &str,
    ctx: &PackCtx,
    tx: &EventSender,
    cancel: &CancelFlag,
) -> PackResult {
    let final_path = unique_dest_file(app_dir, base_name, Format::TarGz);
    let tmp_path = app_dir.join(format!(
        "{}.tar.gz.tmp",
        final_path.file_name().unwrap().to_string_lossy()
    ));

    let f = std::fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
    let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    let mut done: u64 = 0;
    let mut bytes_done: u64 = 0;

    for dir in empty_dirs {
        if cancel.is_cancelled() {
            let _ = builder.finish();
            let _ = std::fs::remove_file(&tmp_path);
            return Err("cancelled".into());
        }
        for (root, prefix) in roots {
            if dir.starts_with(root) {
                let name = archive_name(prefix, root, dir);
                let _ = builder.append_dir(&dir, &name);
            }
        }
    }

    for (root, prefix) in roots {
        for path in files {
            if cancel.is_cancelled() {
                let _ = builder.finish();
                let _ = std::fs::remove_file(&tmp_path);
                return Err("cancelled".into());
            }
            if !path.starts_with(root) {
                continue;
            }
            let name = archive_name(prefix, root, path);
            let written = match append_file_to_tar(&mut builder, path, &name) {
                Ok(n) => n,
                Err(e) => {
                    let _ = tx.send(Event::Log {
                        level: LogLevel::Warn,
                        msg: format!("failed to pack {}, skipped: {e}", path.display()),
                    });
                    continue;
                }
            };
            done += 1;
            bytes_done += written;
            let _ = tx.send(Event::AppProgress {
                app_id: ctx.app_id.clone(),
                files_done: done,
                files_total: ctx.files_total,
                bytes_done,
                bytes_total: ctx.bytes_total,
                eta: None,
            });
        }
    }

    builder.finish().map_err(|e| e.to_string())?;
    let enc = builder.into_inner().map_err(|e| e.to_string())?;
    let f = enc.finish().map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp_path, &final_path).map_err(|e| e.to_string())?;
    let size = std::fs::metadata(&final_path)
        .map(|m| m.len())
        .map_err(|e| e.to_string())?;
    Ok((final_path, size))
}

fn append_file_to_tar(
    builder: &mut tar::Builder<flate2::write::GzEncoder<std::fs::File>>,
    path: &Path,
    name: &str,
) -> Result<u64, std::io::Error> {
    let meta = std::fs::metadata(path)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(target.as_os_str().len() as u64);
        header.set_mode(0o120777);
        header.set_cksum();
        builder.append_data(&mut header, name, target.as_os_str().as_encoded_bytes())?;
        Ok(0)
    } else {
        builder.append_path_with_name(path, name)?;
        Ok(meta.len())
    }
}

fn pack_dir(
    roots: &[(PathBuf, String)],
    empty_dirs: &[PathBuf],
    files: &[PathBuf],
    app_dir: &Path,
    base_name: &str,
    ctx: &PackCtx,
    tx: &EventSender,
    cancel: &CancelFlag,
) -> PackResult {
    let final_path = app_dir.join(format!("{}.dir", base_name));
    let tmp_path = app_dir.join(format!("{}.dir.tmp", base_name));
    if tmp_path.exists() {
        let _ = std::fs::remove_dir_all(&tmp_path);
    }
    std::fs::create_dir_all(&tmp_path).map_err(|e| e.to_string())?;

    for dir in empty_dirs {
        for (root, prefix) in roots {
            if dir.starts_with(root) {
                let target = archive_target(&tmp_path, prefix, root, dir);
                std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            }
        }
    }

    let mut done: u64 = 0;
    let mut bytes_done: u64 = 0;

    for (root, prefix) in roots {
        for path in files {
            if cancel.is_cancelled() {
                let _ = std::fs::remove_dir_all(&tmp_path);
                return Err("cancelled".into());
            }
            if !path.starts_with(root) {
                continue;
            }
            let target = archive_target(&tmp_path, prefix, root, path);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::copy(path, &target) {
                Ok(n) => {
                    done += 1;
                    bytes_done += n;
                    let _ = tx.send(Event::AppProgress {
                        app_id: ctx.app_id.clone(),
                        files_done: done,
                        files_total: ctx.files_total,
                        bytes_done,
                        bytes_total: ctx.bytes_total,
                        eta: None,
                    });
                }
                Err(e) => {
                    let _ = tx.send(Event::Log {
                        level: LogLevel::Warn,
                        msg: format!("failed to copy {}, skipped: {e}", path.display()),
                    });
                }
            }
        }
    }

    std::fs::rename(&tmp_path, &final_path).map_err(|e| e.to_string())?;
    let size = dir_size(&final_path);
    Ok((final_path, size))
}
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if let Ok(m) = e.metadata() {
                if m.is_dir() {
                    total += dir_size(&p);
                } else {
                    total += m.len();
                }
            }
        }
    }
    total
}

fn sha256_hex(path: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
