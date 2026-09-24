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

/// Luma from a binary (P5) 8-bit PGM, as [`write_pgm`] writes it.
pub fn pgm_to_luma(pgm: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    // Header: magic, width, height, maxval, each separated by whitespace, then one whitespace
    // byte before the pixels. Comments are not written by us and not supported.
    let mut fields = Vec::with_capacity(4);
    let mut i = 0;
    while fields.len() < 4 {
        while pgm.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        let start = i;
        while pgm.get(i).is_some_and(|b| !b.is_ascii_whitespace()) {
            i += 1;
        }
        if start == i {
            return Err("truncated PGM header".into());
        }
        fields.push(std::str::from_utf8(&pgm[start..i]).map_err(|e| e.to_string())?);
    }
    if fields[0] != "P5" || fields[3] != "255" {
        return Err(format!(
            "not an 8-bit binary PGM ({} maxval {})",
            fields[0], fields[3]
        ));
    }
    let parse = |s: &str| s.parse::<u32>().map_err(|e| format!("PGM size {s:?}: {e}"));
    let (w, h) = (parse(fields[1])?, parse(fields[2])?);
    let pixels = &pgm[(i + 1).min(pgm.len())..];
    let n = w as usize * h as usize;
    if pixels.len() < n {
        return Err(format!("PGM has {} of {n} pixels", pixels.len()));
    }
    Ok((w, h, pixels[..n].to_vec()))
}

/// Luma from a recorded frame file's bytes, by extension: `.pgm` (macOS) or MJPEG.
pub fn frame_to_luma(path: &std::path::Path, data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if path.extension().is_some_and(|e| e == "pgm") {
        pgm_to_luma(data)
    } else {
        mjpeg_to_luma(data)
    }
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
    #[test]
    fn pgm_round_trips() {
        let dir = std::env::temp_dir().join(format!("sindenrs-pgm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("f.pgm");
        let luma: Vec<u8> = (0..12).collect();
        super::write_pgm(&path, 4, 3, &luma).expect("write");
        let data = std::fs::read(&path).expect("read");
        assert_eq!(super::frame_to_luma(&path, &data), Ok((4, 3, luma)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pgm_rejects_bad_input() {
        assert!(super::pgm_to_luma(b"P6\n2 2\n255\n0000").is_err());
        assert!(super::pgm_to_luma(b"P5\n2 2\n255\n00").is_err());
        assert!(super::pgm_to_luma(b"P5\n2").is_err());
    }

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
