//! AVC444 YUV plane split/combine helpers ([MS-RDPEGFX] §3.3.8.3.2, v1).
//!
//! The AVC444 server sends *two* H.264-encoded YUV420 frames per video frame: a
//! "main" view carrying full-resolution luma plus a 4:2:0 chroma subsample, and
//! an "auxiliary" view carrying the chroma samples that 4:2:0 subsampling
//! dropped. The receiver decodes both as YUV420, then merges them into a single
//! full-resolution YUV444 frame for display.
//!
//! - [`split_yuv444_to_yuv420_v1`] does the *encoder* side — full-res YUV444 →
//!   main + auxiliary YUV420.
//! - [`combine_luma_to_yuv444`] / [`combine_chroma_v1_to_yuv444`] do the
//!   *decoder* side. Both must be applied (luma first, then chroma) to fully
//!   reconstruct YUV444.
//!
//! The combine functions are ported essentially verbatim from FreeRDP's
//! `general_LumaToYUV444` / `general_ChromaV1ToYUV444`
//! (`libfreerdp/primitives/prim_YUV.c`, Apache-2.0); they're the canonical
//! reference because every real AVC444 client in the wild today is FreeRDP- or
//! mstsc-based and decodes against the same scheme. The split is *not* a
//! verbatim port: FreeRDP's `general_YUV444SplitToYUV420` swaps U and V in the
//! main view's chroma (writes `pU[x] = pSrcV[2*x]`) which contradicts both the
//! §3.3.8.3.2 pseudo-code and FreeRDP's own combine, and is unreachable through
//! their normal RGB→AVC444YUV server path. This implementation follows the spec
//! pseudo-code instead, verified by the roundtrip test below.
//!
//! Buffer-size contract:
//! - **Width and height MUST be even.** [MS-RDPEGFX] §2.2.4.4 requires
//!   multiples of 16 on the H.264 wire (which implies even); the math here
//!   only needs even, but production callers should always pad to 16.
//! - Main view: Y = `width × height`, U/V = `(width / 2) × (height / 2)`.
//! - Auxiliary view: Y = `width × padded_aux_height(height)` (height is padded
//!   to a multiple of 16; the §3.3.8.3.2 B4/B5 packing uses 8-line strips and
//!   spec/FreeRDP both overpad), U/V = same as main.
//! - All planes are tightly packed with `stride = plane_width` in the test
//!   helpers; the production API takes explicit strides.

#![allow(dead_code)] // wired into the h264 pipeline in a follow-up commit

/// Height the auxiliary Y plane must be allocated to. Matches FreeRDP's
/// `padHeigth = height + 16 - height % 16`. Adds 16 when already aligned, which
/// is intentional padding margin — B4/B5 packing only writes through `height`
/// odd rows, so the over-allocation is just unused space.
pub fn padded_aux_height(height: usize) -> usize {
    height - height % 16 + 16
}

