//! Lightroom's saved Upright matrices (issue #205): which slot each mode reads, what is refused,
//! the conversion into LightCraft's frame, the geometry against measurements of Lightroom's own
//! exports, and the import, re-read and save paths.
//!
//! The matrices below come from public Lightroom Classic 12 sidecars of public raws
//! (github.com/zjqc/photographing-art, MIT): numbers only, no image data.

use std::path::Path;

use lightcraft_develop::{DevelopSettings, Upright};
use lightcraft_geom::{Homography, Orientation, Point};
use serde_json::{Value, json};

use crate::Session;
use crate::crs::{Props, SavedUpright, Target, saved_upright, to_partial, upright_in_frame, upright_mode};
use crate::tests_xmp::temp_dir;

const IDENTITY: &str = "1.000000000,0.000000000,0.000000000,0.000000000,1.000000000,0.000000000,0.000000000,0.000000000,1.000000000";

/// The six matrices Lightroom saved for DSC_1457.NEF (slots 0 Off, 1 Auto, 2 Full, 3 Level,
/// 4 Vertical, 5 Guided).
const SLOTS: [&str; 6] = [
    IDENTITY,
    "1.010477639,-0.008540717,-0.006124940,0.014938050,1.011645918,-0.015359712,-0.007190889,-0.003396748,1.000000000",
    "1.000410884,-0.014998297,0.000328989,0.012818795,1.012962929,-0.022290590,-0.000090999,-0.018590990,1.000000000",
    "1.015108937,-0.006862808,-0.004123065,0.015441320,1.015108937,-0.015275128,0.000000000,0.000000000,1.000000000",
    "1.000505332,-0.014999682,0.000327389,0.012654172,1.013013074,-0.022230588,-0.000000003,-0.018592354,1.000000000",
    IDENTITY,
];

const ORIENTATIONS: [Orientation; 8] = [
    Orientation::Normal,
    Orientation::FlipH,
    Orientation::Rotate180,
    Orientation::FlipV,
    Orientation::Transpose,
    Orientation::Rotate90,
    Orientation::Transverse,
    Orientation::Rotate270,
];

