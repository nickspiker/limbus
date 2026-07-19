//! limbus, a format gateway: any image format in, VSF-Image out. Camera RAW carries sensor truth ([`RawInfo`] + native-depth counts + both DNG matrices); creative formats (JPEG/PNG/TIFF, eventually PSD) enter as `assumed`-grade profiled code values; sources with no implied observer enter honestly uncharacterized. The VERICHROME stack is the first consumer (opsin translaterates and views, chameleon calibrates, neither touches a foreign format directly), but anything that wants VSF-Image enters the same way. Named for the cornea-sclera border ring where light crosses to enter the eye, bidirectional by intent (the DNG/TIFF writers migrate in from chameleon); today the RAW/DNG read side is wired and the VSF write still lives in opsin's convert path.
//!
//! Pixel-buffer policy:
//!   - Uncompressed strip DNG  → hand-rolled strip read + bit unpack.
//!   - Compressed and/or tiled → delegate the pixel buffer to `rawler`,
//!     used purely as a decompression black box. No rawler types or
//!     colour interpretation cross into the rest of the pipeline.
//!
//! Metadata is always read by the hand-rolled IFD parser so we keep exact control over which
//! `ColorMatrix1`/`ColorMatrix2` / `CalibrationIlluminant1`/`2` / black / white / CFA values flow
//! downstream; opsin needs both matrices + illuminant codes to build the tiered colour_profile.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::str;

/// TIFF declares its byte order in the header ("II" little-endian, "MM" big-endian) and every multi-byte value in the file follows it — IFD entries, inline values, rationals, and 16-bit samples alike. One reader, two orders: all multi-byte fetches go through these, keyed off the flag carried in [`RawInfo::bigendian`]. Inline values sit left-justified in the 4-byte field regardless of order, so the slice indices never change — only the byte interpretation does.
#[inline]
fn u16e(b: &[u8], be: bool) -> u16 {
    let a: [u8; 2] = b[..2].try_into().unwrap();
    if be { u16::from_be_bytes(a) } else { u16::from_le_bytes(a) }
}

#[inline]
fn u32e(b: &[u8], be: bool) -> u32 {
    let a: [u8; 4] = b[..4].try_into().unwrap();
    if be { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) }
}

#[inline]
fn i32e(b: &[u8], be: bool) -> i32 {
    let a: [u8; 4] = b[..4].try_into().unwrap();
    if be { i32::from_be_bytes(a) } else { i32::from_le_bytes(a) }
}

#[derive(Clone)]
pub struct RawInfo {
    pub make: String,
    pub makeoffset: u32,
    pub makelen: u32,
    pub model: String,
    pub modeloffset: u32,
    pub modellen: u32,
    pub width: usize,
    pub height: usize,
    pub bitdepth: u8,
    pub bitdepthold: u8,
    pub compression: bool,
    pub rgb: bool,
    pub cfa: Vec<u8>,
    pub cfaw: u16,
    pub cfah: u16,
    pub black: f32,
    pub blackoffset: u32,
    pub blackcount: u32,
    pub blacktype: u16,
    pub white: f32,
    /// EXIF Orientation (tag 274): codes 1–8; 9 = tag absent (out-of-band sentinel — valid codes stop at 8).
    pub orientation: u16,
    /// TIFF byte order: true = big-endian ("MM"), false = little-endian ("II"). Every offset stored in this struct must be re-read with this order.
    pub bigendian: bool,
    pub cam2terminal9: [f32; 9],
    pub magic9inv: [u8; 8 * 9],
    pub colourmatrix1: Option<[f32; 9]>,
    pub colourmatrix1_offset: u32,
    pub colourmatrix2: Option<[f32; 9]>,
    pub colourmatrix2_offset: u32,
    /// DNG CalibrationIlluminant1/2: EXIF LightSource code for the illuminant each ColorMatrix was measured under (17 = Standard A, 21 = D65, 23 = D50...). 0 = absent. Lets a consumer pick the matrix matching its assumed scene illuminant instead of guessing.
    pub calibrationilluminant1: u16,
    pub calibrationilluminant2: u16,
    pub magicoffset: u32,
    pub profileoffset: u32,
    pub curveoffset: u32,
    pub imagedataoffset: u32,
    pub ifdoffset: u32,
    pub duck: bool,
    pub save_scan: bool,
}

