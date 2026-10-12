//! Full-frame Dehaze statistics for windows. Each renderer keeps its own scalar so its
//! sampling, upstream stages and floating-point implementation match its full render.

use crate::{Plan, SourceInfo, local};
use lightcraft_raster::Rgb32f;
use std::sync::{Arc, Mutex, Weak};

#[derive(Default)]
pub struct AirlightCache {
    entries: Mutex<Vec<(Weak<Rgb32f>, u64, f32)>>,
}

impl AirlightCache {
    pub fn clear(&self) {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Compute once for this source and full-frame linear-stage key. The key excludes the
    /// window rectangle and downstream adjustments, including Exposure and Dehaze amount.
    pub fn get_or_compute(&self, src: &Arc<Rgb32f>, key: u64, compute: impl FnOnce() -> f32) -> f32 {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, air)) = entries.iter().find(|(s, k, _)| *k == key && s.upgrade().is_some_and(|s| Arc::ptr_eq(&s, src))) {
            return *air;
        }
        let air = compute();
        Self::put(&mut entries, src, key, air);
        air
    }

    /// Seed the scalar from an already prepared full-frame dark channel.
    pub fn remember(&self, src: &Arc<Rgb32f>, key: u64, air: f32) {
        Self::put(&mut self.entries.lock().unwrap_or_else(|e| e.into_inner()), src, key, air);
    }

    fn put(entries: &mut Vec<(Weak<Rgb32f>, u64, f32)>, src: &Arc<Rgb32f>, key: u64, air: f32) {
        entries.retain(|(s, k, _)| s.upgrade().is_some_and(|s| *k != key || !Arc::ptr_eq(&s, src)));
        entries.push((Arc::downgrade(src), key, air));
        if entries.len() > 2 {
            entries.remove(0);
        }
    }
}

/// CPU full-render statistic, with the same resolution, upstream processing, blur and
/// every-seventh-value selection as `local::prepare`. `full` must describe window=None.
pub fn compute(src: &Rgb32f, info: &SourceInfo, full: &Plan<'_>) -> f32 {
    let mut img = full.frame.sample(src, full.w, full.h);
    crate::lin_cpu(&mut img, info, full);
    local::denoise(&mut img, &full.settings, full.src_long, full.frame.output_long(full.w, full.h));
    let dark = img.map(local::dark_of);
    drop(img);
    let sigma = (0.02 * full.px_per_long as f32).max(1.0);
    local::airlight(&lightcraft_raster::blur::gaussian(&dark, sigma))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PixelWindow, RenderRequest, StageCache};
    use lightcraft_develop::{DevelopSettings, Mask, MaskComponent, MaskOp, MaskShape, WbMode};
    use lightcraft_geom::Point;

    #[test]
    fn statistic_matches_full_prepare_and_cache_tracks_upstream_inputs() {
        let src = Arc::new(Rgb32f::from_fn(121, 83, |x, y| {
            let l = 0.01 + ((x * 17 + y * 23) % 103) as f32 / 103.0;
            [l, l * 0.82, l * 1.15]
        }));
        let mut info = SourceInfo::default();
        let mut s = DevelopSettings::default();
        s.effects.dehaze = 100.0;
        let req = RenderRequest::fit(481, 327);
        let cache = AirlightCache::default();
        let mut last_key = None;
        for step in 0..9 {
            match step {
                1 => s.light.exposure = 0.5,
                2 => s.effects.dehaze = 80.0,
                3 => {
                    s.wb.mode = WbMode::Custom;
                    s.wb.temp = 9000.0;
                }
                4 => s.detail.nr_luminance = 80.0,
                5 => s.detail.nr_color = 60.0,
                6 => s.optics.defringe_purple_amount = 50.0,
                7 => s.crop.flip_h = true,
                8 => info.relative_wb = true,
                _ => {}
            }
            let full = crate::plan(&src, &info, &s, &req);
            let air = compute(&src, &info, &full);
            let mut img = full.frame.sample(&src, full.w, full.h);
            crate::lin_cpu(&mut img, &info, &full);
            local::denoise(&mut img, &s, full.src_long, full.frame.output_long(full.w, full.h));
            let mut planes = local::Planes::default();
            let prep = local::prepare(
                Arc::new(img),
                &s,
                &full.frame,
                full.px_per_long,
                req.quality,
                &mut planes,
                None,
                (&mut crate::masks::AlphaCache::default(), crate::masks::AlphaKeys { geo: full.geo, image: full.lin_key }),
            );
            assert_eq!(air.to_bits(), prep.air.to_bits(), "upstream step {step}");
            let mut computed = false;
            assert_eq!(
                cache.get_or_compute(&src, full.lin_key, || {
                    computed = true;
                    air
                }),
                air
            );
            assert_eq!(computed, last_key != Some(full.lin_key));
            if matches!(step, 1 | 2) {
                assert!(!computed, "downstream edit invalidated airlight");
            } else if step < 8 {
                assert!(computed, "upstream edit reused airlight");
            }
            last_key = Some(full.lin_key);
        }
        let other = Arc::new((*src).clone());
        let full = crate::plan(&other, &info, &s, &req);
        let mut computed = false;
        cache.get_or_compute(&other, full.lin_key, || {
            computed = true;
            0.25
        });
        assert!(computed, "source identity must invalidate the statistic");
        cache.clear();
        assert!(cache.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn window_pan_and_exposure_reuse_full_statistic_and_refined_alphas() {
        let src = Arc::new(Rgb32f::from_fn(301, 203, |x, y| {
            let l = 0.01 + ((x * 17 + y * 23) % 103) as f32 / 103.0;
            [l, l * 0.82, l * 1.15]
        }));
        let info = SourceInfo::default();
        let mut s = DevelopSettings::default();
        s.effects.dehaze = 100.0;
        for (id, shape) in
            [MaskShape::Linear { start: Point::new(0.2, 0.1), end: Point::new(0.8, 0.9) }, MaskShape::Subject, MaskShape::Sky].into_iter().enumerate()
        {
            s.masks.push(Mask {
                id: id as u32,
                refine: 100.0,
                components: vec![MaskComponent { name: None, op: MaskOp::Add, invert: false, shape }],
                ..Default::default()
            });
        }
        let cache = StageCache::default();
        let mut previous = Vec::new();
        for i in 0..3 {
            s.light.exposure = i as f64 * 0.1;
            let req = RenderRequest {
                window: Some(PixelWindow { x: 51 + if i == 2 { 3 } else { 0 }, y: 13, w: 71, h: 53 }),
                ..RenderRequest::fit(601, 403)
            };
            let warm = crate::render_cached(&src, &info, &s, &req, &cache);
            assert_eq!(warm.image, crate::render(&src, &info, &s, &req).image);
            let entries = cache.lock();
            let current: Vec<_> = entries.last().unwrap().masks.planes().cloned().collect();
            if i == 1 {
                assert!(Arc::ptr_eq(&previous[0], &current[0]), "geometric refined alpha");
                assert!(Arc::ptr_eq(&previous[1], &current[1]), "Subject heuristic refined alpha");
                assert!(!Arc::ptr_eq(&previous[2], &current[2]), "Sky reads Exposure");
            }
            if i == 2 {
                assert!(previous.iter().zip(&current).all(|(a, b)| !Arc::ptr_eq(a, b)), "a different work rect invalidates alphas");
            }
            previous = current;
            assert_eq!(cache.airlight.entries.lock().unwrap().len(), 1, "panning must reuse the full-frame statistic");
        }
    }
}
