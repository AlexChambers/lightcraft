//! CPU render admission for wasm32, based on the buffers in the selected plan.
//!
//! Estimates count live image allocations, not decoding or browser memory. Keep
//! 64 MiB of the 4 GiB address space for the runtime and allocator. Cache owners
//! supply their other live buffers, with shared allocations counted once.

use crate::{OutputDepth, Overlay, Plan, RenderRequest, SourceInfo, geometry, local};
use lightcraft_develop::{DevelopSettings, MaskShape};
use lightcraft_raster::{Image, Rgb32f};

pub const WASM_RENDER_BYTES: u64 = (4_u64 << 30) - (64 << 20);
const BOOKKEEPING_BYTES: u64 = 256 << 10;

/// An allocation census shared by the source owner and all its stage caches.
/// Seed it with the active source/info, then subtract that starting size when
/// passing `retained_bytes` to admission (the render estimate includes them).
#[derive(Default)]
pub struct LiveImages {
    seen: std::collections::HashSet<usize>,
    bytes: u64,
}

impl LiveImages {
    pub fn image<T>(&mut self, image: &Image<T>) {
        if image.data.capacity() != 0 {
            self.add(image.data.as_ptr() as usize, (image.data.capacity() as u64).saturating_mul(std::mem::size_of::<T>() as u64));
        }
    }

    pub fn source(&mut self, src: &Rgb32f, info: &SourceInfo) {
        self.image(src);
        if let Some(mattes) = &info.mattes {
            self.mattes(mattes);
        }
        if let Some(tone) = &info.local_tone {
            self.add(tone.map.gains.as_ptr() as usize, (tone.map.gains.capacity() as u64).saturating_mul(4));
        }
    }

    pub(crate) fn mattes(&mut self, mattes: &crate::masks::Mattes) {
        self.add(mattes as *const _ as usize, mattes.bytes() as u64);
    }

