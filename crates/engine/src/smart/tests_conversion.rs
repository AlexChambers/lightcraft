//! A format conversion that ends with the process, while the disk thumbnail cache holds
//! thumbnails rendered from the proxies it replaces.

use super::*;
use crate::{Session, media};
use lightcraft_catalog::{PhotoId, Source};
use lightcraft_preview::{Hash128, PreviewCache};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};

const EXIT_CODE: i32 = 82;

fn originals(root: &Path) -> [PathBuf; 2] {
    [root.join("first.tif"), root.join("second.tif")]
}

fn set_online(root: &Path, online: bool) {
    for path in originals(root) {
        let offline = path.with_extension("offline");
        let (from, to) = if online { (offline, path) } else { (path, offline) };
        if from.exists() {
            std::fs::rename(from, to).unwrap();
        }
    }
}

fn photo_id(s: &Session, name: &str) -> PhotoId {
    s.catalog.photos().find(|p| matches!(&p.source, Source::File { path } if path.ends_with(name))).unwrap().id
}

fn ids(s: &Session) -> [u64; 2] {
    [photo_id(s, "first.tif").0, photo_id(s, "second.tif").0]
}

/// Two photos with Compact proxies. The first one's thumbnail, rendered from its proxy while
/// the original was offline, is in the disk cache. Both originals are online again.
fn write_originals(dir: &Path) {
    for (path, scale) in originals(dir).iter().zip([1.0f32, 0.55]) {
        let mut image = Rgb32f::new(96, 64);
        for (i, p) in image.data.iter_mut().enumerate() {
            let x = (i % 96) as f32 / 96.0;
            let y = (i / 96) as f32 / 64.0;
            *p = [x * 2.2 * scale - 0.02, y * 0.7 + 0.01, (x + y) * 0.6 * scale];
        }
        let encoded = lightcraft_codecs::EncodeImage::new(96, 64, 3, lightcraft_codecs::Samples::F32(image.data.as_flattened()));
        let bytes = lightcraft_codecs::encode_tiff(&encoded, lightcraft_codecs::TiffCompression::Deflate, &Default::default()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

fn fixture(tag: &str) -> PathBuf {
    let root = super::tests::temp(tag);
    write_originals(&root);
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    s.execute("library.import", &json!({"paths": originals(&root)})).unwrap();
    assert_eq!(s.catalog.photos().count(), 2);
    s.execute("library.smartPreviews", &json!({"format": "compact", "ids": ids(&s)})).unwrap();
    set_online(&root, false);
    s.media.clear_sources();
    let job = s.thumb_job(photo_id(&s, "first.tif"), 128).unwrap();
    let key = job.cache.as_ref().unwrap().1;
    let thumbnail = job.run().rendered.unwrap().image;
    s.media.rendered.put(key, Arc::new(thumbnail));
    let saved = s.media.rendered.disk().unwrap().get(key).unwrap();
    std::fs::write(root.join("old-thumbnail.rgba"), saved.as_bytes()).unwrap();
    set_online(&root, true);
    s.close_library().unwrap();
    root
}

/// The first photo's proxy is Lossless, and its thumbnail matches a render of that proxy.
fn assert_fresh_thumbnail(s: &mut Session, root: &Path) {
    let id = photo_id(s, "first.tif");
    let photo = s.catalog.photo(id).unwrap().clone();
    let path = file_path(s.media.smart_dir.as_ref().unwrap(), &photo);
    assert_eq!(header(&path).unwrap().0, Format::Lossless, "the first proxy was converted before the exit");
    let mut fresh = s.thumb_job(id, 128).unwrap();
    fresh.source = media::SourceRef::Loaded(Box::new(load(&path).unwrap()));
    // A separate memory cache keeps the reference on the thumbnail CPU path.
    fresh.cache = Some((Arc::new(PreviewCache::memory(1_000_000)), Hash128(0)));
    let reference = fresh.run().rendered.unwrap().image;
    let encoded = lightcraft_preview::decode_jpeg(&lightcraft_preview::encode_jpeg(&reference).unwrap()).unwrap();
    let old = std::fs::read(root.join("old-thumbnail.rgba")).unwrap();
    assert_ne!(old, encoded.as_bytes(), "fixture must tell Compact and Lossless apart under the cache encoding");
    let job = s.thumb_job(id, 128).unwrap();
    let hit = job.cache.as_ref().is_some_and(|(cache, key)| cache.get(*key).is_some());
    let actual = job.run().rendered.unwrap().image;
    // A cache miss returns the uncompressed render, a disk hit returns JPEG-decoded pixels.
    let correct = actual.data == reference.data || actual.data == encoded.data;
    let differing = actual.data.iter().zip(&encoded.data).filter(|(a, b)| a != b).count();
    assert!(
        correct,
        "stale Compact thumbnail after the interrupted conversion: cache_hit={hit}, same_as_old={}, differing_pixels={differing}",
        actual.as_bytes() == old
    );
}

/// Photo `name`'s thumbnail matches a render of its current proxy (originals offline).
fn assert_thumbnail_matches_proxy(s: &mut Session, name: &str) {
    let id = photo_id(s, name);
    let photo = s.catalog.photo(id).unwrap().clone();
    let path = file_path(s.media.smart_dir.as_ref().unwrap(), &photo);
    let mut fresh = s.thumb_job(id, 128).unwrap();
    fresh.source = media::SourceRef::Loaded(Box::new(load(&path).unwrap()));
    fresh.cache = Some((Arc::new(PreviewCache::memory(1_000_000)), Hash128(0)));
    let reference = fresh.run().rendered.unwrap().image;
    let encoded = lightcraft_preview::decode_jpeg(&lightcraft_preview::encode_jpeg(&reference).unwrap()).unwrap();
    let job = s.thumb_job(id, 128).unwrap();
    let hit = job.cache.as_ref().is_some_and(|(cache, key)| cache.get(*key).is_some());
    let actual = job.run().rendered.unwrap().image;
    let differing = actual.data.iter().zip(&encoded.data).filter(|(a, b)| a != b).count();
    assert!(
        actual.data == reference.data || actual.data == encoded.data,
        "{name}: thumbnail of a replaced proxy: cache_hit={hit}, differing_pixels={differing}"
    );
}

/// Files the thumbnail cache does not own, left at the top of its folder.
fn loose_files(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join("library/thumbs")) else { return Vec::new() };
    entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect()
}

#[test]
#[ignore = "child process for the interrupted conversion regression"]
fn interrupted_conversion_child() {
    let root = PathBuf::from(std::env::var_os("LC_SMART_CONVERSION_ROOT").unwrap());
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    let loader = s.media.file_loader.clone().unwrap();
    let first_read = AtomicBool::new(false);
    s.media.file_loader = Some(Arc::new(move |path, edge| {
        if path.ends_with("second.tif") {
            // The first photo's Lossless proxy is durable; the build's final clear has not run.
            assert!(first_read.load(Ordering::SeqCst), "the first photo converts first");
            std::process::exit(EXIT_CODE);
        }
        first_read.store(true, Ordering::SeqCst);
        loader(path, edge)
    }));
    let ids = ids(&s);
    s.execute("library.smartPreviews", &json!({"format": "lossless", "ids": ids})).unwrap();
    panic!("the build finished without reading the second original");
}

#[test]
fn regression709_interrupted_conversion_reopens_without_stale_thumbnails() {
    let root = fixture("conversion-exit");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "smart::tests_conversion::interrupted_conversion_child", "--ignored"])
        .env("LC_SMART_CONVERSION_ROOT", &root)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(EXIT_CODE));
    set_online(&root, false);
    for _ in 0..2 {
        let mut s = Session::new().with_fs();
        s.open_library(root.join("library"), false).unwrap();
        assert_fresh_thumbnail(&mut s, &root);
        assert!(loose_files(&root).is_empty(), "{:?}", loose_files(&root));
    }
    // A retry with the originals still offline changes no proxy. It must not bring the old
    // thumbnail back, and a completed build leaves nothing behind in the thumbnail folder.
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    let ids = ids(&s);
    let r = s.execute("library.smartPreviews", &json!({"format": "lossless", "ids": ids})).unwrap();
    assert_eq!((r["built"].as_u64(), r["staleKept"].as_u64()), (Some(2), Some(1)), "{r}");
    assert_fresh_thumbnail(&mut s, &root);
    s.close_library().unwrap();
    drop(s);
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    assert_fresh_thumbnail(&mut s, &root);
    assert!(loose_files(&root).is_empty(), "{:?}", loose_files(&root));
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn conversion_is_refused_when_the_thumbnail_folder_cannot_record_it() {
    let root = fixture("conversion-unrecorded");
    let thumbs = root.join("library/thumbs");
    std::fs::rename(&thumbs, root.join("thumbs-aside")).unwrap();
    std::fs::write(&thumbs, b"not a folder").unwrap();
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    let ids = ids(&s);
    let error = s.execute("library.smartPreviews", &json!({"format": "lossless", "ids": ids})).unwrap_err().to_string();
    assert!(error.contains("thumbnail"), "{error}");
    for id in ids {
        let photo = s.catalog.photo(PhotoId(id)).unwrap();
        assert_eq!(header(&file_path(s.media.smart_dir.as_ref().unwrap(), photo)).unwrap().0, Format::Compact, "no proxy changed");
    }
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reopening_during_a_running_build_keeps_its_marker() {
    use std::sync::{Mutex, mpsc};
    use std::time::{Duration, Instant};
    let root = fixture("conversion-running");
    let library = root.join("library");
    let mut s = Session::new().with_fs();
    s.open_library(&library, false).unwrap();
    let generation = s.smart_previews_cache_generation;
    let (entered, waiting) = mpsc::channel();
    let (release, blocked) = mpsc::channel::<()>();
    let blocked = Mutex::new(blocked);
    let loader = s.media.file_loader.clone().unwrap();
    s.media.file_loader = Some(Arc::new(move |path, edge| {
        if path.ends_with("second.tif") {
            entered.send(()).unwrap();
            blocked.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
        }
        loader(path, edge)
    }));
    let ids = ids(&s);
    s.execute("library.smartPreviews", &json!({"format": "lossless", "ids": ids, "background": true})).unwrap();
    waiting.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(loose_files(&root).len(), 1, "the build records its change before the first proxy");
    s.open_library(&library, false).unwrap();
    s.media.file_loader = Session::new().with_fs().media.file_loader;
    assert_eq!(s.smart_previews_cache_generation, generation, "a running build is not an interrupted one");
    assert_eq!(loose_files(&root).len(), 1, "opening keeps the running build's marker");
    release.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while !s.preview_build.as_ref().unwrap().finished.load(Ordering::Acquire) {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(loose_files(&root).is_empty(), "the finished build cleared the cache, then removed its marker");
    set_online(&root, false);
    s.open_library(&library, false).unwrap();
    assert_eq!(s.smart_previews_cache_generation, generation);
    assert_fresh_thumbnail(&mut s, &root);
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

/// Blocks the build in the second original's load (after the first proxy converted) until released.
#[allow(clippy::type_complexity)]
fn block_second(s: &mut Session) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    use std::sync::{Mutex, mpsc};
    let (entered, waiting) = mpsc::channel();
    let (release, blocked) = mpsc::channel::<()>();
    let blocked = Mutex::new(blocked);
    let loader = s.media.file_loader.clone().unwrap();
    s.media.file_loader = Some(Arc::new(move |path, edge| {
        if path.ends_with("second.tif") {
            entered.send(()).unwrap();
            blocked.lock().unwrap().recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        }
        loader(path, edge)
    }));
    (waiting, release)
}

fn wait_for_build(s: &Session) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !s.preview_build.as_ref().unwrap().finished.load(Ordering::Acquire) {
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn reopening_through_another_path_spelling_keeps_the_running_build() {
    let root = fixture("conversion-spelling");
    std::fs::create_dir(root.join("aside")).unwrap();
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    let generation = s.smart_previews_cache_generation;
    let (waiting, release) = block_second(&mut s);
    let ids = ids(&s);
    s.execute("library.smartPreviews", &json!({"format": "lossless", "ids": ids, "background": true})).unwrap();
    waiting.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert_eq!(loose_files(&root).len(), 1, "the build records its change before the first proxy");
    // The same library, reached through another spelling of its path.
    s.open_library(root.join("aside/../library"), false).unwrap();
    s.media.file_loader = Session::new().with_fs().media.file_loader;
    assert_eq!(loose_files(&root).len(), 1, "opening keeps the running build's marker");
    assert_eq!(s.smart_previews_cache_generation, generation, "a running build is not an interrupted one");
    // The grid shows the second photo from its Compact proxy while its original is offline.
    let second = root.join("second.tif");
    std::fs::rename(&second, second.with_extension("offline")).unwrap();
    s.media.clear_sources();
    let job = s.thumb_job(photo_id(&s, "second.tif"), 128).unwrap();
    let key = job.cache.as_ref().unwrap().1;
    let shown = job.run().rendered.unwrap().image;
    s.media.rendered.put(key, Arc::new(shown));
    std::fs::rename(second.with_extension("offline"), &second).unwrap();
    release.send(()).unwrap();
    wait_for_build(&s);
    assert!(loose_files(&root).is_empty());
    set_online(&root, false);
    s.media.clear_sources();
    // The finished build cleared the cache this session uses.
    assert_thumbnail_matches_proxy(&mut s, "second.tif");
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

/// Fails every `prefs.json` save (an I/O error, or a file that could not be read at opening).
struct FailPrefs(lightcraft_catalog::store::FsStore);

impl lightcraft_catalog::store::Store for FailPrefs {
    fn read(&mut self, name: &str) -> std::io::Result<Option<Vec<u8>>> {
        self.0.read(name)
    }
    fn write_atomic(&mut self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        if name == "prefs.json" {
            return Err(std::io::Error::other("injected prefs.json failure"));
        }
        self.0.write_atomic(name, bytes)
    }
    fn append(&mut self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.0.append(name, bytes)
    }
    fn truncate(&mut self, name: &str, len: u64) -> std::io::Result<()> {
        self.0.truncate(name, len)
    }
    fn describe(&self) -> String {
        self.0.describe()
    }
}

#[test]
#[ignore = "child process for the failed preferences save regression"]
fn failed_preferences_child() {
    use lightcraft_catalog::store::FsStore;
    let root = PathBuf::from(std::env::var_os("LC_SMART_PREFS_ROOT").unwrap());
    let dir = root.join("library");
    let mut s = Session::new().with_fs();
    let stores = crate::library::LibraryStores {
        dir: dir.clone(),
        catalog: Box::new(FsStore::open(&dir).unwrap()),
        files: Box::new(FailPrefs(FsStore::open(&dir).unwrap())),
        on_disk: true,
    };
    s.open_library_in(stores, false).unwrap();
    // This session could not save the namespace it took. The grid shows the first photo from
    // its Compact proxy while the original is offline.
    set_online(&root, false);
    s.media.clear_sources();
    let job = s.thumb_job(photo_id(&s, "first.tif"), 128).unwrap();
    let key = job.cache.as_ref().unwrap().1;
    let shown = job.run().rendered.unwrap().image;
    s.media.rendered.put(key, Arc::new(shown));
    set_online(&root, true);
    // A conversion replaces the first proxy, then the process ends while reading the second original.
    let loader = s.media.file_loader.clone().unwrap();
    s.media.file_loader = Some(Arc::new(move |path, edge| {
        if path.ends_with("second.tif") {
            std::process::exit(EXIT_CODE);
        }
        loader(path, edge)
    }));
    let ids = ids(&s);
    s.execute("library.smartPreviews", &json!({"format": "lossless", "ids": ids})).unwrap();
    panic!("the build finished without reading the second original");
}

#[test]
fn regression709_failed_preferences_save_at_opening_still_isolates_the_next_interrupted_build() {
    let root = fixture("conversion-prefs");
    // A run that ended before its cache clear left its marker.
    std::fs::write(root.join("library/thumbs/smart-previews-changing-1-2-3"), b"").unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "smart::tests_conversion::failed_preferences_child", "--ignored"])
        .env("LC_SMART_PREFS_ROOT", &root)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(EXIT_CODE));
    set_online(&root, false);
    for _ in 0..2 {
        let mut s = Session::new().with_fs();
        s.open_library(root.join("library"), false).unwrap();
        assert_fresh_thumbnail(&mut s, &root);
        assert!(loose_files(&root).is_empty(), "{:?}", loose_files(&root));
    }
    std::fs::remove_dir_all(root).unwrap();
}

/// A small disk image (macOS), detached and deleted when dropped.
#[cfg(target_os = "macos")]
struct Volume {
    base: PathBuf,
    mount: PathBuf,
}

#[cfg(target_os = "macos")]
impl Volume {
    /// `None` when this machine cannot create or attach disk images (the test is then skipped).
    fn new(tag: &str, filesystem: &str) -> Option<Volume> {
        use std::process::Command;
        let base = super::tests::temp(tag);
        let (image, mount) = (base.join("volume.dmg"), base.join("mnt"));
        std::fs::create_dir(&mount).unwrap();
        let created = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "40m", "-fs", filesystem, "-volname", "lcfull", "-type", "UDIF"])
            .arg(&image)
            .status()
            .is_ok_and(|s| s.success());
        let attached = created
            && Command::new("hdiutil")
                .args(["attach", "-quiet", "-nobrowse", "-noverify", "-noautoopen", "-mountpoint"])
                .arg(&mount)
                .arg(&image)
                .status()
                .is_ok_and(|s| s.success());
        if !attached {
            eprintln!("skipped: hdiutil could not create and attach a {filesystem} disk image");
            let _ = std::fs::remove_dir_all(&base);
            return None;
        }
        Some(Volume { base, mount })
    }

    /// Write until the volume refuses even one byte, as when something else fills the drive.
    /// APFS can still have room in a folder's own records, so `folder` (where the library is
    /// about to write) also gets empty files until it refuses one.
    fn fill(&self, folder: &Path) {
        use std::io::Write;
        let mut filler = std::fs::OpenOptions::new().create(true).append(true).open(self.mount.join("filler")).unwrap();
        for size in [1 << 20, 4096, 512, 1] {
            let chunk = vec![0u8; size];
            while filler.write_all(&chunk).is_ok() {}
        }
        let _ = filler.sync_all();
        let full = (0..200_000).any(|i| std::fs::File::create(folder.join(format!("filler-{i}"))).and_then(|f| f.sync_all()).is_err());
        assert!(full, "the volume never filled up");
    }
}

