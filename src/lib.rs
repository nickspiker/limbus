//! iris, a format gateway: any image format in, VSF-Image out. Camera RAW carries sensor truth ([`RawInfo`] + native-depth counts + both DNG matrices); creative formats (JPEG/PNG/TIFF, eventually PSD) enter as `assumed`-grade profiled code values; sources with no implied observer enter honestly uncharacterized. The VERICHROME stack is the first consumer (opsin translaterates and views, chameleon calibrates, neither touches a foreign format directly), but anything that wants VSF-Image enters the same way. Named for the aperture light passes through, bidirectional by intent (the DNG/TIFF writers migrate in from chameleon); today the RAW/DNG read side is wired and the VSF write still lives in opsin's convert path.
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

use bitvec::prelude::*;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::str;

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
    pub orientation: u16,
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
            "iris: pixel count {} != width*height {}*{}",
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
fn read_rational_matrix9(file: &mut File, offset: u32) -> Option<[f32; 9]> {
    file.seek(SeekFrom::Start(offset as u64)).ok()?;
    let mut buffer = vec![0u8; 9 * 8];
    let s = file.read(&mut buffer).ok()?;
    if s != buffer.len() {
        return None;
    }
    let mut matrix = [0f32; 9];
    for i in 0..9 {
        let off = i * 8;
        let numerator = i32::from_le_bytes(buffer[off..off + 4].try_into().ok()?);
        let denominator = i32::from_le_bytes(buffer[off + 4..off + 8].try_into().ok()?);
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
    if s != buffer.len() || &buffer[0..4] != &[73, 73, 42, 0] {
        return None;
    }
    let mut offset = u32::from_le_bytes(buffer[4..].try_into().unwrap());

    let mut buf = vec![0u8; 2];
    file.seek(SeekFrom::Start(offset as u64)).ok()?;
    let s = file.read(&mut buf).ok()?;
    if s != buf.len() {
        return None;
    }
    offset += 2;
    let mut numifd = u16::from_le_bytes([buf[0], buf[1]]);
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
    (rawinfo, subifdoffset, imgoffset, _, trees) = decode_ifd(numifd, entries, rawinfo);
    rawinfo.imagedataoffset = imgoffset;

    if trees == 1 {
        let mut buf = vec![0u8; 2];
        file.seek(SeekFrom::Start(subifdoffset as u64)).ok()?;
        let s = file.read(&mut buf).ok()?;
        if s != buf.len() {
            return None;
        }
        offset = subifdoffset + 2;
        let numifd = u16::from_le_bytes([buf[0], buf[1]]);
        let mut entries = vec![0u8; numifd as usize * 12];
        file.seek(SeekFrom::Start(offset as u64)).ok()?;
        let s = file.read(&mut entries).ok()?;
        if s != entries.len() {
            return None;
        }
        (rawinfo, _, _, _, _) = decode_ifd(numifd, entries, rawinfo);
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
            offset = u32::from_le_bytes(
                treelist[treecount as usize * 4..treecount as usize * 4 + 4]
                    .try_into()
                    .ok()?,
            );
            file.seek(SeekFrom::Start(offset as u64)).ok()?;
            let s = file.read(&mut buf).ok()?;
            if s != buf.len() {
                return None;
            }
            numifd = u16::from_le_bytes([buf[0], buf[1]]);
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
                    decode_ifd(numifd, entries, rawinfo);
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
                    let numerator = i32::from_le_bytes(
                        buffer[count * 2 * 4..(count * 2 + 1) * 4]
                            .try_into()
                            .ok()?,
                    );
                    let denominator = i32::from_le_bytes(
                        buffer[(count * 2 + 1) * 4..(count * 2 + 2) * 4]
                            .try_into()
                            .ok()?,
                    );
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
        rawinfo.colourmatrix1 = read_rational_matrix9(&mut file, rawinfo.colourmatrix1_offset);
    }
    if rawinfo.colourmatrix2_offset != 0 {
        rawinfo.colourmatrix2 = read_rational_matrix9(&mut file, rawinfo.colourmatrix2_offset);
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
    Some(convert_raw_to_u16(&img, info.bitdepthold))
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
        tagid = buffer[ifd * 12] as usize + ((buffer[ifd * 12 + 1] as usize) << 8);
        fieldtype = u16::from_le_bytes(buffer[ifd * 12 + 2..][..2].try_into().unwrap());
        numval = u32::from_le_bytes(buffer[ifd * 12 + 4..][..4].try_into().unwrap());
        valueoffset = buffer[ifd * 12 + 8..][..4].try_into().unwrap();

        match tagid {
            254 => {
                if fieldtype == 4 && numval == 1 && u32::from_le_bytes(valueoffset) == 0 {
                    mainifd = true;
                }
            }
            256 => {
                if numval == 1 && mainifd {
                    if fieldtype == 3 {
                        rawinfo.width =
                            u16::from_le_bytes(valueoffset[0..2].try_into().unwrap()) as usize;
                    } else if fieldtype == 4 {
                        rawinfo.width = u32::from_le_bytes(valueoffset) as usize;
                    }
                }
            }
            257 => {
                if numval == 1 && mainifd {
                    if fieldtype == 3 {
                        rawinfo.height =
                            u16::from_le_bytes(valueoffset[0..2].try_into().unwrap()) as usize;
                    } else if fieldtype == 4 {
                        rawinfo.height = u32::from_le_bytes(valueoffset) as usize;
                    }
                }
            }
            258 => {
                if fieldtype == 3 && numval == 1 && (rawinfo.bitdepth == 0 || mainifd) {
                    rawinfo.bitdepthold = valueoffset[0];
                }
            }
            259 => {
                if fieldtype == 3 && numval == 1 && valueoffset[0] != 1 {
                    compression = true;
                }
            }
            262 => {
                if fieldtype == 3 && numval == 1 && u32::from_le_bytes(valueoffset) == 32803 {}
            }
            271 => {
                if fieldtype == 2 && rawinfo.makeoffset == 0 {
                    rawinfo.makelen = numval - 1;
                    if numval > 4 {
                        rawinfo.makeoffset = u32::from_le_bytes(valueoffset);
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
                        rawinfo.modeloffset = u32::from_le_bytes(valueoffset);
                    } else if let Ok(s) = String::from_utf8(valueoffset[0..numval as usize].to_vec())
                    {
                        rawinfo.model = s;
                    }
                }
            }
            273 => {
                if fieldtype == 4 && numval == 1 {
                    imgoffset = u32::from_le_bytes(valueoffset);
                }
            }
            274 => {
                if fieldtype == 3 && numval == 1 {
                    rawinfo.orientation = u16::from_le_bytes(valueoffset[0..2].try_into().unwrap());
                }
            }
            277 => {
                if fieldtype == 3 && numval == 1 && valueoffset[0] == 1 {}
            }
            284 => {
                if fieldtype == 3 && numval == 1 && valueoffset[0] == 1 {}
            }
            330 => {
                if fieldtype == 4 {
                    trees = numval;
                    subifdoffset = u32::from_le_bytes(valueoffset);
                }
            }
            33421 => {
                if fieldtype == 3 && numval == 2 {
                    rawinfo.cfah = u16::from_le_bytes(valueoffset[0..2].try_into().unwrap());
                    rawinfo.cfaw = u16::from_le_bytes(valueoffset[2..4].try_into().unwrap());
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
                        rawinfo.black =
                            u16::from_le_bytes(valueoffset[0..2].try_into().unwrap()) as f32;
                    } else if fieldtype == 4 {
                        rawinfo.black = u32::from_le_bytes(valueoffset) as f32;
                    }
                } else {
                    rawinfo.blackcount = numval;
                    rawinfo.blacktype = fieldtype;
                    rawinfo.blackoffset = u32::from_le_bytes(valueoffset);
                }
            }
            50717 => {
                if numval == 1 {
                    if fieldtype == 3 {
                        rawinfo.white =
                            u16::from_le_bytes(valueoffset[0..2].try_into().unwrap()) as f32;
                    } else if fieldtype == 4 {
                        rawinfo.white = u32::from_le_bytes(valueoffset) as f32;
                    }
                }
            }
            50721 => {
                if (fieldtype == 5 || fieldtype == 10) && numval == 9 {
                    rawinfo.colourmatrix1_offset = u32::from_le_bytes(valueoffset);
                }
            }
            50722 => {
                if (fieldtype == 5 || fieldtype == 10) && numval == 9 {
                    rawinfo.colourmatrix2_offset = u32::from_le_bytes(valueoffset);
                }
            }
            // CalibrationIlluminant1/2: SHORT, value inline in the offset field.
            50778 => {
                if fieldtype == 3 && numval == 1 {
                    rawinfo.calibrationilluminant1 = u16::from_le_bytes(valueoffset[0..2].try_into().unwrap());
                }
            }
            50779 => {
                if fieldtype == 3 && numval == 1 {
                    rawinfo.calibrationilluminant2 = u16::from_le_bytes(valueoffset[0..2].try_into().unwrap());
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

fn convert_raw_to_u16(raw_bytes: &[u8], bitdepth: u8) -> Vec<u16> {
    let mut result = Vec::new();
    if bitdepth == 16 {
        for chunk in raw_bytes.chunks_exact(2) {
            result.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }
    } else {
        let bits = raw_bytes.view_bits::<Msb0>();
        let num_pixels = bits.len() / bitdepth as usize;
        for i in 0..num_pixels {
            let start = i * bitdepth as usize;
            let end = start + bitdepth as usize;
            if end <= bits.len() {
                let mut value = 0u16;
                for (j, bit) in bits[start..end].iter().enumerate() {
                    if *bit {
                        value |= 1 << (bitdepth as usize - 1 - j);
                    }
                }
                result.push(value);
            }
        }
    }
    result
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
