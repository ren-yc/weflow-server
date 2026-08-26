//! File-system-event-driven sync trigger (cross-platform), WeChat-flavored.
//!
//! Watches `<account>/db_storage` recursively (session/ and message/
//! subdirectories) with notify — ReadDirectoryChangesW on Windows — and
//! debounces event bursts before running `AccountSync::poll_once`, exactly
//! like WeFlow's native monitor (350 ms debounce) plus its 500 ms second
//! pass tolerance (handled by SQLite's own ordering + watermarks).
//!
//! Reliability: watch backends can silently drop events, so a slow fallback
//! poll (default 30 s) re-runs the sync — `poll_once` is cheap when nothing
//! changed (metadata-only skip in the mirror). A dead watcher (directory
//! recreated) is re-attached on each fallback tick.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use notify::{RecursiveMode, RecommendedWatcher};
use notify_debouncer_mini::{
    new_debouncer_opt, Config as DebounceConfig, DebounceEventResult, Debouncer,
};
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

use super::AccountSync;

/// Watch behavior for one account.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// How long the watcher waits for an event burst to quiet down before
    /// triggering a sync (WeFlow-aligned; batch mode worst case ~2x this).
    pub debounce: Duration,
    /// Slow fallback poll interval; `None` disables it.
    pub fallback: Option<Duration>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(350),
            fallback: None,
        }
    }
}

/// Re-attach retry interval when the slow fallback poll is disabled.
const REATTACH_INTERVAL: Duration = Duration::from_secs(10);

/// Only database writes matter: `*.db`, `*-wal`, `*-shm` (WAL files are
/// preallocated to a fixed 4 MB; their SIZE never changes but mtime does).
fn is_relevant(name: &OsStr) -> bool {
    let n = name.to_string_lossy().to_ascii_lowercase();
    n.ends_with(".db") || n.ends_with("-wal") || n.ends_with("-shm")
}

/// Notify thread -> watch task messages.
enum WatchMsg {
    Changed(PathBuf),
    BackendError,
}

/// Run the watch loop for one account until `shutdown` turns true.
pub async fn spawn(
    account: Arc<Mutex<AccountSync>>,
    watch_dir: PathBuf,
    cfg: WatchConfig,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<WatchMsg>();
    let mut watcher = rebuild_watcher(&tx, &watch_dir, cfg.debounce);
    let mut iv = tokio::time::interval(cfg.fallback.unwrap_or(REATTACH_INTERVAL));
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => { drop(watcher); break; }
            msg = rx.recv() => {
                match msg {
                    Some(WatchMsg::Changed(p)) => {
                        while let Ok(rest) = rx.try_recv() {
                            if matches!(rest, WatchMsg::BackendError) {
                                watcher = None;
                            }
                        }
                        tracing::debug!(path = ?p, "watch event -> sync");
                        sync_once(account.clone()).await;
                    }
                    Some(WatchMsg::BackendError) => {
                        tracing::warn!("watcher backend error; re-attaching on next tick");
                        watcher = None;
                    }
                    None => {}
                }
            }
            _ = iv.tick() => {
                if watcher.is_none() {
                    watcher = rebuild_watcher(&tx, &watch_dir, cfg.debounce);
                }
                if cfg.fallback.is_some() {
                    sync_once(account.clone()).await;
                }
            }
        }
    }
    Ok(())
}

async fn sync_once(account: Arc<Mutex<AccountSync>>) {
    match tokio::task::spawn_blocking(move || account.lock().poll_once()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!("watch sync error: {e:#}"),
        Err(e) => tracing::error!("sync task panicked: {e}"),
    }
}

/// Create the debouncer and attach the watch; `None` on failure (the
/// fallback tick retries).
fn rebuild_watcher(
    tx: &mpsc::UnboundedSender<WatchMsg>,
    watch_dir: &Path,
    debounce: Duration,
) -> Option<Debouncer<RecommendedWatcher>> {
    let handler_tx = tx.clone();
    let handler = move |res: DebounceEventResult| match res {
        Ok(events) => {
            for e in events {
                if is_relevant(e.path.file_name().unwrap_or_default()) {
                    let _ = handler_tx.send(WatchMsg::Changed(e.path));
                }
            }
        }
        Err(_) => {
            let _ = handler_tx.send(WatchMsg::BackendError);
        }
    };
    let mut debouncer = match new_debouncer_opt(
        DebounceConfig::default().with_timeout(debounce).with_batch_mode(true),
        handler,
    ) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("create watcher failed: {e}");
            return None;
        }
    };
    if let Err(e) = debouncer.watcher().watch(watch_dir, RecursiveMode::Recursive) {
        tracing::warn!(
            "watch {} failed: {e}（目录可能不存在，兜底轮询将重试）",
            watch_dir.display()
        );
        return None;
    }
    tracing::info!("watching {} (debounce {:?})", watch_dir.display(), debounce);
    Some(debouncer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_filter() {
        assert!(is_relevant(OsStr::new("session.db")));
        assert!(is_relevant(OsStr::new("message_0.db")));
        assert!(is_relevant(OsStr::new("session.db-wal")));
        assert!(is_relevant(OsStr::new("session.db-shm")));
        assert!(is_relevant(OsStr::new("SESSion.DB-WAL"))); // case-insensitive
        assert!(!is_relevant(OsStr::new("session.db.bak")));
        assert!(!is_relevant(OsStr::new("db_storage")));
    }
}