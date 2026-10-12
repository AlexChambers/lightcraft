//! HEIF, HEIC and AVIF: the metadata items of the image file format built on the ISO base media
//! file format (ISO/IEC 23008-12 on ISO/IEC 14496-12), read from the container alone. No picture
//! is decoded and no decoder is needed, so this works in builds without the codecs' optional
//! `heif` feature.
//!
//! - `ftyp` names a HEIF or AVIF brand ([`is_heif`]).
//! - The top-level `meta` box lists the items (`iinf`, one `infe` each), says where each item's
//!   bytes are (`iloc`: extents in the file, construction method 0, or in the `meta` box's own
//!   `idat`, method 1), which item is the photo (`pitm`) and how items relate (`iref`).
//! - EXIF is an item of type `Exif`: a 4-byte big-endian offset to the TIFF header, then the
//!   payload (ISO/IEC 23008-12 annex A). iPhones write offset 6 and an `Exif\0\0` prefix; libheif
//!   and avif-serialize write offset 0 and the TIFF stream.
//! - XMP is an item of type `mime` with content type `application/rdf+xml`.
//! - A `cdsc` ("content describes") reference ties a metadata item to the image it describes. The
//!   primary image's items are read, else one tied to no image (it describes the file). An item
//!   tied only to other images (a thumbnail, a depth map, another photo of a collection) is not
//!   this photo's and is never read.
//!
//! The ICC profile is left to the decoder, which reads it for rendering; nothing here needs it.
//! Every size and offset is checked against the buffer; the boxes read, the items considered and
//! the bytes one item may gather are bounded.
#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::container::Embedded;
use crate::cr3::{BoxRef, be16, be32, be64, boxes};

/// The `ftyp` brands of HEIF still images and image sequences, and of AVIF: those
/// `lightcraft_codecs::sniff` takes as HEIF or AVIF.
const BRANDS: [&[u8; 4]; 12] = [b"mif1", b"msf1", b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"avif", b"avis"];
/// Brands of other formats in the same container (Canon CR3, JPEG XL), which win as they do there.
const OTHERS: [&[u8; 4]; 2] = [b"crx ", b"jxl "];
/// How far into the `ftyp` box brands are looked for.
const FTYP_MAX: usize = 4096;
/// Boxes read in one file. An iPhone photo has about a hundred items, a large grid a few hundred.
const MAX_BOXES: usize = 1 << 16;
/// Metadata items considered (a real file has one or two).
const MAX_CANDIDATES: usize = 64;
/// The most bytes one metadata item may gather (EXIF and XMP are kilobytes).
const MAX_ITEM: usize = 16 << 20;

/// Does the `ftyp` box name a HEIF or AVIF brand (and not CR3 or JPEG XL)?
pub(crate) fn is_heif(bytes: &[u8]) -> bool {
    if bytes.get(4..8) != Some(b"ftyp".as_slice()) {
        return false;
    }
    // major brand at 8, minor version at 12, compatible brands from 16 to the end of the box
    let size = be32(bytes, 0).and_then(|s| usize::try_from(s).ok()).unwrap_or(0);
    let end = size.clamp(12, FTYP_MAX).min(bytes.len());
    let compatible = (16..end).step_by(4).filter_map(|at| bytes.get(at..at.checked_add(4).filter(|&e| e <= end)?));
    let mut heif = false;
    for brand in bytes.get(8..12).into_iter().chain(compatible) {
        if OTHERS.iter().any(|b| b.as_slice() == brand) {
            return false;
        }
        heif |= BRANDS.iter().any(|b| b.as_slice() == brand);
    }
    heif
}

/// The EXIF and XMP of a HEIF file's primary image; `None` when `bytes` is not HEIF.
pub(crate) fn embedded(bytes: &[u8]) -> Option<Embedded> {
    if !is_heif(bytes) {
        return None;
    }
    let mut budget = MAX_BOXES;
    let top = boxes(bytes, 0, bytes.len(), &mut budget);
    let Some(meta) = top.iter().find(|b| &b.kind == b"meta") else { return Some(Embedded::default()) };
    // `meta` is a full box: version and flags, then the boxes inside it
    let inside = boxes(bytes, meta.body.saturating_add(4), meta.end, &mut budget);
    let child = |kind: &[u8; 4]| inside.iter().find(|b| &b.kind == kind);
    let payload = |kind: &[u8; 4]| child(kind).and_then(|b| bytes.get(b.body..b.end));
    let primary = payload(b"pitm").and_then(primary_item);
    let mut items = child(b"iinf").map(|b| metadata_items(bytes, b, &mut budget)).unwrap_or_default();
    if let Some(iref) = child(b"iref") {
        link(bytes, iref, primary, &mut items, &mut budget);
    }
    let (iloc, idat) = (payload(b"iloc"), payload(b"idat"));
    // the primary image's items first, then those tied to no image; the first that reads well wins
    let read = |kind: Kind| {
        let mut order: Vec<(u8, u32)> = items.iter().filter(|c| c.kind == kind).filter_map(|c| Some((c.rank(primary)?, c.id))).collect();
        order.sort_by_key(|&(rank, _)| rank);
        order.into_iter().filter_map(move |(_, id)| item_bytes(bytes, iloc?, idat, id))
    };
    let exif = read(Kind::Exif).find_map(|item| exif_tiff(&item).map(<[u8]>::to_vec));
    let xmp = read(Kind::Xmp).next().map(|item| String::from_utf8_lossy(&item).into_owned());
    Some(Embedded { exif, xmp, ..Default::default() })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Exif,
    Xmp,
}

