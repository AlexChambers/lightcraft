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
fn fixture(tag: &str) -> PathBuf {
    let root = super::tests::temp(tag);
    for (path, scale) in originals(&root).iter().zip([1.0f32, 0.55]) {
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
