//! PICA200 texture decoding.

use crate::format::{morton_interleave, morton_offset};

/// A PICA texture format, straight from GPUREG_TEXUNIT0_TYPE and the
/// equivalent registers for units 1/2. The numbering is fixed by hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextureFormat {
    Rgba8,
    Rgb8,
    Rgba5551,
    Rgb565,
    Rgba4,
    /// luminance-alpha, 8 bits each.
    La8,
    /// two-component 8-bit, used for normal maps. Decoded as (0, g, r, 1).
    Hilo8,
    /// luminance only, 8 bit.
    L8,
    /// Alpha only, 8 bit.
    A8,
    /// luminance-alpha, 4 bits each.
    La4,
    /// luminance only, 4 bit.
    L4,
    /// Alpha only, 4 bit.
    A4,
    Etc1,
    Etc1A4,
    Unknown(u32),
}

impl TextureFormat {
    pub fn from_raw(value: u32) -> TextureFormat {
        match value {
            0 => TextureFormat::Rgba8,
            1 => TextureFormat::Rgb8,
            2 => TextureFormat::Rgba5551,
            3 => TextureFormat::Rgb565,
            4 => TextureFormat::Rgba4,
            5 => TextureFormat::La8,
            6 => TextureFormat::Hilo8,
            7 => TextureFormat::L8,
            8 => TextureFormat::A8,
            9 => TextureFormat::La4,
            10 => TextureFormat::L4,
            11 => TextureFormat::A4,
            12 => TextureFormat::Etc1,
            13 => TextureFormat::Etc1A4,
            other => TextureFormat::Unknown(other),
        }
    }

    /// bits per pixel.
    pub fn bits_per_pixel(self) -> u32 {
        match self {
            TextureFormat::Rgba8 => 32,
            TextureFormat::Rgb8 => 24,
            TextureFormat::Rgba5551
            | TextureFormat::Rgb565
            | TextureFormat::Rgba4
            | TextureFormat::La8
            | TextureFormat::Hilo8 => 16,
            TextureFormat::L8 | TextureFormat::A8 | TextureFormat::La4 => 8,
            TextureFormat::L4 | TextureFormat::A4 => 4,
            // ETC1 packs a 4x4 block (16 pixels) into 8 bytes = 4 bits/pixel,
            // ETC1A4 adds a 4-bit alpha per pixel on top, for 8 bits/pixel.
            TextureFormat::Etc1 => 4,
            TextureFormat::Etc1A4 => 8,
            TextureFormat::Unknown(_) => 32,
        }
    }
}

/// reads one texel as straight RGBA8.
/// an ETC1 texture as the standard blocks a GPU samples itself, row by row
/// of blocks in the order the texels are decoded, each big-endian. the PICA
/// keeps them four to an 8x8 tile, little-endian, an ETC1A4 block after its
/// alpha, which follows the blocks here a byte a texel, row by row.
pub fn etc1_blocks(data: &[u8], width: u32, height: u32, alpha: bool) -> Vec<u8> {
    let size = if alpha { 16 } else { 8 };
    let mut out = Vec::with_capacity((width * height / 2) as usize);
    let mut alphas = vec![0u8; if alpha { (width * height) as usize } else { 0 }];
    for by in 0..height / 4 {
        for bx in 0..width / 4 {
            let tile = (by / 2) * (width / 8).max(1) + bx / 2;
            let at = ((tile * 4 + bx % 2 + 2 * (by % 2)) * size) as usize;
            let mut block = [0u8; 16];
            if let Some(source) = data.get(at..at + size as usize) {
                block[..size as usize].copy_from_slice(source);
            }
            let (texels, color) = block[..size as usize].split_at_mut(size as usize - 8);
            color.reverse();
            out.extend_from_slice(color);
            if alpha {
                for y in 0..4 {
                    for x in 0..4 {
                        alphas[((by * 4 + y) * width + bx * 4 + x) as usize] = decode_etc1a4_alpha(texels, x, y);
                    }
                }
            }
        }
    }
    out.extend(alphas);
    out
}

