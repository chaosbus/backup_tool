use backup_core::config::Config;
use backup_core::events::{new_event_stream, AppResult, CancelFlag, Event, LogLevel};
use backup_core::history::load_history;
use backup_core::summary::read_summary;
use backup_core::{backup, BackupOptions, BackupReport, Format};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
}

fn sha256_hex(path: &Path) -> String {
    let mut f = std::fs::File::open(path).unwrap();
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher).unwrap();
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn config_for(dest: &Path, paths: Vec<PathBuf>, format: Format, retention: usize) -> Config {
    let platform = backup_core::Platform::current().as_str().to_string();
    let mut app_paths: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    app_paths.insert(
        platform,
        paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    );

    let config_json = serde_json::json!({
        "version": 1,
        "backup": {
            "dest": dest.to_string_lossy(),
            "format": format,
            "parallel": 2,
            "retention": retention,
            "checksum": true
        },
        "apps": [
            {
                "id": "testapp",
                "name": "Test App",
                "enabled": true,
                "paths": app_paths,
                "excludes": ["**/*.log"]
            }
        ]
    });
    serde_json::from_value(config_json).unwrap()
}

fn build_config(src: &Path, dest: &Path) -> Config {
    config_for(dest, vec![src.to_path_buf()], Format::Zip, 0)
}

fn run_backup(config: &Config) -> BackupReport {
    let (tx, _rx) = new_event_stream();
    let cancel = CancelFlag::new();
    backup(config, &BackupOptions::default(), tx, &cancel).unwrap()
}

fn app_dir_files(app_dir: &Path) -> Vec<String> {
    std::fs::read_dir(app_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn find_archive(app_dir: &Path, suffix: &str) -> String {
    app_dir_files(app_dir)
        .iter()
        .find(|n| n.ends_with(suffix) && !n.ends_with(".tmp"))
        .unwrap_or_else(|| panic!("no {suffix} archive in {}", app_dir.display()))
        .clone()
}

#[test]
fn backup_creates_zip_and_history() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dest = tmp.path().join("backups");

    write_file(&src.join("settings.json"), "{\"theme\":\"dark\"}");
    write_file(&src.join("keybindings.json"), "{\"ctrl+k\":\"cmd\"}");
    write_file(&src.join("Cache").join("big.bin"), "cache-data");
    write_file(&src.join("debug.log"), "should be excluded");
    std::fs::create_dir_all(src.join("EmptyDir")).unwrap();

    let config = build_config(&src, &dest);
    let (tx, rx) = new_event_stream();
    let cancel = CancelFlag::new();
    let report = backup(&config, &BackupOptions::default(), tx, &cancel).unwrap();

    assert_eq!(report.ok(), 1);
    let outcome = &report.outcomes[0];
    assert_eq!(outcome.result, AppResult::Ok);
    assert!(outcome.size > 0);
    assert!(outcome.checksum.is_some());

    // Archive preserves the root directory as `src/...`.
    let app_dir = dest.join("testapp");
    let zip_file = find_archive(&app_dir, ".zip");
    let zip_path = app_dir.join(&zip_file);

    let file = std::fs::File::open(&zip_path).unwrap();
    let archive = zip::ZipArchive::new(file).unwrap();
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
    assert!(
        names.contains(&"src/settings.json".to_string()),
        "names: {names:?}"
    );
    assert!(names.contains(&"src/keybindings.json".to_string()));
    assert!(names.contains(&"src/Cache/big.bin".to_string()));
    assert!(names.contains(&"src/EmptyDir/".to_string()));
    assert!(
        !names.iter().any(|n| n.ends_with(".log")),
        "excluded .log leaked"
    );

    // Summary sidecar describes the archive and matches the bytes on disk.
    let summary = read_summary(&zip_path).expect("summary sidecar");
    assert_eq!(summary.version, 1);
    assert_eq!(summary.app_id, "testapp");
    assert_eq!(summary.app_name, "Test App");
    assert_eq!(summary.platform, backup_core::Platform::current().as_str());
    assert_eq!(summary.archive, zip_file);
    assert_eq!(summary.format, "zip");
    assert_eq!(summary.size, outcome.size);
    assert_eq!(summary.files, 3);
    let expected_bytes = std::fs::metadata(src.join("settings.json")).unwrap().len()
        + std::fs::metadata(src.join("keybindings.json"))
            .unwrap()
            .len()
        + std::fs::metadata(src.join("Cache").join("big.bin"))
            .unwrap()
            .len();
    assert_eq!(summary.bytes_total, expected_bytes);
    assert_eq!(summary.status, "ok");
    assert_eq!(summary.excludes, vec!["**/*.log".to_string()]);
    let checksum = summary.checksum.as_ref().expect("zip checksum");
    assert_eq!(checksum.algorithm, "sha256");
    assert_eq!(checksum.digest, sha256_hex(&zip_path));
    assert_eq!(outcome.checksum.as_deref(), Some(checksum.digest.as_str()));
    assert_eq!(summary.paths.len(), 1);
    assert_eq!(summary.paths[0].archive_root, "src");
    assert_eq!(summary.paths[0].restore_to, src.to_string_lossy());
    assert!(summary.paths[0].existed);

    // History index written, with the same checksum.
    let history = load_history(&dest);
    let entry = history.entries_for("testapp");
    assert_eq!(entry.len(), 1);
    assert_eq!(entry[0].status, "ok");
    assert_eq!(entry[0].files, 3);
    assert_eq!(entry[0].checksum.as_deref(), Some(checksum.digest.as_str()));

    // Events were published.
    let mut saw_started = false;
    let mut saw_finished = false;
    for ev in rx.try_iter() {
        match ev {
            Event::AppStarted { app_id } if app_id == "testapp" => saw_started = true,
            Event::AppFinished { app_id, result, .. }
                if app_id == "testapp" && result == AppResult::Ok =>
            {
                saw_finished = true
            }
            Event::Log {
                level: LogLevel::Warn,
                ..
            } => {
                panic!("unexpected warning event")
            }
            _ => {}
        }
    }
    assert!(saw_started);
    assert!(saw_finished);
}

#[test]
fn backup_missing_path_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("does-not-exist");
    let dest = tmp.path().join("backups");

    let config = build_config(&src, &dest);
    let (tx, _rx) = new_event_stream();
    let cancel = CancelFlag::new();
    let report = backup(&config, &BackupOptions::default(), tx, &cancel).unwrap();

    assert_eq!(report.skipped(), 1);
    assert_eq!(report.ok(), 0);
}