impl Default for RawInfo {
    fn default() -> Self {
        Self {
            make: String::new(),
            makeoffset: 0,
            makelen: 0,
            model: String::new(),
            modeloffset: 0,
            modellen: 0,
            width: 0,
            height: 0,
            bitdepth: 255,
            bitdepthold: 0,
            compression: false,
            rgb: false,
            cfa: Vec::new(),
            cfaw: 0,
            cfah: 0,
            black: 0.,
            blackoffset: 0,
            blackcount: 0,
            blacktype: 0,
            white: 65535.,
            orientation: 9,
            bigendian: false,
            cam2terminal9: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
            magic9inv: [0; 8 * 9],
            colourmatrix1: None,
            colourmatrix1_offset: 0,
            colourmatrix2: None,
            colourmatrix2_offset: 0,
            calibrationilluminant1: 0,
            calibrationilluminant2: 0,
            magicoffset: 0,
            profileoffset: 0,
            curveoffset: 0,
            imagedataoffset: 0,
            ifdoffset: 0,
            duck: false,
            save_scan: false,
        }
    }
}

/// Read a DNG into (metadata, pixel buffer).
///
/// Pixel buffer is u16 in the file's native bit depth (e.g. 14-bit
/// values stored in u16 for 14-bit raws). Length == `width * height`.
/// For uncompressed strip DNGs this is bit-exact with the previous
/// hand-rolled reader. For compressed/tiled DNGs the buffer comes from
/// rawler's lossless decoder.
pub fn read_dng(filename: &Path) -> Option<(RawInfo, Vec<u16>)> {
    let rawinfo = read_metadata(filename)?;
    let pixels = if !rawinfo.compression && rawinfo.imagedataoffset != 0 {
        read_strip_pixels(filename, &rawinfo)?
    } else {
        read_via_rawler(filename, &rawinfo)?
    };
    if pixels.len() != rawinfo.width * rawinfo.height {
        eprintln!(
            "limbus: pixel count {} != width*height {}*{}",
            pixels.len(),
            rawinfo.width,
            rawinfo.height
        );
        return None;
    }
    Some((rawinfo, pixels))
}

/// Walk IFDs, fill RawInfo. Does not read the pixel buffer.
/// Read a 9-entry SRATIONAL matrix (numerator/denominator i32 pairs) at `offset`. Shared by ColorMatrix1/2.
fn read_rational_matrix9(file: &mut File, offset: u32, be: bool) -> Option<[f32; 9]> {
    file.seek(SeekFrom::Start(offset as u64)).ok()?;
    let mut buffer = vec![0u8; 9 * 8];
    let s = file.read(&mut buffer).ok()?;
    if s != buffer.len() {
        return None;
    }
    let mut matrix = [0f32; 9];
    for i in 0..9 {
        let off = i * 8;
        let numerator = i32e(&buffer[off..off + 4], be);
        let denominator = i32e(&buffer[off + 4..off + 8], be);
        matrix[i] = numerator as f32 / denominator as f32;
    }
    Some(matrix)
}

