//! A Compact proxy cut short (by a version before atomic writes, issue #106, or an interrupted
//! copy or sync) still decodes as JPEG, with part of the picture wrong. It must not count as a
//! smart preview or render offline, Build Smart Previews must repair it, and a folder move must
//! never keep it over the complete copy.

use super::*;
use crate::{Session, media};
use lightcraft_preview::{Hash128, PreviewCache};
use serde_json::json;

#[test]
fn regression709_cut_short_compact_proxies_are_repaired_and_never_kept() {
    let root = super::tests::temp("cut-short-compact");
    let (w, h) = (1200usize, 800usize);
    let mut image = Rgb32f::new(w, h);
    for (i, p) in image.data.iter_mut().enumerate() {
        let x = (i % w) as f32 / w as f32;
        let y = (i / w) as f32 / h as f32;
        *p = [x * 0.8 + y * 0.2, (x * 9.0).sin() * 0.3 + 0.4, y * 0.9 - x * 0.1 + 0.1];
    }
    let original = root.join("original.tif");
    let encoded = lightcraft_codecs::EncodeImage::new(w as u32, h as u32, 3, lightcraft_codecs::Samples::F32(image.data.as_flattened()));
    std::fs::write(&original, lightcraft_codecs::encode_tiff(&encoded, lightcraft_codecs::TiffCompression::Deflate, &Default::default()).unwrap())
        .unwrap();
    let mut s = Session::new().with_fs();
    s.open_library(root.join("library"), false).unwrap();
    s.execute("library.import", &json!({"paths": [original]})).unwrap();
    let id = s.catalog.photos().next().unwrap().id;
    s.execute("library.smartPreviews", &json!({"format": "compact", "ids": [id.0]})).unwrap();
    let photo = s.catalog.photo(id).unwrap().clone();
    let file = file_path(s.media.smart_dir.as_ref().unwrap(), &photo);
    let full = std::fs::read(&file).unwrap();
    assert_eq!(header(&file).unwrap().0, Format::Compact);
    assert!(full.len() > 20_000, "a realistic proxy: {} bytes", full.len());
    // The render path once the original is known to be offline (`SourceRef::Smart`).
    let offline_render = |s: &mut Session, path: &Path| {
        let mut job = s.thumb_job(id, 256).unwrap();
        job.source = media::SourceRef::Smart { path: path.to_path_buf() };
        job.cache = Some((Arc::new(PreviewCache::memory(1_000_000)), Hash128(0)));
        job.run().rendered.map(|r| r.image)
    };
    let intact = offline_render(&mut s, &file).unwrap();
    let mut problems = Vec::new();
    for percent in [50, 75, 90, 99] {
        std::fs::write(&file, &full[..full.len() * percent / 100]).unwrap();
        if s.execute("photo.smartPreview", &json!({"id": id.0})).unwrap()["smartPreview"] != json!(false) {
            problems.push(format!("{percent}%: Info reports a smart preview"));
        }
        if let Ok(picture) = offline_render(&mut s, &file) {
            let wrong = picture.data.iter().zip(&intact.data).filter(|(a, b)| a != b).count();
            problems.push(format!("{percent}%: rendered offline, {wrong} of {} pixels wrong", picture.data.len()));
        }
        let r = s.execute("library.smartPreviews", &json!({"ids": [id.0]})).unwrap();
        if r["repaired"] != json!(1) || std::fs::read(&file).unwrap() != full {
            problems.push(format!("{percent}%: Build kept it ({r})"));
            std::fs::write(&file, &full).unwrap();
        }
    }
    // A move into a folder that already holds a cut-short copy (an earlier interrupted copy).
    let to = root.join("moved");
    let copy = sharded_file_path(&to, &photo);
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    std::fs::write(&copy, &full[..full.len() * 9 / 10]).unwrap();
    s.execute("library.smartPreviewsLocation", &json!({"path": to, "existing": "move"})).unwrap();
    assert_eq!(s.media.smart_dir.as_ref(), Some(&to));
    let active = std::fs::read(file_path(&to, &photo)).unwrap();
    if active != full {
        problems.push(format!(
            "the move kept the {}-byte cut copy and removed the complete {}-byte proxy (old exists={})",
            active.len(),
            full.len(),
            file.exists()
        ));
    }
    assert!(problems.is_empty(), "{problems:#?}");
    assert_eq!(offline_render(&mut s, &file_path(&to, &photo)).unwrap().data, intact.data);
    drop(s);
    std::fs::remove_dir_all(root).unwrap();
}