/// Split a full-resolution YUV444 frame into main + auxiliary YUV420 frames per
/// [MS-RDPEGFX] §3.3.8.3.2 v1. All slices are tightly packed with the given
/// strides; the caller is responsible for sizing them per the contract above.
#[allow(clippy::too_many_arguments)]
pub fn split_yuv444_to_yuv420_v1(
    src_y: &[u8],
    src_u: &[u8],
    src_v: &[u8],
    src_stride_y: usize,
    src_stride_u: usize,
    src_stride_v: usize,
    main_y: &mut [u8],
    main_u: &mut [u8],
    main_v: &mut [u8],
    main_stride_y: usize,
    main_stride_u: usize,
    main_stride_v: usize,
    aux_y: &mut [u8],
    aux_u: &mut [u8],
    aux_v: &mut [u8],
    aux_stride_y: usize,
    aux_stride_u: usize,
    aux_stride_v: usize,
    width: usize,
    height: usize,
) {
    debug_assert!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "dimensions must be even"
    );
    let half_width = width / 2;
    let half_height = height / 2;
    let pad_height = padded_aux_height(height);

    // B1: main Y = src Y, full resolution.
    for y in 0..height {
        let src_row = &src_y[y * src_stride_y..y * src_stride_y + width];
        let dst_row = &mut main_y[y * main_stride_y..y * main_stride_y + width];
        dst_row.copy_from_slice(src_row);
    }

    // B2, B3: main U/V = even-row even-column samples (the standard 4:2:0
    // subsample). Per §3.3.8.3.2 pseudo-code: B2[x,y] = U444[2x, 2y].
    for y in 0..half_height {
        let src_u_row = &src_u[2 * y * src_stride_u..2 * y * src_stride_u + width];
        let src_v_row = &src_v[2 * y * src_stride_v..2 * y * src_stride_v + width];
        let dst_u_row = &mut main_u[y * main_stride_u..y * main_stride_u + half_width];
        let dst_v_row = &mut main_v[y * main_stride_v..y * main_stride_v + half_width];
        for x in 0..half_width {
            dst_u_row[x] = src_u_row[2 * x];
            dst_v_row[x] = src_v_row[2 * x];
        }
    }

    // B4, B5: odd-row samples of U then V, interleaved into the auxiliary Y
    // plane in 8-line strips. Within each 16-line block: first 8 lines carry
    // odd U rows, next 8 lines carry odd V rows. The pad_height overshoot is
    // skipped via `pos >= height` checks (matches FreeRDP behaviour).
    let mut u_y = 0usize;
    let mut v_y = 0usize;
    for y in 0..pad_height {
        if (y % 16) < 8 {
            let pos = 2 * u_y + 1;
            u_y += 1;
            if pos >= height {
                continue;
            }
            let src_row = &src_u[pos * src_stride_u..pos * src_stride_u + width];
            let dst_row = &mut aux_y[y * aux_stride_y..y * aux_stride_y + width];
            dst_row.copy_from_slice(src_row);
        } else {
            let pos = 2 * v_y + 1;
            v_y += 1;
            if pos >= height {
                continue;
            }
            let src_row = &src_v[pos * src_stride_v..pos * src_stride_v + width];
            let dst_row = &mut aux_y[y * aux_stride_y..y * aux_stride_y + width];
            dst_row.copy_from_slice(src_row);
        }
    }

    // B6, B7: even-row odd-column samples of U and V → auxiliary U and V planes
    // (themselves 4:2:0-shaped). Per §3.3.8.3.2: B6[x,y] = U444[2x+1, 2y].
    for y in 0..half_height {
        let src_u_row = &src_u[2 * y * src_stride_u..2 * y * src_stride_u + width];
        let src_v_row = &src_v[2 * y * src_stride_v..2 * y * src_stride_v + width];
        let dst_u_row = &mut aux_u[y * aux_stride_u..y * aux_stride_u + half_width];
        let dst_v_row = &mut aux_v[y * aux_stride_v..y * aux_stride_v + half_width];
        for x in 0..half_width {
            // 2x+1 is the odd column; clamp to width-1 on the right edge so a
            // non-even width doesn't read past the source row.
            let col = (2 * x + 1).min(width - 1);
            dst_u_row[x] = src_u_row[col];
            dst_v_row[x] = src_v_row[col];
        }
    }
}

/// Decoder LUMA pass — ported from FreeRDP's `general_LumaToYUV444`. Copies the
/// main-view Y to dst Y at full resolution and replicates the main-view U/V
/// samples across every position of each 2x2 block. The chroma pass below
/// overwrites the odd positions with the auxiliary samples.
#[allow(clippy::too_many_arguments)]
pub fn combine_luma_to_yuv444(
    main_y: &[u8],
    main_u: &[u8],
    main_v: &[u8],
    main_stride_y: usize,
    main_stride_u: usize,
    main_stride_v: usize,
    dst_y: &mut [u8],
    dst_u: &mut [u8],
    dst_v: &mut [u8],
    dst_stride_y: usize,
    dst_stride_u: usize,
    dst_stride_v: usize,
    width: usize,
    height: usize,
) {
    debug_assert!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "dimensions must be even"
    );
    let half_width = width / 2;
    let half_height = height / 2;

    // B1: dst Y = main Y.
    for y in 0..height {
        let src_row = &main_y[y * main_stride_y..y * main_stride_y + width];
        let dst_row = &mut dst_y[y * dst_stride_y..y * dst_stride_y + width];
        dst_row.copy_from_slice(src_row);
    }

    // B2, B3: replicate main U/V into the four positions of each 2x2 block.
    for y in 0..half_height {
        let src_u_row = &main_u[y * main_stride_u..y * main_stride_u + half_width];
        let src_v_row = &main_v[y * main_stride_v..y * main_stride_v + half_width];
        let val_2y = 2 * y;
        let val_2y1 = val_2y + 1;
        // Slice both rows we'll write (guarded by even-height path; odd height
        // skips the second).
        let (dst_u_top, mut dst_u_bot) =
            split_two_rows_mut(dst_u, dst_stride_u, val_2y, val_2y1, width);
        let (dst_v_top, mut dst_v_bot) =
            split_two_rows_mut(dst_v, dst_stride_v, val_2y, val_2y1, width);
        for x in 0..half_width {
            let val_2x = 2 * x;
            let val_2x1 = val_2x + 1;
            let u = src_u_row[x];
            let v = src_v_row[x];
            dst_u_top[val_2x] = u;
            dst_v_top[val_2x] = v;
            if val_2x1 < width {
                dst_u_top[val_2x1] = u;
                dst_v_top[val_2x1] = v;
            }
            if let (Some(du), Some(dv)) = (dst_u_bot.as_deref_mut(), dst_v_bot.as_deref_mut()) {
                du[val_2x] = u;
                dv[val_2x] = v;
                if val_2x1 < width {
                    du[val_2x1] = u;
                    dv[val_2x1] = v;
                }
            }
        }
    }
}