/// how a texture goes to a GPU at a size of its own rather than four bytes
/// a texel: ETC1 as its blocks, the PICA's 16-bit formats as they are, and
/// its luminance and alpha formats as one or two 8-bit channels the GPU
/// spreads over RGBA itself, as decode_texels does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packing {
    Etc1,
    /// ETC1's blocks, then the alpha a byte a texel.
    Etc1A4,
    Rgb565,
    Rgba5551,
    Rgba4,
    /// one channel, read as (l, l, l, 1).
    Luminance,
    /// one channel, read as (0, 0, 0, a).
    Alpha,
    /// two channels, read as (l, l, l, a).
    LuminanceAlpha,
    /// two channels, read as (0, hi, lo, 1).
    Hilo,
}

/// a texture packed for a GPU, row by row like decode_texels, when its
/// format packs and it is whole tiles.
pub fn pack_texels(data: &[u8], format: TextureFormat, width: u32, height: u32) -> Option<(Packing, Vec<u8>)> {
    if !width.is_multiple_of(8) || !height.is_multiple_of(8) {
        return None;
    }
    let (packing, bytes) = match format {
        TextureFormat::Etc1 => return Some((Packing::Etc1, etc1_blocks(data, width, height, false))),
        TextureFormat::Etc1A4 => return Some((Packing::Etc1A4, etc1_blocks(data, width, height, true))),
        TextureFormat::Rgb565 => (Packing::Rgb565, 2),
        TextureFormat::Rgba5551 => (Packing::Rgba5551, 2),
        TextureFormat::Rgba4 => (Packing::Rgba4, 2),
        TextureFormat::L8 | TextureFormat::L4 => (Packing::Luminance, 1),
        TextureFormat::A8 | TextureFormat::A4 => (Packing::Alpha, 1),
        TextureFormat::La8 | TextureFormat::La4 => (Packing::LuminanceAlpha, 2),
        TextureFormat::Hilo8 => (Packing::Hilo, 2),
        _ => return None,
    };
    let mut out = vec![0u8; (width * height) as usize * bytes];
    let tiles_across = width / 8;
    for tile in 0..tiles_across * (height / 8) {
        let (tile_x, tile_y) = (tile % tiles_across * 8, tile / tiles_across * 8);
        for y in 0..8 {
            for x in 0..8 {
                let texel = (tile * 64 + morton_interleave(x, y)) as usize;
                let at = ((tile_y + y) * width + tile_x + x) as usize * bytes;
                let texel_out = &mut out[at..at + bytes];
                match format {
                    TextureFormat::L4 | TextureFormat::A4 => {
                        let Some(&byte) = data.get(texel / 2) else { continue };
                        texel_out[0] = expand4(if texel.is_multiple_of(2) { byte & 0xF } else { byte >> 4 });
                    }
                    TextureFormat::La4 => {
                        let Some(&byte) = data.get(texel) else { continue };
                        texel_out.copy_from_slice(&[expand4(byte >> 4), expand4(byte & 0xF)]);
                    }
                    // luminance or hi in the high byte
                    TextureFormat::La8 | TextureFormat::Hilo8 => {
                        let Some(b) = data.get(texel * 2..texel * 2 + 2) else { continue };
                        texel_out.copy_from_slice(&[b[1], b[0]]);
                    }
                    _ => {
                        let Some(b) = data.get(texel * bytes..(texel + 1) * bytes) else { continue };
                        texel_out.copy_from_slice(b);
                    }
                }
            }
        }
    }
    Some((packing, out))
}