fn read_metadata(filename: &Path) -> Option<RawInfo> {
    let mut rawinfo = RawInfo::default();
    let mut file = File::open(filename).ok()?;

    let mut buffer = [0u8; 8];
    file.seek(SeekFrom::Start(0)).ok()?;
    let s = file.read(&mut buffer).ok()?;
    if s != buffer.len() {
        return None;
    }
    let be = match &buffer[0..4] {
        [73, 73, 42, 0] => false, // "II" + 42 little-endian
        [77, 77, 0, 42] => true,  // "MM" + 42 big-endian
        _ => return None,
    };
    rawinfo.bigendian = be;
    let mut offset = u32e(&buffer[4..], be);

    let mut buf = vec![0u8; 2];
    file.seek(SeekFrom::Start(offset as u64)).ok()?;
    let s = file.read(&mut buf).ok()?;
    if s != buf.len() {
        return None;
    }
    offset += 2;
    let mut numifd = u16e(&buf, be);
    let mut entries = vec![0u8; numifd as usize * 12];
    file.seek(SeekFrom::Start(offset as u64)).ok()?;
    let s = file.read(&mut entries).ok()?;
    if s != entries.len() {
        return None;
    }

    let mut subifdoffset: u32;
    let mut imgoffset: u32;
    let mut trees: u32;
    let mut mainifd: bool;
    (rawinfo, subifdoffset, imgoffset, _, trees) = decode_ifd(numifd, entries, rawinfo, be);
    rawinfo.imagedataoffset = imgoffset;

    if trees == 1 {
        let mut buf = vec![0u8; 2];
        file.seek(SeekFrom::Start(subifdoffset as u64)).ok()?;
        let s = file.read(&mut buf).ok()?;
        if s != buf.len() {
            return None;
        }
        offset = subifdoffset + 2;
        let numifd = u16e(&buf, be);
        let mut entries = vec![0u8; numifd as usize * 12];
        file.seek(SeekFrom::Start(offset as u64)).ok()?;
        let s = file.read(&mut entries).ok()?;
        if s != entries.len() {
            return None;
        }
        (rawinfo, _, _, _, _) = decode_ifd(numifd, entries, rawinfo, be);
    } else {
        offset = subifdoffset;
        let mut treelist = vec![0u8; trees as usize * 4];
        file.seek(SeekFrom::Start(offset as u64)).ok()?;
        let s = file.read(&mut treelist).ok()?;
        if s != treelist.len() {
            return None;
        }
        for treecount in 0..trees {
            let mut buf = vec![0u8; 2];
            offset = u32e(&treelist[treecount as usize * 4..treecount as usize * 4 + 4], be);
            file.seek(SeekFrom::Start(offset as u64)).ok()?;
            let s = file.read(&mut buf).ok()?;
            if s != buf.len() {
                return None;
            }
            numifd = u16e(&buf, be);
            offset += 2;
            let mut entries = vec![0u8; numifd as usize * 12];
            file.seek(SeekFrom::Start(offset as u64)).ok()?;
            let s = file.read(&mut entries).ok()?;
            if s != entries.len() {
                return None;
            }
            #[allow(unused_assignments)]
            {
                (rawinfo, subifdoffset, imgoffset, mainifd, trees) =
                    decode_ifd(numifd, entries, rawinfo, be);
            }
            if mainifd {
                rawinfo.imagedataoffset = imgoffset;
            }
        }
    }

    if rawinfo.makeoffset != 0 {
        file.seek(SeekFrom::Start(rawinfo.makeoffset as u64)).ok()?;
        let mut buffer = vec![0u8; rawinfo.makelen as usize];
        let s = file.read(&mut buffer).ok()?;
        if s != buffer.len() {
            return None;
        }
        rawinfo.make = str::from_utf8(&buffer).ok()?.to_owned();
    }
    if rawinfo.modeloffset != 0 {
        file.seek(SeekFrom::Start(rawinfo.modeloffset as u64)).ok()?;
        let mut buffer = vec![0u8; rawinfo.modellen as usize];
        let s = file.read(&mut buffer).ok()?;
        if s != buffer.len() {
            return None;
        }
        rawinfo.model = str::from_utf8(&buffer).ok()?.to_owned();
    }

    if rawinfo.blackoffset != 0 {
        if rawinfo.blackcount == 4 {
            let mut blacks = [0f32; 4];
            if rawinfo.blacktype == 5 {
                let mut buffer = vec![0u8; rawinfo.blackcount as usize * 2 * 4];
                file.seek(SeekFrom::Start(rawinfo.blackoffset as u64))
                    .ok()?;
                let s = file.read(&mut buffer).ok()?;
                if s != buffer.len() {
                    return None;
                }
                for count in 0..rawinfo.blackcount as usize {
                    let numerator = i32e(&buffer[count * 2 * 4..(count * 2 + 1) * 4], rawinfo.bigendian);
                    let denominator = i32e(&buffer[(count * 2 + 1) * 4..(count * 2 + 2) * 4], rawinfo.bigendian);
                    blacks[count] = numerator as f32 / denominator as f32;
                }
                if blacks[0] != blacks[1] || blacks[1] != blacks[2] || blacks[2] != blacks[3] {
                    println!(
                        "Non-uniform black levels. This colour profile may not map properly."
                    );
                    rawinfo.black = blacks.iter().sum::<f32>() / rawinfo.blackcount as f32;
                } else {
                    rawinfo.black = blacks[0];
                }
            } else {
                return None;
            }
        } else {
            return None;
        }
    }

    if rawinfo.colourmatrix1_offset != 0 {
        rawinfo.colourmatrix1 = read_rational_matrix9(&mut file, rawinfo.colourmatrix1_offset, be);
    }
    if rawinfo.colourmatrix2_offset != 0 {
        rawinfo.colourmatrix2 = read_rational_matrix9(&mut file, rawinfo.colourmatrix2_offset, be);
    }

    rawinfo.white = rawinfo
        .white
        .min((2usize.pow(rawinfo.bitdepthold as u32) - 1) as f32);

    if rawinfo.width == 0 || rawinfo.height == 0 || rawinfo.bitdepthold == 0 {
        return None;
    }

    rawinfo.bitdepth = ((rawinfo.white + 1.).log2().ceil() as u8).min(rawinfo.bitdepthold);
    Some(rawinfo)
}