/// Decoder CHROMAv1 pass — ported from FreeRDP's `general_ChromaV1ToYUV444`.
/// Reverses [`split_yuv444_to_yuv420_v1`]'s B4/B5/B6/B7 packing back into the
/// dst U/V planes' odd positions.
#[allow(clippy::too_many_arguments)]
pub fn combine_chroma_v1_to_yuv444(
    aux_y: &[u8],
    aux_u: &[u8],
    aux_v: &[u8],
    aux_stride_y: usize,
    aux_stride_u: usize,
    aux_stride_v: usize,
    dst_u: &mut [u8],
    dst_v: &mut [u8],
    dst_stride_u: usize,
    dst_stride_v: usize,
    width: usize,
    height: usize,
) {
    let half_width = width / 2;
    let half_height = height / 2;
    let pad_height = padded_aux_height(height);

    // B4, B5: auxiliary Y plane carries odd U rows in 8-line strips, then
    // odd V rows in the next 8 lines, per 16-line block.
    let mut u_y = 0usize;
    let mut v_y = 0usize;
    for y in 0..pad_height {
        if (y % 16) < 8 {
            let pos = 2 * u_y + 1;
            u_y += 1;
            if pos >= height {
                continue;
            }
            let src_row = &aux_y[y * aux_stride_y..y * aux_stride_y + width];
            let dst_row = &mut dst_u[pos * dst_stride_u..pos * dst_stride_u + width];
            dst_row.copy_from_slice(src_row);
        } else {
            let pos = 2 * v_y + 1;
            v_y += 1;
            if pos >= height {
                continue;
            }
            let src_row = &aux_y[y * aux_stride_y..y * aux_stride_y + width];
            let dst_row = &mut dst_v[pos * dst_stride_v..pos * dst_stride_v + width];
            dst_row.copy_from_slice(src_row);
        }
    }

    // B6, B7: even-row odd-column samples. Writes 2x+1 of each even dst row.
    for y in 0..half_height {
        let src_u_row = &aux_u[y * aux_stride_u..y * aux_stride_u + half_width];
        let src_v_row = &aux_v[y * aux_stride_v..y * aux_stride_v + half_width];
        let val_2y = 2 * y;
        let dst_u_row = &mut dst_u[val_2y * dst_stride_u..val_2y * dst_stride_u + width];
        let dst_v_row = &mut dst_v[val_2y * dst_stride_v..val_2y * dst_stride_v + width];
        for x in 0..half_width {
            let val_2x1 = 2 * x + 1;
            dst_u_row[val_2x1] = src_u_row[x];
            dst_v_row[val_2x1] = src_v_row[x];
        }
    }
}

