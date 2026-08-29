use crate::summary::read_summary;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub app_id: String,
    pub file: String,
    pub format: String,
    pub size: u64,
    pub files: u64,
    pub bytes_total: u64,
    pub started_at: String,
    pub finished_at: String,
    pub checksum: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<HistoryEntry>,
}

fn default_version() -> u32 {
    1
}

impl History {
    pub fn entries_for(&self, app_id: &str) -> Vec<&HistoryEntry> {
        self.entries.iter().filter(|e| e.app_id == app_id).collect()
    }
}

pub fn history_path(dest: &Path) -> PathBuf {
    dest.join("history.json")
}

/// Load history index, rebuilding from the filesystem if missing/corrupt.
pub fn load_history(dest: &Path) -> History {
    let path = history_path(dest);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(h) => h,
            Err(_) => rebuild_from_fs(dest),
        },
        Err(_) => rebuild_from_fs(dest),
    }
}

/// Rebuild the index by scanning `dest/{app}/*` for known archive extensions.
/// When a `.summary.json` sidecar exists it takes precedence for checksum,
/// file count, byte total, timestamps and status; archives without a sidecar
/// are still listed with whatever metadata can be read from disk.
pub fn rebuild_from_fs(dest: &Path) -> History {
    let mut entries = Vec::new();
    if let Ok(read) = std::fs::read_dir(dest) {
        for dir in read.flatten() {
            if !dir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let app_id = dir.file_name().to_string_lossy().into_owned();
            if let Ok(files) = std::fs::read_dir(dir.path()) {
                for f in files.flatten() {
                    let name = f.file_name().to_string_lossy().into_owned();
                    if !is_archive_name(&name) {
                        continue;
                    }
                    let archive_path = f.path();
                    let meta = f.metadata().ok();
                    let summary = read_summary(&archive_path);
                    entries.push(HistoryEntry {
                        app_id: summary
                            .as_ref()
                            .map(|s| s.app_id.clone())
                            .unwrap_or_else(|| app_id.clone()),
                        file: name.clone(),
                        format: guess_format(&name).into(),
                        size: summary
                            .as_ref()
                            .map(|s| s.size)
                            .or_else(|| meta.map(|m| m.len()))
                            .unwrap_or(0),
                        files: summary.as_ref().map(|s| s.files).unwrap_or(0),
                        bytes_total: summary.as_ref().map(|s| s.bytes_total).unwrap_or(0),
                        started_at: summary
                            .as_ref()
                            .map(|s| s.started_at.clone())
                            .unwrap_or_default(),
                        finished_at: summary
                            .as_ref()
                            .map(|s| s.finished_at.clone())
                            .unwrap_or_default(),
                        checksum: summary
                            .as_ref()
                            .and_then(|s| s.checksum.as_ref())
                            .map(|c| c.digest.clone()),
                        status: summary
                            .as_ref()
                            .map(|s| s.status.clone())
                            .unwrap_or_else(|| "ok".into()),
                    });
                }
            }
        }
    }
    entries.sort_by(|a, b| b.file.cmp(&a.file));
    History {
        version: 1,
        entries,
    }
}

fn is_archive_name(name: &str) -> bool {
    !name.ends_with(".summary.json")
        && ["zip", "tar.gz", "dir"]
            .iter()
            .any(|ext| name.ends_with(ext) && !name.ends_with(".tmp"))
}

fn guess_format(name: &str) -> &'static str {
    if name.ends_with(".tar.gz") {
        "tar.gz"
    } else if name.ends_with(".zip") {
        "zip"
    } else {
        "dir"
    }
}

/// Shared handle for serializing concurrent history writes.
#[derive(Clone, Default)]
pub struct HistoryWriter {
    lock: Arc<Mutex<()>>,
}

impl HistoryWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an entry to the index at `dest`, preserving existing entries.
    pub fn append(&self, dest: &Path, entry: HistoryEntry) -> std::io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut history = load_history(dest);
        history
            .entries
            .retain(|e| e.file != entry.file || e.app_id != entry.app_id);
        history.entries.push(entry);
        history.entries.sort_by(|a, b| b.file.cmp(&a.file));
        write_history(dest, &history)
    }

    /// Remove an entry by file name (used by delete).
    pub fn remove(&self, dest: &Path, app_id: &str, file: &str) -> std::io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut history = load_history(dest);
        history
            .entries
            .retain(|e| e.file != file || e.app_id != app_id);
        write_history(dest, &history)
    }
    /// Apply retention to the index, serialized with other history writes.
    pub fn apply_retention(
        &self,
        dest: &Path,
        keep: usize,
    ) -> std::io::Result<Vec<(String, String)>> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        apply_retention_index(dest, keep)
    }
}

fn write_history(dest: &Path, history: &History) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    let path = history_path(dest);
    let text = serde_json::to_string_pretty(history).unwrap_or_else(|_| "{}".into());
    // Atomic-ish write: temp then rename.
    let tmp = dest.join("history.json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Apply retention: keep the newest `keep` entries per app in the index file.
/// Returns files that were removed so callers can delete them from disk.
pub fn apply_retention_index(dest: &Path, keep: usize) -> std::io::Result<Vec<(String, String)>> {
    let mut history = load_history(dest);
    let mut removed = Vec::new();
    if keep > 0 {
        let mut counts: std::collections::HashMap<String, usize> = Default::default();
        history.entries.retain(|e| {
            let c = counts.entry(e.app_id.clone()).or_insert(0);
            *c += 1;
            if *c <= keep {
                true
            } else {
                removed.push((e.app_id.clone(), e.file.clone()));
                false
            }
        });
        write_history(dest, &history)?;
    }
    Ok(removed)
}
