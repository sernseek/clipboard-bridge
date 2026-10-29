//! Device-independent bitmaps (CF_DIB, CF_DIBV5) to PNG and back, for apps
//! that put or take images on the Windows clipboard only as bitmaps.

use std::io::Cursor;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
/// BGRA, the only channel order bitmaps on the clipboard use in practice.
const MASKS: [u32; 3] = [0x00ff_0000, 0x0000_ff00, 0x0000_00ff];

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// 24- and 32-bit bitmaps; a 32-bit one whose alpha is all zero is opaque.
pub fn to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let header = u32_at(dib, 0)? as usize;
    let width = u32_at(dib, 4)? as i32;
    let height = u32_at(dib, 8)? as i32;
    let bits = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?);
    let compression = u32_at(dib, 16)?;
    let colors = u32_at(dib, 32)? as usize;
    if header < 40 || width <= 0 || height == 0 {
        return None;
    }
    let mut offset = header + colors * 4;
    match (bits, compression) {
        (24 | 32, BI_RGB) => {}
        (32, BI_BITFIELDS) => {
            // In the V4/V5 header, or right after a plain one.
            let at = if header >= 52 { 40 } else { header };
            if header < 52 {
                offset += 12;
            }
            let masks = [u32_at(dib, at)?, u32_at(dib, at + 4)?, u32_at(dib, at + 8)?];
            if masks != MASKS {
                return None;
            }
        }
        _ => return None,
    }
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let px = usize::from(bits / 8);
    let stride = (w * px).div_ceil(4) * 4;
    let data = dib.get(offset..offset + stride * h)?;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let src = if height < 0 { row } else { h - 1 - row };
        for p in data[src * stride..][..w * px].chunks_exact(px) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], if px == 4 { p[3] } else { 255 }]);
        }
    }
    if px == 4 && rgba.chunks_exact(4).all(|p| p[3] == 0) {
        rgba.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&rgba).ok()?;
    writer.finish().ok()?;
    Some(out)
}

/// A bottom-up 32-bit BI_RGB bitmap (CF_DIB).
pub fn from_png(png: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let px = info.color_type.samples();
    let mut dib = Vec::with_capacity(40 + w * h * 4);
    for v in [40, w as u32, h as u32] {
        dib.extend_from_slice(&v.to_le_bytes());
    }
    dib.extend_from_slice(&1u16.to_le_bytes()); // planes
    dib.extend_from_slice(&32u16.to_le_bytes());
    for v in [BI_RGB, (w * h * 4) as u32, 0, 0, 0, 0] {
        dib.extend_from_slice(&v.to_le_bytes());
    }
    for row in (0..h).rev() {
        for p in buf[row * info.line_size..][..w * px].chunks_exact(px) {
            let (r, g, b, a) = match *p {
                [r, g, b, a] => (r, g, b, a),
                [r, g, b] => (r, g, b, 255),
                [l, a] => (l, l, l, a),
                [l] => (l, l, l, 255),
                _ => return None,
            };
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        // 3x2 RGB with an odd width, so rows need padding as 24-bit.
        let mut rgb = Vec::new();
        for i in 0..6u8 {
            rgb.extend_from_slice(&[i * 40, 255 - i * 40, i]);
        }
        let mut png_bytes = Vec::new();
        let mut enc = png::Encoder::new(&mut png_bytes, 3, 2);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&rgb).unwrap();
        w.finish().unwrap();

        let dib = from_png(&png_bytes).unwrap();
        assert_eq!(dib.len(), 40 + 3 * 2 * 4);
        // Bottom-up: the first stored pixel is the bottom-left one (index 3).
        assert_eq!(&dib[40..44], &[3, 255 - 120, 120, 255]);

        let back = to_png(&dib).unwrap();
        let mut dec = png::Decoder::new(Cursor::new(back)).read_info().unwrap();
        let mut buf = vec![0; dec.output_buffer_size().unwrap()];
        dec.next_frame(&mut buf).unwrap();
        let expect: Vec<u8> = rgb
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect();
        assert_eq!(buf, expect);
    }

    #[test]
    fn zero_alpha_bitfields_are_opaque() {
        // 1x1, plain header, BI_BITFIELDS with the masks after it.
        let mut dib = Vec::new();
        for v in [40u32, 1, 1] {
            dib.extend_from_slice(&v.to_le_bytes());
        }
        dib.extend_from_slice(&[1, 0, 32, 0]);
        for v in [BI_BITFIELDS, 4, 0, 0, 0, 0] {
            dib.extend_from_slice(&v.to_le_bytes());
        }
        for m in MASKS {
            dib.extend_from_slice(&m.to_le_bytes());
        }
        dib.extend_from_slice(&[10, 20, 30, 0]);
        let png_bytes = to_png(&dib).unwrap();
        let mut dec = png::Decoder::new(Cursor::new(png_bytes))
            .read_info()
            .unwrap();
        let mut buf = vec![0; dec.output_buffer_size().unwrap()];
        dec.next_frame(&mut buf).unwrap();
        assert_eq!(buf, vec![30, 20, 10, 255]);
    }

    #[test]
    fn truncated_bitmaps_are_rejected() {
        assert_eq!(to_png(&[40, 0, 0]), None);
        let mut dib = vec![0; 40];
        dib[0] = 40;
        dib[4] = 4;
        dib[8] = 4;
        dib[14] = 32;
        assert_eq!(to_png(&dib), None);
    }
}
