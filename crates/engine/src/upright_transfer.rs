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
#[derive(Clone, Debug)]
pub struct UprightSource {
    source: Source,
    master: PhotoId,
    /// Its Rotate Left / Right when the settings were taken.
    user: Orientation,
    /// The library it was in (photo ids start again in each library).
    library: std::sync::Weak<()>,
}

impl UprightSource {
    /// Whether it was taken in the library `identity` is the identity of
    /// (`Session::library_identity`). Photo ids belong to one library, so a source from another
    /// one can't be matched to a photo by id.
    pub fn taken_in(&self, identity: &std::sync::Arc<()>) -> bool {
        std::sync::Weak::ptr_eq(&self.library, &std::sync::Arc::downgrade(identity))
    }
}

/// Photo `p` with settings `s`, in the library whose identity is `library`, as the source of a
/// transfer.
pub fn source_of(p: &Photo, s: &DevelopSettings, library: &std::sync::Arc<()>) -> UprightSource {
    UprightSource { source: p.source.clone(), master: p.copy_of.unwrap_or(p.id), user: s.orientation, library: std::sync::Arc::downgrade(library) }
}

/// Whether photo `p` (in the library `from` was taken in) shows the same file as `from`: the photo
/// itself, its original or another of its virtual copies, or another entry of the file.
fn same_file(from: &UprightSource, p: &Photo) -> bool {
    from.master == p.copy_of.unwrap_or(p.id) || from.source == p.source
}

/// The turn that takes a photo shown with `from` to `to` (both after the file's own orientation).
fn turn(from: Orientation, to: Orientation) -> Orientation {
    from.inverse().then(to)
}

/// Settings `after` of photo `p` (which had `before`) once `patch` was applied, with its Upright
/// correction settled as the module describes. `source`: the photo the patch was taken from, in
/// this photo's library (`None`: none, as for a preset).
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

/// Whether settings JSON `patch` states a correction other than the one `before` holds: settings
/// sent back whole (as `develop.get` returned them) state the photo's own, unchanged, and that one
/// is turned with the photo like any kept correction.
pub fn states_new_correction(patch: &Value, before: &DevelopSettings) -> bool {
    patch
        .pointer("/geometry/upright_transform")
        .is_some_and(|v| serde_json::from_value::<Option<lightcraft_geom::Homography>>(v.clone()).ok() != Some(before.geometry.upright_transform))
}

/// `patch` applied to settings `before` of photo `p`, taken from `source` ([`settle`]).
pub fn transferred(p: &Photo, before: &DevelopSettings, patch: &Value, source: Option<&UprightSource>) -> DevelopSettings {
    let mut after = lightcraft_develop::apply_partial(before, patch, 1.0);
    settle(p, before, &mut after, patch, source);
    after
}

/// `preset` at `amount` applied to settings `before` of photo `p`, as Apply Preset stores it: a
/// preset carries no Upright correction ([`settle`] with no source photo). Previews use it too.
pub fn apply_preset(p: &Photo, before: &DevelopSettings, preset: &lightcraft_develop::Preset, amount: f64) -> DevelopSettings {
    let mut after = preset.apply(before, amount);
    settle(p, before, &mut after, &preset.settings, None);
    after
}

/// Preset settings without a saved Upright correction (the mode and the sliders stay).
pub fn without_correction(mut settings: Value) -> Value {
    if let Some(g) = settings.get_mut("geometry").and_then(Value::as_object_mut) {
        g.remove("upright_transform");
    }
    settings
}
