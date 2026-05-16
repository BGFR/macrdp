//! MS-RDPNSC encoder.
//!
//! Implements the codec defined in MS-RDPNSC §3.1.5:
//! 1. RGB → YCoCg color-space conversion (lossy on chroma when CLL > 0).
//! 2. Optional 4:2:0 chroma subsampling — **not enabled yet** (Phase 3 will
//!    add it). For now we always emit `ChromaSubsamplingLevel = 0`.
//! 3. Per-plane RLE compression (custom MS-RDPNSC byte-level RLE).
//! 4. 20-byte frame header + concatenated Y, Co, Cg, A planes.
//!
//! The output goes inside a `SurfaceBitsPdu` via `set_surface()` in
//! `encoder/mod.rs`; this module is just the codec, no PDU plumbing.

use ironrdp_graphics::image_processing::PixelFormat;

use crate::display::BitmapUpdate;

/// 0xFF in the third byte of a run header signals "long run; read u32 LE next."
const RLE_LONG_ESCAPE: u8 = 0xFF;

/// Encode a bitmap as an NSCodec frame.
///
/// `color_loss_level` (1..=7 per MS-RDPNSC) shifts the chroma planes right
/// by that many bits — higher values lose more chroma precision for smaller
/// output. We currently advertise CLL=3 in the server caps; the value passed
/// here must match what was sent in the `NsCodec` capability or the client
/// won't decode correctly.
pub(crate) fn encode(bitmap: &BitmapUpdate, color_loss_level: u8) -> Vec<u8> {
    let w = usize::from(bitmap.width.get());
    let h = usize::from(bitmap.height.get());
    let pixels = w * h;
    let cll = i32::from(color_loss_level);
    let bpp = usize::from(bitmap.format.bytes_per_pixel());
    let stride = bitmap.stride.get();

    let mut y_plane = Vec::with_capacity(pixels);
    let mut co_plane = Vec::with_capacity(pixels);
    let mut cg_plane = Vec::with_capacity(pixels);
    let mut a_plane = Vec::with_capacity(pixels);

    // Surface-bits PDU clients expect rows bottom-up — `BitmapUpdate.data`
    // arrives top-down from SCK, so we iterate in reverse. Without this each
    // dirty rect is rendered upside-down inside its own bounding box.
    for row in (0..h).rev() {
        let row_off = row * stride;
        for col in 0..w {
            let off = row_off + col * bpp;
            let p = &bitmap.data[off..off + bpp];
            let (r, g, b, _a) = extract_rgba(bitmap.format, p);
            let (y, co, cg) = rgb_to_ycocg(r, g, b, cll);
            y_plane.push(y);
            co_plane.push(co);
            cg_plane.push(cg);
            // Desktop captures are always opaque; the source `A` byte can be
            // zero on macOS (premultiplied/unused), and NSCodec clients treat
            // the alpha plane as actual blending — alpha=0 makes everything
            // transparent and the canvas renders as black.
            a_plane.push(0xFF);
        }
    }

    let y_rle = rle_encode(&y_plane);
    let co_rle = rle_encode(&co_plane);
    let cg_rle = rle_encode(&cg_plane);
    let a_rle = rle_encode(&a_plane);

    let body_len = y_rle.len() + co_rle.len() + cg_rle.len() + a_rle.len();
    let mut out = Vec::with_capacity(20 + body_len);
    // 20-byte fixed header per MS-RDPNSC §2.2.1.2.
    out.extend_from_slice(&(y_rle.len() as u32).to_le_bytes());
    out.extend_from_slice(&(co_rle.len() as u32).to_le_bytes());
    out.extend_from_slice(&(cg_rle.len() as u32).to_le_bytes());
    out.extend_from_slice(&(a_rle.len() as u32).to_le_bytes());
    out.push(color_loss_level);
    out.push(0); // ChromaSubsamplingLevel — Phase 3 will set this to 1.
    out.push(0); // Reserved (2 bytes).
    out.push(0);
    out.extend_from_slice(&y_rle);
    out.extend_from_slice(&co_rle);
    out.extend_from_slice(&cg_rle);
    out.extend_from_slice(&a_rle);

    out
}

/// Pull (R, G, B, A) out of a 4-byte pixel in the given format.
#[inline]
fn extract_rgba(fmt: PixelFormat, p: &[u8]) -> (u8, u8, u8, u8) {
    match fmt {
        PixelFormat::ARgb32 | PixelFormat::XRgb32 => (p[1], p[2], p[3], p[0]),
        PixelFormat::ABgr32 | PixelFormat::XBgr32 => (p[3], p[2], p[1], p[0]),
        PixelFormat::BgrA32 | PixelFormat::BgrX32 => (p[2], p[1], p[0], p[3]),
        PixelFormat::RgbA32 | PixelFormat::RgbX32 => (p[0], p[1], p[2], p[3]),
    }
}