/// A metadata item and the images it describes.
#[derive(Debug)]
struct Candidate {
    id: u32,
    kind: Kind,
    /// Tied to some image by a `cdsc` reference.
    describes: bool,
    /// Tied to the primary image.
    describes_primary: bool,
}

impl Candidate {
    /// 0 for the primary image's, 1 for one tied to no image (or when there is no primary item);
    /// `None` for one that describes other images only.
    fn rank(&self, primary: Option<u32>) -> Option<u8> {
        if self.describes_primary {
            Some(0)
        } else if !self.describes || primary.is_none() {
            Some(1)
        } else {
            None
        }
    }
}

/// `pitm`: the primary item's id (16 bits in version 0, else 32).
fn primary_item(pitm: &[u8]) -> Option<u32> {
    match pitm.first()? {
        0 => be16(pitm, 4).map(u32::from),
        _ => be32(pitm, 4),
    }
}

/// The `Exif` and XMP items listed in `iinf` (the first [`MAX_CANDIDATES`]).
fn metadata_items(bytes: &[u8], iinf: &BoxRef, budget: &mut usize) -> Vec<Candidate> {
    // full box, then the entry count: 16 bits in version 0, else 32
    let Some(&version) = bytes.get(iinf.body) else { return Vec::new() };
    let first = iinf.body.saturating_add(if version == 0 { 6 } else { 8 });
    boxes(bytes, first, iinf.end, budget)
        .iter()
        .filter(|b| &b.kind == b"infe")
        .filter_map(|b| item_info(bytes.get(b.body..b.end)?))
        .take(MAX_CANDIDATES)
        .collect()
}

/// An `infe` entry, when it is an unprotected `Exif` item or an uncompressed XMP item (`mime`,
/// `application/rdf+xml`).
fn item_info(infe: &[u8]) -> Option<Candidate> {
    let version = *infe.first()?;
    // version and flags, the item id (32 bits in version 3), the protection index
    let (id, at) = match version {
        0..=2 => (u32::from(be16(infe, 4)?), 6usize),
        3 => (be32(infe, 4)?, 8usize),
        _ => return None,
    };
    if be16(infe, at)? != 0 {
        return None; // protected (encrypted)
    }
    let at = at.checked_add(2)?;
    // versions 0 and 1 have no item type and describe MIME items; 2 and 3 name the type
    let (kind, at) = if version >= 2 { (infe.get(at..at.checked_add(4)?)?, at.checked_add(4)?) } else { (b"mime".as_slice(), at) };
    let kind = match kind {
        b"Exif" => Kind::Exif,
        b"mime" => {
            let (_name, at) = string(infe, at);
            let (content_type, at) = string(infe, at);
            let (encoding, _) = string(infe, at);
            if !content_type.trim_ascii().eq_ignore_ascii_case(b"application/rdf+xml") || !encoding.trim_ascii().is_empty() {
                return None;
            }
            Kind::Xmp
        }
        _ => return None,
    };
    Some(Candidate { id, kind, describes: false, describes_primary: false })
}

/// The NUL-terminated string at `at` and where the next field starts. A missing terminator
/// ends the string at the end of the box.
fn string(b: &[u8], at: usize) -> (&[u8], usize) {
    let rest = b.get(at..).unwrap_or_default();
    match rest.iter().position(|&c| c == 0) {
        Some(n) => (rest.get(..n).unwrap_or_default(), at.saturating_add(n).saturating_add(1)),
        None => (rest, b.len()),
    }
}

/// Mark the candidates that `cdsc` references tie to an image, and those tied to the primary one.
fn link(bytes: &[u8], iref: &BoxRef, primary: Option<u32>, items: &mut [Candidate], budget: &mut usize) {
    // full box; item ids are 16 bits in version 0, 32 in version 1
    let width = match bytes.get(iref.body) {
        Some(0) => 2usize,
        Some(1) => 4,
        _ => return,
    };
    for r in boxes(bytes, iref.body.saturating_add(4), iref.end, budget) {
        if &r.kind != b"cdsc" {
            continue;
        }
        let Some(body) = bytes.get(r.body..r.end) else { continue };
        let Some(from) = id_at(body, 0, width) else { continue };
        let Some(item) = items.iter_mut().find(|c| c.id == from) else { continue };
        let count = be16(body, width).unwrap_or(0);
        let mut at = width.saturating_add(2);
        for _ in 0..count {
            let Some(to) = id_at(body, at, width) else { break };
            item.describes = true;
            item.describes_primary |= Some(to) == primary;
            at = at.saturating_add(width);
        }
    }
}

fn id_at(b: &[u8], at: usize, width: usize) -> Option<u32> {
    if width == 2 { be16(b, at).map(u32::from) } else { be32(b, at) }
}