#[cfg(target_os = "macos")]
impl Drop for Volume {
    fn drop(&mut self) {
        let _ = std::process::Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&self.mount).status();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// The library drive is full. Discard Smart Previews, the in-app way to get space back, still
/// removes the proxies, and a Lossless conversion into a proxy folder on another drive with room
/// (what the storage dialog suggests) still converts, without serving thumbnails of the old proxies.
#[test]
#[cfg(target_os = "macos")]
fn regression709_full_library_drive_still_discards_and_converts_elsewhere() {
    for filesystem in ["APFS", "HFS+"] {
        let tag = filesystem.trim_end_matches('+').to_lowercase();
        let Some(volume) = Volume::new(&format!("full-volume-{tag}"), filesystem) else { continue };
        let elsewhere = super::tests::temp(&format!("full-volume-elsewhere-{tag}"));
        write_originals(&elsewhere);
        // Library A keeps its Lossless proxies in the library, on the drive that fills up.
        let mut a = Session::new().with_fs();
        a.open_library(volume.mount.join("a"), false).unwrap();
        a.execute("library.import", &json!({"paths": originals(&elsewhere)})).unwrap();
        let a_ids = ids(&a);
        a.execute("library.smartPreviews", &json!({"format": "lossless", "ids": a_ids})).unwrap();
        // Library B keeps Compact proxies on another drive; its thumbnails, rendered from them
        // while the originals were offline, are in its thumbnail cache on the full drive.
        let mut b = Session::new().with_fs();
        b.open_library(volume.mount.join("b"), false).unwrap();
        b.execute("library.import", &json!({"paths": originals(&elsewhere)})).unwrap();
        std::fs::create_dir(elsewhere.join("proxies")).unwrap();
        b.execute("library.smartPreviewsLocation", &json!({"path": elsewhere.join("proxies"), "existing": "move"})).unwrap();
        let b_ids = ids(&b);
        b.execute("library.smartPreviews", &json!({"format": "compact", "ids": b_ids})).unwrap();
        set_online(&elsewhere, false);
        for s in [&mut a, &mut b] {
            s.media.clear_sources();
            for name in ["first.tif", "second.tif"] {
                let job = s.thumb_job(photo_id(s, name), 128).unwrap();
                let key = job.cache.as_ref().unwrap().1;
                let shown = job.run().rendered.unwrap().image;
                s.media.rendered.put(key, Arc::new(shown));
            }
        }
        set_online(&elsewhere, true);
        volume.fill(&volume.mount.join("b/thumbs"));
        let converted = b.execute("library.smartPreviews", &json!({"format": "lossless", "ids": b_ids}));
        volume.fill(&volume.mount.join("a/thumbs"));
        let discarded = a.execute("library.smartPreviews", &json!({"discard": true, "ids": a_ids}));
        assert!(converted.is_ok() && discarded.is_ok(), "{filesystem}: conversion {converted:?}, discard {discarded:?}");
        assert_eq!(discarded.unwrap()["removed"], json!(2), "{filesystem}");
        assert_eq!(stats(a.media.smart_dir.as_ref().unwrap()).0, 0, "{filesystem}: the discard freed the proxies");
        for id in b_ids {
            let photo = b.catalog.photo(PhotoId(id)).unwrap();
            assert_eq!(header(&file_path(b.media.smart_dir.as_ref().unwrap(), photo)).unwrap().0, Format::Lossless, "{filesystem}");
        }
        set_online(&elsewhere, false);
        b.media.clear_sources();
        for name in ["first.tif", "second.tif"] {
            assert_thumbnail_matches_proxy(&mut b, name);
        }
        drop((a, b));
        drop(volume);
        std::fs::remove_dir_all(elsewhere).unwrap();
    }
}

static FAIL_MARKERS: std::sync::Mutex<Vec<(PathBuf, std::io::ErrorKind, usize)>> = std::sync::Mutex::new(Vec::new());

/// The next `count` markers in the library's thumbnail folder fail with `kind`.
fn fail_markers(root: &Path, kind: std::io::ErrorKind, count: usize) {
    let thumbs = crate::media::thumbnail_folder(&root.join("library/thumbs"));
    FAIL_MARKERS.lock().unwrap().push((thumbs, kind, count));
}

/// Test seam in marker creation (an empty file still needs space on APFS and HFS+).
pub(crate) fn injected_marker_failure(dir: &Path) -> Option<std::io::Error> {
    let mut failures = FAIL_MARKERS.lock().unwrap();
    let (_, kind, left) = failures.iter_mut().find(|(thumbs, _, left)| thumbs == dir && *left > 0)?;
    *left -= 1;
    Some(std::io::Error::from(*kind))
}

/// Thumbnail files in the cache's shard folders.
fn cached_thumbnails(root: &Path) -> usize {
    let Ok(shards) = std::fs::read_dir(root.join("library/thumbs")) else { return 0 };
    shards.flatten().filter(|e| e.path().is_dir()).map(|e| std::fs::read_dir(e.path()).map_or(0, |files| files.count())).sum()
}

#[test]
fn a_full_thumbnail_drive_is_cleared_before_the_first_replacement_then_recorded() {
    let root = fixture("conversion-full-once");
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    assert!(cached_thumbnails(&root) > 0);
    fail_markers(&root, std::io::ErrorKind::StorageFull, 1);
    let (waiting, release) = block_second(&mut s);
    let ids = ids(&s);
    s.execute("library.smartPreviews", &json!({"format": "lossless", "ids": ids, "background": true})).unwrap();
    waiting.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert_eq!(cached_thumbnails(&root), 0, "the cache was cleared before the first proxy was replaced");
    assert_eq!(loose_files(&root).len(), 1, "the clear made room for the marker");
    release.send(()).unwrap();
    wait_for_build(&s);
    assert!(loose_files(&root).is_empty());
    drop(s);
    set_online(&root, false);
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    assert_fresh_thumbnail(&mut s, &root);
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_thumbnail_drive_that_stays_full_still_converts() {
    let root = fixture("conversion-stays-full");
    fail_markers(&root, std::io::ErrorKind::StorageFull, 2);
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    let ids = ids(&s);
    let r = s.execute("library.smartPreviews", &json!({"format": "lossless", "ids": ids})).unwrap();
    assert_eq!((r["built"].as_u64(), r["refreshed"].as_u64()), (Some(2), Some(2)), "{r}");
    assert!(loose_files(&root).is_empty());
    set_online(&root, false);
    s.media.clear_sources();
    assert_fresh_thumbnail(&mut s, &root);
    drop(s);
    for _ in 0..2 {
        let mut s = Session::new().with_fs();
        s.open_library(root.join("library"), false).unwrap();
        assert_fresh_thumbnail(&mut s, &root);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn discarding_needs_no_marker() {
    let root = fixture("discard-without-marker");
    // A folder that cannot hold a marker (and has nothing cached).
    let thumbs = root.join("library/thumbs");
    std::fs::remove_dir_all(&thumbs).unwrap();
    std::fs::write(&thumbs, b"not a folder").unwrap();
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    let ids = ids(&s);
    let r = s.execute("library.smartPreviews", &json!({"discard": true, "ids": ids})).unwrap();
    assert_eq!(r["removed"], json!(2), "{r}");
    assert_eq!(stats(s.media.smart_dir.as_ref().unwrap()).0, 0);
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}