/// every texel as straight RGBA8, row by row, what sample_texel reads at
/// each, worked out tile by tile and an ETC1 block's colors once a block.
pub fn decode_texels(data: &[u8], format: TextureFormat, width: u32, height: u32) -> Vec<[u8; 4]> {
    if !width.is_multiple_of(8) || !height.is_multiple_of(8) {
        return (0..height).flat_map(|y| (0..width).map(move |x| sample_texel(data, format, width, x, y))).collect();
    }
    let mut out = vec![[0u8; 4]; (width * height) as usize];
    let tiles_across = width / 8;
    let bits = format.bits_per_pixel();
    let etc1a4 = format == TextureFormat::Etc1A4;
    for tile in 0..tiles_across * (height / 8) {
        let (tile_x, tile_y) = (tile % tiles_across * 8, tile / tiles_across * 8);
        match format {
            TextureFormat::Etc1 | TextureFormat::Etc1A4 => {
                let size = if etc1a4 { 16 } else { 8 };
                for sub_block in 0..4 {
                    let at = ((tile * 4 + sub_block) * size) as usize;
                    let Some(block) = data.get(at..at + size as usize) else { continue };
                    let (alpha, color) = if etc1a4 { (&block[..8], &block[8..]) } else { (&[][..], block) };
                    let header = etc1_header(color);
                    let (x0, y0) = (tile_x + sub_block % 2 * 4, tile_y + sub_block / 2 * 4);
                    for y in 0..4 {
                        for x in 0..4 {
                            let [r, g, b] = etc1_texel(&header, x, y);
                            let a = if etc1a4 { decode_etc1a4_alpha(alpha, x, y) } else { 0xFF };
                            out[((y0 + y) * width + x0 + x) as usize] = [r, g, b, a];
                        }
                    }
                }
            }
            _ => {
                for y in 0..8 {
                    for x in 0..8 {
                        let texel = (tile * 64 + morton_interleave(x, y)) as usize;
                        let decoded = if bits >= 8 {
                            let bytes = (bits / 8) as usize;
                            data.get(texel * bytes..(texel + 1) * bytes).map_or([0; 4], |b| decode_bytes(format, b))
                        } else {
                            data.get(texel / 2).map_or([0; 4], |&byte| decode_nibble(format, if texel.is_multiple_of(2) { byte & 0xF } else { byte >> 4 }))
                        };
                        out[((tile_y + y) * width + tile_x + x) as usize] = decoded;
                    }
                }
            }
        }
    }
    out
}

pub fn sample_texel(data: &[u8], format: TextureFormat, width: u32, x: u32, y: u32) -> [u8; 4] {
    match format {
        TextureFormat::Etc1 | TextureFormat::Etc1A4 => sample_etc1(data, format, width, x, y),
        _ => sample_uncompressed(data, format, width, x, y),
    }
}

fn sample_uncompressed(
    data: &[u8],
    format: TextureFormat,
    width: u32,
    x: u32,
    y: u32,
) -> [u8; 4] {
    let bits = format.bits_per_pixel();
    if bits >= 8 {
        let bytes = (bits / 8) as usize;
        let offset = morton_offset(x, y, width, bytes as u32) as usize;
        let Some(bytes_slice) = data.get(offset..offset + bytes) else {
            return [0, 0, 0, 0];
        };
        decode_bytes(format, bytes_slice)
    } else {
        // sub-byte formats (L4, A4), the Morton tile index addresses a
        // nibble, two texels per byte, low nibble first.
        let bit_offset = morton_offset(x, y, width, bits) ;
        let byte_index = (bit_offset / 8) as usize;
        let Some(&byte) = data.get(byte_index) else {
            return [0, 0, 0, 0];
        };
        let nibble = if bit_offset.is_multiple_of(8) {
            byte & 0xF
        } else {
            byte >> 4
        };
        decode_nibble(format, nibble)
    }
}

fn decode_bytes(format: TextureFormat, b: &[u8]) -> [u8; 4] {
    match format {
        // textures store components in the same reversed byte order the
        // framebuffer formats do.
        TextureFormat::Rgba8 => [b[3], b[2], b[1], b[0]],
        TextureFormat::Rgb8 => [b[2], b[1], b[0], 0xFF],
        TextureFormat::Rgba5551 => {
            let v = u16::from_le_bytes([b[0], b[1]]);
            [
                expand5(((v >> 11) & 0x1F) as u8),
                expand5(((v >> 6) & 0x1F) as u8),
                expand5(((v >> 1) & 0x1F) as u8),
                if v & 1 != 0 { 0xFF } else { 0 },
            ]
        }
        TextureFormat::Rgb565 => {
            let v = u16::from_le_bytes([b[0], b[1]]);
            [
                expand5(((v >> 11) & 0x1F) as u8),
                expand6(((v >> 5) & 0x3F) as u8),
                expand5((v & 0x1F) as u8),
                0xFF,
            ]
        }
        TextureFormat::Rgba4 => {
            let v = u16::from_le_bytes([b[0], b[1]]);
            [
                expand4(((v >> 12) & 0xF) as u8),
                expand4(((v >> 8) & 0xF) as u8),
                expand4(((v >> 4) & 0xF) as u8),
                expand4((v & 0xF) as u8),
            ]
        }
        TextureFormat::La8 => [b[1], b[1], b[1], b[0]],
        TextureFormat::Hilo8 => [0, b[1], b[0], 0xFF],
        TextureFormat::L8 => [b[0], b[0], b[0], 0xFF],
        TextureFormat::A8 => [0, 0, 0, b[0]],
        TextureFormat::La4 => {
            let l = expand4(b[0] >> 4);
            let a = expand4(b[0] & 0xF);
            [l, l, l, a]
        }
        _ => [0, 0, 0, 0],
    }
}

