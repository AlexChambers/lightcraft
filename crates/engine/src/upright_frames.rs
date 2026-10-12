//! Upright corrections belong to the frame of the photo they are stored with
//! (`Geometry::upright_transform`: centred coordinates of the photo as shown, after its file's
//! orientation and its own Rotate Left / Right). Settings moved between photos (Paste, Paste from
//! Previous, Sync, Auto Sync) re-express a correction in the target's frame through the stored
//! image, which is how Lightroom keeps it, and a settings patch that turns a photo turns the
//! correction it keeps (issue #205).

use lightcraft_catalog::Photo;
use lightcraft_develop::DevelopSettings;
use lightcraft_geom::{Homography, Orientation};
use serde_json::Value;

use crate::sidecar::StoredFrame;

/// A photo's frame for its Upright correction: its file's stored image, then its own rotation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhotoFrame {
    pub stored: StoredFrame,
    pub user: Orientation,
}

/// The frame of photo `p` with settings `s`. A photo imported before the file's orientation was
/// recorded is taken as stored the way it is shown.
pub fn frame_of(p: &Photo, s: &DevelopSettings) -> Option<PhotoFrame> {
    let stored = crate::sidecar::recorded_frame(p).or_else(|| StoredFrame::of_shown(Orientation::Normal, f64::from(p.width), f64::from(p.height)))?;
    Some(PhotoFrame { stored, user: s.orientation })
}

/// Correction `h`, made in frame `from`, as the same correction of the stored image in frame `to`.
pub fn reexpress(h: &Homography, from: &PhotoFrame, to: &PhotoFrame) -> Option<Homography> {
    let on_stored = crate::crs::upright_on_stored(h, from.stored.orientation.then(from.user), from.stored.width, from.stored.height)?;
    let saved = crate::crs::SavedUpright { mode: lightcraft_develop::Upright::Auto, matrix: on_stored };
    crate::crs::upright_in_frame(&saved, to.stored.orientation.then(to.user), to.stored.width, to.stored.height)
}

/// Settings `after` of photo `p` (which had `before`) once `patch` is applied: a correction the
/// patch brought from photo frame `source` is put into `p`'s frame; a correction `p` keeps is
/// turned when the patch turned the photo. A patch from nowhere in particular (a preset) leaves a
/// correction it brings as it is.
pub fn settle(p: &Photo, before: &DevelopSettings, after: &mut DevelopSettings, patch: &Value, source: Option<&PhotoFrame>) {
    let Some(target) = frame_of(p, after) else { return };
    let (from, h) = if patch.pointer("/geometry/upright_transform").is_some() {
        (source.copied(), after.geometry.upright_transform)
    } else if after.orientation != before.orientation {
        (frame_of(p, before), after.geometry.upright_transform)
    } else {
        return;
    };
    if let (Some(from), Some(h)) = (from, h) {
        // a frame it can't be put in (no size) leaves it to be solved again
        after.geometry.upright_transform = reexpress(&h, &from, &target);
    }
}

/// `patch` applied to settings `before` of photo `p` for a transfer from photo frame `source`,
/// with the Upright correction in `p`'s frame ([`settle`]).
pub fn transferred(p: &Photo, before: &DevelopSettings, patch: &Value, source: Option<&PhotoFrame>) -> DevelopSettings {
    let mut after = lightcraft_develop::apply_partial(before, patch, 1.0);
    settle(p, before, &mut after, patch, source);
    after
}