/// Borrow two disjoint rows from a strided plane. The bottom row is `None`
/// when it falls outside `height` (caller's responsibility to verify).
fn split_two_rows_mut(
    plane: &mut [u8],
    stride: usize,
    top: usize,
    bot: usize,
    row_len: usize,
) -> (&mut [u8], Option<&mut [u8]>) {
    let top_off = top * stride;
    let bot_off = bot * stride;
    if bot_off + row_len > plane.len() {
        return (&mut plane[top_off..top_off + row_len], None);
    }
    let (lo, hi) = plane.split_at_mut(bot_off);
    (&mut lo[top_off..top_off + row_len], Some(&mut hi[..row_len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a deterministic YUV444 source using a per-plane seeded LCG so
    /// each pixel has a distinct value with no correlation to its neighbours.
    /// Width/height are arbitrary (any positive values, even or odd).
    fn synthetic_yuv444(width: usize, height: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut y = vec![0u8; width * height];
        let mut u = vec![0u8; width * height];
        let mut v = vec![0u8; width * height];
        let lcg = |seed: u64, n: usize| -> u8 {
            let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(n as u64);
            s ^= s >> 33;
            (s & 0xff) as u8
        };
        for j in 0..height {
            for i in 0..width {
                let idx = j * width + i;
                y[idx] = lcg(0x1111_1111, idx);
                u[idx] = lcg(0x2222_2222, idx);
                v[idx] = lcg(0x3333_3333, idx);
            }
        }
        (y, u, v)
    }

    /// End-to-end roundtrip: split full-res YUV444 → main + aux YUV420 →
    /// combine back to YUV444 → assert pixel-equality with the source.
    fn roundtrip(width: usize, height: usize) {
        let (src_y, src_u, src_v) = synthetic_yuv444(width, height);

        // Even dimensions enforced by the helper itself; matches the prod
        // contract (always padded to a multiple of 16).
        let half_w = width / 2;
        let half_h = height / 2;
        let pad_h = padded_aux_height(height);

        let mut main_y = vec![0u8; width * height];
        let mut main_u = vec![0u8; half_w * half_h];
        let mut main_v = vec![0u8; half_w * half_h];
        let mut aux_y = vec![0u8; width * pad_h];
        let mut aux_u = vec![0u8; half_w * half_h];
        let mut aux_v = vec![0u8; half_w * half_h];

        split_yuv444_to_yuv420_v1(
            &src_y, &src_u, &src_v, width, width, width, &mut main_y, &mut main_u, &mut main_v,
            width, half_w, half_w, &mut aux_y, &mut aux_u, &mut aux_v, width, half_w, half_w, width,
            height,
        );

        let mut dst_y = vec![0u8; width * height];
        let mut dst_u = vec![0u8; width * height];
        let mut dst_v = vec![0u8; width * height];

        combine_luma_to_yuv444(
            &main_y, &main_u, &main_v, width, half_w, half_w, &mut dst_y, &mut dst_u, &mut dst_v,
            width, width, width, width, height,
        );
        combine_chroma_v1_to_yuv444(
            &aux_y, &aux_u, &aux_v, width, half_w, half_w, &mut dst_u, &mut dst_v, width, width,
            width, height,
        );

        // Y is sent at full resolution; every byte must match.
        assert_eq!(dst_y, src_y, "Y plane mismatch ({width}x{height})");

        // U/V: every sample participated in the split — even-even via B2/B3,
        // even-odd via B6/B7, odd-* via B4/B5. The whole plane must roundtrip.
        for j in 0..height {
            for i in 0..width {
                let idx = j * width + i;
                assert_eq!(
                    dst_u[idx], src_u[idx],
                    "U mismatch at ({i},{j}) for {width}x{height}"
                );
                assert_eq!(
                    dst_v[idx], src_v[idx],
                    "V mismatch at ({i},{j}) for {width}x{height}"
                );
            }
        }
    }

    #[test]
    fn roundtrip_16x16() {
        roundtrip(16, 16);
    }

    #[test]
    fn roundtrip_64x48() {
        roundtrip(64, 48);
    }

    #[test]
    fn roundtrip_1920x1080() {
        roundtrip(1920, 1080);
    }

    /// Even, non-16-aligned sizes. AVC444's wire contract requires 16-alignment
    /// so production never sees these (we pad), but the split/combine math
    /// only needs even dimensions.
    #[test]
    fn roundtrip_even_non_16_aligned() {
        roundtrip(18, 16);
        roundtrip(34, 32);
        roundtrip(100, 64);
        roundtrip(64, 100);
    }

    /// The real Mac display size (1920x1080) needs height-padding to 1088 to
    /// be spec-legal for AVC444. Cover both: native 1920x1080 (even, just
    /// not 16-aligned in height) and the 16-aligned padded form.
    #[test]
    fn roundtrip_realistic_display_sizes() {
        roundtrip(1920, 1080);
        roundtrip(1920, 1088);
        roundtrip(2560, 1440);
        roundtrip(3840, 2160);
    }

    #[test]
    fn padded_aux_height_examples() {
        // Multiples of 16 still add 16 (FreeRDP defensive overpad).
        assert_eq!(padded_aux_height(16), 32);
        assert_eq!(padded_aux_height(64), 80);
        // Non-aligned rounds up to next multiple of 16.
        assert_eq!(padded_aux_height(60), 64);
        assert_eq!(padded_aux_height(1), 16);
        assert_eq!(padded_aux_height(17), 32);
    }
}
