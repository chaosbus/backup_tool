//! Per-backup JSON summary sidecar.
//!
//! After every successful backup a `<archive-name>.summary.json` file is
//! written next to the archive itself. It records how the archive was built
//! and where each root should be restored, so the archive is self-describing
//! even if `history.json` is lost or corrupted.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecksumInfo {
    pub algorithm: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupPathInfo {
    /// Raw path as written in the config.
    pub raw: String,
    /// Resolved absolute path (empty when an environment variable was undefined).
    pub resolved: String,
    /// Whether the path existed and was actually archived.
    pub existed: bool,
    /// Root prefix used inside the archive for this path.
    pub archive_root: String,
    /// Where this root should be restored (original location).
    pub restore_to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSummary {
    pub version: u32,
    pub app_id: String,
    pub app_name: String,
    pub platform: String,
    pub archive: String,
    pub format: String,
    pub size: u64,
    pub files: u64,
    pub bytes_total: u64,
    pub started_at: String,
    pub finished_at: String,
    pub checksum: Option<ChecksumInfo>,
    pub status: String,
    pub excludes: Vec<String>,
    pub paths: Vec<BackupPathInfo>,
}

/// Sidecar path for an archive, e.g. `a.zip` -> `a.zip.summary.json`.
pub fn summary_path_for(archive: &Path) -> PathBuf {
    let mut name = archive
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".summary.json");
    archive.with_file_name(name)
}

/// Read the sidecar for `archive` when present and parseable.
pub fn read_summary(archive: &Path) -> Option<BackupSummary> {
    let text = std::fs::read_to_string(summary_path_for(archive)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write the summary atomically: temp file + fsync + rename.
pub fn write_summary(archive: &Path, summary: &BackupSummary) -> std::io::Result<()> {
    let final_path = summary_path_for(archive);
    let mut tmp_name = final_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp_path = final_path.with_file_name(tmp_name);

    let mut file = std::fs::File::create(&tmp_path)?;
    if let Err(err) = serde_json::to_writer_pretty(&mut file, summary) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(std::io::Error::other(err));
    }
    if let Err(err) = file.write_all(b"\n").and_then(|_| file.sync_all()) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    drop(file);
    std::fs::rename(&tmp_path, &final_path)
}
