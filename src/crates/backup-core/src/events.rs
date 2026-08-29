use crossbeam_channel::{bounded, unbounded, Sender};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppResult {
    Ok,
    Skipped,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub enum Event {
    Log {
        level: LogLevel,
        msg: String,
    },
    AppStarted {
        app_id: String,
    },
    ScanUpdate {
        app_id: String,
        files_scanned: u64,
    },
    ScanDone {
        app_id: String,
        files_total: u64,
        bytes_total: u64,
    },
    FileDone {
        app_id: String,
        path: String,
        bytes_written: u64,
    },
    AppProgress {
        app_id: String,
        files_done: u64,
        files_total: u64,
        bytes_done: u64,
        bytes_total: u64,
        eta: Option<Duration>,
    },
    OverallProgress {
        apps_done: usize,
        apps_total: usize,
        bytes_done: u64,
        bytes_total: u64,
        eta: Option<Duration>,
    },
    AppFinished {
        app_id: String,
        result: AppResult,
        detail: String,
        size: u64,
        checksum: Option<String>,
    },
}

pub type Receiver = crossbeam_channel::Receiver<Event>;
pub type EventSender = Sender<Event>;

pub fn new_event_stream() -> (EventSender, Receiver) {
    let (tx, rx) = unbounded();
    (tx, rx)
}

/// A no-op sender for headless/tests that don't care about events.
pub fn null_tx() -> EventSender {
    let (tx, _rx) = unbounded();
    tx
}

/// A thread-safe cancellation flag shared with running workers.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Spawns a thread that forwards raw events to the returned receiver while
/// tracking cross-app totals and emitting throttled `OverallProgress` events.
pub fn spawn_aggregator(
    raw_rx: crossbeam_channel::Receiver<Event>,
    apps_total: usize,
) -> crossbeam_channel::Receiver<Event> {
    let (out_tx, out_rx) = bounded(4096);
    // Extra consumer handle used inside the thread to shed the oldest queued
    // events when the UI falls behind.
    let drain = out_rx.clone();

    std::thread::spawn(move || {
        let mut app_done_bytes: HashMap<String, u64> = HashMap::new();
        let mut bytes_total: u64 = 0;
        let mut apps_done: usize = 0;
        let started_at = Instant::now();
        let mut last_emit = Instant::now();

        for ev in raw_rx.iter() {
            match &ev {
                Event::ScanDone {
                    bytes_total: bt, ..
                } => {
                    bytes_total += bt;
                }
                Event::AppProgress {
                    app_id, bytes_done, ..
                } => {
                    let entry = app_done_bytes.entry(app_id.clone()).or_insert(0);
                    *entry = (*entry).max(*bytes_done);
                }
                Event::AppFinished { result, .. } if *result != AppResult::Cancelled => {
                    apps_done += 1;
                }
                _ => {}
            }

            let is_progress = matches!(
                ev,
                Event::AppProgress { .. }
                    | Event::ScanUpdate { .. }
                    | Event::OverallProgress { .. }
            );

            // Throttle overall progress by time only: formats that emit only
            // AppProgress (tar.gz/dir) would otherwise never refresh it.
            if last_emit.elapsed() >= Duration::from_millis(100) {
                last_emit = Instant::now();
                let bytes_done = app_done_bytes.values().sum();
                let eta = compute_eta(started_at, bytes_done, bytes_total);
                let _ = out_tx.try_send(Event::OverallProgress {
                    apps_done,
                    apps_total,
                    bytes_done,
                    bytes_total,
                    eta,
                });
            }

            // Forward raw event. If the UI is slow and the bound is full, drop
            // the oldest progress events but keep structural ones.
            if is_progress {
                if out_tx.try_send(ev.clone()).is_err() {
                    let _ = drain.try_recv();
                    let _ = out_tx.try_send(ev);
                }
            } else if out_tx.send(ev).is_err() {
                break; // consumer gone
            }
        }

        // Final flush
        let bytes_done = app_done_bytes.values().sum();
        let eta = compute_eta(started_at, bytes_done, bytes_total);
        let _ = out_tx.send(Event::OverallProgress {
            apps_done,
            apps_total,
            bytes_done,
            bytes_total,
            eta,
        });
    });

    out_rx
}

fn compute_eta(started_at: Instant, bytes_done: u64, bytes_total: u64) -> Option<Duration> {
    let elapsed = started_at.elapsed();
    if bytes_total == 0 || bytes_done == 0 || elapsed.as_secs() < 5 {
        return None; // "估算中…"
    }
    let rate = bytes_done as f64 / elapsed.as_secs_f64();
    if rate <= 0.0 {
        return None;
    }
    let remaining = bytes_total.saturating_sub(bytes_done) as f64 / rate;
    Some(Duration::from_secs_f64(remaining.max(0.0)))
}
