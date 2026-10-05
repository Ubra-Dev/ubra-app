//! Incremental, daemon-free usage accounting for Claude Code and Codex transcripts.
//!
//! The shared Rust parser estimates usage from local and remote transcripts.
//! Only aggregates cross SSH.

pub use ubra_usage::PRICING_ENTRY_COUNT;
pub use ubra_usage::transcripts::{
    Clock, ClockReading, ProviderUsage, RefreshStats, ScanPaths, SystemClock, UsageFormat,
    UsageHourAgg, UsageProvider, UsageStore, UsageTotals, dashboard,
};
mod fleet;
mod watcher;
pub(crate) use fleet::merge_fleet_usage;
pub(crate) use watcher::{TranscriptInvalidation, TranscriptWatcher};
pub type UsageSnapshot = ubra_usage::transcripts::UsageSnapshot;

mod remote;
pub(crate) use remote::{RemoteUsageViewer, watch_remote_usage};
pub use ubra_usage::transcripts::{RemoteUsageSnapshot, RemoteUsageStatus};

mod cursor;
pub(crate) use cursor::{CursorBatch, CursorRefresh};