/// Item `id`'s bytes as `iloc` places them: its extents in order, from the file (construction
/// method 0) or from the `meta` box's `idat` (method 1). `None` when the item is not listed,
/// lies outside the data, would gather more than [`MAX_ITEM`], is in another file or is built
/// from other items (method 2).
fn item_bytes(file: &[u8], iloc: &[u8], idat: Option<&[u8]>, id: u32) -> Option<Vec<u8>> {
    let version = *iloc.first()?;
    if version > 2 {
        return None;
    }
    // version and flags, then four 4-bit field sizes: offset, length, base offset, extent index
    let (sizes, more) = (*iloc.get(4)?, *iloc.get(5)?);
    let (offset_size, length_size) = (usize::from(sizes / 16), usize::from(sizes % 16));
    let base_size = usize::from(more / 16);
    let index_size = if version == 0 { 0 } else { usize::from(more % 16) }; // reserved in version 0
    if [offset_size, length_size, base_size, index_size].into_iter().any(|n| !matches!(n, 0 | 4 | 8)) {
        return None;
    }
    let id_size = if version == 2 { 4 } else { 2 };
    let (count, mut at) = if version == 2 { (be32(iloc, 6)?, 10usize) } else { (u32::from(be16(iloc, 6)?), 8usize) };
    let extent_size = index_size.checked_add(offset_size)?.checked_add(length_size)?;
    // (every entry takes at least six bytes, so a count larger than the box ends at its end)
    for _ in 0..count {
        let item = uint(iloc, at, id_size)?;
        at = at.checked_add(id_size)?;
        let method = if version == 0 {
            0
        } else {
            let m = be16(iloc, at)? & 15;
            at = at.checked_add(2)?;
            m
        };
        let data_reference = be16(iloc, at)?;
        let base = uint(iloc, at.checked_add(2)?, base_size)?;
        at = at.checked_add(2)?.checked_add(base_size)?;
        let extents = usize::from(be16(iloc, at)?);
        at = at.checked_add(2)?;
        if item != u64::from(id) {
            at = at.checked_add(extents.checked_mul(extent_size)?)?;
            continue;
        }
        // data reference 0 is this file
        let source = match (data_reference, method) {
            (0, 0) => file,
            (0, 1) => idat?,
            _ => return None,
        };
        let mut out = Vec::new();
        for _ in 0..extents {
            at = at.checked_add(index_size)?;
            let offset = uint(iloc, at, offset_size)?;
            at = at.checked_add(offset_size)?;
            let length = uint(iloc, at, length_size)?;
            at = at.checked_add(length_size)?;
            let rest = source.get(usize::try_from(base.checked_add(offset)?).ok()?..)?;
            // a length of 0 runs to the end of the data
            let piece = if length == 0 { rest } else { rest.get(..usize::try_from(length).ok()?)? };
            if out.len().checked_add(piece.len())? > MAX_ITEM {
                return None;
            }
            out.extend_from_slice(piece);
        }
        return Some(out);
    }
    None
}

/// A big-endian unsigned field of `size` bytes (0 reads as 0).
fn uint(b: &[u8], at: usize, size: usize) -> Option<u64> {
    match size {
        0 => Some(0),
        2 => be16(b, at).map(u64::from),
        4 => be32(b, at).map(u64::from),
        8 => be64(b, at),
        _ => None,
    }
}