fn read_strip_pixels(filename: &Path, info: &RawInfo) -> Option<Vec<u16>> {
    let mut file = File::open(filename).ok()?;
    let bytes_len = (info.width * info.height * info.bitdepthold as usize - 1) / 8 + 1;
    let mut img = vec![0u8; bytes_len];
    file.seek(SeekFrom::Start(info.imagedataoffset as u64))
        .ok()?;
    let s = file.read(&mut img).ok()?;
    if s != img.len() {
        return None;
    }
    Some(convert_raw_to_u16(&img, info.bitdepthold, info.bigendian))
}

fn read_via_rawler(filename: &Path, info: &RawInfo) -> Option<Vec<u16>> {
    let raw = match rawler::decode_file(filename) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rawler failed to decode {}: {}", filename.display(), e);
            return None;
        }
    };
    let pixels = match raw.data {
        rawler::RawImageData::Integer(v) => v,
        rawler::RawImageData::Float(_) => {
            eprintln!("rawler returned float data; expected integer u16");
            return None;
        }
    };
    if raw.width != info.width || raw.height != info.height {
        eprintln!(
            "rawler dimensions {}×{} disagree with parsed metadata {}×{}",
            raw.width, raw.height, info.width, info.height
        );
        return None;
    }
    Some(pixels)
}

