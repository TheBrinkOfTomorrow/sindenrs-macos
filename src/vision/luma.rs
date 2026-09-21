//! Getting a luma plane out of what the camera delivers.

use std::io::Write;

/// Extract the Y plane from packed YUYV (`Y0 U0 Y1 V0 ...`). Scalar; a SIMD path can replace
/// this once it shows up in a profile.
pub fn yuyv_to_luma(src: &[u8], dst: &mut Vec<u8>) {
    dst.clear();
    dst.extend(src.iter().step_by(2));
}

/// Decode an MJPEG frame straight to 8-bit luma (no chroma upsampling, no colour conversion).
pub fn mjpeg_to_luma(jpeg: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    let opts = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::Luma)
        .set_strict_mode(false);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(jpeg), opts);
    let pixels = dec.decode().map_err(|e| e.to_string())?;
    let info = dec.info().ok_or("no image info after decode")?;
    Ok((u32::from(info.width), u32::from(info.height), pixels))
}

/// Write an 8-bit grayscale image as binary PGM.
pub fn write_pgm(
    path: &std::path::Path,
    width: u32,
    height: u32,
    luma: &[u8],
) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(f, "P5\n{width} {height}\n255\n")?;
    f.write_all(luma)?;
    f.flush()
}

/// Mean and max of a luma plane; a cheap exposure sanity check.
pub fn luma_stats(luma: &[u8]) -> (f64, u8) {
    if luma.is_empty() {
        return (0.0, 0);
    }
    let sum: u64 = luma.iter().map(|&v| u64::from(v)).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum as f64 / luma.len() as f64;
    (mean, luma.iter().copied().max().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_keeps_even_bytes() {
        let src = [10, 128, 20, 128, 30, 128, 40, 128];
        let mut dst = Vec::new();
        yuyv_to_luma(&src, &mut dst);
        assert_eq!(dst, [10, 20, 30, 40]);
        assert_eq!(luma_stats(&dst), (25.0, 40));
    }
}