fn decode_nibble(format: TextureFormat, nibble: u8) -> [u8; 4] {
    match format {
        TextureFormat::L4 => {
            let l = expand4(nibble);
            [l, l, l, 0xFF]
        }
        // alpha alone reads with black color, as in OpenGL ES
        TextureFormat::A4 => [0, 0, 0, expand4(nibble)],
        _ => [0, 0, 0, 0],
    }
}

// ---------------------------------------------------------------------------
// ETC1 / ETC1A4
// ---------------------------------------------------------------------------

/// the four per-table delta values for each of ETC1's 8 modifier tables.
const ETC1_MODIFIERS: [[i32; 4]; 8] = [
    [2, 8, -2, -8],
    [5, 17, -5, -17],
    [9, 29, -9, -29],
    [13, 42, -13, -42],
    [18, 60, -18, -60],
    [24, 80, -24, -80],
    [33, 106, -33, -106],
    [47, 183, -47, -183],
];

fn sample_etc1(data: &[u8], format: TextureFormat, width: u32, x: u32, y: u32) -> [u8; 4] {
    // like every other format, the texture is split into 8x8 tiles stored row
    // by row.
    let block_size = if format == TextureFormat::Etc1A4 { 16 } else { 8 };
    let tile = (y / 8) * (width / 8).max(1) + x / 8;
    let sub_block = (x % 8) / 4 + 2 * ((y % 8) / 4);
    let block_offset = ((tile * 4 + sub_block) * block_size) as usize;
    let local_x = x % 4;
    let local_y = y % 4;

    let Some(block) = data.get(block_offset..block_offset + block_size as usize) else {
        return [0, 0, 0, 0];
    };

    let (alpha_bits, color_block) = if format == TextureFormat::Etc1A4 {
        (&block[0..8], &block[8..16])
    } else {
        (&[][..], &block[0..8])
    };

    let rgb = decode_etc1_block(color_block, local_x, local_y);
    let alpha = if alpha_bits.is_empty() {
        0xFF
    } else {
        decode_etc1a4_alpha(alpha_bits, local_x, local_y)
    };
    [rgb[0], rgb[1], rgb[2], alpha]
}

/// decodes one pixel of a plain 8-byte ETC1 block.
fn decode_etc1_block(block: &[u8], x: u32, y: u32) -> [u8; 3] {
    etc1_texel(&etc1_header(block), x, y)
}

/// a plain ETC1 block's pixel index planes, flip bit, base colors and
/// modifier tables.
struct Etc1Header {
    low: u32,
    flip: bool,
    bases: [[u8; 3]; 2],
    tables: [usize; 2],
}

fn etc1_header(block: &[u8]) -> Etc1Header {
    // little-endian, because why the hell would it match the spec
    let word = u64::from_le_bytes(block.try_into().unwrap());
    let low = (word & 0xFFFF_FFFF) as u32;
    let high = (word >> 32) as u32;

    let flip = high & 1 != 0;
    let diff = high & 2 != 0;

    let (base0, base1) = if diff {
        let r0 = ((high >> 27) & 0x1F) as i32;
        let g0 = ((high >> 19) & 0x1F) as i32;
        let b0 = ((high >> 11) & 0x1F) as i32;
        let dr = sign_extend_3((high >> 24) & 0x7);
        let dg = sign_extend_3((high >> 16) & 0x7);
        let db = sign_extend_3((high >> 8) & 0x7);
        (
            [expand5((r0) as u8), expand5((g0) as u8), expand5((b0) as u8)],
            [
                expand5((r0 + dr).clamp(0, 31) as u8),
                expand5((g0 + dg).clamp(0, 31) as u8),
                expand5((b0 + db).clamp(0, 31) as u8),
            ],
        )
    } else {
        (
            [
                expand4(((high >> 28) & 0xF) as u8),
                expand4(((high >> 20) & 0xF) as u8),
                expand4(((high >> 12) & 0xF) as u8),
            ],
            [
                expand4(((high >> 24) & 0xF) as u8),
                expand4(((high >> 16) & 0xF) as u8),
                expand4(((high >> 8) & 0xF) as u8),
            ],
        )
    };

    let table0 = ((high >> 5) & 0x7) as usize;
    let table1 = ((high >> 2) & 0x7) as usize;
    Etc1Header { low, flip, bases: [base0, base1], tables: [table0, table1] }
}