fn decode_ifd(
    numifd: u16,
    buffer: Vec<u8>,
    mut rawinfo: RawInfo,
    be: bool,
) -> (RawInfo, u32, u32, bool, u32) {
    let mut subifdoffset: u32 = 0;
    let mut imgoffset: u32 = 0;
    let mut mainifd = false;
    let mut tagid: usize;
    let mut fieldtype: u16;
    let mut numval: u32;
    let mut valueoffset: [u8; 4];
    let mut trees: u32 = 0;
    let mut compression = false;

    for ifd in 0..numifd as usize {
        tagid = u16e(&buffer[ifd * 12..], be) as usize;
        fieldtype = u16e(&buffer[ifd * 12 + 2..], be);
        numval = u32e(&buffer[ifd * 12 + 4..], be);
        valueoffset = buffer[ifd * 12 + 8..][..4].try_into().unwrap();

        match tagid {
            254 => {
                if fieldtype == 4 && numval == 1 && u32e(&valueoffset, be) == 0 {
                    mainifd = true;
                }
            }
            256 => {
                if numval == 1 && mainifd {
                    if fieldtype == 3 {
                        rawinfo.width = u16e(&valueoffset, be) as usize;
                    } else if fieldtype == 4 {
                        rawinfo.width = u32e(&valueoffset, be) as usize;
                    }
                }
            }
            257 => {
                if numval == 1 && mainifd {
                    if fieldtype == 3 {
                        rawinfo.height = u16e(&valueoffset, be) as usize;
                    } else if fieldtype == 4 {
                        rawinfo.height = u32e(&valueoffset, be) as usize;
                    }
                }
            }
            258 => {
                if fieldtype == 3 && numval == 1 && (rawinfo.bitdepth == 0 || mainifd) {
                    rawinfo.bitdepthold = u16e(&valueoffset, be) as u8;
                }
            }
            259 => {
                if fieldtype == 3 && numval == 1 && u16e(&valueoffset, be) != 1 {
                    compression = true;
                }
            }
            262 => {
                if fieldtype == 3 && numval == 1 && u16e(&valueoffset, be) == 32803 {}
            }
            271 => {
                if fieldtype == 2 && rawinfo.makeoffset == 0 {
                    rawinfo.makelen = numval - 1;
                    if numval > 4 {
                        rawinfo.makeoffset = u32e(&valueoffset, be);
                    } else if let Ok(s) = String::from_utf8(valueoffset[0..numval as usize].to_vec())
                    {
                        rawinfo.make = s;
                    }
                }
            }
            272 => {
                if fieldtype == 2 && rawinfo.modeloffset == 0 {
                    rawinfo.modellen = numval - 1;
                    if numval > 4 {
                        rawinfo.modeloffset = u32e(&valueoffset, be);
                    } else if let Ok(s) = String::from_utf8(valueoffset[0..numval as usize].to_vec())
                    {
                        rawinfo.model = s;
                    }
                }
            }
            273 => {
                if fieldtype == 4 && numval == 1 {
                    imgoffset = u32e(&valueoffset, be);
                }
            }
            274 => {
                if fieldtype == 3 && numval == 1 {
                    rawinfo.orientation = u16e(&valueoffset, be);
                }
            }
            277 => {
                if fieldtype == 3 && numval == 1 && u16e(&valueoffset, be) == 1 {}
            }
            284 => {
                if fieldtype == 3 && numval == 1 && u16e(&valueoffset, be) == 1 {}
            }
            330 => {
                if fieldtype == 4 {
                    trees = numval;
                    subifdoffset = u32e(&valueoffset, be);
                }
            }
            33421 => {
                if fieldtype == 3 && numval == 2 {
                    rawinfo.cfah = u16e(&valueoffset[0..2], be);
                    rawinfo.cfaw = u16e(&valueoffset[2..4], be);
                }
            }
            33422 => {
                if fieldtype == 1 && numval == 4 {
                    rawinfo.cfa = Vec::from(valueoffset);
                }
            }
            50714 => {
                if numval == 1 {
                    if fieldtype == 3 {
                        rawinfo.black = u16e(&valueoffset, be) as f32;
                    } else if fieldtype == 4 {
                        rawinfo.black = u32e(&valueoffset, be) as f32;
                    }
                } else {
                    rawinfo.blackcount = numval;
                    rawinfo.blacktype = fieldtype;
                    rawinfo.blackoffset = u32e(&valueoffset, be);
                }
            }
            50717 => {
                if numval == 1 {
                    if fieldtype == 3 {
                        rawinfo.white = u16e(&valueoffset, be) as f32;
                    } else if fieldtype == 4 {
                        rawinfo.white = u32e(&valueoffset, be) as f32;
                    }
                }
            }
            50721 => {
                if (fieldtype == 5 || fieldtype == 10) && numval == 9 {
                    rawinfo.colourmatrix1_offset = u32e(&valueoffset, be);
                }
            }
            50722 => {
                if (fieldtype == 5 || fieldtype == 10) && numval == 9 {
                    rawinfo.colourmatrix2_offset = u32e(&valueoffset, be);
                }
            }
            // CalibrationIlluminant1/2: SHORT, value inline in the offset field.
            50778 => {
                if fieldtype == 3 && numval == 1 {
                    rawinfo.calibrationilluminant1 = u16e(&valueoffset, be);
                }
            }
            50779 => {
                if fieldtype == 3 && numval == 1 {
                    rawinfo.calibrationilluminant2 = u16e(&valueoffset, be);
                }
            }
            _ => {}
        }
    }
    if mainifd {
        rawinfo.compression = compression;
    }
    (rawinfo, subifdoffset, imgoffset, mainifd, trees)
}

