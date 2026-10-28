//! Cache pixel validation for status queries. Builds and migrations always validate afresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

const CAPACITY: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Stamp { modified: metadata.modified().ok()?, len: metadata.len() })
}

#[derive(Default)]
pub(crate) struct UsabilityCache {
    entries: Mutex<HashMap<PathBuf, (Stamp, bool)>>,
}

impl UsabilityCache {
    /// Reuse the pixel check until the file's path, modification time or length changes.
    pub(crate) fn is_usable(&self, path: &Path) -> bool {
        self.check(path, super::is_usable)
    }

    fn check(&self, path: &Path, validate: impl FnOnce(&Path) -> bool) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(before) = stamp(path) else {
            entries.remove(path);
            return false;
        };
        if let Some((_, usable)) = entries.get(path).filter(|(cached, _)| *cached == before) {
            return *usable;
        }
        drop(entries);
        let usable = validate(path);
        // A replacement during decoding must not cache the old file's answer for the new one.
        if stamp(path) != Some(before) {
            return false;
        }
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= CAPACITY {
            entries.clear();
        }
        entries.insert(path.to_path_buf(), (before, usable));
        usable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs::{File, FileTimes};
    use std::time::Duration;

    #[test]
    fn status_reuses_validation_and_rechecks_changed_or_missing_files() {
        let root = crate::smart::tests::temp("status-cache");
        let path = root.join("proxy.lcsp");
        let cache = UsabilityCache::default();
        let calls = Cell::new(0);
        let validate = |_: &Path| {
            calls.set(calls.get() + 1);
            true
        };
        assert!(!cache.check(&path, validate));
        std::fs::write(&path, b"first").unwrap();
        assert!(cache.check(&path, validate));
        assert!(cache.check(&path, validate));
        assert_eq!(calls.get(), 1, "unchanged status queries do not decode again");
        let modified = stamp(&path).unwrap().modified + Duration::from_secs(10);
        File::options().write(true).open(&path).unwrap().set_times(FileTimes::new().set_modified(modified)).unwrap();
        assert!(cache.check(&path, validate));
        assert_eq!(calls.get(), 2, "a new modification time triggers validation");
        std::fs::write(&path, b"longer").unwrap();
        File::options().write(true).open(&path).unwrap().set_times(FileTimes::new().set_modified(modified)).unwrap();
        assert!(cache.check(&path, validate));
        assert_eq!(calls.get(), 3, "length also invalidates a coarse modification time");
        std::fs::write(root.join("other.lcsp"), b"longer").unwrap();
        assert!(cache.check(&root.join("other.lcsp"), validate));
        assert_eq!(calls.get(), 4, "another path needs its own validation");
        std::fs::remove_file(&path).unwrap();
        assert!(!cache.check(&path, validate));
        std::fs::write(&path, b"longer").unwrap();
        assert!(cache.check(&path, validate));
        assert_eq!(calls.get(), 5, "a removed file forgets its cached result");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_checks_pixels_and_recovers_after_repair() {
        let root = crate::smart::tests::temp("status-corrupt");
        let path = root.join("proxy.lcsp");
        let bytes = crate::smart::encode(&lightcraft_raster::Rgb32f::new(2, 2), None).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let cache = UsabilityCache::default();
        assert!(cache.is_usable(&path));
        let (_, _, body) = crate::smart::format::split(&bytes).unwrap();
        let offset = bytes.len() - body.len();
        let modified = stamp(&path).unwrap().modified + Duration::from_secs(10);
        let mut corrupt = bytes.clone();
        corrupt[offset + 4..bytes.len() - 2].fill(0);
        std::fs::write(&path, &corrupt).unwrap();
        File::options().write(true).open(&path).unwrap().set_times(FileTimes::new().set_modified(modified)).unwrap();
        assert!(crate::smart::is_complete(&path), "the JPEG end marker alone does not prove pixel integrity");
        assert!(!cache.is_usable(&path));
        assert!(!cache.is_usable(&path), "invalid answers are cached too");
        std::fs::write(&path, &bytes).unwrap();
        File::options().write(true).open(&path).unwrap().set_times(FileTimes::new().set_modified(modified + Duration::from_secs(10))).unwrap();
        assert!(cache.is_usable(&path));
        let modified = stamp(&path).unwrap().modified + Duration::from_secs(10);
        File::options().write(true).open(&path).unwrap().set_times(FileTimes::new().set_modified(modified)).unwrap();
        assert!(!cache.check(&path, |path| {
            std::fs::write(path, b"replaced during validation").unwrap();
            true
        }));
        std::fs::remove_dir_all(root).unwrap();
    }
}
