# Spatial context for zoom windows

A window render prepares the surrounding pixels needed by its active local filters,
then returns the requested rectangle. The CPU and GPU use the same calculation in
`pipeline::plan` and `crates/pipeline/src/window.rs` (issue #1146).

## Why the old margin was insufficient

`region::margin_for` requests 4.5% of the output long edge, rounds up to 256 pixels,
and caps that margin at 768 pixels. `window_for` can reduce it further to fit the
display texture. These are useful bounds for the displayed window and pan reuse,
but they do not describe the filters' support.

The current loupe renders windows at no more than the source's native resolution.
Above 100%, the display magnifies those pixels. The issue's 4:1 and 8:1 examples
use larger virtual frames through the pipeline API. Both paths need the same
context rule, independent of how the caller chooses its window.

For an uncropped 2400-pixel long edge at full quality:

| Plane | Sigma | Context in each axis |
|---|---|---|
| Dehaze dark channel / transmission | 48 px | 142 px |
| Clarity | 28.8 px | 162 px, including resampling support |
| Highlights and Shadows, V1 | 36 px | 192 px, including resampling support |
| Highlights and Shadows, V2 | 36 px | 196 px, including the 4 px median |
| Texture / sharpening | 4.32 px | 11 px |

A Gaussian here is three finite box passes, so its support is the sum of their
radii (`raster::blur::box_radii`). A guided filter applies that blur twice. Its
fast version downsamples and upsamples too; the calculation includes those taps.
The V2 tone base also reads the median's radius. The maximum support of parallel
planes is enough, while sequential noise reduction adds its support. Local Noise
and Moire use the same fine-detail and chromaticity planes. Mask feathering,
heuristic blurs, and Refine Edges have their own support in the same calculation.
Supplied segmentation mattes skip the heuristic support that their evaluation
bypasses. Background uses the Subject matte before inversion. Refine Edges still
adds support when a matte is supplied. Hidden masks and masks without components
add no unused support, and hidden Dehaze does not trigger the full-frame airlight calculation.
Rendering still selects the same planes as main, including those selected by
hidden and empty masks. This preserves existing V1/V2 pixels. Clarity consumes
the tone base even with Highlights and Shadows at zero, so an inactive tone
mask's base support remains in the context when global or visible local Clarity
is active. CPU and GPU use the same render selector and context plan.

Auto Mask dab reads retain the existing spot/brush context calculation. Global
Defringe adds its shared min/max radius after spot source expansion, because
spots copy already-defringed pixels. Defringe runs before noise reduction and
local planes, so its support adds to theirs, including around spot source reads.

Dehaze windows use the same full-resolution airlight statistic as a full render.
The statistic and transmission formula for `window=None` are unchanged.

## Design and cost

The display window and its texture limits stay unchanged. The render plan expands
its private work rectangle, clipped only at the physical frame edges. It includes
spot source reads for that expanded rectangle. If the spot-read limit cannot
hold them, the plan uses the full frame. The GPU checks its storage-buffer limit
against the expanded size and uses the existing CPU fallback when necessary.
Only the requested pixels leave the renderer, including float and 16-bit output.

This uses the existing filters at their existing scale. A shared, reduced-size
analysis image would change full-render pixels unless it reproduced those filters
exactly. The work rectangle and its cached stages preserve V1 and V2 full renders byte
for byte. Window airlight retains a scalar, and mask caches retain the alphas
whose inputs still match. No new process version is introduced.

The cost is extra sampled pixels and filter work, and larger stage caches. A warm
Exposure edit finishes only the requested rectangle on both CPU and GPU. Grain,
vignette and plane reads keep their coordinates in the work image. GPU output
allocation and readback also use the requested extent. Deep CPU output follows
the same rule. Diagnostic overlays retain the full work-image finish because
some read neighbouring finished pixels. At very large virtual scales or tight
crops, the context can approach the full frame. It is not
silently capped to a wrong result. Finishing fewer pixels does not shrink the
spatial planes or stage caches. The worst case with every spatial filter and
three refined masks still needs a full-frame work rectangle.

## Full-frame sampling for reduced planes

The fast guided filter uses a nominal step from the full-frame sigma. Its actual
sample spacing is `full / ceil(full / step)` on each axis. For example, the
2397-pixel frame at step 11 has 218 samples, spaced about 10.9954 pixels apart.
Snapping the full-resolution work rectangle to multiples of 11 would still use
the wrong positions.

`local::GuidedGrid` snaps the reduced coefficient rectangle outwards to that
full-frame grid. Both renderers use the same subsets of the full-frame Box and
Bilinear taps. The downsample has the original scale, origin and edge weights;
the upsample has the original phase. Only taps for the work rectangle are
allocated. Missing taps at its outer boundary clamp there, and the existing
context excludes their support from the requested pixels. The full-resolution
work rectangle and cache geometry stay unchanged.

Full renders, whole-frame windows and filters with step 1 use the original path.
The V2 median still runs before the tone base. Resampling alone is bit-identical
to the corresponding full-frame samples. Final rendering can differ by a code
value because the blur's floating-point running sums start at different places.

| Path checked | Sampling behavior |
|---|---|
| Fast guided filter, Highlights/Shadows base, Clarity | Shared full-frame reduced grid on CPU and GPU |
| Dehaze dark channel / transmission | Full-resolution Gaussian; no reduced spatial grid |
| Luminance and color noise reduction | Full-resolution guided filter and Gaussian |
| Refine Edges and Auto Mask refinement | Full-resolution cross-guided filter |
| Supplied mask mattes and segmentation grids | Existing frame-to-source coordinates |
| Dehaze airlight | Full-output-resolution statistic on each renderer, including upstream processing and every-seventh-value selection |

## Full-frame airlight and mask reuse (round 5)

Planning describes whether a window needs airlight and does no airlight pixel
work. Rendering resolves a separate `window=None` plan at the requested full
output dimensions. Its source sampling, camera local tone, white balance,
Defringe, spots, red-eye corrections, noise reduction and dark-channel blur are
the same as a full render. CPU and Metal compute their own scalar with their
existing every-seventh-value selection and 99.5th percentile. A whole-frame work
buffer can supply the statistic directly. A Metal full frame that exceeds its
buffer limit uses the CPU statistic, matching the full-render fallback.

Each renderer keeps two scalar entries keyed by source identity and the full
linear-stage key. Panning, Exposure, Dehaze amount and downstream color edits
reuse the scalar. Source, framing, output dimensions, white balance, retouching,
noise reduction and relevant source metadata invalidate it. The source is held
weakly by the scalar cache. Full-frame temporary pixels are released before the
window stages are allocated. A first zoom can therefore cost a full-resolution
upstream pass; it never substitutes a smaller statistic.

Native `Session::region_job_of` first calls `render_job` with the full frame's
size and then adds the window. Source selection therefore matches the equivalent
full render, including a smaller preview source and crop-driven source selection.
`RenderJob::draft` changes quality, not the source. The web `WireJob` keeps the
source level, dimensions and window; `WorkerCore::render` passes the decoded
source and its metadata to the same cached CPU pipeline. The tests include a
601 by 403 source rendered into an 1800 by 1207 frame. This covers a source
smaller than the requested virtual frame without a second airlight proxy.

The CPU and GPU alpha caches incorporate #1147's implementation and tests
(`2d85c4a3` and `99b89cd9`). Each mask's key includes its components, component
operations and inversion, mask inversion, Amount, Refine Edges, and work-frame
geometry. Image-dependent masks also include the linear-stage key. Source
identity and supplied matte identity are retained by the containing caches.
Renaming, reordering and changing a mask's adjustment sliders other than Amount
do not change its alpha. Hidden and removed masks release their cached alphas.

| Alpha input | Image dependency | Exposure dependency |
|---|---|---|
| Linear, radial and ordinary brush | None | None |
| Auto Mask brush | Pre-exposure RGB and log luminance | None |
| Subject, Background and People heuristics | Pre-exposure log luminance | None |
| Sky heuristic | RGB and log luminance | Yes |
| Luminance and Color Range | Log luminance or RGB | Yes |
| Stored Object / Prompt segmentation | Stored segmentation and geometry | None |
| Refine Edges | Pre-exposure log luminance, in addition to the shape | No additional dependency |

The imported dependency classifier conservatively treats supplied mattes like
heuristics. A supplied Sky matte can therefore be rebuilt unnecessarily after
Exposure changes. This does not affect the three-mask probe, which has no mattes.
Refine Edges reuses coefficient buffers and releases its means and correlations
after their last read. `gaussian_owned` consumes disposable inputs instead of
cloning them. Its arithmetic and blur order are unchanged. Native sampling with
no orientation, prefilter or transform shares the source Arc instead of caching
a duplicate RGB buffer. `StageCache::bytes` counts owned chroma and alpha buffers
and excludes this shared source, which belongs to the caller.

## Known limits

Final window pixels can differ by a code value because floating-point blur sums
start at different places. The measured bounds apply to the regression matrix;
there is no universal bit-identical final-pixel guarantee. Full renders retain
their existing V1/V2 output. Automatic spot sources still use the existing window
source-selection policy, which can differ from a full render's selection.

The all-filters case still needs a full-frame work rectangle. Exposure changes
must recompute the Sky heuristic and its refinement because its alpha changes.
Radial and Subject refined alphas remain reusable. Measurements below separate
that remaining work from the resolved airlight mismatch.

## Checks

Round 4 adds 168 windows per renderer at 2397 by 1598, including odd origins,
frame edges, repeating detail, hidden/empty masks, local Clarity, isolated tone
and Clarity, and NR with mask refinement. The tests assert max 1 and mean below
0.01 on both CPU and Metal. Measured maxima are 1 on both; worst means are
0.000896 CPU and 0.002456 Metal. CPU results are not universally bit-identical.
The resampling-only test is bit-identical at steps 2, 7, 11 and 16.

These regressions failed on `a4b0bbca` before the fix. The reported V2 edge window
had max 12 / mean about 0.582; it now has max 0 on CPU and max 1 / mean 0.000266
on Metal. The reported V1 stripe center had max 9 / mean 3.798898; it is now exact
on both. The wider regression matrix previously reached max 13 and mean 4.589532.

The unchanged independent round 3 probe also passes all 288 windows per renderer
within max 1. Worst means are 0.000100 CPU, 0.001593 Metal and 0.002257 CPU/Metal
parity. All 32 whole-frame windows per renderer are exact. All 26 full-render
reference hashes still match main `76b53778`; round 4 evidence is retained in
`scratch/dehaze-round4/`.

The following coverage and CI record describe round 3.

`tests_window` covers the original 2400 by 1600 blocks experiment, tight windows
at each corner and at a block boundary, both process versions, combined global
and local edits, upstream noise reduction, refined masks, and deep output sizes.
Round 2 adds supplied Subject, Background, Sky and People mattes, hidden mask
context, the Defringe boundary repro, and context around spot source reads. All four new regressions failed on `a9d23dc0`: matte context
was 654 by 554 instead of 400 by 300; hidden context was 562 by 462; Defringe had
max 75 and mean 0.879; the spot source had no extra Defringe support.

All 33 window tests and both new compatibility integration tests pass. The
pipeline library has 188 passing tests and two existing ignored tests. All 19
Metal equivalence tests pass without skips. Ten original V1/V2 full CPU renders
and eight new CPU renders with hidden or empty masks match main `76b53778` by
exact hashes. Eight new Metal full-render hashes also match that main revision
on the same GPU. The finish-stage test compares the requested pixels byte for
byte across 8-bit, 16-bit and float output.

Round 3 restores main's plane selection after the hidden-mask optimization
changed full-render Clarity. Eight new 1200 by 800 CPU reference hashes cover V1
and V2, hidden and empty tone masks, and global and local Clarity. Separate
window regressions check the base's full support at a scale without guided-filter
subsampling. The HDR finish case enables HDR and requires samples above 1 before
comparing requested-pixel finishing with full finishing and cropping.

The GPU equivalence suite covers the same local operators on Metal, comparing
both against the CPU window and against the corresponding full-render region.

CI was run with the temporary PR #990 and #997 patches, then those patches were
removed. Formatting, clippy and HEIF passed. Workspace tests reproduced exactly
the twelve known failures in #989, with no additional failing test names. Parity
references, layer rules, asset attribution and all 28 WASM package checks passed.

Reproduce window timings with:

```sh
CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 RAYON_NUM_THREADS=2 N=5 \
  CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=../target-dehaze \
  nice -n 15 cargo run --release -p lightcraft-gpu --example window_context
```

The benchmark uses 6000 by 4000 and 9504 by 6336 procedural sources, a 2800 by
1700 physical-pixel canvas, and the current UI's native-resolution windows at
1:1 and 2:1. It reports a cold render and the median of warm Exposure edits,
separately for CPU and GPU, with Dehaze 100 alone and combined spatial edits.

## Performance measurements

These are historical round 2 measurements at `f9f64711`. Round 3 restores
main's render plane selection, so the hidden-mask timing and cache-size results
below do not describe the corrected renderer. No new performance claim is made.

Round 2 measurements compare `a9d23dc0` with these fixes on the Apple M3 Max,
36 GB RAM. All timings are indicative on this shared machine. Builds and runs
use `nice -n 15`, `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2` and
`RAYON_NUM_THREADS=2`. Sources are procedural RGB images already in memory.
Cold means an empty stage cache. Warm means the median of five Exposure edits;
the reported value is the median of three runs. Peak footprint comes from
`/usr/bin/time -l`. Load averages are recorded with `uptime` around each run.

The 61 MP class source is 9504 by 6336 (60,217,344 pixels); its requested window
is 4096 by 2816 at (2560, 1792). The 24 MP source is 6000 by 4000; its requested
window is 4096 by 3072 at (1024, 512). The review's 246 to 387 ms Dehaze figures
came from its 61 MP probe. Both source sizes are measured here explicitly.

Reproduce the CPU cases with `crates/pipeline/examples/window_context_cost.rs`:

```sh
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 RAYON_NUM_THREADS=2
export CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=../target-dehaze
uptime
nice -n 15 cargo build --release -p lightcraft-pipeline --example window_context_cost
nice -n 15 /usr/bin/time -l ../target-dehaze/release/examples/window_context_cost matte
uptime
```

Use `hidden`, `dehaze24`, `dehaze`, or `all` for the other cases. The `all` case
has Dehaze 80, Clarity 80, Texture 60, Highlights -80, Shadows 80, luminance NR 70,
colour NR 60, sharpening 60, Purple Defringe 10, and radial, Subject and Sky masks
with Refine Edges 100. Its timing and footprint are one run, with five warm edits.
The masks also have local Exposure, Clarity, Texture, Noise, Moire and Defringe.

| Case | Work Mpx, old / new | Cold ms, old / new | Warm ms, old / new |
|---|---:|---:|---:|
| 61 MP, Subject matte + Exposure | 35.78 / 11.53 | 1216.0 / 435.3 | 978.0 / 338.9 |
| 61 MP, hidden spatial mask | 26.09 / 11.53 | 1858.9 / 290.5 | 567.5 / 219.4 |
| 24 MP, Dehaze 100 | 18.23 / 18.23 | 803.9 / 568.8 | 581.5 / 394.3 |
| 61 MP, Dehaze 100 | 20.70 / 20.70 | 941.6 / 675.7 | 709.0 / 410.7 |
| 61 MP, all spatial filters + 3 refined masks | 60.22 / 60.22 | 19638.5 / 14779.4 | 12128.0 / 8002.9 |

| Case | Peak footprint GiB, old / new | Historical `StageCache::bytes` GiB, old / new |
|---|---:|---:|
| 61 MP, Subject matte + Exposure | 1.92 / 1.06 | 0.93 / 0.30 |
| 61 MP, hidden spatial mask | 2.87 / 1.02 | 0.97 / 0.30 |
| 24 MP, Dehaze 100 | 0.97 / 0.97 | 0.54 / 0.54 |
| 61 MP, Dehaze 100 | 1.49 / 1.49 | 0.62 / 0.62 |
| 61 MP, all spatial filters + 3 refined masks | 7.97 / 7.97 | 2.47 / 2.47 |

Old is `a9d23dc0`; new includes all three review fixes and requested-pixel
finishing. Baseline runs precede fixed runs, so changing machine load is a
confounder. These are indicative times, not isolated speedup measurements.

The CPU batches recorded these `uptime` load averages (1, 5 and 15 minutes):

| Batch | Start | End |
|---|---|---|
| baseline | 7.00 16.46 22.61 | 9.79 13.76 20.44 |
| fixed | 4.59 8.41 15.36 | 8.56 8.02 14.40 |

These historical revisions used a full 60.22 Mpx work image and reached 7.97 GiB
peak footprint. Their `StageCache::bytes` counter reported 2.47 GiB but omitted
the 0.673 GiB RGB chroma plane, so the cached pixel buffers occupied about
3.14 GiB. The process footprint measurement is unaffected. Round 5 fixes this
counter and also counts cached mask alphas; its current results are separate.

### Historical round 2 GPU cost

One Metal run of the existing `lightcraft-gpu` example, `N=5`, with the same
two-thread limits. Warm values are medians of five Exposure edits. Combined
means Dehaze 100, Clarity 80, Texture 60, Highlights -80 and Shadows 80. It does
not include the three-mask worst case above. These are round 2 absolute costs;
no fresh GPU baseline comparison was made.

| Source | Zoom | Edits | Cold ms | Warm ms |
|---|---|---|---:|---:|
| 24 MP | 1:1 | Dehaze | 126.6 | 47.6 |
| 24 MP | 1:1 | Combined | 148.0 | 41.5 |
| 24 MP | 2:1 | Dehaze | 64.7 | 37.6 |
| 24 MP | 2:1 | Combined | 105.9 | 37.5 |
| 61 MP | 1:1 | Dehaze | 245.7 | 91.9 |
| 61 MP | 1:1 | Combined | 917.5 | 100.3 |
| 61 MP | 2:1 | Dehaze | 130.8 | 90.8 |
| 61 MP | 2:1 | Combined | 214.3 | 87.7 |

GPU benchmark `uptime` load averages were 8.51 8.02 14.36 at the start and
12.61 9.15 14.52 at the end.

## Round 5 measurements

Fresh release builds compare main `25ef65c8`, round 4 `824c2017`, and round 5.
Main's change from the original `76b53778` baseline is only the version bump.
The unchanged independent `review_probe all` ran serially on the Apple M3 Max
with 36 GiB RAM, two build jobs, two test threads, two Rayon threads and
`nice -n 15`. These timings are indicative on a shared machine. Each row is one
cold render and the median of five warm Exposure edits. Peak process footprint
comes from `/usr/bin/time -l`, not maximum resident size.

| Revision | Cold ms | Warm ms | Peak footprint GiB | Cache counter GiB |
|---|---:|---:|---:|---:|
| 25ef65c8 | 4164.8 | 2427.2 | 2.092 | 0.473 |
| 824c2017 | 15741.9 | 8269.1 | 7.977 | 2.468 |
| round5 | 15243.5 | 3142.1 | 6.609 | 3.141 |

The main and round 4 cache counters omit chroma. Their values are historical
`StageCache::bytes`, not total retained memory. Round 5 counts every owned pixel
buffer, including chroma and cached alphas. It excludes the shared source.

| Revision | Load averages before (1, 5, 15 min) | Load averages after |
|---|---|---|
| 25ef65c8 | 9.21 9.64 6.84 | 8.15 9.26 6.83 |
| 824c2017 | 8.39 9.24 6.89 | 6.43 8.30 6.77 |
| round5 | 5.86 6.76 6.69 | 6.33 6.77 6.70 |

Warm edits are 1.29x main, within the requested approximate 1.5x target. Peak
footprint falls by 1.37 GiB from round 4, but remains 3.16x main. Cold time is
3.66x main. The expanded work rectangle still has 60.22 Mpx, against main's
11.53 Mpx. All spatial stages must prepare that context initially. On an
Exposure edit the radial and Subject alphas remain cached; the Sky alpha and
its full-resolution refinement change and must run again. A concrete memory
follow-up is to share scratch buffers between these sequential mask blur passes;
a further speed follow-up is to retain the refinement guide's means and variance
across edits, with an explicit cache budget. Neither requires truncating support.

The separate 61 MP Dehaze-only run measured 962.8 ms cold, 340.3 ms warm and
2.836 GiB peak footprint, with loads 7.64 7.03 6.68 before and 7.64 7.03 6.68
after. It includes the first exact full-resolution statistic; subsequent Exposure
edits reuse it. This run preceded the owned-buffer optimization, which changes
only mask blur inputs and identity source sharing; its partial Dehaze window
uses neither optimization.

Round 5 regressions fail on `824c2017`: whole-frame Dehaze differs by max 48
and the reported partial window by max 47 on CPU and Metal, in both V1 and V2.
An unchanged refined alpha has a new allocation on every Exposure edit. The
chroma counter test reports 144 bytes where its two buffers require 288 bytes.

After the fix, the reported whole-frame window is exact on CPU and Metal.
The partial window is exact on CPU and has max 1 / mean 0.000597 on Metal.
The independent expanded probe checks 490 windows and 98 whole-frame cases per
renderer. All whole-frame cases are exact. All partial cases are within 1 code
value on each renderer, including all 70 Dehaze cases. Dehaze worst means are
0.000100 CPU and 0.002722 Metal; CPU/Metal Dehaze parity is max 1. The unchanged
non-Dehaze parity bound remains max 2, with worst mean 0.176707.

All 26 reference full-render hashes match the preserved main references in
`scratch/dehaze-round3/hash-comparison.json`. All 196 additional full-render
hashes match the round 4 review's main capture. No process version changes.
Evidence and exact commands are in `scratch/dehaze-round5/`.

## Round 5 validation

With the temporary #990/#997 patches applied, `cargo xtask ci` passed formatting,
workspace clippy and HEIF, then stopped at the 11 known engine failures.
`cargo test --workspace --no-fail-fast` completed with exactly the 12 known
failures, matching the round 4 failure names with no additions or omissions.
The final run passed 207 pipeline tests and 51 GPU tests, including all 24 Metal
equivalence tests with no skips. The workspace feature set includes five more
GPU unit tests than the focused package run. Existing ignored tests remain ignored.

Parity, dependency layers, asset attribution and all 28 WASM package checks
passed separately. Parity emitted its existing non-failing stale-summary notice.
The WASM engine check retained the existing `unused_mut` warning in `devices.rs`.
The three temporary patch files were restored byte for byte to the starting
commit. The disk guard paused parity once below 40 GiB and resumed at 54.81 GiB
on its first two-minute poll. Evidence is in `scratch/dehaze-round5/`.