/// Unpack the strip bitstream to u16 samples. 16-bit samples follow the file's byte order; everything below is MSB-first per TIFF FillOrder 1 (default) in either order, extracted through a 4-byte big-endian window — intra-byte offset ≤ 7 plus ≤ 15 bits stays under 32, so it's one shift + one mask per sample at ANY depth (10/12/14 and every intermediary alike, no per-depth cases). The window straddles the tail only for the last ≤ 3 samples, which take the zero-padded slow assembly. Every sample's bit offset is computable from its index alone, so the pass splits across the rayon pool (already in the tree via rawler).
fn convert_raw_to_u16(raw_bytes: &[u8], bitdepth: u8, be: bool) -> Vec<u16> {
    use rayon::prelude::*;
    if bitdepth == 16 {
        return raw_bytes.par_chunks_exact(2).map(|chunk| u16e(chunk, be)).collect();
    }
    let bits = bitdepth as usize;
    let mask = ((1u32 << bits) - 1) as u16;
    let num_pixels = raw_bytes.len() * 8 / bits;
    (0..num_pixels)
        .into_par_iter()
        .map(|i| {
            let bit_offset = i * bits;
            let byte = bit_offset >> 3;
            let intra = bit_offset & 7;
            let window = if byte + 4 <= raw_bytes.len() {
                u32::from_be_bytes(raw_bytes[byte..byte + 4].try_into().unwrap())
            } else {
                let mut b = [0u8; 4];
                for (j, slot) in b.iter_mut().enumerate() {
                    if byte + j < raw_bytes.len() {
                        *slot = raw_bytes[byte + j];
                    }
                }
                u32::from_be_bytes(b)
            };
            ((window >> (32 - bits - intra)) as u16) & mask
        })
        .collect()
}

