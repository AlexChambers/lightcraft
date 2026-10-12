# Calibrated Contrast and Blacks

> **Last updated:** 2026-10-13 · **Change:** minor (composed CPU preview tables and aggregate publication threshold)

Process V3 calibrates global Contrast and Blacks on the camera-look path: a raw with
`SourceInfo::camera_tone`. Contrast acts per channel. Blacks blends a per-channel result with
luminance scaling. Whites in V3 behaves exactly as V2, for positive and negative amounts.
Whites calibration is a planned follow-up.

This is the Contrast and Blacks split of the original V3 calibration work. The serialized
number is defined by the `Process::V3` arm in `Process::version()`; callers use the enum.
Open PR #944 also proposes V3 and V4, #1283 proposes V3 for Dehaze, and #1286 follows #1283.
Maintainers must assign the final process numbers and inheritance in merge order. V3 here
inherits V2 only.

Raw files without a camera curve, rendered sources such as JPEG and TIFF, and local mask
adjustments keep their V2 operators. This process does not change decoding, source metadata,
smart-preview formats, library opening, or Update and Reset checks.

## Render order

The camera tone curve uses V2 Whites with Contrast and Blacks set to zero. The renderer
maps luminance through that curve, applies its chroma scaling and highlight desaturation,
then applies calibrated Contrast followed by calibrated Blacks, before the colour stage.
When both calibrated sliders are zero, no new operator or colour conversion runs. With any
Whites value and both calibrated sliders at zero, the result is byte-identical to V2.
Combined edits use V2 Whites before the two calibrated operators.

The operators convert linear Rec.2020 to linear ProPhoto RGB (D50, with Bradford white-point
adaptation), encode values with the sRGB transfer curve, apply the fitted curves, and convert
back. The matrices come from `RgbSpace::to_space`. CPU `finish.rs` and GPU `finish.wgsl` use
the same matrices and operator tables. CPU previews compose the encoding and operator
lookups into tables indexed by linear input. The computation is pointwise, so a render window needs
no whole-photo measurement.

## Operator family

For a slider amount `a` between -100 and 100, with `t = abs(a) / 100`:

- Positive amounts use `F_a(v) = v + s_pos(t) * D_pos(v)`.
- Negative amounts use `F_a(v) = max(0, v - s_neg(t) * D_neg(v))`.
- Each displacement shape is piecewise linear on 12 knots: 0, .02, .05, .09, .14, .2, .28,
  .38, .5, .65, .82 and 1. Both shapes satisfy `D(1) = 0`. Contrast also fixes `D(0) = 0`.
- The amount law is `s(t) = ((1 - exp(-t / tau)) / (1 - exp(-1 / tau)))^q`, with separate
  fitted `tau` and `q` for each sign. Zero amount is the identity.

Contrast applies `F` to each encoded ProPhoto channel. For Blacks, the luminance result
scales linear RGB by `F(Y) / Y`, where the linear ProPhoto luminance coefficients are
`[0.2880402, 0.7118741, 0.0000857]` and `F(Y)` includes encoding and decoding. Blacks blends
that result with the per-channel result in linear light, using one fitted weight per sign.

Each active operator has 1024 table entries over encoded values between 0 and 1. The entries
store linear output and are interpolated linearly. A running maximum prevents a fitted
curve from reversing tone order. Neither shipped fit folds back. Non-finite amounts are
skipped, and finite amounts are clamped to the slider range.

When the resolved per-channel weight is exactly one or zero in f32, the renderer selects
only the per-channel or luminance operation. In particular, negative Blacks is per channel.
For 8-bit CPU renders, per-channel Contrast and Blacks share one table with 16,384 intervals
over linear input. With a genuine luminance mix, each operator has a separate linear-input
table and the renderer preserves their order. U16 and F32 exports retain the original
operator tables and exact sRGB encoding. Metal retains the original tables too.

### HDR and gamut limits

A per-channel component above 1 passes unchanged. A component below zero keeps its distance
below the image of zero. Luminance scaling leaves a pixel unchanged when its luminance is
above 1 or at most `1e-6`. Otherwise it scales the entire pixel, including components above
1. Blacks blends these two results, so an above-white component can move when the pixel's
luminance is below white. The functions are continuous at white. HDR tone mapping and
Whites retain V2's behavior. GPU renders use 8-bit previews; high-bit-depth exports use the
CPU. These are numerical rules, not measured Lightroom HDR parity.

## Fit provenance and reproduction

The coefficients were fitted to black-box observations of the owner's Lightroom exports:
16-bit sRGB exports of Sony ILCE-7CR photos, paired with the same photo's Neutral or another
slider amount exported in the same batch. Only exported pixels were used. No Adobe code,
profiles, tables or application assets were used. The photos were split into tune photos
used for fitting and check photos held out for measurement.