/// The TIFF stream of an `Exif` item: past its 4-byte offset field and the bytes that offset
/// skips. An item whose offset is wrong, or that has no offset field, is read too when an
/// `Exif\0\0` prefix or the TIFF header itself shows where the stream starts.
fn exif_tiff(item: &[u8]) -> Option<&[u8]> {
    let tiff = |b: &&[u8]| b.starts_with(b"II*\0") || b.starts_with(b"MM\0*");
    let stated = be32(item, 0).and_then(|n| usize::try_from(n).ok()?.checked_add(4)).and_then(|at| item.get(at..));
    let after_field = item.get(4..).map(crate::strip_exif_header);
    [stated, after_field, Some(crate::strip_exif_header(item))].into_iter().flatten().find(tiff)
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    /// The 100-byte EXIF of the issue sweep's `synthetic-metadata.heic` (#693): Make "Synthetic",
    /// Model "Issue sweep camera", DateTime 2026:10:12 12:34:56, big-endian.
    const SWEEP_TIFF: [u8; 100] = [
        0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08, 0x00, 0x03, 0x01, 0x0f, 0x00, 0x02, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, //
        0x00, 0x32, 0x01, 0x10, 0x00, 0x02, 0x00, 0x00, 0x00, 0x13, 0x00, 0x00, 0x00, 0x3c, 0x01, 0x32, 0x00, 0x02, 0x00, 0x00, //
        0x00, 0x14, 0x00, 0x00, 0x00, 0x50, 0x00, 0x00, 0x00, 0x00, 0x53, 0x79, 0x6e, 0x74, 0x68, 0x65, 0x74, 0x69, 0x63, 0x00, //
        0x49, 0x73, 0x73, 0x75, 0x65, 0x20, 0x73, 0x77, 0x65, 0x65, 0x70, 0x20, 0x63, 0x61, 0x6d, 0x65, 0x72, 0x61, 0x00, 0x00, //
        0x32, 0x30, 0x32, 0x36, 0x3a, 0x31, 0x30, 0x3a, 0x31, 0x32, 0x20, 0x31, 0x32, 0x3a, 0x33, 0x34, 0x3a, 0x35, 0x36, 0x00,
    ];

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn full(kind: &[u8; 4], version: u8, payload: &[u8]) -> Vec<u8> {
        bx(kind, &[&[version, 0, 0, 0][..], payload].concat())
    }

    fn put(v: &mut Vec<u8>, x: u64, size: u8) {
        match size {
            0 => assert_eq!(x, 0, "a value needs a field"),
            2 => v.extend_from_slice(&(x as u16).to_be_bytes()),
            4 => v.extend_from_slice(&(x as u32).to_be_bytes()),
            8 => v.extend_from_slice(&x.to_be_bytes()),
            _ => panic!("no {size}-byte fields"),
        }
    }

    /// An EXIF item as HEIF stores it: the 4-byte offset, `skip` (which the offset jumps), the TIFF.
    fn exif_item(skip: &[u8], tiff: &[u8]) -> Vec<u8> {
        [&(skip.len() as u32).to_be_bytes()[..], skip, tiff].concat()
    }

    fn tiff_with(model: &str) -> Vec<u8> {
        crate::write_exif(&crate::Metadata { model: Some(model.into()), ..Default::default() })
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Store {
        File,
        Idat,
    }

    struct Item {
        id: u32,
        kind: &'static [u8; 4],
        /// `mime` items: content type and content encoding.
        mime: (&'static str, &'static str),
        data: Vec<u8>,
        store: Store,
        /// Extents the data is split into.
        extents: usize,
    }

    /// A HEIF file for the tests: `ftyp`, `meta` (`hdlr`, `pitm`, `iinf`, `iref`, `iloc`, `idat`)
    /// and `mdat`.
    struct Heif {
        ftyp: Vec<u8>,
        primary: u32,
        items: Vec<Item>,
        cdsc: Vec<(u32, Vec<u32>)>,
        /// `iloc` version and its offset, length, base-offset and index field sizes.
        iloc: (u8, [u8; 4]),
        /// 32-bit item ids: `infe` version 3, `pitm` and `iref` version 1 (`iloc` must be version 2).
        wide: bool,
    }

    impl Heif {
        /// A primary HEVC picture, item 1 (its bytes don't matter here).
        fn new() -> Heif {
            let picture = Item { id: 1, kind: b"hvc1", mime: ("", ""), data: b"coded picture".to_vec(), store: Store::File, extents: 1 };
            Heif { ftyp: b"heic\0\0\0\0mif1heic".to_vec(), primary: 1, items: vec![picture], cdsc: Vec::new(), iloc: (1, [4, 4, 0, 0]), wide: false }
        }

        fn item(mut self, id: u32, kind: &'static [u8; 4], data: Vec<u8>, store: Store, extents: usize) -> Heif {
            self.items.push(Item { id, kind, mime: ("", ""), data, store, extents });
            self
        }

        fn xmp(mut self, id: u32, mime: (&'static str, &'static str), data: Vec<u8>) -> Heif {
            self.items.push(Item { id, kind: b"mime", mime, data, store: Store::File, extents: 1 });
            self
        }

        fn cdsc(mut self, from: u32, to: &[u32]) -> Heif {
            self.cdsc.push((from, to.to_vec()));
            self
        }

        fn meta(&self, mdat_at: usize) -> Vec<u8> {
            let id = |v: &mut Vec<u8>, id: u32| put(v, u64::from(id), if self.wide { 4 } else { 2 });
            let mut hdlr = vec![0u8; 4];
            hdlr.extend_from_slice(b"pict");
            hdlr.extend_from_slice(&[0; 13]);
            let mut pitm = Vec::new();
            id(&mut pitm, self.primary);
            let mut iinf = Vec::new();
            put(&mut iinf, self.items.len() as u64, if self.wide { 4 } else { 2 });
            for it in &self.items {
                let mut e = Vec::new();
                id(&mut e, it.id);
                e.extend_from_slice(&[0, 0]); // not protected
                e.extend_from_slice(it.kind);
                e.push(0); // no name
                if it.kind == b"mime" {
                    e.extend_from_slice(it.mime.0.as_bytes());
                    e.push(0);
                    e.extend_from_slice(it.mime.1.as_bytes());
                    e.push(0);
                }
                iinf.extend(full(b"infe", if self.wide { 3 } else { 2 }, &e));
            }
            let mut iref = Vec::new();
            for (from, to) in &self.cdsc {
                let mut r = Vec::new();
                id(&mut r, *from);
                put(&mut r, to.len() as u64, 2);
                to.iter().for_each(|t| id(&mut r, *t));
                iref.extend(bx(b"cdsc", &r));
            }
            let (version, [o, l, b, i]) = self.iloc;
            let mut iloc = vec![o << 4 | l, b << 4 | i];
            put(&mut iloc, self.items.len() as u64, if version == 2 { 4 } else { 2 });
            let (mut in_file, mut in_idat) = (mdat_at as u64, 0u64);
            let mut idat = Vec::new();
            for it in &self.items {
                put(&mut iloc, u64::from(it.id), if version == 2 { 4 } else { 2 });
                let at = match it.store {
                    Store::File => &mut in_file,
                    Store::Idat => &mut in_idat,
                };
                if version > 0 {
                    put(&mut iloc, if it.store == Store::Idat { 1 } else { 0 }, 2);
                }
                put(&mut iloc, 0, 2); // this file
                let base = if b > 0 { *at } else { 0 };
                put(&mut iloc, base, b);
                let chunk = it.data.len().div_ceil(it.extents.max(1)).max(1);
                let pieces: Vec<&[u8]> = it.data.chunks(chunk).collect();
                put(&mut iloc, pieces.len() as u64, 2);
                for piece in pieces {
                    if version > 0 {
                        put(&mut iloc, 0, i);
                    }
                    put(&mut iloc, *at - base, o);
                    put(&mut iloc, piece.len() as u64, l);
                    *at += piece.len() as u64;
                }
                if it.store == Store::Idat {
                    idat.extend_from_slice(&it.data);
                }
            }
            let ref_version = if self.wide { 1 } else { 0 };
            let boxes = [
                full(b"hdlr", 0, &hdlr),
                full(b"pitm", ref_version, &pitm),
                full(b"iinf", ref_version, &iinf),
                full(b"iref", ref_version, &iref),
                full(b"iloc", version, &iloc),
                bx(b"idat", &idat),
            ];
            full(b"meta", 0, &boxes.concat())
        }

        fn build(&self) -> Vec<u8> {
            let ftyp = bx(b"ftyp", &self.ftyp);
            let mdat_at = ftyp.len() + self.meta(0).len() + 8;
            let mdat: Vec<u8> = self.items.iter().filter(|i| i.store == Store::File).flat_map(|i| i.data.iter().copied()).collect();
            [ftyp, self.meta(mdat_at), bx(b"mdat", &mdat)].concat()
        }
    }

    /// The issue sweep's case (#693), laid out as the repo's HEIF test builder writes it: `iloc`
    /// version 1, the EXIF item (offset 0, then the TIFF) tied to the primary image by `cdsc`.
    #[test]
    fn the_issue_sweep_heic_gives_its_camera_and_capture_time() {
        let f = Heif::new().item(8, b"Exif", exif_item(&[], &SWEEP_TIFF), Store::File, 1).cdsc(8, &[1]).build();
        assert_eq!(crate::embedded(&f).exif.as_deref(), Some(SWEEP_TIFF.as_slice()));
        let m = crate::extract(&f);
        assert_eq!((m.make.as_deref(), m.model.as_deref()), (Some("Synthetic"), Some("Issue sweep camera")));
        assert_eq!(m.capture_time.map(|d| d.to_iso()).as_deref(), Some("2026-10-12T12:34:56"));
        // All Metadata's rows come from the same block
        let rows = crate::file_tag_rows(&f);
        assert!(rows.iter().any(|r| r.name == "Model" && r.value == "Issue sweep camera"), "{rows:?}");
    }

    /// GPS comes with the EXIF, so a HEIC's location is seen (export's "remove location" refuses to
    /// copy such an original out instead of letting the location through unnoticed).
    #[test]
    fn a_heic_location_is_seen() {
        let gps = crate::Gps { latitude: 38.7, longitude: -9.1, altitude: None };
        let tiff = crate::write_exif(&crate::Metadata { gps: Some(gps), ..Default::default() });
        let f = Heif::new().item(2, b"Exif", exif_item(b"Exif\0\0", &tiff), Store::File, 1).cdsc(2, &[1]).build();
        let back = crate::extract(&f).gps.expect("the location");
        assert!((back.latitude - 38.7).abs() < 1e-6 && (back.longitude + 9.1).abs() < 1e-6, "{back:?}");
    }

    /// The 4-byte offset in front of the TIFF stream: iPhones write 6 and `Exif\0\0`, libheif and
    /// avif-serialize 0. Writers that left the field out, or got it wrong, are read when the stream
    /// is plain to see; anything else is no EXIF.
    #[test]
    fn exif_items_with_and_without_their_offset() {
        let t = &SWEEP_TIFF[..];
        let tiff = Some(t);
        assert_eq!(exif_tiff(&exif_item(b"Exif\0\0", t)), tiff, "iPhone");
        assert_eq!(exif_tiff(&exif_item(&[], t)), tiff, "libheif, avif-serialize");
        assert_eq!(exif_tiff(&exif_item(b"\0\0padding", t)), tiff, "any bytes the offset skips");
        assert_eq!(exif_tiff(&[b"\0\0\0\0Exif\0\0".as_slice(), t].concat()), tiff, "offset 0 though a prefix follows");
        assert_eq!(exif_tiff(&[&1000u32.to_be_bytes()[..], t].concat()), tiff, "offset past the end");
        assert_eq!(exif_tiff(&[b"Exif\0\0".as_slice(), t].concat()), tiff, "no offset field, JPEG style");
        assert_eq!(exif_tiff(t), tiff, "no offset field at all");
        assert_eq!(exif_tiff(&exif_item(&[], b"not a TIFF stream")), None);
        assert_eq!(exif_tiff(&[0, 0]), None);
        assert_eq!(exif_tiff(&[]), None);
        // and through a whole file, the iPhone way
        let f = Heif::new().item(2, b"Exif", exif_item(b"Exif\0\0", t), Store::File, 1).cdsc(2, &[1]).build();
        assert_eq!(crate::extract(&f).model.as_deref(), Some("Issue sweep camera"));
    }

    /// Items in several extents, in the file or the `meta` box's `idat`, with every `iloc` version
    /// and field size.
    #[test]
    fn item_extents_and_construction_methods() {
        let xmp = crate::write_xmp(&crate::Metadata { title: Some("Harbour at dusk".into()), rating: Some(3), ..Default::default() }, None);
        let rdf = ("application/rdf+xml", "");
        // EXIF in `idat` (method 1), XMP in the file in three extents, a 32-bit base offset
        let mut h = Heif::new().item(2, b"Exif", exif_item(&[], &SWEEP_TIFF), Store::Idat, 1).cdsc(2, &[1]).xmp(3, rdf, xmp.clone().into_bytes());
        h.items[2].extents = 3;
        h.iloc = (1, [8, 4, 4, 0]);
        let f = h.cdsc(3, &[1]).build();
        let e = crate::embedded(&f);
        assert_eq!(e.exif.as_deref(), Some(SWEEP_TIFF.as_slice()));
        assert_eq!(e.xmp.as_deref(), Some(xmp.as_str()));
        let m = crate::extract(&f);
        assert_eq!((m.model.as_deref(), m.title.as_deref(), m.rating), (Some("Issue sweep camera"), Some("Harbour at dusk"), Some(3)));
        // version 2: 32-bit item ids, an extent index, 64-bit lengths; EXIF in two extents
        let mut h = Heif::new().item(70_000, b"Exif", exif_item(b"Exif\0\0", &SWEEP_TIFF), Store::File, 2).cdsc(70_000, &[70_001]);
        (h.primary, h.items[0].id, h.wide, h.iloc) = (70_001, 70_001, true, (2, [4, 8, 0, 4]));
        assert_eq!(crate::embedded(&h.build()).exif.as_deref(), Some(SWEEP_TIFF.as_slice()));
        // version 0: no construction method, file offsets only
        let mut h = Heif::new().item(2, b"Exif", exif_item(&[], &SWEEP_TIFF), Store::File, 1).cdsc(2, &[1]);
        h.iloc = (0, [4, 4, 0, 0]);
        assert_eq!(crate::embedded(&h.build()).exif.as_deref(), Some(SWEEP_TIFF.as_slice()));
        // an XMP item in `idat`
        let mut h = Heif::new().xmp(5, rdf, xmp.clone().into_bytes());
        h.items[1].store = Store::Idat;
        assert_eq!(crate::embedded(&h.build()).xmp.as_deref(), Some(xmp.as_str()));
    }

    /// `iloc` entries the builder doesn't write: a length of 0 (to the end of the data), an item
    /// in another file, an item built from other items, unknown versions and field sizes.
    #[test]
    fn iloc_entries_that_are_read_or_refused() {
        let file = b"0123456789".as_slice();
        let idat = b"abcdefgh".as_slice();
        // version 1, offset and length 4 bytes, no base or index; one item with one extent
        let iloc = |version: u8, sizes: [u8; 2], method: u16, data_ref: u16, offset: u32, length: u32| {
            let mut l = vec![version, 0, 0, 0];
            l.extend_from_slice(&sizes);
            l.extend_from_slice(&1u16.to_be_bytes());
            l.extend_from_slice(&7u16.to_be_bytes());
            if version > 0 {
                l.extend_from_slice(&method.to_be_bytes());
            }
            l.extend_from_slice(&data_ref.to_be_bytes());
            l.extend_from_slice(&1u16.to_be_bytes());
            l.extend_from_slice(&offset.to_be_bytes());
            l.extend_from_slice(&length.to_be_bytes());
            l
        };
        let read = |l: &[u8]| item_bytes(file, l, Some(idat), 7);
        assert_eq!(read(&iloc(1, [0x44, 0], 0, 0, 2, 3)).as_deref(), Some(b"234".as_slice()));
        assert_eq!(read(&iloc(1, [0x44, 0], 1, 0, 2, 3)).as_deref(), Some(b"cde".as_slice()), "from idat");
        assert_eq!(read(&iloc(1, [0x44, 0], 0, 0, 6, 0)).as_deref(), Some(b"6789".as_slice()), "length 0: to the end of the file");
        assert_eq!(read(&iloc(1, [0x44, 0], 1, 0, 5, 0)).as_deref(), Some(b"fgh".as_slice()), "length 0: to the end of idat");
        assert_eq!(item_bytes(file, &iloc(1, [0x44, 0], 1, 0, 0, 1), None, 7), None, "method 1 without idat");
        assert_eq!(read(&iloc(1, [0x44, 0], 2, 0, 0, 1)), None, "method 2 (from other items)");
        assert_eq!(read(&iloc(1, [0x44, 0], 0, 1, 0, 1)), None, "in another file");
        assert_eq!(read(&iloc(1, [0x44, 0], 0, 0, 8, 3)), None, "past the end");
        assert_eq!(read(&iloc(1, [0x44, 0], 0, 0, 11, 0)), None, "starts past the end");
        assert_eq!(read(&iloc(1, [0x22, 0], 0, 0, 0, 1)), None, "2-byte fields are not in the standard");
        assert_eq!(read(&iloc(3, [0x44, 0], 0, 0, 0, 1)), None, "unknown version");
        assert_eq!(item_bytes(file, &iloc(1, [0x44, 0], 0, 0, 0, 1), None, 8), None, "no such item");
        // a base offset near u64::MAX plus an extent offset overflows: refused, not wrapped
        let mut l = vec![1, 0, 0, 0, 0x44, 0x80];
        l.extend_from_slice(&1u16.to_be_bytes());
        l.extend_from_slice(&7u16.to_be_bytes());
        l.extend_from_slice(&[0, 0, 0, 0]); // method 0, data reference 0
        l.extend_from_slice(&u64::MAX.to_be_bytes());
        l.extend_from_slice(&1u16.to_be_bytes());
        l.extend_from_slice(&1u32.to_be_bytes());
        l.extend_from_slice(&1u32.to_be_bytes());
        assert_eq!(read(&l), None);
    }

    /// The primary image's metadata is read; metadata tied only to another image is not.
    #[test]
    fn the_primary_images_metadata_is_read() {
        let thumb = |h: Heif| h.item(2, b"hvc1", b"thumbnail".to_vec(), Store::File, 1);
        // a thumbnail's EXIF comes first in the file; the primary image's is the one read
        let f = thumb(Heif::new())
            .item(10, b"Exif", exif_item(&[], &tiff_with("Thumbnail")), Store::File, 1)
            .item(11, b"Exif", exif_item(&[], &tiff_with("Primary")), Store::File, 1)
            .cdsc(10, &[2])
            .cdsc(11, &[1])
            .build();
        assert_eq!(crate::extract(&f).model.as_deref(), Some("Primary"));
        // EXIF tied only to another image is not this photo's
        let f = thumb(Heif::new()).item(10, b"Exif", exif_item(&[], &tiff_with("Thumbnail")), Store::File, 1).cdsc(10, &[2]).build();
        assert_eq!(crate::embedded(&f).exif, None);
        // tied to no image: it describes the file; tied to both: read
        let f = Heif::new().item(10, b"Exif", exif_item(&[], &tiff_with("Whole file")), Store::File, 1).build();
        assert_eq!(crate::extract(&f).model.as_deref(), Some("Whole file"));
        let f = thumb(Heif::new()).item(10, b"Exif", exif_item(&[], &tiff_with("Both")), Store::File, 1).cdsc(10, &[2, 1]).build();
        assert_eq!(crate::extract(&f).model.as_deref(), Some("Both"));
        // a primary image's EXIF that can't be read gives way to one tied to no image
        let f = Heif::new()
            .item(10, b"Exif", exif_item(&[], b"damaged"), Store::File, 1)
            .item(11, b"Exif", exif_item(&[], &tiff_with("Fallback")), Store::File, 1)
            .cdsc(10, &[1])
            .build();
        assert_eq!(crate::extract(&f).model.as_deref(), Some("Fallback"));
        // XMP: only uncompressed `application/rdf+xml` items, the primary image's first
        let xmp = |title: &str| crate::write_xmp(&crate::Metadata { title: Some(title.into()), ..Default::default() }, None).into_bytes();
        let f = thumb(Heif::new())
            .xmp(20, ("image/jpeg", ""), b"\xff\xd8 not XMP".to_vec())
            .xmp(21, ("application/rdf+xml", "deflate"), b"compressed".to_vec())
            .xmp(22, ("application/rdf+xml", ""), xmp("Thumbnail"))
            .xmp(23, (" Application/RDF+XML ", ""), xmp("Primary"))
            .cdsc(22, &[2])
            .cdsc(23, &[1])
            .build();
        assert_eq!(crate::extract(&f).title.as_deref(), Some("Primary"));
        // 32-bit ids in `pitm`, `iinf` and `iref`
        let mut h = Heif::new().item(70_000, b"Exif", exif_item(&[], &tiff_with("Wide")), Store::File, 1).cdsc(70_000, &[70_001]);
        (h.primary, h.items[0].id, h.wide, h.iloc.0) = (70_001, 70_001, true, 2);
        assert_eq!(crate::extract(&h.build()).model.as_deref(), Some("Wide"));
    }

    /// HEIF, HEIC and AVIF brands are read; other files built of the same boxes are not HEIF.
    #[test]
    fn heif_and_avif_brands() {
        let with = |ftyp: &[u8]| {
            let mut h = Heif::new().item(2, b"Exif", exif_item(&[], &SWEEP_TIFF), Store::File, 1).cdsc(2, &[1]);
            h.ftyp = ftyp.to_vec();
            h.build()
        };
        for ftyp in [&b"heic\0\0\0\0mif1heic"[..], b"mif1\0\0\0\0mif1", b"heix\0\0\0\0mif1heix", b"avif\0\0\0\0avifmif1miaf", b"msf1\0\0\0\0msf1hevc"]
        {
            assert!(is_heif(&with(ftyp)), "{}", String::from_utf8_lossy(ftyp));
            assert_eq!(crate::extract(&with(ftyp)).model.as_deref(), Some("Issue sweep camera"), "{}", String::from_utf8_lossy(ftyp));
        }
        // MP4 and QuickTime video, JPEG XL, Canon CR3
        for ftyp in [&b"isom\0\0\x02\0isomiso2mp41"[..], b"qt  \0\0\0\0qt  ", b"jxl \0\0\0\0jxl ", b"crx \0\0\0\x01crx isom", b"mif1\0\0\0\0crx "] {
            assert!(!is_heif(&with(ftyp)), "{}", String::from_utf8_lossy(ftyp));
            assert_eq!(crate::extract(&with(ftyp)).model, None);
        }
        assert!(!is_heif(b"\0\0\0\x0cftyp"), "no brand");
        assert!(!is_heif(&[0xff, 0xd8, 0xff, 0xe0, 0, 0x10, b'J', b'F', b'I', b'F', 0, 1]), "JPEG");
        // a HEIF file without a `meta` box has no metadata
        assert_eq!(crate::embedded(&bx(b"ftyp", b"heic\0\0\0\0mif1heic")), Embedded::default());
    }

    /// Damaged structure: an out-of-range box, an `iinf` or `iref` that claims more than it holds,
    /// extents that overlap to gather more than [`MAX_ITEM`], an item count far past the box.
    #[test]
    fn malformed_bounds_are_refused() {
        let good = Heif::new().item(2, b"Exif", exif_item(&[], &SWEEP_TIFF), Store::File, 1).cdsc(2, &[1]).build();
        // the `meta` box says it is longer than the file: nothing is read
        let mut f = good.clone();
        let at = u32::from_be_bytes(f[0..4].try_into().unwrap()) as usize; // `meta` follows `ftyp`
        assert_eq!(&f[at + 4..at + 8], b"meta");
        f[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(crate::embedded(&f), Embedded::default());
        // an extent past the end of the file
        let mut f = good.clone();
        f.truncate(f.len() - 1);
        assert_eq!(crate::embedded(&f).exif, None);
        // 65,535 extents over the same 64 KiB: refused at 16 MiB, quickly
        let file = vec![0u8; 64 << 10];
        let mut l = vec![1, 0, 0, 0, 0x44, 0];
        l.extend_from_slice(&1u16.to_be_bytes());
        l.extend_from_slice(&7u16.to_be_bytes());
        l.extend_from_slice(&[0, 0, 0, 0]);
        l.extend_from_slice(&u16::MAX.to_be_bytes());
        for _ in 0..u16::MAX {
            l.extend_from_slice(&0u32.to_be_bytes());
            l.extend_from_slice(&(file.len() as u32).to_be_bytes());
        }
        assert_eq!(item_bytes(&file, &l, None, 7), None);
        // an item count of 2^32 - 1 in a box holding one entry
        let mut l = vec![2, 0, 0, 0, 0x44, 0];
        l.extend_from_slice(&u32::MAX.to_be_bytes());
        l.extend_from_slice(&1u32.to_be_bytes());
        l.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        assert_eq!(item_bytes(&file, &l, None, 7), None);
        // a `cdsc` claiming 65,535 targets with none there, an `infe` cut short
        let mut iref = vec![0, 0, 0, 0];
        iref.extend(bx(b"cdsc", &[0, 2, 0xff, 0xff]));
        let iref = bx(b"iref", &iref);
        let mut items = vec![Candidate { id: 2, kind: Kind::Exif, describes: false, describes_primary: false }];
        let mut budget = MAX_BOXES;
        let b = boxes(&iref, 0, iref.len(), &mut budget);
        link(&iref, &b[0], Some(1), &mut items, &mut budget);
        assert!(!items[0].describes);
        assert!(item_info(&[2, 0, 0, 0, 0, 2, 0, 0, b'E', b'x']).is_none());
        assert!(item_info(&[4, 0, 0, 0, 0, 2, 0, 0, b'E', b'x', b'i', b'f', 0]).is_none(), "unknown infe version");
        assert!(item_info(&[2, 0, 0, 0, 0, 2, 0, 1, b'E', b'x', b'i', b'f', 0]).is_none(), "protected");
    }

    /// Every cut-short or damaged file is read without a panic.
    #[test]
    fn truncated_and_damaged_files_never_panic() {
        let xmp = crate::write_xmp(&crate::Metadata { title: Some("t".into()), ..Default::default() }, None).into_bytes();
        let mut h = Heif::new()
            .item(2, b"Exif", exif_item(b"Exif\0\0", &SWEEP_TIFF), Store::Idat, 2)
            .xmp(3, ("application/rdf+xml", ""), xmp)
            .cdsc(2, &[1])
            .cdsc(3, &[1]);
        h.items[2].extents = 3;
        h.iloc = (1, [4, 8, 4, 4]);
        let f = h.build();
        assert!(crate::embedded(&f).exif.is_some() && crate::embedded(&f).xmp.is_some());
        for n in 0..f.len() {
            let _ = crate::extract(&f[..n]);
        }
        for i in 0..f.len() {
            for v in [0x00, 0xff, 0x7f, 0x01] {
                let mut g = f.clone();
                g[i] = v;
                let _ = crate::extract(&g);
                let _ = crate::file_tag_rows(&g);
            }
        }
    }

    proptest::proptest! {
        /// Arbitrary bytes behind a HEIF `ftyp` never panic.
        #[test]
        fn arbitrary_boxes_never_panic(body in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
            let f = [bx(b"ftyp", b"heic\0\0\0\0mif1heic"), body.clone()].concat();
            let _ = crate::embedded(&f);
            let meta = [bx(b"ftyp", b"heic\0\0\0\0mif1heic"), full(b"meta", 0, &body)].concat();
            let _ = crate::extract(&meta);
        }
    }
}