/// RGB → (Y, Co, Cg) using the FreeRDP formulation (matches what Microsoft
/// clients decode against). Y is unsigned 0..=253; Co and Cg are signed
/// values that we reinterpret as `u8` for byte storage (bit pattern preserved).
#[inline]
fn rgb_to_ycocg(r: u8, g: u8, b: u8, cll: i32) -> (u8, u8, u8) {
    let ri = i32::from(r);
    let gi = i32::from(g);
    let bi = i32::from(b);
    let y = (ri >> 2) + (gi >> 1) + (bi >> 2);
    let co = (ri - bi) >> cll;
    let cg = (-(ri >> 1) + gi - (bi >> 1)) >> cll;
    (y as u8, co as i8 as u8, cg as i8 as u8)
}

/// MS-RDPNSC RLE.
///
/// A run is introduced by a value byte appearing twice in succession; the
/// third byte is either `runlength - 2` (0..=253, runs of 2..=255) or `0xFF`
/// (long-run escape) followed by a 32-bit LE runlength. A single occurrence
/// of a value is a plain literal.
///
/// **The last 4 bytes of each plane are copied raw, *not* RLE-encoded** —
/// this matches the FreeRDP reference encoder, which Microsoft NSCodec
/// clients are written against. The decoder unconditionally reads the last
/// 4 bytes of compressed plane data as raw output, so emitting RLE there
/// makes the entire frame undecodable.
fn rle_encode(plane: &[u8]) -> Vec<u8> {
    let n = plane.len();
    if n <= 4 {
        // Plane too small for any RLE — emit raw.
        return plane.to_vec();
    }
    let body_end = n - 4;
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < body_end {
        let v = plane[i];
        let mut run = 1usize;
        while i + run < body_end && plane[i + run] == v {
            run += 1;
            if run as u64 >= u32::MAX as u64 {
                break;
            }
        }
        if run == 1 {
            out.push(v);
        } else if run <= 255 {
            out.push(v);
            out.push(v);
            out.push((run - 2) as u8);
        } else {
            out.push(v);
            out.push(v);
            out.push(RLE_LONG_ESCAPE);
            out.extend_from_slice(&(run as u32).to_le_bytes());
        }
        i += run;
    }
    // Last 4 bytes of the plane are copied raw, by spec/convention.
    out.extend_from_slice(&plane[body_end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_short_input_is_raw() {
        // Inputs of <= 4 bytes can't have a raw 4-byte tail AND a body, so
        // the whole thing is emitted as-is.
        assert_eq!(rle_encode(&[7]), vec![7]);
        assert_eq!(rle_encode(&[7, 8]), vec![7, 8]);
        assert_eq!(rle_encode(&[1, 2, 3, 4]), vec![1, 2, 3, 4]);
    }

    #[test]
    fn rle_no_runs_in_body() {
        // 5-byte plane: body is 1 byte (plane[0]); tail is 4 bytes raw.
        // plane[0]=1 is a literal; remaining 4 bytes copied raw.
        assert_eq!(rle_encode(&[1, 2, 2, 2, 2]), vec![1, 2, 2, 2, 2]);
    }

    #[test]
    fn rle_short_run_in_body_with_raw_tail() {
        // 6 bytes of 7 -> body is plane[0..2] = [7, 7] -> short run of 2.
        // Tail: plane[2..6] = [7, 7, 7, 7].
        let plane = vec![7u8; 6];
        assert_eq!(rle_encode(&plane), vec![7, 7, 0, 7, 7, 7, 7]);
    }

    #[test]
    fn rle_long_run_in_body_with_raw_tail() {
        // 1000 bytes of 4 -> body 996 bytes of 4 -> long run, then 4 raw.
        let plane = vec![4u8; 1000];
        let mut want = vec![4, 4, RLE_LONG_ESCAPE];
        want.extend_from_slice(&996u32.to_le_bytes());
        want.extend_from_slice(&[4, 4, 4, 4]);
        assert_eq!(rle_encode(&plane), want);
    }

    #[test]
    fn ycocg_white_is_white() {
        // White (255,255,255) should give Y near 254 and Co/Cg near 0.
        let (y, co, cg) = rgb_to_ycocg(255, 255, 255, 3);
        assert_eq!(y, 253);
        assert_eq!(co, 0);
        assert_eq!(cg, 0);
    }

    #[test]
    fn ycocg_black_is_zero() {
        let (y, co, cg) = rgb_to_ycocg(0, 0, 0, 3);
        assert_eq!(y, 0);
        assert_eq!(co, 0);
        assert_eq!(cg, 0);
    }
}