fn etc1_texel(header: &Etc1Header, x: u32, y: u32) -> [u8; 3] {
    let low = header.low;
    // which sub-block this pixel belongs to depends on the flip bit, an
    // unflipped block splits into left/right halves, a flipped one into
    // top/bottom.
    let first_half = if header.flip { y < 2 } else { x < 2 };
    let half = if first_half { 0 } else { 1 };
    let (base, table) = (header.bases[half], header.tables[half]);

    // the two 16-bit pixel-index planes are stored transposed (column-major)
    // relative to (x, y).
    let pixel_index = x * 4 + y;
    let msb = (low >> (pixel_index + 16)) & 1;
    let lsb = (low >> pixel_index) & 1;
    // the table's four entries already alternate sign (e.g. [2, 8, -2, -8]),
    // so the (msb, lsb) pair selecting one of them via modifier_index
    // already encodes the sign, nothing further needs to branch on msb.
    let modifier_index = ((msb << 1) | lsb) as usize;
    let delta = ETC1_MODIFIERS[table][modifier_index];

    [
        (base[0] as i32 + delta).clamp(0, 255) as u8,
        (base[1] as i32 + delta).clamp(0, 255) as u8,
        (base[2] as i32 + delta).clamp(0, 255) as u8,
    ]
}

/// decodes one pixel's alpha from ETC1A4's 8-byte, 4-bit-per-texel alpha plane.
fn decode_etc1a4_alpha(block: &[u8], x: u32, y: u32) -> u8 {
    let word = u64::from_le_bytes(block.try_into().unwrap());
    let pixel_index = x * 4 + y;
    let nibble = ((word >> (pixel_index * 4)) & 0xF) as u8;
    expand4(nibble)
}

#[inline]
fn sign_extend_3(value: u32) -> i32 {
    let value = value as i32;
    if value & 0x4 != 0 {
        value - 8
    } else {
        value
    }
}

#[inline]
const fn expand4(v: u8) -> u8 {
    (v << 4) | v
}

#[inline]
const fn expand5(v: u8) -> u8 {
    (v << 3) | (v >> 2)
}

