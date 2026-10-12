//! A proxy worker's invalidation follows cache replacements for the same library directory.

use lightcraft_preview::PreviewCache;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};

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

#[derive(Clone)]
pub(crate) struct Invalidation(Arc<Current>);

impl Invalidation {
    pub(crate) fn new(cache: Arc<PreviewCache>) -> Self {
        Self(Arc::new(Mutex::new(State { cache })))
    }

    /// Library paths are resolved before attaching. A worker retains this entry even if its
    /// session closes, so a new session opening that directory joins the same invalidation.
    pub(crate) fn for_disk(dir: &Path, cache: Arc<PreviewCache>) -> Self {
        let mut libraries = LIBRARIES.get_or_init(Default::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let mut previous = current.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !Arc::ptr_eq(&previous.cache, &cache) {
                previous.cache.retire();
            }
            previous.cache = cache;
        }
        Self(current)
    }

    pub(crate) fn clear(&self) {
        let current = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        current.cache.clear();
        if current.cache.generation() == u64::MAX {
            // No active cache owns this directory. Clear on this worker before releasing
            // the lock that a replacement takes, even if nobody ever reopens the old session.
            if let Some(disk) = current.cache.disk() {
                disk.clear();
            }
        }
    }
}
