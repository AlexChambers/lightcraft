//! An Upright correction (`Geometry::upright_transform`) is one photo's own analysis result: the
//! correction found for that image (by LightCraft, or Lightroom's saved one for it), stored so
//! previews and exports agree, in the frame the photo is shown in. When settings move or change
//! (issue #205):
//!
//! - between entries of the same file (a photo and its virtual copies) the correction moves too,
//!   turned by the difference in their Rotate Left / Right;
//! - between different files only the Upright mode and the Transform sliders move: the target
//!   keeps its own correction while its mode stays the same, else solves its own, as Lightroom
//!   does when it syncs the Upright mode (a correction found for one scene is wrong for another);
//! - presets never carry a correction (they are for other photos);
//! - a change that turns a photo turns the correction it keeps, and a change of mode drops it to
//!   be solved again.

use lightcraft_catalog::{Photo, PhotoId, Source};
use lightcraft_develop::DevelopSettings;
use lightcraft_geom::Orientation;
use serde_json::Value;

/// The photo settings were taken from, as far as its Upright correction is concerned.
#[derive(Clone, Debug, PartialEq)]
pub struct UprightSource {
    source: Source,
    master: PhotoId,
    /// Its Rotate Left / Right when the settings were taken.
    user: Orientation,
}

/// Photo `p` with settings `s` as the source of a transfer.
pub fn source_of(p: &Photo, s: &DevelopSettings) -> UprightSource {
    UprightSource { source: p.source.clone(), master: p.copy_of.unwrap_or(p.id), user: s.orientation }
}

/// Whether photo `p` shows the same file as `from`: the photo itself, its original or another of
/// its virtual copies.
fn same_file(from: &UprightSource, p: &Photo) -> bool {
    from.master == p.copy_of.unwrap_or(p.id) || from.source == p.source
}

/// The turn that takes a photo shown with `from` to `to` (both after the file's own orientation).
fn turn(from: Orientation, to: Orientation) -> Orientation {
    from.inverse().then(to)
}

/// Settings `after` of photo `p` (which had `before`) once `patch` was applied, with its Upright
/// correction settled as the module describes. `source`: the photo the patch was taken from
/// (`None`: none, as for a preset).
pub fn settle(p: &Photo, before: &DevelopSettings, after: &mut DevelopSettings, patch: &Value, source: Option<&UprightSource>) {
    if patch.pointer("/geometry/upright_transform").is_none() {
        return settle_edit(before, after);
    }
    let g = &mut after.geometry;
    g.upright_transform = match source.filter(|from| same_file(from, p)) {
        // the same file's correction, in this entry's rotation
        Some(from) => g.upright_transform.map(|h| lightcraft_pipeline::upright::turned(&h, turn(from.user, after.orientation))),
        // another file's (or a preset's): not this photo's; it keeps its own while the mode stays
        None if g.upright == before.geometry.upright => {
            before.geometry.upright_transform.map(|h| lightcraft_pipeline::upright::turned(&h, turn(before.orientation, after.orientation)))
        }
        None => None,
    };
}

/// A change of a photo's own settings (`before` to `after`) that doesn't state a correction: one
/// that turns the photo turns the correction it keeps; one that changes the Upright mode drops it
/// to be solved again.
pub fn settle_edit(before: &DevelopSettings, after: &mut DevelopSettings) {
    let g = &mut after.geometry;
    if g.upright != before.geometry.upright {
        g.upright_transform = None;
    } else if after.orientation != before.orientation {
        g.upright_transform = g.upright_transform.map(|h| lightcraft_pipeline::upright::turned(&h, turn(before.orientation, after.orientation)));
    }
}

/// `patch` applied to settings `before` of photo `p`, taken from `source` ([`settle`]).
pub fn transferred(p: &Photo, before: &DevelopSettings, patch: &Value, source: Option<&UprightSource>) -> DevelopSettings {
    let mut after = lightcraft_develop::apply_partial(before, patch, 1.0);
    settle(p, before, &mut after, patch, source);
    after
}

/// Preset settings without a saved Upright correction (the mode and the sliders stay).
pub fn without_correction(mut settings: Value) -> Value {
    if let Some(g) = settings.get_mut("geometry").and_then(Value::as_object_mut) {
        g.remove("upright_transform");
    }
    settings
}