#[inline]
const fn expand6(v: u8) -> u8 {
    (v << 2) | (v >> 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a packed texture, read the way the GPU reads its format with the
    /// channels spread as the packing says, is what decode_texels makes.
    #[test]
    fn packed_texels_read_as_decoded() {
        let data: Vec<u8> = (0..1024u32).map(|i| ((i * 37 + 11) ^ (i >> 3)) as u8).collect();
        use TextureFormat::*;
        for format in [Rgb565, Rgba5551, Rgba4, L8, A8, L4, A4, La8, La4, Hilo8] {
            let size = (16 * 16 * format.bits_per_pixel() / 8) as usize;
            let (packing, packed) = pack_texels(&data[..size], format, 16, 16).unwrap();
            let decoded = decode_texels(&data[..size], format, 16, 16);
            for (i, &want) in decoded.iter().enumerate() {
                let word = |i: usize| u16::from_le_bytes([packed[i * 2], packed[i * 2 + 1]]);
                let got = match packing {
                    Packing::Rgb565 => {
                        let v = word(i);
                        [expand5((v >> 11) as u8 & 0x1F), expand6((v >> 5) as u8 & 0x3F), expand5(v as u8 & 0x1F), 0xFF]
                    }
                    Packing::Rgba5551 => {
                        let v = word(i);
                        [expand5((v >> 11) as u8 & 0x1F), expand5((v >> 6) as u8 & 0x1F), expand5((v >> 1) as u8 & 0x1F), if v & 1 != 0 { 0xFF } else { 0 }]
                    }
                    Packing::Rgba4 => {
                        let v = word(i);
                        [expand4((v >> 12) as u8 & 0xF), expand4((v >> 8) as u8 & 0xF), expand4((v >> 4) as u8 & 0xF), expand4(v as u8 & 0xF)]
                    }
                    Packing::Luminance => [packed[i], packed[i], packed[i], 0xFF],
                    Packing::Alpha => [0, 0, 0, packed[i]],
                    Packing::LuminanceAlpha => [packed[i * 2], packed[i * 2], packed[i * 2], packed[i * 2 + 1]],
                    Packing::Hilo => [0, packed[i * 2], packed[i * 2 + 1], 0xFF],
                    Packing::Etc1 | Packing::Etc1A4 => unreachable!(),
                };
                assert_eq!(got, want, "{format:?} texel {i}");
            }
        }
    }

    /// ETC1A4 packs as its color blocks would as ETC1, then each texel's
    /// alpha as decoded.
    #[test]
    fn etc1a4_packs_as_etc1_and_its_alpha() {
        let data: Vec<u8> = (0..16 * 16u32).map(|i| ((i * 37 + 11) ^ (i >> 3)) as u8).collect();
        let color: Vec<u8> = data.chunks(16).flat_map(|block| block[8..].to_vec()).collect();
        let (packing, packed) = pack_texels(&data, TextureFormat::Etc1A4, 16, 16).unwrap();
        assert_eq!(packing, Packing::Etc1A4);
        let (_, blocks) = pack_texels(&color, TextureFormat::Etc1, 16, 16).unwrap();
        assert_eq!(&packed[..blocks.len()], &blocks[..]);
        let alpha: Vec<u8> = decode_texels(&data, TextureFormat::Etc1A4, 16, 16).iter().map(|texel| texel[3]).collect();
        assert_eq!(&packed[blocks.len()..], &alpha[..]);
    }

    /// decoding a whole texture gives what reading each texel does, for
    /// every format, short data included.
    #[test]
    fn whole_textures_decode_as_their_texels() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let data: Vec<u8> = (0..64 * 64 * 4)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        for raw in 0..15 {
            let format = TextureFormat::from_raw(raw);
            for (width, height) in [(8, 8), (64, 16), (16, 64), (4, 4), (12, 8)] {
                for length in [data.len(), (width * height * format.bits_per_pixel() / 8) as usize - 5] {
                    let data = &data[..length];
                    let texels: Vec<[u8; 4]> = (0..height).flat_map(|y| (0..width).map(move |x| sample_texel(data, format, width, x, y))).collect();
                    assert_eq!(decode_texels(data, format, width, height), texels, "{format:?} {width}x{height} {length}");
                }
            }
        }
    }

    #[test]
    fn rgba8_round_trips() {
        // ABGR in memory (reversed), so bytes [A,B,G,R] decode to [R,G,B,A].
        let bytes = [0x11u8, 0x22, 0x33, 0x44];
        let decoded = decode_bytes(TextureFormat::Rgba8, &bytes);
        assert_eq!(decoded, [0x44, 0x33, 0x22, 0x11]);
    }

    /// alpha alone comes with black color, luminance alone with full alpha.
    #[test]
    fn a4_and_l4_expand_to_full_range() {
        assert_eq!(decode_nibble(TextureFormat::A4, 0xF), [0, 0, 0, 0xFF]);
        assert_eq!(decode_nibble(TextureFormat::A4, 0x0), [0, 0, 0, 0x00]);
        assert_eq!(decode_nibble(TextureFormat::L4, 0xF), [0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(decode_bytes(TextureFormat::A8, &[0x80]), [0, 0, 0, 0x80]);
    }

    /// builds a diff-mode (diff=1) ETC1 block from named fields, using the
    /// exact same bit positions [decode_etc1_block] reads.
    #[allow(clippy::too_many_arguments)]
    fn build_diff_block(
        r0: u8,
        g0: u8,
        b0: u8,
        dr: i8,
        dg: i8,
        db: i8,
        table0: u8,
        table1: u8,
        flip: bool,
        pixel_bits: u32,
    ) -> [u8; 8] {
        let three = |d: i8| (d as i32 & 0x7) as u32;
        let high = ((r0 as u32 & 0x1F) << 27)
            | (three(dr) << 24)
            | ((g0 as u32 & 0x1F) << 19)
            | (three(dg) << 16)
            | ((b0 as u32 & 0x1F) << 11)
            | (three(db) << 8)
            | ((table0 as u32 & 0x7) << 5)
            | ((table1 as u32 & 0x7) << 2)
            | (1 << 1) // diff
            | (flip as u32);
        let mut block = [0u8; 8];
        // little-endian 64-bit word, the index planes in the low half, the
        // colors and flags in the high half.
        block[0..4].copy_from_slice(&pixel_bits.to_le_bytes());
        block[4..8].copy_from_slice(&high.to_le_bytes());
        block
    }

    /// a solid-color block (zero deltas, every pixel selecting the same
    /// modifier) must decode to one uniform color across all 16 texels,
    /// the standard "does the bit-unpacking hang together" smoke test.
    #[test]
    fn etc1_flat_block_is_uniform() {
        // all pixel-index bits zero selects modifier 0 for every texel in
        // both sub-blocks, so a flat base color stays flat.
        let block = build_diff_block(16, 16, 16, 0, 0, 0, 0, 0, false, 0);

        let mut colors = std::collections::HashSet::new();
        for y in 0..4 {
            for x in 0..4 {
                colors.insert(decode_etc1_block(&block, x, y));
            }
        }
        assert_eq!(colors.len(), 1, "a flat block must decode to one color");
    }

    /// an unflipped block's two sub-blocks are its left and right halves
    /// (x<2 vs x>=2), each with its own base color, so making the two base
    /// colors different must produce different left/right averages while
    /// every row stays internally consistent between x=0/x=1 and x=2/x=3.
    #[test]
    fn etc1_unflipped_splits_left_and_right() {
        // a non-zero delta makes sub-block 1's base color (base0 + delta)
        // different from sub-block 0's, which is what should actually make
        // the two halves differ.
        let block = build_diff_block(4, 4, 4, 3, 3, 3, 0, 0, false, 0);
        let left = decode_etc1_block(&block, 0, 0);
        let right = decode_etc1_block(&block, 3, 0);
        assert_ne!(left, right, "differing base colors must differ left vs right");
        assert_eq!(decode_etc1_block(&block, 0, 0), decode_etc1_block(&block, 1, 0));
        assert_eq!(decode_etc1_block(&block, 2, 0), decode_etc1_block(&block, 3, 0));
    }

    /// a flipped block's sub-blocks are its top and bottom halves instead.
    #[test]
    fn etc1_flipped_splits_top_and_bottom() {
        let block = build_diff_block(4, 4, 4, 3, 3, 3, 0, 0, true, 0);
        let top = decode_etc1_block(&block, 0, 0);
        let bottom = decode_etc1_block(&block, 0, 3);
        assert_ne!(top, bottom, "differing base colors must differ top vs bottom");
        assert_eq!(decode_etc1_block(&block, 0, 0), decode_etc1_block(&block, 0, 1));
        assert_eq!(decode_etc1_block(&block, 0, 2), decode_etc1_block(&block, 0, 3));
    }

    /// blocks sit in 8x8 tiles stored row by row, four blocks per tile in
    /// Z order, not a Morton curve over the blocks themselves.
    #[test]
    fn etc1_blocks_are_laid_out_in_row_major_tiles() {
        let width = 32;
        let height = 8;
        // one flat block per 4x4 region, each a different gray.
        let mut data = vec![0u8; (width * height / 2) as usize];
        for block in 0..(data.len() / 8) {
            let gray = (block as u8) * 2;
            let raw = build_diff_block(gray, gray, gray, 0, 0, 0, 0, 0, false, 0);
            data[block * 8..block * 8 + 8].copy_from_slice(&raw);
        }
        let gray_at = |x: u32, y: u32| sample_etc1(&data, TextureFormat::Etc1, width, x, y)[0];
        let block_gray = |n: usize| sample_etc1(&data[n * 8..], TextureFormat::Etc1, 8, 0, 0)[0];
        // tile 1 (pixels 8..16 across) holds blocks 4-7.
        assert_eq!(gray_at(8, 0), block_gray(4));
        assert_eq!(gray_at(12, 0), block_gray(5));
        assert_eq!(gray_at(8, 4), block_gray(6));
        assert_eq!(gray_at(12, 4), block_gray(7));
    }
}