| Slider | Fit | Mode | Tune | Check |
|---|---|---|---|---|
| Contrast | `v2-contrast-pc` | Per channel | 155 pairs, 20 photos | 61 pairs, 6 photos |
| Blacks | `v2-blacks-mix` | Per-channel/luminance mix | 148 pairs, 19 photos | 60 pairs, 6 photos |

The exports and the optimizer that reads them remain private. Public tools in this change
contain the operator family, sanitized fits with coefficients and aggregate errors, and the
generator. Fit files contain no photo names, paths or per-photo values. Their provenance
records the original fit filename and SHA-256. The Rust constants record each sanitized
fit's SHA-256 as well.

The sanitizer omits error groups and individual bands with fewer than three distinct
contributing photos. Published rows record the distinct photo counts for the group and its
retained bands. Repeated pairs from one photo do not satisfy the threshold. Coefficients
and overall counts remain available. These omissions do not change the fits or the full
paired agreement measurements below.

```sh
python3 -m pip install -r tools/requirements-light-v3.txt
python3 tools/light_v3_constants.py rust
python3 tools/light_v3_constants.py rust --check
```

`--check` must reproduce `crates/pipeline/src/light_v3_params.rs` byte for byte. The generator
writes reference outputs from `tools/light_v3_family.py` to 12 significant digits, avoiding
platform-dependent final bits in NumPy. Rust tests compare the port against those references.
`sanitize` accepts only Contrast and Blacks fits and writes their public aggregate form.
`python3 -m unittest discover -s tools -p 'test_light_v3_constants.py'` checks the publication
threshold with synthetic inputs and scans all committed fit files.

## Measured agreement

The private agreement harness compares the log2-luminance response of each export pair with
LightCraft's response to the same two recipes. The reported error, in stops, is the mean over
rows (amount, reference and size) of the median over photos. Both builds use the same photos;
a registration rejected in either run is omitted from the paired comparison. The historical
V2 baseline is main at `76b53778`. The six check photos were not used to fit the operators.

| Slider | V2 tune | Calibrated tune | V2 check | Calibrated check |
|---|---:|---:|---:|---:|
| Contrast | 0.168 | 0.059 | 0.136 | 0.038 |
| Blacks | 0.540 | 0.112 | 0.424 | 0.077 |
| Whites | 0.176 | 0.176 | 0.075 | 0.075 |

Contrast and Blacks values are the source calibration's measured results, reproduced by
this split's verification. Whites uses V2. A single camera and one person's photos do not
establish fidelity across cameras. Blacks still has larger errors in the darkest band.
Exposure, Highlights, Shadows, Whites and other operators need further work. Local
adjustments and sources without a camera curve have no new calibration.

## Cost and stability

Each active operator has a 4 KiB source table, at most 8 KiB for both. CPU previews also
build one composed table of 128 KiB, or two such tables when Blacks uses a genuine luminance
mix. Each of its 16,384 intervals stores an intercept and slope. The composed per-channel
path uses three lookups per pixel and two colour matrix multiplications. A luminance mix
also computes luminance scaling and a blend.
There is no extra image pass, source read, per-photo state or statistic. Zero Contrast and
Blacks skip the new work. Metal and high-bit-depth CPU exports allocate only the source tables.

Release measurements on an M3 Max used two Rayon threads and nine interleaved samples per
case against main `701f78ea` and the original V3 commit `344bf75b`. With Contrast +40 and
Blacks -30, cached CPU renders took 1.20 to 1.25 times main across Sony ILCE-7M3 and Nikon D750
fit views, 1:1 windows and full frames. The original V3 took 1.98 to 2.06 times main in the
same run. Full-frame CPU medians fell from 1535 to 927 ms for Sony and from 1567 to 960 ms
for Nikon. Fit and window output was 1920 by 1280; full frames were about 24 MP. These
measurements exclude decoding, cache preparation and file encoding.

Metal medians were 3.07 to 4.76 ms for fit and window views and 18.24 to 22.83 ms for full
frames. The initial Nikon window median rose from 3.67 to 4.76 ms; a 27-sample interleaved
confirmation measured 3.92 ms for the original V3 and 3.91 ms for this version. Other work
on the shared machine was not controlled. These samples do not establish tail latency.

V1 and V2 keep their rendering and persistent cache keys. Persistent keys from process V3
on include the process this build actually renders with, so saved settings from a newer
build cannot reuse pixels generated by a different resolved process. V3 keeps V2's
Highlights and Shadows base. New imports and Reset select the latest process; Update to
Current Process remains one undoable settings change and never downgrades a future number.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-13 | minor | Compose CPU preview lookups, preserve exact deep exports, and require three distinct photos per published error aggregate |
| 2026-10-12 | new | Split Contrast and Blacks calibration, preserve V2 Whites and legacy rendering |