/// An XMP packet in Lightroom's form: `fields` (attribute text) plus the six saved slots.
fn packet(fields: &str) -> String {
    let slots: String = SLOTS.iter().enumerate().map(|(i, m)| format!(r#" crs:UprightTransform_{i}="{m}""#)).collect();
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
      <rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
        crs:Exposure2012="+0.30" crs:UprightCenterMode="0" crs:UprightCenterNormX="0.5" crs:UprightCenterNormY="0.5"
        crs:UprightFocalMode="0" crs:UprightFocalLength35mm="267.885" crs:UprightPreview="False"
        crs:UprightFourSegmentsCount="0" {fields}{slots}/>
    </rdf:RDF></x:xmpmeta>"#
    )
}

fn lightroom(mode: u8) -> String {
    packet(&format!(r#"crs:PerspectiveUpright="{mode}" crs:UprightVersion="151388160" crs:UprightTransformCount="6""#))
}

fn props(x: &str) -> Props {
    lightcraft_meta::parse_xmp(x).unwrap().properties
}

fn matrix(text: &str) -> [f64; 9] {
    let v: Vec<f64> = text.split(',').map(|t| t.trim().parse().unwrap()).collect();
    v.try_into().unwrap()
}

fn apply(m: &[f64; 9], p: Point) -> Point {
    Homography(*m).apply(p)
}

#[test]
fn perspective_upright_values_name_lightroom_modes() {
    let expect = [Upright::Off, Upright::Auto, Upright::Full, Upright::Level, Upright::Vertical, Upright::Guided];
    for (value, mode) in expect.into_iter().enumerate() {
        assert_eq!(upright_mode(value as f64), Some(mode));
    }
    for odd in [6.0, -1.0, 1.5, f64::NAN, f64::INFINITY] {
        assert_eq!(upright_mode(odd), None, "{odd}");
    }
    // a stated mode never keeps a transform solved for another one
    for (value, name) in [(2, "full"), (3, "level"), (4, "vertical")] {
        let partial = to_partial(&props(&lightroom(value)), Target::Any);
        assert_eq!(partial["geometry"]["upright"], name, "{value}");
        assert!(partial["geometry"]["upright_transform"].is_null(), "{partial}");
        let mut before = DevelopSettings::default();
        before.geometry.upright_transform = Some(Homography([1.1, 0.0, 0.0, 0.0, 1.1, 0.0, 0.0, 0.0, 1.0]));
        assert_eq!(lightcraft_develop::apply_partial(&before, &partial, 1.0).geometry.upright_transform, None);
    }
}

#[test]
fn each_mode_reads_its_own_slot() {
    for (value, mode) in [(1, Upright::Auto), (2, Upright::Full), (3, Upright::Level), (4, Upright::Vertical)] {
        let saved = saved_upright(&props(&lightroom(value))).unwrap();
        assert_eq!(saved, SavedUpright { mode, matrix: matrix(SLOTS[value as usize]) });
    }
    // Off has nothing to apply; Guided's slot holds no result (Lightroom solves it from the guides)
    assert_eq!(saved_upright(&props(&lightroom(0))), None);
    assert_eq!(saved_upright(&props(&lightroom(5))), None);
    // the shapes that identify the slots: Level is a rotation about the centre, Vertical has no
    // horizontal keystone, Full has both
    let level = matrix(SLOTS[3]);
    assert_eq!((level[6], level[7]), (0.0, 0.0));
    assert!(apply(&level, Point::new(0.5, 0.5)).dist(Point::new(0.5, 0.5)) < 1e-8);
    assert!(matrix(SLOTS[4])[6].abs() < 1e-6 && matrix(SLOTS[4])[7].abs() > 1e-3);
    assert!(matrix(SLOTS[2])[6].abs() > 1e-5 && matrix(SLOTS[2])[7].abs() > 1e-3);
}

#[test]
fn saved_matrices_lightcraft_cannot_apply_are_refused() {
    let base = lightroom(1);
    let auto = SLOTS[1];
    let refused = |what: &str, xmp: String| assert_eq!(saved_upright(&props(&xmp)), None, "{what}");
    refused("no version", base.replace(r#" crs:UprightVersion="151388160""#, ""));
    refused("another version", base.replace("151388160", "151388161"));
    refused("a version that isn't a number", base.replace("151388160", "9.6"));
    refused("no count", base.replace(r#" crs:UprightTransformCount="6""#, ""));
    refused("a count without the slot", base.replace(r#"crs:UprightTransformCount="6""#, r#"crs:UprightTransformCount="1""#));
    refused("a count that isn't a number", base.replace(r#"crs:UprightTransformCount="6""#, r#"crs:UprightTransformCount="six""#));
    refused("a negative count", base.replace(r#"crs:UprightTransformCount="6""#, r#"crs:UprightTransformCount="-6""#));
    refused("no matrix for the mode", base.replace(&format!(r#" crs:UprightTransform_1="{auto}""#), ""));
    refused("an empty matrix", base.replace(auto, ""));
    refused("an unknown mode", base.replace(r#"crs:PerspectiveUpright="1""#, r#"crs:PerspectiveUpright="9""#));
    refused("a fractional mode", base.replace(r#"crs:PerspectiveUpright="1""#, r#"crs:PerspectiveUpright="1.5""#));
    for (what, m) in [
        ("eight numbers", "1,0,0,0,1,0,0,0"),
        ("ten numbers", "1,0,0,0,1,0,0,0,1,0"),
        ("text", "1,0,0,0,one,0,0,0,1"),
        ("NaN", "1,0,0,0,NaN,0,0,0,1"),
        ("infinity", "1,0,0,0,inf,0,0,0,1"),
        ("an overflow", "1,0,0,0,1e400,0,0,0,1"),
        ("a zero last element", "1,0,0,0,1,0,0,0,0"),
        ("a huge element", "1,0,250,0,1,0,0,0,1"),
        ("the horizon inside the photo", "1,0,0,0,1,0,-2,0,1"),
        ("a mirror", "-1,0,1,0,1,0,0,0,1"),
        ("a collapse", "1,1,0,1,1,0,0,0,1"),
        ("separators only", ",,,,,,,,"),
    ] {
        refused(what, base.replace(auto, m));
    }
    // a matrix scaled by its last element is the same correction
    let scaled: Vec<String> = matrix(auto).iter().map(|v| format!("{}", v * 2.0)).collect();
    let saved = saved_upright(&props(&base.replace(auto, &scaled.join(", ")))).unwrap();
    assert!(saved.matrix.iter().zip(matrix(auto)).all(|(a, b)| (a - b).abs() < 1e-12), "{saved:?}");
}

/// A deterministic stream of hostile field values: none may panic, and whatever is accepted maps
/// to finite coordinates in every orientation.
#[test]
fn hostile_upright_fields_never_panic() {
    let pieces = ["1", "0", "-0", "1e308", "-1e308", "1e-320", "NaN", "inf", "", " ", ",", "0.5", "2", "-3", "x", "1e400", "١"];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..4000 {
        let n = (next() % 12) as usize;
        let m: Vec<&str> = (0..n).map(|_| pieces[(next() % pieces.len() as u64) as usize]).collect();
        let mode = ["1", "2", "3", "4", "0", "5", "-1", "1e9", "x"][(next() % 9) as usize];
        let count = ["6", "0", "5", "-1", "18446744073709551616", "x"][(next() % 6) as usize];
        let version = ["151388160", "0", "x", "-151388160"][(next() % 4) as usize];
        let xmp = packet(&format!(r#"crs:PerspectiveUpright="{mode}" crs:UprightVersion="{version}" crs:UprightTransformCount="{count}""#))
            .replace(SLOTS[1], &m.join(","))
            .replace(SLOTS[3], &m.join(","));
        let Ok(d) = lightcraft_meta::parse_xmp(&xmp) else { continue };
        if let Some(saved) = saved_upright(&d.properties) {
            for o in ORIENTATIONS {
                for (w, h) in [(3.0, 2.0), (1.0, 1e-300), (1e300, 1.0), (f64::NAN, 1.0), (0.0, 1.0)] {
                    if let Some(t) = upright_in_frame(&saved, o, w, h) {
                        assert!(t.0.iter().all(|v| v.is_finite()), "{saved:?} {o:?}");
                    }
                }
            }
        }
    }
}

/// Lightroom's own reading of a matrix (stored, normalized coordinates) and LightCraft's reading of
/// the converted one (oriented, centred coordinates) move every point of the photo to the same
/// place, in every orientation; converting back gives Lightroom's matrix.
#[test]
fn frame_conversion_agrees_with_lightroom_point_for_point() {
    let (sw, sh) = (6000.0, 4000.0);
    for slot in [1, 2, 3, 4] {
        let saved = SavedUpright { mode: Upright::Auto, matrix: matrix(SLOTS[slot]) };
        for o in ORIENTATIONS {
            let h = upright_in_frame(&saved, o, sw, sh).unwrap();
            let (ow, oh) = if o.swaps_axes() { (sh, sw) } else { (sw, sh) };
            let l = ow.max(oh) / 2.0;
            let oriented = |p: Point| {
                let (x, y) = o.map(p.x * sw, p.y * sh, sw, sh);
                Point::new(x, y)
            };
            for i in 0..=10 {
                for j in 0..=10 {
                    let p = Point::new(i as f64 / 10.0, j as f64 / 10.0);
                    let want = oriented(apply(&saved.matrix, p));
                    let a = oriented(p);
                    let c = h.apply(Point::new((a.x - ow / 2.0) / l, (a.y - oh / 2.0) / l));
                    let got = Point::new(c.x * l + ow / 2.0, c.y * l + oh / 2.0);
                    assert!(got.dist(want) < 1e-6, "slot {slot} {o:?} {p:?}: {got:?} vs {want:?}");
                }
            }
            // and back: LightCraft's homography read in Lightroom's frame is the saved matrix
            let corners = [Point::new(0.0, 0.0), Point::new(1.0, 0.0), Point::new(1.0, 1.0), Point::new(0.0, 1.0)];
            let moved = corners.map(|p| {
                let a = oriented(p);
                let c = h.apply(Point::new((a.x - ow / 2.0) / l, (a.y - oh / 2.0) / l));
                stored_from_oriented(o, Point::new(c.x * l + ow / 2.0, c.y * l + oh / 2.0), sw, sh)
            });
            let back = Homography::from_quads(&corners, &moved).unwrap();
            assert!(back.0.iter().zip(saved.matrix).all(|(a, b)| (a - b).abs() < 1e-9), "slot {slot} {o:?}: {back:?}");
        }
    }
    // identity stays identity, in every orientation and frame
    let identity = SavedUpright { mode: Upright::Auto, matrix: matrix(IDENTITY) };
    for o in ORIENTATIONS {
        for (w, h) in [(6000.0, 4000.0), (40.0, 24.0), (1.0, 1.0), (24.0, 40.0)] {
            let t = upright_in_frame(&identity, o, w, h).unwrap();
            assert!(t.0.iter().zip(Homography::IDENTITY.0).all(|(a, b)| (a - b).abs() < 1e-12), "{o:?} {t:?}");
        }
    }
    for (w, h) in [(0.0, 1.0), (1.0, -1.0), (f64::NAN, 1.0), (1.0, f64::INFINITY)] {
        assert_eq!(upright_in_frame(&identity, Orientation::Normal, w, h), None);
    }
}

/// Oriented px (an oriented image of the stored `sw × sh`) → normalized stored coordinates.
fn stored_from_oriented(o: Orientation, p: Point, sw: f64, sh: f64) -> Point {
    // invert `Orientation::map` from three of its points (it is affine)
    let at = |x: f64, y: f64| {
        let (u, v) = o.map(x, y, 1.0, 1.0);
        Point::new(u, v)
    };
    let (z, ex, ey) = (at(0.0, 0.0), at(1.0, 0.0), at(0.0, 1.0));
    let fwd = Homography([ex.x - z.x, ey.x - z.x, z.x, ex.y - z.y, ey.y - z.y, z.y, 0.0, 0.0, 1.0]);
    let (ow, oh) = if o.swaps_axes() { (sh, sw) } else { (sw, sh) };
    fwd.inverse().unwrap().apply(Point::new(p.x / ow, p.y / oh))
}

/// Lightroom's Level matrix is a rotation about the centre of the stored image: in LightCraft's
/// isotropic coordinates it is a rotation about the origin, of the same angle.
#[test]
fn level_matrix_is_a_rotation_in_lightcraft_coordinates() {
    let saved = SavedUpright { mode: Upright::Level, matrix: matrix(SLOTS[3]) };
    let h = upright_in_frame(&saved, Orientation::Normal, 4800.0, 3200.0).unwrap().0;
    // (the saved matrix has nine decimals: its centre stays put to about 1e-9)
    assert!(h[2].abs() < 1e-8 && h[5].abs() < 1e-8 && h[6].abs() < 1e-12 && h[7].abs() < 1e-12, "{h:?}");
    assert!((h[0] - h[4]).abs() < 1e-9 && (h[1] + h[3]).abs() < 2e-6, "{h:?}");
    let degrees = h[3].atan2(h[0]).to_degrees();
    assert!((degrees - 0.581).abs() < 0.001, "{degrees}");
}

/// One Lightroom export of a public raw with Upright Auto, measured (SIFT matches between
/// LightCraft's render without geometry and Lightroom's export, RANSAC, then least squares on
/// the inliers): the fitted map from LightCraft's base pixels to export pixels.
struct Reference {
    name: &'static str,
    /// `crs:UprightTransform_1` of the export's own packet.
    auto: &'static str,
    orientation: Orientation,
    /// Lightroom's stored image (its default crop), before orientation.
    stored: (f64, f64),
    /// Where Lightroom's oriented image starts inside LightCraft's (LightCraft decodes these NEFs
    /// a little larger), in oriented pixels; measured on Upright Off exports to 0.5 px.
    inset: (f64, f64),
    crop: [f64; 4],
    export: (usize, usize),
    /// Base px → export px (OpenCV pixel coordinates: centres at whole numbers).
    fit: [f64; 9],
}

const REFERENCES: [Reference; 3] = [
    Reference {
        name: "DSC_1457 (landscape)",
        auto: SLOTS[1],
        orientation: Orientation::Normal,
        stored: (4800.0, 3200.0),
        inset: (6.0, 6.0),
        crop: [0.0, 0.0, 1.0, 1.0],
        export: (4800, 3200),
        fit: [
            1.0103827566515864,
            -0.012910769838480735,
            -35.18705406883105,
            0.009940071369263049,
            1.0117804655183038,
            -55.14972108764364,
            -1.5048134911065293e-06,
            -1.0989184648032297e-06,
            1.0,
        ],
    },
    Reference {
        name: "DSC_1316 (portrait, EXIF orientation 8)",
        auto: "1.014491182,0.006583939,-0.010537101,-0.014802884,1.014513584,0.000149262,0.000001442,0.000009357,0.999998558",
        orientation: Orientation::Rotate270,
        stored: (6000.0, 4000.0),
        inset: (10.0, 18.0),
        crop: [0.0, 0.0, 1.0, 1.0],
        export: (4000, 6000),
        fit: [
            1.0145773365814015,
            0.00989331650336279,
            -69.00371740408151,
            -0.009872550000906167,
            1.0145972704719037,
            -42.124240481845156,
            -1.2235571140649089e-09,
            1.328135644733238e-08,
            1.0,
        ],
    },
    Reference {
        name: "DSC_2051 (cropped, Constrain Crop on)",
        auto: "1.015255953,0.006622517,-0.010961914,-0.016162078,1.014735275,0.000170720,0.000065470,-0.001142520,1.000000000",
        orientation: Orientation::Normal,
        stored: (4800.0, 3200.0),
        inset: (6.0, 6.0),
        crop: [0.127635, 0.195079, 0.85347, 0.920914],
        export: (3484, 2323),
        fit: [
            1.0154458088182998,
            0.010299904328396522,
            -672.1052981190101,
            -0.01076867585799946,
            1.0153630727100376,
            -629.6318219157675,
            1.638771482679291e-08,
            -2.8365852308384464e-07,
            1.0,
        ],
    },
];

/// LightCraft's render mapping (`Frame`: crop, perspective) with the imported matrix agrees with
/// Lightroom's exports to about a pixel at full resolution; without it, by tens of pixels.
#[test]
fn imported_upright_renders_like_lightroom_exports() {
    for r in &REFERENCES {
        let (ow, oh) = if r.orientation.swaps_axes() { (r.stored.1, r.stored.0) } else { r.stored };
        let saved = SavedUpright { mode: Upright::Auto, matrix: matrix(r.auto) };
        let error = |transform: Option<Homography>| {
            let mut s = DevelopSettings::default();
            s.geometry.upright = Upright::Auto;
            s.geometry.upright_transform = transform;
            let [x0, y0, x1, y1] = r.crop;
            s.crop.geometry.rect = lightcraft_geom::Rect { x0, y0, x1, y1 };
            let f = lightcraft_pipeline::geometry::Frame::new(ow as usize, oh as usize, &s, true);
            let to_oriented = f.out_to_oriented(r.export.0, r.export.1);
            let fit = Homography(r.fit).inverse().unwrap();
            let (mut worst, mut sum, mut n) = (0.0f64, 0.0, 0.0);
            for i in 0..9 {
                for j in 0..9 {
                    let e = Point::new((0.05 + 0.1125 * i as f64) * r.export.0 as f64, (0.05 + 0.1125 * j as f64) * r.export.1 as f64);
                    // continuous coordinates (pixel centres at +0.5) through LightCraft's frame
                    let t = to_oriented.apply(Point::new(e.x + 0.5, e.y + 0.5));
                    let src = f.warp.as_ref().map_or(t, |w| w.to_corrected(t));
                    let predicted = Point::new(src.x - 0.5 + r.inset.0, src.y - 0.5 + r.inset.1);
                    let d = predicted.dist(fit.apply(e));
                    worst = worst.max(d);
                    sum += d;
                    n += 1.0;
                }
            }
            (sum / n, worst)
        };
        let (mean, worst) = error(upright_in_frame(&saved, r.orientation, r.stored.0, r.stored.1));
        assert!(mean < 0.75 && worst < 1.25, "{}: mean {mean:.3} px, worst {worst:.3} px", r.name);
        let (mean_without, _) = error(None);
        assert!(mean_without > 15.0, "{}: {mean_without:.1} px without the saved matrix", r.name);
    }
}

fn write_png(path: &Path) {
    let (w, h) = (40usize, 24usize);
    let data: Vec<[u8; 4]> = (0..w * h).map(|i| [(i % w * 6) as u8, (i / w * 9) as u8, 90, 255]).collect();
    let img = lightcraft_raster::Rgba8 { width: w, height: h, data };
    let bytes = lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&img), &lightcraft_codecs::EncodeMeta::default()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// A 40 × 24 JPEG stored on its side: EXIF orientation 6 shows it 24 × 40.
fn write_turned_jpeg(path: &Path) {
    write_jpeg(path, 40, 24, 6);
}

/// A `w × h` JPEG (stored size) with EXIF orientation `exif`.
fn write_jpeg(path: &Path, w: usize, h: usize, exif: u16) {
    let data: Vec<[u8; 4]> = (0..w * h).map(|i| [(i % w * 6) as u8, (i / w * 9) as u8, 30, 255]).collect();
    let img = lightcraft_raster::Rgba8 { width: w, height: h, data };
    let exif = lightcraft_codecs::exif::minimal_exif(exif);
    let meta = lightcraft_codecs::EncodeMeta { exif: Some(&exif), ..Default::default() };
    let bytes =
        lightcraft_codecs::encode_jpeg(&lightcraft_codecs::EncodeImage::rgba8(&img), 90, lightcraft_codecs::ChromaSubsampling::S444, &meta).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn upright_warnings(report: &Value) -> Vec<Value> {
    report["warnings"].as_array().map(|w| w.iter().filter(|w| w["code"] == "uprightGeometry").cloned().collect()).unwrap_or_default()
}

fn geometry(s: &Session) -> lightcraft_develop::Geometry {
    s.catalog.photos().next().unwrap().develop.geometry.clone()
}

fn placed(slot: usize, o: Orientation, w: f64, h: f64) -> Homography {
    upright_in_frame(&SavedUpright { mode: Upright::Auto, matrix: matrix(SLOTS[slot]) }, o, w, h).unwrap()
}

#[test]
fn import_keeps_lightroom_upright_and_rereads_follow_the_sidecar() {
    let src = temp_dir("upright-import");
    let photo = src.join("tower.png");
    write_png(&photo);
    let sidecar = src.join("tower.xmp");
    std::fs::write(&sidecar, lightroom(1)).unwrap();
    let mut s = Session::new().with_fs();
    let r = s.execute("library.import", &json!({"paths": [photo.to_string_lossy()]})).unwrap();
    assert!(upright_warnings(&r).is_empty(), "{r}");
    // the analysis record is not an adjustment that was lost
    let warnings = r["warnings"].as_array().cloned().unwrap_or_default();
    let mut unmapped = warnings.iter().filter(|w| w["code"] == "unmappedXmp").flat_map(|w| w["fields"].as_array().cloned().unwrap_or_default());
    assert!(unmapped.all(|f| !f.as_str().unwrap_or_default().starts_with("Upright")), "{r}");
    let g = geometry(&s);
    assert_eq!(g.upright, Upright::Auto);
    assert_eq!(g.upright_transform, Some(placed(1, Orientation::Normal, 40.0, 24.0)));
    let id = s.catalog.photos().next().unwrap().id;
    s.execute("library.select", &json!({"ids": [id.0]})).unwrap();

    // re-reading follows a changed mode to its own slot
    std::fs::write(&sidecar, lightroom(4)).unwrap();
    let reread = s.execute("photo.readMetadataFromFile", &json!({"ids": [id.0]})).unwrap();
    assert!(upright_warnings(&reread).is_empty(), "{reread}");
    let g = geometry(&s);
    assert_eq!((g.upright, g.upright_transform), (Upright::Vertical, Some(placed(4, Orientation::Normal, 40.0, 24.0))));

    // Guided: Lightroom keeps no result to import, so it is solved here and says so
    std::fs::write(&sidecar, lightroom(5)).unwrap();
    let reread = s.execute("photo.readMetadataFromFile", &json!({"ids": [id.0]})).unwrap();
    assert_eq!(upright_warnings(&reread).first().map(|w| w["mode"].clone()), Some(json!("guided")), "{reread}");
    assert_eq!((geometry(&s).upright, geometry(&s).upright_transform), (Upright::Guided, None));

    // an Upright version whose convention is unknown: solved again, with the warning
    std::fs::write(&sidecar, lightroom(3).replace("151388160", "151388161")).unwrap();
    let reread = s.execute("photo.readMetadataFromFile", &json!({"ids": [id.0]})).unwrap();
    assert_eq!(upright_warnings(&reread).first().map(|w| w["mode"].clone()), Some(json!("level")), "{reread}");
    assert_eq!((geometry(&s).upright, geometry(&s).upright_transform), (Upright::Level, None));

    // Upright Off: Lightroom applies none of its saved matrices, so nothing is lost
    std::fs::write(&sidecar, lightroom(0)).unwrap();
    let reread = s.execute("photo.readMetadataFromFile", &json!({"ids": [id.0]})).unwrap();
    assert!(upright_warnings(&reread).is_empty(), "{reread}");
    assert_eq!((geometry(&s).upright, geometry(&s).upright_transform), (Upright::Off, None));
    let _ = std::fs::remove_dir_all(&src);
}

/// The matrix is in the stored image's frame: a photo shown on its side gets it turned with it.
#[test]
fn import_turns_lightroom_upright_with_the_file_orientation() {
    let src = temp_dir("upright-turned");
    let photo = src.join("turned.jpg");
    write_turned_jpeg(&photo);
    std::fs::write(src.join("turned.xmp"), lightroom(1)).unwrap();
    let mut s = Session::new().with_fs();
    let r = s.execute("library.import", &json!({"paths": [photo.to_string_lossy()]})).unwrap();
    assert!(upright_warnings(&r).is_empty(), "{r}");
    let p = s.catalog.photos().next().unwrap().clone();
    assert_eq!((p.width, p.height), (24, 40));
    let turned = placed(1, Orientation::Rotate90, 40.0, 24.0);
    assert_eq!(p.develop.geometry.upright_transform, Some(turned));
    assert_ne!(Some(turned), Some(placed(1, Orientation::Normal, 24.0, 40.0)));
    let _ = std::fs::remove_dir_all(&src);
}

/// LightCraft's own sidecar keeps the imported correction exactly (round trip through
/// `lc:settings`), and another library importing it gets the same geometry.
#[test]
fn imported_upright_round_trips_through_lightcraft_sidecars() {
    let src = temp_dir("upright-roundtrip");
    let photo = src.join("hall.png");
    write_png(&photo);
    std::fs::write(src.join("hall.xmp"), lightroom(2)).unwrap();
    let mut s = Session::new().with_fs();
    s.execute("library.import", &json!({"paths": [photo.to_string_lossy()]})).unwrap();
    let first = geometry(&s);
    assert_eq!((first.upright, first.upright_transform), (Upright::Full, Some(placed(2, Orientation::Normal, 40.0, 24.0))));
    let id = s.catalog.photos().next().unwrap().id;
    s.execute("photo.saveMetadataToFile", &json!({"ids": [id.0]})).unwrap();
    let saved = std::fs::read_to_string(src.join("hall.xmp")).unwrap();
    assert!(saved.contains("crs:UprightTransform_2"), "Lightroom's fields are kept byte for byte");
    let mut again = Session::new().with_fs();
    let r = again.execute("library.import", &json!({"paths": [photo.to_string_lossy()]})).unwrap();
    assert!(upright_warnings(&r).is_empty(), "{r}");
    assert_eq!(geometry(&again), first);
    let _ = std::fs::remove_dir_all(&src);
}

/// Lightroom's matrix placed in the frame the file's orientation gives, then turned with the
/// photo's own Rotate Left / Right, is the matrix placed in the composed frame: for every pair.
/// Not turning it would leave DSC_1457's correction tens of pixels off after a Rotate Right.
#[test]
fn a_kept_upright_turns_with_the_photo_orientation() {
    let saved = SavedUpright { mode: Upright::Auto, matrix: matrix(SLOTS[1]) };
    let (sw, sh) = (4800.0, 3200.0);
    for file in ORIENTATIONS {
        for user in ORIENTATIONS {
            let turned = lightcraft_pipeline::upright::turned(&upright_in_frame(&saved, file, sw, sh).unwrap(), user);
            let direct = upright_in_frame(&saved, file.then(user), sw, sh).unwrap();
            assert!(turned.0.iter().zip(direct.0).all(|(a, b)| (a - b).abs() < 1e-12), "{file:?} then {user:?}");
        }
    }
    // the unturned matrix after Rotate Right, in pixels of the 3200 × 4800 frame
    let (right, stale) =
        (upright_in_frame(&saved, Orientation::Rotate90, sw, sh).unwrap(), upright_in_frame(&saved, Orientation::Normal, sw, sh).unwrap());
    let worst = (0..=8)
        .flat_map(|i| (0..=8).map(move |j| Point::new(i as f64 / 8.0 * 2.0 / 3.0 * 2.0 - 2.0 / 3.0, j as f64 / 8.0 * 2.0 - 1.0)))
        .map(|c| right.apply(c).dist(stale.apply(c)) * 2400.0)
        .fold(0.0, f64::max);
    assert!(worst > 15.0, "{worst:.1} px");
}

/// Rotate Right, then Read Metadata from File; and Read Metadata, then Rotate Right: the same
/// correction, in the turned frame. Two more turns bring it back.
#[test]
fn rotating_and_rereading_keep_lightroom_upright_in_the_photo_frame() {
    let src = temp_dir("upright-rotate");
    let photo = src.join("arch.png");
    write_png(&photo);
    std::fs::write(src.join("arch.xmp"), lightroom(1)).unwrap();
    let want = placed(1, Orientation::Rotate90, 40.0, 24.0);
    let close = |a: Option<Homography>, b: Homography| a.is_some_and(|a| a.0.iter().zip(b.0).all(|(x, y)| (x - y).abs() < 1e-12));
    for read_first in [false, true] {
        let mut s = Session::new().with_fs();
        s.execute("library.import", &json!({"paths": [photo.to_string_lossy()]})).unwrap();
        let id = s.catalog.photos().next().unwrap().id;
        s.execute("library.select", &json!({"ids": [id.0]})).unwrap();
        if read_first {
            s.execute("photo.readMetadataFromFile", &json!({"ids": [id.0]})).unwrap();
            s.execute("photo.rotateRight", &json!({})).unwrap();
        } else {
            s.execute("photo.rotateRight", &json!({})).unwrap();
            let r = s.execute("photo.readMetadataFromFile", &json!({"ids": [id.0]})).unwrap();
            assert!(upright_warnings(&r).is_empty(), "{r}");
        }
        let g = geometry(&s);
        assert!(close(g.upright_transform, want), "read first: {read_first}: {g:?}");
        s.execute("photo.rotateLeft", &json!({})).unwrap();
        assert!(close(geometry(&s).upright_transform, placed(1, Orientation::Normal, 40.0, 24.0)), "back");
        s.execute("photo.flipHorizontal", &json!({})).unwrap();
        assert!(close(geometry(&s).upright_transform, placed(1, Orientation::Normal, 40.0, 24.0)), "a flip of the output leaves it");
    }
    let _ = std::fs::remove_dir_all(&src);
}

fn crop_packet(fields: &str) -> String {
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
      <rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:HasCrop="True" {fields}/>
    </rdf:RDF></x:xmpmeta>"#
    )
}

fn crop_of(s: &Session) -> lightcraft_geom::CropGeometry {
    s.catalog.photos().next().unwrap().develop.crop.geometry
}

/// DSC_1486.NEF (zjqc/photographing-art) is stored 3:2 and shown in portrait (EXIF 8); its
/// Lightroom crop cuts the stored image's left 20.7 %, which is the bottom of the portrait, and
/// Lightroom exports 3200 × 3808. The sidecar crop used to cut the portrait's left instead.
#[test]
fn a_sidecar_crop_on_a_turned_photo_lands_where_lightroom_puts_it() {
    let src = temp_dir("crop-portrait");
    let photo = src.join("portrait.jpg");
    write_jpeg(&photo, 60, 40, 8);
    let sidecar = src.join("portrait.xmp");
    std::fs::write(&sidecar, crop_packet(r#"crs:CropLeft="0.206655" crs:CropTop="0" crs:CropRight="1" crs:CropBottom="1" crs:CropAngle="0""#))
        .unwrap();
    let mut s = Session::new().with_fs();
    s.execute("library.import", &json!({"paths": [photo.to_string_lossy()]})).unwrap();
    let check = |s: &Session, what: &str| {
        let g = crop_of(s);
        let r = [g.rect.x0, g.rect.y0, g.rect.x1, g.rect.y1].map(|v| (v * 1e6).round() / 1e6);
        assert_eq!(r, [0.0, 0.0, 1.0, 0.793345], "{what}");
        let (w, h) = s.catalog.photos().next().unwrap().shown_size();
        assert!((w / h - 3200.0 / 3808.0).abs() < 1e-3, "{what}: {w} × {h}");
    };
    check(&s, "import");
    let id = s.catalog.photos().next().unwrap().id;
    s.execute("photo.readMetadataFromFile", &json!({"ids": [id.0]})).unwrap();
    check(&s, "read metadata");
    let _ = std::fs::remove_dir_all(&src);
}

/// Straightened crops from a real Lightroom Classic catalog (stored 9504 × 6336, the sizes of the
/// JPEGs Lightroom exported from it, the same numbers as the catalog import's test): the same
/// fields in a sidecar give the same frame, upright and on its side.
#[test]
fn a_straightened_sidecar_crop_matches_lightroom_exports() {
    for (fields, exif, export) in [
        (
            r#"crs:CropAngle="-9.34444" crs:CropBottom="0.802034" crs:CropLeft="0.05499" crs:CropRight="0.94501" crs:CropTop="0.197966""#,
            1,
            (7725.0, 5150.0),
        ),
        (
            r#"crs:CropAngle="-1.48964" crs:CropBottom="0.962457" crs:CropLeft="0.010429" crs:CropRight="0.989571" crs:CropTop="0.037543""#,
            6,
            (6100.0, 9150.0),
        ),
    ] {
        let src = temp_dir(&format!("crop-straightened-{exif}"));
        let photo = src.join("hall.jpg");
        write_jpeg(&photo, 48, 32, exif);
        std::fs::write(src.join("hall.xmp"), crop_packet(fields)).unwrap();
        let mut s = Session::new().with_fs();
        s.execute("library.import", &json!({"paths": [photo.to_string_lossy()]})).unwrap();
        let r = crop_of(&s).rect;
        let (ow, oh) = if exif == 6 { (6336.0, 9504.0) } else { (9504.0, 6336.0) };
        let (w, h) = (r.width() * ow, r.height() * oh);
        assert!((w - export.0).abs() <= 3.0 && (h - export.1).abs() <= 3.0, "EXIF {exif}: {w:.1} × {h:.1}, Lightroom exported {export:?}");
        let _ = std::fs::remove_dir_all(&src);
    }
}

/// A straightened crop on top of a saved Upright isn't measured yet: the mode is solved again
/// and the warning says so.
#[test]
fn a_straightened_crop_on_a_saved_upright_is_solved_again_with_the_warning() {
    let src = temp_dir("upright-straightened");
    let photo = src.join("bridge.png");
    write_png(&photo);
    let xmp = lightroom(1).replace(r#"crs:UprightFourSegmentsCount="0""#, r#"crs:UprightFourSegmentsCount="0" crs:HasCrop="True" crs:CropLeft="0.1" crs:CropTop="0.1" crs:CropRight="0.9" crs:CropBottom="0.9" crs:CropAngle="2.5""#);
    std::fs::write(src.join("bridge.xmp"), xmp).unwrap();
    let mut s = Session::new().with_fs();
    let r = s.execute("library.import", &json!({"paths": [photo.to_string_lossy()]})).unwrap();
    assert_eq!(upright_warnings(&r).first().map(|w| w["mode"].clone()), Some(json!("auto")), "{r}");
    assert_eq!((geometry(&s).upright, geometry(&s).upright_transform), (Upright::Auto, None));
    let _ = std::fs::remove_dir_all(&src);
}

/// Guided Upright's guides aren't read: a packet with guides and no automatic matrices still says
/// so; without guides or matrices nothing is lost.
#[test]
fn guided_upright_guides_that_are_not_read_are_reported() {
    let src = temp_dir("upright-guides");
    let photo = src.join("tower.png");
    write_png(&photo);
    let guides = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
      <rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:Exposure2012="+0.30"
        crs:PerspectiveUpright="5" crs:UprightFourSegmentsCount="2"
        crs:UprightFourSegments_0="0.21, 0.12, 0.23, 0.88" crs:UprightFourSegments_1="0.78, 0.10, 0.76, 0.90"/>
    </rdf:RDF></x:xmpmeta>"#;
    std::fs::write(src.join("tower.xmp"), guides).unwrap();
    let mut s = Session::new().with_fs();
    let r = s.execute("library.import", &json!({"paths": [photo.to_string_lossy()]})).unwrap();
    assert_eq!(upright_warnings(&r).first().map(|w| w["mode"].clone()), Some(json!("guided")), "{r}");
    let none = props(
        &guides
            .replace(r#"crs:UprightFourSegmentsCount="2""#, r#"crs:UprightFourSegmentsCount="0""#)
            .replace(r#"crs:UprightFourSegments_0="0.21, 0.12, 0.23, 0.88" crs:UprightFourSegments_1="0.78, 0.10, 0.76, 0.90""#, ""),
    );
    assert_eq!(crate::crs::upright_at_risk(&none), None);
    let _ = std::fs::remove_dir_all(&src);
}
