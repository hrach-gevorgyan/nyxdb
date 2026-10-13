//! Background upkeep that runs without a request asking for it:
//!
//! - once at startup, deleting attachment temp files left by a process
//!   killed mid-write, and moving attachment bytes written before v0.1.8
//!   out of sled into files (`Db::migrate_legacy_attachments`);
//! - shortly after that, and then about once a day when the server is
//!   idle, sweeping every database's unreferenced attachment files
//!   (`Db::compact_attachments`, also run by `POST /{db}/_compact`).

use crate::routes::AppState;
use crate::storage::Db;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long after startup the first sweep runs.
const FIRST_SWEEP_DELAY: Duration = Duration::from_secs(60);
/// How often sweeps run after that.
const SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// A due sweep waits until no request has arrived for this long.
const IDLE_BEFORE_SWEEP: Duration = Duration::from_secs(5 * 60);
/// Migration progress is logged every this many attachments.
const PROGRESS_EVERY: usize = 25;

/// When the last request arrived, for "only sweep while idle".
#[derive(Default)]
pub struct Activity(AtomicU64);

impl Activity {
    pub fn touch(&self) {
        self.0.store(now_millis(), Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        Duration::from_millis(now_millis().saturating_sub(self.0.load(Ordering::Relaxed)))
    }
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default()
}

/// Every database `PUT /{db}` has created.
fn database_names(root: &sled::Db) -> Vec<String> {
    root.tree_names()
        .iter()
        .filter_map(|name| std::str::from_utf8(name).ok()?.strip_suffix("::docs").map(str::to_string))
        .collect()
}

fn open(state: &AppState, name: &str) -> Option<Db> {
    Db::open(&state.root, &state.att_root, name, &state.locks)
        .map_err(|e| tracing::error!("maintenance: can't open {name}: {e}"))
        .ok()
}

/// Moves pre-v0.1.8 attachments of every database into files. Reads fall
/// back to the sled copy until each one has moved, so serving doesn't wait.
fn migrate_all(state: &AppState) {
    let mut moved = 0;
    for name in database_names(&state.root) {
        let Some(db) = open(state, &name) else { continue };
        match db.remove_stale_attachment_temps() {
            Ok(0) => {}
            Ok(n) => tracing::info!("removed {n} unfinished attachment file(s) of {name} left by an earlier run"),
            Err(e) => tracing::error!("cleaning up unfinished attachment files of {name}: {e}"),
        }
        let total = db.legacy_attachments.len();
        if total == 0 {
            continue;
        }
        tracing::info!("moving {total} attachments of {name} from sled to files");
        match db.migrate_legacy_attachments(PROGRESS_EVERY, |done, total| {
            tracing::info!("migrated {done}/{total} attachments of {name}");
        }) {
            Ok(n) => moved += n,
            Err(e) => tracing::error!("moving the attachments of {name} to files failed (will retry at next start): {e}"),
        }
    }
    tracing::info!("attachment migration finished: {moved} moved");
}

/// Sweeps every database's attachment folder.
pub fn sweep_all(state: &AppState) {
    for name in database_names(&state.root) {
        let Some(db) = open(state, &name) else { continue };
        match db.compact_attachments(state.attachment_grace) {
            Ok(s) if s.files + s.temp_files > 0 => {
                tracing::info!("swept {name}: {} attachment file(s), {} bytes, {} temp file(s) deleted", s.files, s.bytes, s.temp_files)
            }
            Ok(_) => {}
            Err(e) => tracing::error!("sweeping the attachments of {name} failed: {e}"),
        }
    }
}

/// Starts the background upkeep: the migration, then sweeps.
pub fn spawn(state: AppState, activity: Arc<Activity>) {
    tokio::spawn(async move {
        let migrate_state = state.clone();
        let _ = tokio::task::spawn_blocking(move || migrate_all(&migrate_state)).await;
        tokio::time::sleep(FIRST_SWEEP_DELAY).await;
        loop {
            let sweep_state = state.clone();
            let _ = tokio::task::spawn_blocking(move || sweep_all(&sweep_state)).await;
            tokio::time::sleep(SWEEP_INTERVAL).await;
            while activity.idle_for() < IDLE_BEFORE_SWEEP {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    });
}
