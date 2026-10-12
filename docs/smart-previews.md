# Smart previews

Smart previews are reduced sources for editing and export while originals are offline. Online originals still take precedence. A proxy does not retain the original's full resolution.

## Choose a format

Settings > Performance > Smart previews has a per-library format preference. Compact (JPEG) remains the default. Lossless (32-bit TIFF) preserves every finite float sample of the decoded proxy source. It uses substantially more disk space, about 40 to 50 MB on the two public Sony samples measured for issue #709, compared with 0.5 to 1 MB for Compact.

File > Previews > Build Smart Previews offers the library format or a format for that build. A build override does not change the library preference. Settings also has Build for All Photos. Before a Lossless build starts, a dialog shows the photo count, estimated total size and free space on the destination volume. The estimate uses about 39.5 MB per photo from the measured A7CR proxy; actual sizes vary. If the estimate exceeds free space, the dialog warns and offers Build Anyway or Cancel. Builds run in the background, report progress, and can be stopped.

Changing the preference does not rewrite files. A subsequent build converts existing previews from available originals. If an original is offline or unreadable, its existing preview stays unchanged. Reconnect the drive and build again to convert it. Converting a JPEG proxy cannot restore precision, so builds never use an old proxy as their source.

The displayed count and total size use the actual files in both the old flat layout and the current sharded layout. Mixed-format libraries are supported.

## Stored pixels and metadata

Both formats store the decoded, oriented, reduced scene-linear working RGB source (Rec.2020), before develop controls and the camera tone curve. The camera colour fit is already in these pixels. Tone and chroma curves remain in the JSON header and apply at render time.

Compact keeps the existing `LCSP1` container and JPEG quality 92. Its stored scale covers the brightest 99.95% of samples, so extreme highlights and negative values can be lost. PR #850 separately proposes raising JPEG quality and versioning that encoding. Increasing JPEG quality retains the 8-bit limit.

Lossless uses `LCSP2`, a JSON header, a Deflate-compressed RGB float TIFF from the existing pure-Rust codec, and a completeness marker. It stores the source values directly, including negative values and values above 1.0. No scaling, transfer curve, integer quantization, or render process version is added.

The header has `format: "tiff32"`, format-local `enc: 1`, dimensions, `look_version`, payload length, a 128-bit checksum, and optional `tone`. Unknown top-level and tone fields survive rewrites. This includes a newer build's `white_stat`. When integrating the light-calibration branch, its header backfill must recognize both container magics and preserve the pixel payload and completeness marker.

The reader bounds the JSON header to 4096 bytes, the file to 256 MiB, dimensions to 8192 on either axis, and the pixel count to 16 Mi pixels. The decoder must agree with the header dimensions. Lossless payload length and checksum are verified before decoding. The cheap is_complete check verifies length and the trailing marker for staleness hints. The separate is_usable check verifies the checksum and decodes the pixels. Build Smart Previews uses is_usable before skipping a rebuild. Folder migration verifies the destination before deleting the source. A malformed file returns an error.

Builds write atomically. Format conversions, repairs, discards, and automatic look refreshes invalidate rendered caches. Decoded sources follow the same cache generation, and a pending older render cannot refill them. Automatic look refreshes preserve each proxy's format. Their candidate scan checks for an older look version. It does not decode current-look proxies to find corruption. Use Build Smart Previews to verify and repair those files.

Invalidation follows the cache currently attached to the same library, including reopening it while a conversion or look refresh is running. Switching to another library does not invalidate that other library's cache.

## Move the preview folder

Settings runs folder changes on a worker and shows progress and cancellation in the activity stack. Builds and look refreshes must finish before a folder change starts. New builds wait until the folder change finishes.

For Move, the worker copies and verifies every destination before the library saves and uses the new location. This also applies on the same volume. Renaming each source early would leave the old folder incomplete after cancellation or a crash, so there is no rename fast path. The destination needs enough free space for the copies while the old folder is retained.

Cancel during copying keeps the entire old folder in use. Verified copies already written to the new folder may remain; retrying reuses them. A copy, verification or preference-save failure also retains all sources and the old location. Reopening a library during copying cancels the folder change without waiting for the worker's I/O.

After the new location is saved, cleanup removes redundant sources on the worker and cannot be cancelled. Each destination is verified again before its source is removed. A crash before the location is saved reopens the complete old folder; retry the move. A crash during cleanup reopens the complete new folder and can leave redundant old files. Those leftovers can be removed after checking that the new folder is available. Neither crash point leaves the active folder split. Leave retains the old files. Delete removes them only after the new location is saved.

## Codec comparison

The existing codecs support PNG16, TIFF16 with Deflate and a horizontal predictor, native lossless JXL16, and TIFF32 with Deflate. Float TIFF was selected because it preserves source samples exactly without adding a dependency. TIFF16 used roughly half the space, with small quantization error. PNG16 was larger than TIFF16. The original JXL comparison mixed sRGB-tagged integer samples with decoded linear Rec.2020 pixels. Converting the decoded pixels back to the integer sample space before restoring the range corrects the comparison. Maximum source error on the public A7CR sample is 1.47e-5, and the direct 16-bit integer round trip is exact. There is no JXL codec bug. Float TIFF keeps its exact-float advantage over integer quantization. A linear DNG would add raw-container metadata and decoding rules without improving preservation of an already decoded source.

Reproduce the comparison with `cargo run -p lightcraft-engine --example smart_formats -- sample.ARW`. It reports sizes, encoding and decoding times, maximum source error, and rendered CIE76 delta E at neutral settings, Exposure -2 with Highlights -100, and Exposure +2. Render comparisons use the same proxy grid and metadata to isolate storage error. They do not measure the difference between a full-resolution original render and a reduced source render.
