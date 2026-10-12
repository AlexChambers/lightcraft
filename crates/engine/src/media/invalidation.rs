//! A proxy worker's invalidation follows cache replacements for the same library directory.
//! Before a worker changes its first proxy it records that on disk ([`Pending`]), so a process
//! exit before its cache clear reopens the library with a new thumbnail namespace.

use lightcraft_preview::PreviewCache;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

type Current = Mutex<State>;
struct State {
    cache: Arc<PreviewCache>,
}

impl Drop for State {
    fn drop(&mut self) {
        // A session can close while render jobs still hold the cache. Once its last
        // invalidation owner leaves, those jobs must not write into a future session's disk cache.
        self.cache.retire();
    }
}
static LIBRARIES: OnceLock<Mutex<HashMap<std::path::PathBuf, Weak<Current>>>> = OnceLock::new();

/// Marker files sit at the top of the thumbnail folder, outside the cache's two-digit shard
/// folders, so pruning and clearing never touch them. They are empty: creating one needs no
/// free data blocks, so on a full drive the proxy writes still fail one by one, not the run.
const MARKER_PREFIX: &str = "smart-previews-changing-";
/// Markers of workers still running in this process: opening their library keeps them.
static RUNNING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[derive(Clone)]
pub(crate) struct Invalidation(Arc<Current>);

impl Invalidation {
    pub(crate) fn new(cache: Arc<PreviewCache>) -> Self {
        Self(Arc::new(Mutex::new(State { cache })))
    }

    /// Library paths are resolved before attaching. A worker retains this entry even if its
    /// session closes, so a new session opening that directory joins the same invalidation.
    pub(crate) fn for_disk(dir: &Path, cache: Arc<PreviewCache>) -> Self {
        let mut libraries = LIBRARIES.get_or_init(Default::default).lock().unwrap_or_else(PoisonError::into_inner);
        libraries.retain(|_, value| value.strong_count() > 0);
        let current = match libraries.get(dir).and_then(Weak::upgrade) {
            Some(current) => current,
            None => {
                let current = Arc::new(Mutex::new(State { cache: cache.clone() }));
                libraries.insert(dir.to_path_buf(), Arc::downgrade(&current));
                current
            }
        };
        {
            let mut previous = current.lock().unwrap_or_else(PoisonError::into_inner);
            let different_namespace = previous.cache.disk().zip(cache.disk()).is_some_and(|(old, new)| old.namespace() != new.namespace());
            if !Arc::ptr_eq(&previous.cache, &cache) && !different_namespace {
                previous.cache.retire();
            }
            previous.cache = cache;
        }
        Self(current)
    }

    pub(crate) fn clear(&self) {
        let current = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        current.cache.clear();
        if current.cache.generation() == u64::MAX {
            // No active cache owns this directory. Clear on this worker before releasing
            // the lock that a replacement takes, even if nobody ever reopens the old session.
            if let Some(disk) = current.cache.disk() {
                disk.clear();
            }
        }
    }

    /// The thumbnail changes of one proxy run (see [`Pending`]).
    pub(crate) fn pending(&self) -> Pending {
        Pending { invalidation: self.clone(), marker: None, recorded: false, changed: false }
    }
}

/// One run of Build / Discard Smart Previews or of the look refresh. It clears the thumbnail
/// cache once, when it ends, if it changed a proxy. [`Pending::before_change`] first creates a
/// durable marker in the thumbnail folder, which is removed after that clear. If the process
/// ends in between, the marker stays: the next opening finds it ([`interrupted_runs`]) and
/// takes a new thumbnail namespace before reading any thumbnail.
pub(crate) struct Pending {
    invalidation: Invalidation,
    marker: Option<PathBuf>,
    recorded: bool,
    changed: bool,
}

impl Pending {
    /// Call before each proxy write or removal; only the first call does I/O. On an error no
    /// proxy may change, because the change could outlive the cache clear.
    pub(crate) fn before_change(&mut self) -> Result<(), String> {
        if self.recorded {
            return Ok(());
        }
        let dir = self.invalidation.0.lock().unwrap_or_else(PoisonError::into_inner).cache.disk().map(|disk| disk.dir().to_path_buf());
        // (a memory-only cache does not outlive the process)
        if let Some(dir) = dir {
            static N: AtomicU64 = AtomicU64::new(0);
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            let marker = dir.join(format!("{MARKER_PREFIX}{}-{stamp}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            // registered before it exists: an opening in this process never takes it for a leftover
            RUNNING.lock().unwrap_or_else(PoisonError::into_inner).push(marker.clone());
            let written = std::fs::create_dir_all(&dir).and_then(|()| std::fs::File::create(&marker)).and_then(|file| file.sync_all());
            if let Err(e) = written {
                finished(&marker);
                return Err(format!("could not record the thumbnail cache change in {}: {e}", dir.display()));
            }
            lightcraft_catalog::safe_file::sync_dir(&dir);
            self.marker = Some(marker);
        }
        self.recorded = true;
        Ok(())
    }

    /// A proxy was written or removed: the thumbnail cache is cleared when the run ends.
    pub(crate) fn changed(&mut self) {
        self.changed = true;
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if self.changed {
            self.invalidation.clear();
        }
        // Only after the clear. A marker left behind costs a new namespace at the next opening.
        if let Some(marker) = self.marker.take() {
            if let Err(e) = std::fs::remove_file(&marker)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("thumbnail cache: {}: {e}", marker.display());
            }
            finished(&marker);
        }
    }
}

fn finished(marker: &Path) {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner).retain(|m| m != marker);
}

/// The markers in the thumbnail folder `thumbs` of runs that ended without clearing the
/// cache: not those of runs still going in this process.
pub(crate) fn interrupted_runs(thumbs: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(thumbs) else { return Vec::new() };
    let running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|name| name.starts_with(MARKER_PREFIX)))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|path| !running.contains(path))
        .collect()
}