#[test]
fn backup_dir_copy_format_keeps_root_basename() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dest = tmp.path().join("backups");

    write_file(&src.join("a.txt"), "alpha");
    write_file(&src.join("sub").join("b.txt"), "beta");

    let mut config = build_config(&src, &dest);
    config.backup.format = Format::Dir;
    let (tx, _rx) = new_event_stream();
    let cancel = CancelFlag::new();
    let report = backup(&config, &BackupOptions::default(), tx, &cancel).unwrap();

    assert_eq!(report.ok(), 1);
    let app_dir = dest.join("testapp");
    let dir_backup = find_archive(&app_dir, ".dir");
    let copied = app_dir
        .join(&dir_backup)
        .join("src")
        .join("sub")
        .join("b.txt");
    assert_eq!(std::fs::read_to_string(copied).unwrap(), "beta");

    let summary = read_summary(&app_dir.join(&dir_backup)).unwrap();
    assert_eq!(summary.format, "dir");
    assert!(summary.checksum.is_none(), "dir backups have no checksum");
    assert_eq!(summary.files, 2);
    let expected_bytes = std::fs::metadata(src.join("a.txt")).unwrap().len()
        + std::fs::metadata(src.join("sub").join("b.txt"))
            .unwrap()
            .len();
    assert_eq!(summary.bytes_total, expected_bytes);
}

#[test]
fn backup_prefixes_include_root_basename_and_disambiguate() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("backups");
    let root_a = tmp.path().join("a").join("example");
    let root_b = tmp.path().join("b").join("example");
    let single = tmp.path().join("single.txt");
    write_file(&root_a.join("x.txt"), "x");
    write_file(&root_b.join("y.txt"), "y");
    write_file(&single, "single");

    let config = config_for(&dest, vec![root_a, root_b, single], Format::Zip, 0);
    let report = run_backup(&config);
    assert_eq!(report.ok(), 1);

    let app_dir = dest.join("testapp");
    let zip_file = find_archive(&app_dir, ".zip");
    let file = std::fs::File::open(app_dir.join(&zip_file)).unwrap();
    let archive = zip::ZipArchive::new(file).unwrap();
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();

    // Each same-named root gets a unique prefix built from its parent; a
    // single-file root is stored under its own file name.
    assert!(
        names.contains(&"a/example/x.txt".to_string()),
        "names: {names:?}"
    );
    assert!(names.contains(&"b/example/y.txt".to_string()));
    assert!(names.contains(&"single.txt".to_string()));
    assert!(
        !names.iter().any(|n| n == "example/x.txt" || n == "x.txt"),
        "bare root content leaked: {names:?}"
    );

    let summary = read_summary(&app_dir.join(&zip_file)).unwrap();
    let roots: Vec<&str> = summary
        .paths
        .iter()
        .map(|p| p.archive_root.as_str())
        .collect();
    assert!(roots.contains(&"a/example"), "roots: {roots:?}");
    assert!(roots.contains(&"b/example"), "roots: {roots:?}");
    assert!(roots.contains(&"single.txt"), "roots: {roots:?}");
}

