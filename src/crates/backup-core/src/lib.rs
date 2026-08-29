pub mod backup;
pub mod config;
pub mod events;
pub mod history;
pub mod pathres;
pub mod platform;
pub mod summary;

pub use backup::{backup, AppOutcome, BackupOptions, BackupReport};
pub use config::{generate_app_id, load_config, App, BackupSettings, CleanupMode, Config, Format};
pub use events::{spawn_aggregator, AppResult, Event, EventSender, LogLevel, Receiver};
pub use platform::Platform;