    fn add(&mut self, ptr: usize, bytes: u64) {
        if self.seen.insert(ptr) {
            self.bytes = self.bytes.saturating_add(bytes);
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Cold exact-plan peak, including source and source metadata. `cached` means
/// `render_cached`: identity sampling can then share the source allocation.
/// Existing caches are accounted separately, so an estimate never needs a hit.
pub fn estimate_wasm_render(src: &Rgb32f, info: &SourceInfo, settings: &DevelopSettings, req: &RenderRequest, cached: bool) -> u64 {
    let effective = settings.effective();
    let settings = crate::settings_for(&effective, req);
    let plan = crate::plan(src, info, &settings, req);
    let full = plan.needs_airlight.then(|| crate::plan(src, info, &settings, &RenderRequest { window: None, ..*req }));
    estimate(src, info, &plan, full.as_ref(), req, cached, false).saturating_add(nested_hdr(src, info, &effective, req, false))
}

/// Check the same exact-then-approximate policy as the wasm renderer. Retained
/// bytes exclude the active source and metadata, already in the estimate.
/// This uncached check is also available to native regression probes.
pub fn check_wasm_render(
    src: &Rgb32f,
    info: &SourceInfo,
    settings: &DevelopSettings,
    req: &RenderRequest,
    retained_bytes: u64,
) -> Result<(), String> {
    let effective = settings.effective();
    let settings = crate::settings_for(&effective, req);
    let plan = crate::plan(src, info, &settings, req);
    let full = plan.needs_airlight.then(|| crate::plan(src, info, &settings, &RenderRequest { window: None, ..*req }));
    select(src, info, &settings, &effective, req, false, retained_bytes, false, plan, full).map(|_| ())
}

pub(crate) fn nested_hdr(src: &Rgb32f, info: &SourceInfo, settings: &DevelopSettings, req: &RenderRequest, approximate: bool) -> u64 {
    if req.overlay != Overlay::HdrRange || !settings.hdr.enabled {
        return 0;
    }
    let hdr = RenderRequest { depth: OutputDepth::F32Hdr, overlay: Overlay::None, proof: None, ..*req };
    // The outer preparation remains live during the nested render. Source and
    // metadata belong to both calls, but only one allocation exists.
    let plan = crate::plan_window(src, info, settings, &hdr, approximate);
    let full = (plan.needs_airlight && !approximate).then(|| crate::plan(src, info, settings, &RenderRequest { window: None, ..hdr }));
    estimate(src, info, &plan, full.as_ref(), &hdr, false, approximate).saturating_sub(source_bytes(src, info))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn select<'a>(
    src: &Rgb32f,
    info: &SourceInfo,
    settings: &'a DevelopSettings,
    edit: &DevelopSettings,
    req: &RenderRequest,
    cached: bool,
    retained: u64,
    approximate_only: bool,
    exact: Plan<'a>,
    full: Option<Plan<'a>>,
) -> Result<(Plan<'a>, Option<Plan<'a>>, bool), String> {
    let valid = req.window.is_none() || crate::airlight::check_pixels(exact.w, exact.h).is_ok();
    let valid = valid
        && full
            .as_ref()
            .filter(|f| f.lin_key != exact.lin_key)
            .is_none_or(|f| crate::airlight::check_prepare(f, Some(&exact)).is_ok() && crate::airlight::check_pixels(src.width, src.height).is_ok());
    if !approximate_only
        && valid
        && estimate(src, info, &exact, full.as_ref(), req, cached, false)
            .saturating_add(retained)
            .saturating_add(nested_hdr(src, info, edit, req, false))
            <= WASM_RENDER_BYTES
    {
        return Ok((exact, full, false));
    }
    if req.window.is_some() {
        let approximate = crate::plan_window(src, info, settings, req, true);
        if estimate(src, info, &approximate, None, req, cached, true).saturating_add(retained).saturating_add(nested_hdr(src, info, edit, req, true))
            <= WASM_RENDER_BYTES
        {
            return Ok((approximate, None, true));
        }
    }
    Err("The render exceeds the web worker's memory budget. Use a smaller source or reduce the zoom or output size.".into())
}

fn pixels(w: usize, h: usize) -> u64 {
    (w as u64).saturating_mul(h as u64)
}

pub(crate) fn source_bytes(src: &Rgb32f, info: &SourceInfo) -> u64 {
    let mut live = LiveImages::default();
    live.source(src, info);
    // Declared dimensions also bound callers that have not decoded the source yet.
    live.bytes().saturating_add(pixels(src.width, src.height).saturating_sub(src.data.capacity() as u64).saturating_mul(12))
}

/// Additional peak while Frame::sample runs, followed by its retained result.
fn sampling(src: &Rgb32f, frame: &geometry::Frame, w: usize, h: usize, shared: bool) -> (u64, u64) {
    let p = frame.sample_plan(src.width, src.height, w, h);
    let n = pixels(w, h).saturating_mul(12);
    let oriented = if frame.orient == lightcraft_geom::Orientation::Normal { 0 } else { pixels(src.width, src.height).saturating_mul(12) };
    if shared && oriented == 0 && p.prefilter.is_none() && matches!(p.mode, geometry::SampleMode::Copy) {
        return (0, 0);
    }
    let peak = if let Some((pw, ph)) = p.prefilter {
        let base = pixels(pw, ph).saturating_mul(12);
        let tmp = pixels(pw, p.oh).saturating_mul(12);
        // Mitchell tables hold at most roughly four source taps per axis plus
        // destination vectors. Use native-sized table entries on both targets.
        let taps = (p.ow as u64).saturating_add(p.oh as u64).saturating_add(pw as u64).saturating_add(ph as u64).saturating_mul(64);
        base.saturating_add(tmp).saturating_add(taps).max(base.saturating_add(if matches!(p.mode, geometry::SampleMode::Copy) { 0 } else { n }))
    } else {
        n
    };
    (oriented.saturating_add(peak), n)
}

fn upstream_scratch(p: &Plan<'_>) -> u64 {
    let n = pixels(p.w, p.h);
    let s = &p.settings;
    let (lum, col) = local::nr_params(s, p.src_long, p.frame.output_long(p.w, p.h));
    // NR runs its luminance and colour passes sequentially. Guided luminance
    // holds its input plus eight scalar planes; colour holds three RGB planes.
    let mut per_pixel: u64 = if lum.is_some() || col.is_some() { 36 } else { 0 };
    if crate::optics::defringe_radius(s, p.px_per_long / crate::optics::DEFRINGE_REF_LONG) > 0 {
        per_pixel = per_pixel.max(28);
    }
    n.saturating_mul(per_pixel).max(spot_scratch(p))
}

fn spot_scratch(p: &Plan<'_>) -> u64 {
    use lightcraft_develop::SpotMode;
    let transform = p.frame.norm_to_out(p.w, p.h);
    p.settings
        .spots
        .iter()
        .map(|spot| {
            if spot.points.is_empty() {
                return 0;
            }
            let r = (spot.size * p.px_per_long).max(1.0) as f32;
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for point in &spot.points {
                let point = transform.apply(*point);
                let (x, y) = (point.x as f32, point.y as f32);
                x0 = x0.min(x - r);
                y0 = y0.min(y - r);
                x1 = x1.max(x + r);
                y1 = y1.max(y + r);
            }
            // Match spots::apply's patch rectangle, including its Fill padding.
            let pad = if spot.mode == SpotMode::Fill { 2.0 * r } else { r };
            let left = (x0 - pad).floor().max(0.0) as usize;
            let top = (y0 - pad).floor().max(0.0) as usize;
            let right = ((x1 + pad).ceil() as usize).min(p.w);
            let bottom = ((y1 + pad).ceil() as usize).min(p.h);
            // Alpha and two RGB patches; Heal also holds diff/weights, their blurs
            // and the corrected patch. Fill has a bounded inpainting pyramid.
            let bytes = match spot.mode {
                SpotMode::Clone => 28,
                SpotMode::Heal | SpotMode::Remove => 72,
                SpotMode::Fill => 256,
            };
            pixels(right.saturating_sub(left), bottom.saturating_sub(top)).saturating_mul(bytes)
        })
        .max()
        .unwrap_or(0)
}

/// (peak scratch including the result, retained bytes) after log luminance.
fn spatial(p: &Plan<'_>, req: &RenderRequest) -> (u64, u64) {
    let n = pixels(p.w, p.h);
    let sig = local::plane_sigmas(&p.settings, p.px_per_long, req.quality);
    let mut retained = 0_u64;
    let mut peak = 0_u64;
    if sig.chroma.is_some() {
        peak = n.saturating_mul(36);
        retained = n.saturating_mul(12);
    }
    let mut parallel = 0_u64;
    for (sigma, median) in [(sig.base, sig.base_detail > 0), (sig.clarity, false)] {
        if let Some(sigma) = sigma {
            // Guided coefficients are on a reduced grid. Full planes are the
            // two upsampled coefficients and result, plus the V2 median input.
            let step = local::guided_fast_step(sigma);
            let scratch = if step <= 1 {
                n.saturating_mul(32 + if median { 4 } else { 0 })
            } else {
                let lo = pixels(p.w.div_ceil(step).saturating_add(2), p.h.div_ceil(step).saturating_add(2));
                n.saturating_mul(12 + if median { 4 } else { 0 }).saturating_add(lo.saturating_mul(36))
            };
            parallel = parallel.saturating_add(scratch);
        }
    }
    if sig.texture.is_some() {
        parallel = parallel.saturating_add(n.saturating_mul(8));
    }
    if sig.dark.is_some() {
        parallel = parallel.saturating_add(n.saturating_mul(12));
    }
    peak = peak.max(retained.saturating_add(parallel));
    retained =
        retained.saturating_add(n.saturating_mul(4 * [sig.base, sig.clarity, sig.texture, sig.dark].iter().filter(|v| v.is_some()).count() as u64));
    (peak, retained)
}

fn mask_scratch(mask: &lightcraft_develop::Mask) -> u64 {
    let auto = mask.components.iter().any(|c| matches!(&c.shape, MaskShape::Brush { strokes } if strokes.iter().any(|s| s.auto_mask)));
    if auto {
        48
    } else if mask.refine > 0.0 {
        40
    } else if mask.components.is_empty() {
        4
    } else {
        20
    }
}

fn mask_bytes(p: &Plan<'_>, req: &RenderRequest) -> (u64, u64) {
    let n = pixels(p.w, p.h);
    let mut retained = 0_u64;
    let mut peak = 0_u64;
    let mut refined = false;
    for m in p.settings.masks.iter().filter(|m| m.visible && !m.components.is_empty()) {
        // Component planes and heuristic smoothing, plus guidance only for
        // Refine Edges or an automatic brush. Two grouped RGBA blur buffers
        // account for 32 B/px of the refinement peak.
        let scratch = mask_scratch(m);
        let matte = p.mattes.as_ref().map_or(0, |mattes| mattes.sampling_bytes(m, &p.frame, p.w));
        peak = peak.max(retained.saturating_add(n.saturating_mul(scratch)).saturating_add(matte));
        retained = retained.saturating_add(n.saturating_mul(4));
        if m.refine > 0.0 {
            retained = retained.saturating_add(n.saturating_mul(4 + if refined { 0 } else { 8 }));
            refined = true;
        }
    }
    if let Some(m) = req.overlay.mask(&p.settings) {
        let extra = if m.visible && !m.components.is_empty() { 4 } else { mask_scratch(m) };
        let matte = p.mattes.as_ref().map_or(0, |mattes| mattes.sampling_bytes(m, &p.frame, p.w));
        peak = peak.max(retained.saturating_add(n.saturating_mul(extra)).saturating_add(matte));
    }
    (peak, retained)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn estimate(
    src: &Rgb32f,
    info: &SourceInfo,
    p: &Plan<'_>,
    full: Option<&Plan<'_>>,
    req: &RenderRequest,
    cached: bool,
    approximate: bool,
) -> u64 {
    let n = pixels(p.w, p.h);
    let source = source_bytes(src, info);
    let separate = full.filter(|f| f.lin_key != p.lin_key);
    let (sampling_peak, sampled) =
        if separate.is_some() { (n.saturating_mul(28), n.saturating_mul(12)) } else { sampling(src, &p.frame, p.w, p.h, cached) };
    let linear = sampled.saturating_add(n.saturating_mul(12));
    let base = linear.saturating_add(n.saturating_mul(4)); // log luminance is always present
    let (spatial_peak, planes) = spatial(p, req);
    let prepared = base.saturating_add(planes);
    let (mask_peak, masks) = mask_bytes(p, req);
    let out = p.keep.map_or(n, |k| pixels(k.w, k.h));
    let output = match req.depth {
        OutputDepth::U8 if req.overlay == Overlay::None => out.saturating_mul(4),
        OutputDepth::U8 => n.saturating_add(out).saturating_mul(4),
        OutputDepth::U16 => out.saturating_mul(10),
        OutputDepth::F32Linear | OutputDepth::F32Hdr => out.saturating_mul(16),
    };
    let mut work = sampling_peak.max(linear.saturating_add(upstream_scratch(p))).max(base.saturating_add(spatial_peak));
    let overlay_output = if req.overlay == Overlay::None { 0 } else { output };
    work = work.max(prepared.saturating_add(mask_peak).saturating_add(overlay_output)).max(prepared.saturating_add(masks).saturating_add(output));
    if let Some(f) = separate {
        let full_n = pixels(f.w, f.h);
        let (sample_peak, sample) = sampling(src, &f.frame, f.w, f.h, false);
        let lin = sample.saturating_add(full_n.saturating_mul(12));
        let full_peak = sample_peak.max(lin.saturating_add(upstream_scratch(f))).max(lin.saturating_add(full_n.saturating_mul(8)));
        let full_live = full_n.saturating_mul(28);
        let copying = full_live.saturating_add(n.saturating_mul(28));
        work = work.saturating_add(if cached && full_live <= crate::airlight::FrameCache::<()>::BUDGET as u64 { full_live } else { 0 });
        work = work.max(full_peak).max(copying);
    }
    if approximate && p.needs_airlight {
        let frame = geometry::Frame::with_lens(src.width, src.height, &p.settings, req.apply_crop, info.lens.as_ref());
        let (w, h) = frame.fit(384, 384);
        let (sample_peak, sample) = sampling(src, &frame, w, h, false);
        work = work.max(sample_peak.max(sample.saturating_add(pixels(w, h).saturating_mul(12))));
    }
    if p.settings.optics.remove_ca && p.settings.section_enabled("optics") {
        // The first automatic lateral-CA estimate reads three source planes
        // during planning, before any work-window images exist.
        work = work.max(pixels(src.width, src.height).saturating_mul(12));
    }
    // Row scratch, resize taps and small settings/cache records. Unlike a
    // per-pixel multiplier this does not charge inactive full-image filters.
    let edges = (p.w as u64).saturating_add(p.h as u64).saturating_add(src.width as u64).saturating_add(src.height as u64);
    source.saturating_add(work).saturating_add(edges.saturating_mul(256)).saturating_add(BOOKKEEPING_BYTES)
}

/// Main's bounded, white-balanced 384 px Dehaze proxy, used only by the web
/// approximation when exact full-frame preparation cannot be admitted.
pub(crate) fn approximate_airlight(src: &Rgb32f, info: &SourceInfo, settings: &DevelopSettings, req: &RenderRequest) -> f32 {
    let frame = crate::frame_for(src, info, settings, req.apply_crop);
    let (full_w, full_h) = frame.fit(req.max_w, req.max_h);
    let k = (384.0 / full_w.max(full_h).max(1) as f64).min(1.0);
    let (w, h) = (((full_w as f64 * k).round() as usize).max(1), ((full_h as f64 * k).round() as usize).max(1));
    let mut img = frame.sample(src, w, h);
    local::white_balance(&mut img, info, settings);
    let sigma = (0.02 * frame.px_per_long(w)).max(1.0) as f32;
    local::airlight(&lightcraft_raster::blur::gaussian(&img.map(local::dark_of), sigma))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PixelWindow, StageCache};
    use std::sync::Arc;

    #[test]
    fn ordinary_full_renders_fit_and_only_charge_live_phases() {
        for (w, h) in [(6000, 4000), (9504, 6336)] {
            let src = Rgb32f { width: w, height: h, data: vec![] };
            let settings = DevelopSettings::default();
            for depth in [OutputDepth::U8, OutputDepth::F32Linear] {
                let req = RenderRequest { depth, ..RenderRequest::fit(w, h) };
                let n = pixels(w, h);
                for cached in [false, true] {
                    let bytes = estimate_wasm_render(&src, &SourceInfo::default(), &settings, &req, cached);
                    let measured = n * (if cached { 32 } else { 44 } + if depth == OutputDepth::U8 { 0 } else { 12 });
                    assert!(bytes > measured && bytes < measured * 3 / 2, "{w}x{h} {depth:?} cached={cached}: {bytes}");
                    assert!(bytes < WASM_RENDER_BYTES);
                }
                assert!(check_wasm_render(&src, &SourceInfo::default(), &settings, &req, 0).is_ok());
            }
        }
    }

    #[test]
    fn sampling_estimate_distinguishes_shared_oriented_and_prefiltered_sources() {
        let src = Rgb32f::new(60, 40);
        let info = SourceInfo::default();
        let mut s = DevelopSettings::default();
        let req = RenderRequest::fit(60, 40);
        let frame = crate::plan(&src, &info, &s, &req).frame;
        assert_eq!(sampling(&src, &frame, 60, 40, true), (0, 0));
        assert_eq!(sampling(&src, &frame, 60, 40, false), (60 * 40 * 12, 60 * 40 * 12));
        s.orientation = lightcraft_geom::Orientation::Rotate180;
        let frame = crate::plan(&src, &info, &s, &req).frame;
        assert_eq!(sampling(&src, &frame, 60, 40, true), (60 * 40 * 24, 60 * 40 * 12));
        let frame = crate::plan(&src, &info, &s, &RenderRequest::fit(15, 10)).frame;
        assert!(sampling(&src, &frame, 15, 10, true).0 >= (60 * 40 + 15 * 40 + 15 * 10) * 12);
    }

    #[test]
    fn small_windows_count_large_matte_conversion_and_source_analysis() {
        use crate::masks::{MatteKind, Mattes};
        use lightcraft_develop::{Mask, MaskComponent, MaskOp};
        let src = Rgb32f::new(600, 400);
        let mut s = DevelopSettings::default();
        s.masks.push(Mask {
            components: vec![MaskComponent { name: None, op: MaskOp::Add, invert: false, shape: MaskShape::Sky }],
            ..Default::default()
        });
        let mut mattes = Mattes::default();
        mattes.push(MatteKind::Sky, Image::new(600, 400));
        let info = SourceInfo { mattes: Some(Arc::new(mattes)), ..Default::default() };
        let req = RenderRequest { window: Some(PixelWindow { x: 200, y: 100, w: 12, h: 8 }), ..RenderRequest::fit(600, 400) };
        let p = crate::plan(&src, &info, &s, &req);
        assert_eq!(info.mattes.as_ref().unwrap().sampling_bytes(&s.masks[0], &p.frame, p.w), 600 * 400 * 4);
        let bytes = estimate_wasm_render(&src, &info, &s, &req, true);
        assert!(bytes > 600 * 400 * 17, "RGB source, byte matte and converted float matte: {bytes}");
        s.masks.clear();
        s.optics.remove_ca = true;
        assert!(estimate_wasm_render(&src, &SourceInfo::default(), &s, &req, true) > 600 * 400 * 24);
    }

    #[test]
    fn source_and_shared_stage_allocations_are_counted_once() {
        let src = Arc::new(Rgb32f::new(121, 83));
        let weak = Arc::downgrade(&src);
        let cache = StageCache::default();
        crate::render_cached(&src, &SourceInfo::default(), &DevelopSettings::default(), &RenderRequest::fit(121, 83), &cache).unwrap();
        let mut live = LiveImages::default();
        live.source(&src, &SourceInfo::default());
        cache.count_retained_images(&mut live);
        let once = live.bytes();
        live.source(&src, &SourceInfo::default());
        cache.count_retained_images(&mut live);
        assert_eq!(live.bytes(), once);
        assert_eq!(once, cache.bytes() as u64 + (src.data.len() * 12) as u64);
        drop(src);
        assert!(weak.upgrade().is_some());
        assert_eq!(cache.retained_bytes() as u64, once);
        cache.clear();
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn web_falls_back_before_exact_window_limits_but_rejects_unrenderable_output() {
        let src = Arc::new(Rgb32f::new(60, 40));
        let info = SourceInfo::default();
        let mut s = DevelopSettings::default();
        s.effects.dehaze = 80.0;
        let req = RenderRequest { window: Some(PixelWindow { x: 20000, y: 12000, w: 121, h: 83 }), ..RenderRequest::fit(48000, 32000) };
        let exact = crate::plan(&src, &info, &s, &req);
        let full = crate::plan(&src, &info, &s, &RenderRequest { window: None, ..req });
        let (plan, full, approximate) = select(&src, &info, &s, &s, &req, true, 0, false, exact, Some(full)).unwrap();
        assert!(approximate && full.is_none());
        assert_eq!((plan.w, plan.h), (121, 83));
        assert_ne!(plan.geo, crate::plan(&src, &info, &s, &req).geo);
        let out = crate::render_web(&src, &info, &s, &req, None, 0).unwrap();
        assert_eq!((out.image.width, out.image.height), (121, 83));
        // The native policy remains exact and rejects the same virtual frame.
        #[cfg(not(target_arch = "wasm32"))]
        assert!(crate::render(&src, &info, &s, &req).is_err());
        let huge = RenderRequest { window: Some(PixelWindow { x: 0, y: 0, w: 48000, h: 32000 }), ..req };
        assert!(crate::render_web(&src, &info, &s, &huge, None, 0).err().unwrap().contains("memory budget"));
        assert!(crate::render_web(&src, &info, &s, &RenderRequest::fit(60, 40), None, 0).is_ok());
        assert!(check_wasm_render(&src, &info, &s, &req, u64::MAX).is_err());
    }

    #[test]
    fn three_refined_masks_at_61_mp_select_a_fitting_window_plan() {
        use lightcraft_develop::{LocalAdjustments, Mask, MaskComponent, MaskOp};
        use lightcraft_geom::Point;
        let src = Rgb32f { width: 9504, height: 6336, data: vec![] };
        let info = SourceInfo::default();
        let mut s = DevelopSettings::default();
        s.effects.dehaze = 80.0;
        s.effects.clarity = 80.0;
        s.effects.texture = 60.0;
        s.light.highlights = -80.0;
        s.light.shadows = 80.0;
        s.detail.nr_luminance = 70.0;
        s.detail.nr_color = 60.0;
        s.detail.sharpen_amount = 60.0;
        s.optics.defringe_purple_amount = 10.0;
        for shape in [
            MaskShape::Radial { center: Point::new(0.45, 0.5), rx: 0.24, ry: 0.2, angle: 15.0, feather: 65.0, invert: false },
            MaskShape::Subject,
            MaskShape::Sky,
        ] {
            s.masks.push(Mask {
                refine: 100.0,
                components: vec![MaskComponent { name: None, op: MaskOp::Add, invert: false, shape }],
                adjust: LocalAdjustments {
                    exposure: 1.5,
                    clarity: 60.0,
                    texture: 60.0,
                    noise: 70.0,
                    moire: 50.0,
                    defringe: 80.0,
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        let req = RenderRequest { window: Some(PixelWindow { x: 2560, y: 1792, w: 4096, h: 2816 }), ..RenderRequest::fit(9504, 6336) };
        let exact = crate::plan(&src, &info, &s, &req);
        let full = crate::plan(&src, &info, &s, &RenderRequest { window: None, ..req });
        assert!(estimate(&src, &info, &exact, Some(&full), &req, false, false) > WASM_RENDER_BYTES);
        let (plan, full, approximate) = select(&src, &info, &s, &s, &req, false, 0, false, exact, Some(full)).unwrap();
        assert!(approximate && full.is_none());
        assert_eq!((plan.w, plan.h), (4096, 2816));
        let bytes = estimate(&src, &info, &plan, None, &req, false, true);
        assert!(bytes < WASM_RENDER_BYTES);
        println!("61 MP approximate window estimate: {bytes} bytes");
    }

    #[test]
    fn hdr_overlay_falls_back_for_both_renditions() {
        let src = Arc::new(Rgb32f::filled(60, 40, [0.8, 0.4, 0.2]));
        let mut s = DevelopSettings::default();
        s.hdr.enabled = true;
        s.effects.dehaze = 80.0;
        let req = RenderRequest {
            overlay: Overlay::HdrRange,
            window: Some(PixelWindow { x: 20000, y: 12000, w: 121, h: 83 }),
            ..RenderRequest::fit(48000, 32000)
        };
        let result = crate::render_web(&src, &SourceInfo::default(), &s, &req, None, 0).unwrap();
        assert_eq!((result.image.width, result.image.height), (121, 83));
    }

    #[test]
    fn inactive_filters_and_masks_do_not_charge_refinement_or_nr() {
        let src = Rgb32f::new(60, 40);
        let info = SourceInfo::default();
        let mut s = DevelopSettings::default();
        let req = RenderRequest::fit(1200, 800);
        let plain = estimate_wasm_render(&src, &info, &s, &req, true);
        s.masks = vec![lightcraft_develop::Mask::default(); 500];
        assert_eq!(estimate_wasm_render(&src, &info, &s, &req, true), plain);
        s.detail.nr_color = 60.0;
        assert!(estimate_wasm_render(&src, &info, &s, &req, true) > plain);
    }
}