#[test]
fn backup_targz_keeps_root_basename() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dest = tmp.path().join("backups");
    write_file(&src.join("a.txt"), "alpha");
    write_file(&src.join("sub").join("b.txt"), "beta");

    let config = config_for(&dest, vec![src.clone()], Format::TarGz, 0);
    let report = run_backup(&config);
    assert_eq!(report.ok(), 1);

    let app_dir = dest.join("testapp");
    let tar_file = find_archive(&app_dir, ".tar.gz");
    let file = std::fs::File::open(app_dir.join(&tar_file)).unwrap();
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let names: Vec<String> = archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"src/a.txt".to_string()), "names: {names:?}");
    assert!(names.contains(&"src/sub/b.txt".to_string()));
}

#[test]
fn retention_keeps_newest() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dest = tmp.path().join("backups");
    write_file(&src.join("a.txt"), "alpha");

    let mut config = build_config(&src, &dest);
    config.backup.retention = 2;

    run_backup(&config);
    run_backup(&config);
    run_backup(&config);

    let app_dir = dest.join("testapp");
    let zips = app_dir_files(&app_dir)
        .iter()
        .filter(|n| n.ends_with(".zip"))
        .count();
    assert!(
        zips <= 2,
        "expected retention to cap backups at 2, got {zips}"
    );

    let history = load_history(&dest);
    assert!(history.entries_for("testapp").len() <= 2);
}

#[test]
fn retention_deletes_archive_and_sidecar_together() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dest = tmp.path().join("backups");
    write_file(&src.join("a.txt"), "alpha");

    let mut config = build_config(&src, &dest);
    config.backup.retention = 1;

    run_backup(&config);
    run_backup(&config);

    let app_dir = dest.join("testapp");
    let files = app_dir_files(&app_dir);
    let zips: Vec<&String> = files.iter().filter(|n| n.ends_with(".zip")).collect();
    let sidecars: Vec<&String> = files
        .iter()
        .filter(|n| n.ends_with(".summary.json"))
        .collect();
    assert_eq!(zips.len(), 1, "files: {files:?}");
    assert_eq!(sidecars.len(), 1, "files: {files:?}");
    assert_eq!(*sidecars[0], format!("{}.summary.json", zips[0]));

    assert_eq!(load_history(&dest).entries_for("testapp").len(), 1);
}

#[test]
fn summary_write_failure_removes_archive_and_fails_app() {
    for _attempt in 0..8 {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("backups");
        write_file(&src.join("a.txt"), "alpha");

        // Block the predicted sidecar path so the summary write must fail.
        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let app_dir = dest.join("testapp");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir(app_dir.join(format!("testapp_{ts}.zip.summary.json"))).unwrap();

        let config = build_config(&src, &dest);
        let (tx, _rx) = new_event_stream();
        let cancel = CancelFlag::new();
        let report = backup(&config, &BackupOptions::default(), tx, &cancel).unwrap();

        if report.failed() == 1 {
            assert_eq!(report.outcomes[0].result, AppResult::Failed);
            assert!(
                report.outcomes[0].detail.contains("summary"),
                "detail: {}",
                report.outcomes[0].detail
            );
            let remaining = app_dir_files(&app_dir);
            assert!(
                !remaining.iter().any(|n| n.ends_with(".zip")),
                "orphan archive left behind: {remaining:?}"
            );
            assert!(load_history(&dest).entries_for("testapp").is_empty());
            return;
        }
        // The run crossed a second boundary before packing; retry.
    }
    panic!("could not land the summary-write failure within the same second");
}

