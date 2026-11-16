//! the shaders a title's models need, worked out from their materials as the
//! library sets the GPU up for them, NintendoWare's (CGFX) and the SDK's H3D
//! (BCH), for the engine to build before the game starts, with the lines of
//! titles' lists of what runs of the games built. what the title's own code
//! draws, its layouts, text and effects or a pass it sets up itself, is left
//! to be built as the game needs it, unless a list has it.

mod bch;
mod cgfx;
mod lists;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[cfg(test)]
pub(crate) use lists::raw_key;
pub use lists::{learn, Files, TITLE_HEADER};

use crate::rom::Title;
use crate::shaders::{self, Shbin};

pub const HEADER: &str = "pocket-native precache 1";

const STAGES: [usize; 6] = [0xC0, 0xC8, 0xD0, 0xD8, 0xF0, 0xF8];
const UNIT_PARAMS: [usize; 3] = [0x83, 0x93, 0x9B];
/// the coordinates units 2 and 3 read, for configs 0120, 0110, 0111, 0112,
/// 0121 and 0122.
const COORDINATES: [u32; 6] = [0, 0x2000, 0x2100, 0x2200, 0x100, 0x200];
/// the bit that turns each lookup table off: reflection red, green and blue,
/// distribution 0 and 1 and fresnel.
const DISABLE: [u32; 6] = [20, 21, 22, 16, 17, 19];
/// the scene's fog, which the titles set from their own code.
const FOG_MODES: [u32; 2] = [0, 5];
const ETC1A4: u32 = 13;
/// at most this many, the lists' first, then those the most materials share:
/// a Mali-G31 takes about a third of a second a program the first time.
const MOST: usize = 2000;

/// what a title's files hold.
#[derive(Default)]
struct Found {
    /// textures' formats by name.
    formats: HashMap<String, u32>,
    cgfx: cgfx::Models,
    bch: bch::Models,
}

/// writes precache.txt into dir, each line a vertex shader of the list
/// write_vertex_list made from found, file:entry, and a setup as the
/// engine's own list keeps them: the title's own list first, then the lines
/// of the other titles' lists whose shaders this one has too, then what its
/// models need, the most shared first. how many lines, none for a title with
/// none of them, which writes nothing.
pub fn write(title: &Title, found: &[Shbin], dir: &Path, lists: &[&Path]) -> std::io::Result<usize> {
    let mut seen = Found::default();
    shaders::files(title, &mut |data, source| {
        if data.starts_with(b"CGFX") {
            cgfx::read(data, source, &mut seen);
        } else if data.starts_with(b"BCH\0") {
            bch::read(data, source, &mut seen);
        }
    });
    let named: HashSet<Vec<u8>> = title.code().map(|code| code.split(|&byte| byte == 0).map(<[u8]>::to_vec).collect()).unwrap_or_default();
    let mut lines = cgfx::lines(&seen, found, &named);
    bch::lines(&seen, found, &mut lines);
    lists::add(title, found, dir, lists, &mut lines)?;
    let mut lines: Vec<(String, usize)> = lines.into_iter().collect();
    if lines.is_empty() {
        return Ok(0);
    }
    lines.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if lines.len() > MOST {
        println!("{} shaders worked out, the {MOST} the most materials share kept", lines.len());
        lines.truncate(MOST);
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("precache.txt"), std::iter::once(HEADER).chain(lines.iter().map(|(line, _)| line.as_str())).collect::<Vec<_>>().join("\n") + "\n")?;
    Ok(lines.len())
}

/// the lighting registers for config 0 and 1, the tables' absolutes,
/// inputs and scales, a light's flags and how many lights.
fn light_regs(config0: u32, config1: u32, [absolute, inputs, scales]: [u32; 3], light: u32, count: u32) -> BTreeMap<usize, u32> {
    let slots = (0..count).fold(0, |slots, slot| slots | slot << (4 * slot));
    let mut regs = BTreeMap::from([(0x8F, 1), (0x1C2, count - 1), (0x1C3, config0), (0x1C4, config1), (0x1D0, absolute), (0x1D1, inputs), (0x1D2, scales), (0x1D9, slots)]);
    for slot in 0..8 {
        regs.insert(0x149 + 0x10 * slot, if slot < count as usize { light } else { 1 });
    }
    regs
}

/// a material's GPU commands, words long from at, into regs with their
/// byte masks.
fn commands(d: &[u8], at: usize, words: usize, regs: &mut BTreeMap<usize, u32>) -> Option<()> {
    let w = (0..words).map(|k| u32_at(d, at + 4 * k)).collect::<Option<Vec<u32>>>()?;
    let mut j = 0;
    while j + 1 < w.len() {
        let (param, header) = (w[j], w[j + 1]);
        let (register, mask, extra, sequential) = ((header & 0xFFFF) as usize, header >> 16 & 0xF, (header >> 20 & 0xFF) as usize, header >> 31 != 0);
        if header == 0 || register >= 0x300 || mask == 0 {
            break;
        }
        let bytes = (0..4).filter(|byte| mask >> byte & 1 != 0).fold(0u32, |bits, byte| bits | 0xFF << (8 * byte));
        for (k, value) in std::iter::once(param).chain(w[(j + 2).min(w.len())..(j + 2 + extra).min(w.len())].iter().copied()).enumerate() {
            let target = if sequential { register + k } else { register };
            let old = regs.get(&target).copied().unwrap_or(0);
            regs.insert(target, old & !bytes | value & bytes);
        }
        j += 2 + extra + (extra & 1);
    }
    Some(())
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn cstr(d: &[u8], at: usize) -> Option<String> {
    let end = d.get(at..)?.iter().position(|&byte| byte == 0)?;
    Some(d[at..at + end].iter().map(|&byte| byte as char).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_keep_what_their_byte_masks_leave() {
        let mut regs = BTreeMap::from([(0x107, 0x1F00)]);
        // 0x107's low byte alone, then 0x126 whole
        let words: Vec<u8> = [0x41u32, 0x0001_0107, 0x0300_0000, 0x000F_0126].iter().flat_map(|word| word.to_le_bytes()).collect();
        commands(&words, 0, 4, &mut regs).unwrap();
        assert_eq!(regs[&0x107], 0x1F41);
        assert_eq!(regs[&0x126], 0x0300_0000);
    }
}