/// Invert a 3x3 matrix stored as [f32; 9] (row-major).
pub fn invert_magic_9(m: &[f32]) -> [f32; 9] {
    let d = 1.
        / (m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
            + m[2] * (m[3] * m[7] - m[4] * m[6]));
    [
        (m[4] * m[8] - m[5] * m[7]) * d,
        (m[2] * m[7] - m[1] * m[8]) * d,
        (m[1] * m[5] - m[2] * m[4]) * d,
        (m[5] * m[6] - m[3] * m[8]) * d,
        (m[0] * m[8] - m[2] * m[6]) * d,
        (m[2] * m[3] - m[0] * m[5]) * d,
        (m[3] * m[7] - m[4] * m[6]) * d,
        (m[1] * m[6] - m[0] * m[7]) * d,
        (m[0] * m[4] - m[1] * m[3]) * d,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn p16(v: u16, be: bool) -> [u8; 2] {
        if be { v.to_be_bytes() } else { v.to_le_bytes() }
    }
    fn p32(v: u32, be: bool) -> [u8; 4] {
        if be { v.to_be_bytes() } else { v.to_le_bytes() }
    }

    /// One 12-byte IFD entry: tag, type, count in file order; `val4` already laid out (inline values left-justified per spec).
    fn entry(out: &mut Vec<u8>, tag: u16, typ: u16, count: u32, val4: [u8; 4], be: bool) {
        out.extend_from_slice(&p16(tag, be));
        out.extend_from_slice(&p16(typ, be));
        out.extend_from_slice(&p32(count, be));
        out.extend_from_slice(&val4);
    }

    /// Inline SHORT value: left-justified in the 4-byte field, file byte order, trailing pad zero.
    fn short4(v: u16, be: bool) -> [u8; 4] {
        let b = p16(v, be);
        [b[0], b[1], 0, 0]
    }

    /// Minimal single-IFD uncompressed 2×2 16-bit file in either byte order: same logical content, twin layouts.
    fn synth_tiff(be: bool) -> Vec<u8> {
        const NTAGS: u16 = 8;
        let pixel_offset: u32 = 8 + 2 + NTAGS as u32 * 12 + 4;
        let mut f = Vec::new();
        f.extend_from_slice(if be { &[77, 77, 0, 42] } else { &[73, 73, 42, 0] });
        f.extend_from_slice(&p32(8, be));
        f.extend_from_slice(&p16(NTAGS, be));
        entry(&mut f, 254, 4, 1, p32(0, be), be); // NewSubfileType 0 ⇒ main IFD
        entry(&mut f, 256, 3, 1, short4(2, be), be); // width
        entry(&mut f, 257, 3, 1, short4(2, be), be); // height
        entry(&mut f, 258, 3, 1, short4(16, be), be); // bits per sample
        entry(&mut f, 259, 3, 1, short4(1, be), be); // uncompressed
        entry(&mut f, 273, 4, 1, p32(pixel_offset, be), be); // strip offset
        entry(&mut f, 274, 3, 1, short4(6, be), be); // orientation: rotate 90 CW
        entry(&mut f, 50717, 3, 1, short4(60000, be), be); // white level
        f.extend_from_slice(&p32(0, be)); // no next IFD
        for v in [100u16, 200, 300, 400] {
            f.extend_from_slice(&p16(v, be));
        }
        f
    }

    fn read_synth(be: bool, name: &str) -> (RawInfo, Vec<u16>) {
        let path = std::env::temp_dir().join(name);
        File::create(&path).unwrap().write_all(&synth_tiff(be)).unwrap();
        let out = read_dng(&path).expect("synthetic file should decode");
        std::fs::remove_file(&path).ok();
        out
    }

    #[test]
    fn little_and_big_endian_parse_identically() {
        let (le_info, le_px) = read_synth(false, "limbus_synth_le.tif");
        let (be_info, be_px) = read_synth(true, "limbus_synth_be.tif");
        for (info, px, big) in [(&le_info, &le_px, false), (&be_info, &be_px, true)] {
            assert_eq!(info.bigendian, big);
            assert_eq!((info.width, info.height), (2, 2));
            assert_eq!(info.bitdepthold, 16);
            assert_eq!(info.orientation, 6);
            assert_eq!(info.white, 60000.);
            assert!(!info.compression);
            assert_eq!(px.as_slice(), &[100, 200, 300, 400]);
        }
    }

    #[test]
    fn orientation_defaults_to_absent_sentinel() {
        assert_eq!(RawInfo::default().orientation, 9);
    }

    #[test]
    fn window_unpack_matches_hand_built_bitstreams() {
        // MSB-first streams built by hand for the camera depths + an odd intermediary.
        assert_eq!(convert_raw_to_u16(&[0xAB, 0xC1, 0x23], 12, false), vec![0xABC, 0x123]);
        assert_eq!(convert_raw_to_u16(&[0xFF, 0xFC, 0x00, 0x10], 14, false), vec![0x3FFF, 0x0001]);
        assert_eq!(convert_raw_to_u16(&[0xFF, 0xD5, 0x50], 10, false), vec![0x3FF, 0x155]);
        // 11-bit: 11111111111 00000000001 → 11111111 11100000 000001(00) — tail sample straddles the padded window.
        assert_eq!(convert_raw_to_u16(&[0xFF, 0xE0, 0x04], 11, false), vec![0x7FF, 0x001]);
        // 16-bit obeys the file byte order, not the bitstream rule.
        assert_eq!(convert_raw_to_u16(&[0x12, 0x34], 16, false), vec![0x3412]);
        assert_eq!(convert_raw_to_u16(&[0x12, 0x34], 16, true), vec![0x1234]);
    }
}

/// Multiply two 3x3 matrices: result = a × b.
pub fn multiply_matrices_3x3(a: &[f32; 9], b: &[f32; 9]) -> [f32; 9] {
    [
        a[0] * b[0] + a[1] * b[3] + a[2] * b[6],
        a[0] * b[1] + a[1] * b[4] + a[2] * b[7],
        a[0] * b[2] + a[1] * b[5] + a[2] * b[8],
        a[3] * b[0] + a[4] * b[3] + a[5] * b[6],
        a[3] * b[1] + a[4] * b[4] + a[5] * b[7],
        a[3] * b[2] + a[4] * b[5] + a[5] * b[8],
        a[6] * b[0] + a[7] * b[3] + a[8] * b[6],
        a[6] * b[1] + a[7] * b[4] + a[8] * b[7],
        a[6] * b[2] + a[7] * b[5] + a[8] * b[8],
    ]
}