#[test]
fn backup_packs_overlapping_roots_once() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dest = tmp.path().join("backups");
    write_file(&src.join("a.txt"), "alpha");
    write_file(&src.join("sub").join("b.txt"), "beta");

    // src/sub is nested inside src: b.txt is reachable from both roots.
    let config = config_for(&dest, vec![src.clone(), src.join("sub")], Format::Zip, 0);
    let report = run_backup(&config);
    assert_eq!(report.ok(), 1);

    let app_dir = dest.join("testapp");
    let zip_file = find_archive(&app_dir, ".zip");
    let file = std::fs::File::open(app_dir.join(&zip_file)).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
    let count = names.iter().filter(|n| n.ends_with("sub/b.txt")).count();
    assert_eq!(count, 1, "duplicate entry for overlapping root: {names:?}");
}

#[cfg(unix)]
#[test]
fn backup_preserves_symlinks_zip() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dest = tmp.path().join("backups");
    write_file(&src.join("a.txt"), "alpha");
    std::os::unix::fs::symlink("a.txt", src.join("link.txt")).unwrap();

    let config = build_config(&src, &dest);
    let report = run_backup(&config);
    assert_eq!(report.ok(), 1);

    let app_dir = dest.join("testapp");
    let zip_file = find_archive(&app_dir, ".zip");
    let file = std::fs::File::open(app_dir.join(&zip_file)).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut entry = archive.by_name("src/link.txt").expect("link entry");
    assert_eq!(entry.size(), "a.txt".len() as u64);
    assert_eq!(entry.unix_mode(), Some(0o120777));
}

#[cfg(unix)]
#[test]
fn backup_preserves_symlinks_targz() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dest = tmp.path().join("backups");
    write_file(&src.join("a.txt"), "alpha");
    std::os::unix::fs::symlink("a.txt", src.join("link.txt")).unwrap();

    let config = config_for(&dest, vec![src], Format::TarGz, 0);
    let report = run_backup(&config);
    assert_eq!(report.ok(), 1);

    let app_dir = dest.join("testapp");
    let tar_file = find_archive(&app_dir, ".tar.gz");
    let file = std::fs::File::open(app_dir.join(&tar_file)).unwrap();
    let archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut saw_link = false;
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        if entry.path().unwrap().to_string_lossy() == "src/link.txt" {
            assert!(entry.header().entry_type().is_symlink());
            saw_link = true;
        }
    }
    assert!(saw_link, "symlink entry missing from tar");
}

#[test]
fn rebuild_from_fs_uses_summary_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dest = tmp.path().join("backups");
    write_file(&src.join("a.txt"), "alpha");
    write_file(&src.join("sub").join("b.txt"), "beta");

    let config = build_config(&src, &dest);
    let report = run_backup(&config);
    assert_eq!(report.ok(), 1);

    let app_dir = dest.join("testapp");
    let zip_file = find_archive(&app_dir, ".zip");
    let zip_path = app_dir.join(&zip_file);
    let summary = read_summary(&zip_path).unwrap();

    // Missing history.json -> rebuild, enriched from the sidecar.
    std::fs::remove_file(dest.join("history.json")).unwrap();
    let history = load_history(&dest);
    let entries = history.entries_for("testapp");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].file, zip_file);
    assert_eq!(entries[0].format, "zip");
    assert_eq!(entries[0].size, summary.size);
    assert_eq!(entries[0].files, 2);
    assert_eq!(
        entries[0].checksum.as_deref(),
        Some(summary.checksum.as_ref().unwrap().digest.as_str())
    );
    assert!(!entries[0].started_at.is_empty());
    assert!(!entries[0].finished_at.is_empty());
    assert_eq!(entries[0].status, "ok");

    // Corrupt history.json -> rebuild as well.
    write_file(&dest.join("history.json"), "{not valid json");
    assert_eq!(load_history(&dest).entries_for("testapp").len(), 1);

    // A legacy archive without a sidecar is still listed.
    let legacy_name = "testapp_20200101_000000.zip";
    write_file(&app_dir.join(legacy_name), "legacy-bytes");
    std::fs::remove_file(dest.join("history.json")).unwrap();
    let history = load_history(&dest);
    let entries = history.entries_for("testapp");
    let legacy = entries
        .iter()
        .find(|e| e.file == legacy_name)
        .expect("legacy archive should be listed");
    assert_eq!(legacy.files, 0);
    assert_eq!(legacy.status, "ok");
    assert!(legacy.checksum.is_none());
}
