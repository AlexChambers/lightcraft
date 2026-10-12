//! Conservative admission for CPU renders in a wasm32 worker.
//!
//! wasm32 has at most 4 GiB of linear memory. Leave 1 GiB for the worker runtime,
//! settings, encoded input, allocator overhead and fragmentation. This is a render
//! admission estimate, not a promise that every browser can provide that memory.

use crate::{Plan, RenderRequest, SourceInfo};
use lightcraft_develop::DevelopSettings;
use lightcraft_raster::Rgb32f;

pub const WASM_RENDER_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// Check before allocating any work images. `retained_bytes` includes all of the
/// caller's live image caches, including sources retained by older stage entries.
/// Shared buffers may be counted twice; admission must not depend on a cache hit.
/// Available on native so worker tests exercise the same wasm32 budget.
pub fn check_wasm_render(
    src: &Rgb32f,
    info: &SourceInfo,
    settings: &DevelopSettings,
    req: &RenderRequest,
    retained_bytes: u64,
) -> Result<(), String> {
    let effective = settings.effective();
    let nested_hdr = req.overlay == crate::Overlay::HdrRange && effective.hdr.enabled;
    let settings = crate::settings_for(&effective, req);
    let plan = crate::plan(src, info, &settings, req);
    let full = plan.needs_airlight.then(|| crate::plan(src, info, &settings, &RenderRequest { window: None, ..*req }));
    let nested = if nested_hdr {
        // The HDR-range overlay renders a second image while the SDR preparation
        // is still live. Its filter support can differ from the SDR rendition.
        let hdr_req = RenderRequest { depth: crate::OutputDepth::F32Hdr, overlay: crate::Overlay::None, proof: None, ..*req };
        let hdr = crate::plan(src, info, &effective, &hdr_req);
        let hdr_full = hdr.needs_airlight.then(|| crate::plan(src, info, &effective, &RenderRequest { window: None, ..hdr_req }));
        estimate(src, info, &hdr, hdr_full.as_ref())
    } else {
        0
    };
    check(src, info, &plan, full.as_ref(), retained_bytes.saturating_add(nested))
}

fn check(src: &Rgb32f, info: &SourceInfo, work: &Plan<'_>, full: Option<&Plan<'_>>, retained_bytes: u64) -> Result<(), String> {
    let bytes = estimate(src, info, work, full).saturating_add(retained_bytes);
    if bytes > WASM_RENDER_BYTES {
        return Err("The render exceeds the web worker's memory budget. Use a smaller source or reduce the zoom or output size.".into());
    }
    Ok(())
}

fn pixels(w: usize, h: usize) -> u64 {
    (w as u64).saturating_mul(h as u64)
}

fn estimate(src: &Rgb32f, info: &SourceInfo, work: &Plan<'_>, full: Option<&Plan<'_>>) -> u64 {
    // Source, oriented copy and both prefilter passes, even when sampling can share it.
    let source = pixels(src.width, src.height).max(src.data.capacity() as u64).saturating_mul(48);
    let mattes = info.mattes.as_ref().map_or(0, |m| m.bytes() as u64);
    let tone = info.local_tone.as_ref().map_or(0, |t| (t.map.gains.len() as u64).saturating_mul(4));
    // 64 B/px for two RGB images, spatial planes, chroma and output; 64 B/px
    // for peak NR/refinement scratch, including both four-channel blur buffers.
    // Each mask can retain an alpha and a blurred mean. Count hidden masks too:
    // overlays can evaluate them. Reserve the shared pair of guidance planes.
    let masks = work.settings.masks.len() as u64;
    let per_pixel = 128_u64.saturating_add(masks.saturating_mul(8)).saturating_add(if masks == 0 { 0 } else { 8 });
    let work_bytes = pixels(work.w, work.h).saturating_mul(per_pixel);
    // A separate exact Dehaze preparation can coexist with the entire work pass.
    // 96 B/px includes its retained RGB/dark planes and sampling/NR scratch.
    // Work equal to the full frame is already charged above, never skipped.
    let full_bytes = full.filter(|p| p.lin_key != work.lin_key).map_or(0, |p| pixels(p.w, p.h).saturating_mul(96));
    source.saturating_add(mattes).saturating_add(tone).saturating_add(work_bytes).saturating_add(full_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PixelWindow, StageCache};
    use std::sync::Arc;

    #[test]
    fn wasm32_budget_counts_full_work_sources_and_retained_images() {
        let src = Rgb32f::new(600, 400);
        let info = SourceInfo::default();
        let settings = DevelopSettings::default();
        let mut req = RenderRequest::fit(9504, 6336);
        req.window = Some(PixelWindow { x: 0, y: 0, w: 9504, h: 6336 });
        assert!(check_wasm_render(&src, &info, &settings, &req, 0).is_err());
        req.window = None;
        assert!(check_wasm_render(&src, &info, &settings, &req, 0).is_err());
        req = RenderRequest::fit(600, 400);
        assert!(check_wasm_render(&src, &info, &settings, &req, 0).is_ok());
        assert!(check_wasm_render(&src, &info, &settings, &req, WASM_RENDER_BYTES).is_err());
        assert!(check_wasm_render(&src, &info, &settings, &req, u64::MAX).is_err());
        // Dimensions alone exercise source accounting without a huge allocation.
        let declared = Rgb32f { width: 16000, height: 12000, data: vec![] };
        assert!(check_wasm_render(&declared, &info, &settings, &req, 0).is_err());
    }

    #[test]
    fn wasm32_budget_counts_separate_airlight_and_mask_growth() {
        let src = Rgb32f::new(600, 400);
        let info = SourceInfo::default();
        let mut settings = DevelopSettings::default();
        let req = RenderRequest { window: Some(PixelWindow { x: 4000, y: 3000, w: 121, h: 83 }), ..RenderRequest::fit(9504, 6336) };
        assert!(check_wasm_render(&src, &info, &settings, &req, 0).is_ok());
        settings.effects.dehaze = 15.0;
        assert!(check_wasm_render(&src, &info, &settings, &req, 0).is_err());
        settings.effects.dehaze = 0.0;
        let req = RenderRequest::fit(1200, 800);
        settings.masks = vec![lightcraft_develop::Mask::default(); 500];
        assert!(check_wasm_render(&src, &info, &settings, &req, 0).is_err());
    }

    #[test]
    fn retained_bytes_include_sources_after_the_owner_drops_them() {
        let src = Arc::new(Rgb32f::new(121, 83));
        let weak = Arc::downgrade(&src);
        let cache = StageCache::default();
        crate::render_cached(&src, &SourceInfo::default(), &DevelopSettings::default(), &RenderRequest::fit(121, 83), &cache).unwrap();
        let intermediates = cache.bytes();
        let retained = cache.retained_bytes();
        assert!(retained >= intermediates + src.data.len() * 12);
        drop(src);
        assert!(weak.upgrade().is_some());
        assert_eq!(cache.retained_bytes(), retained);
        cache.clear();
        assert_eq!(cache.retained_bytes(), 0);
        assert!(weak.upgrade().is_none());
    }
}
