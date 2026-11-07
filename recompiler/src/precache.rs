//! the shaders a title's NintendoWare (CGFX) models need, worked out from
//! their materials as the library sets the GPU up for them, for the engine
//! to build before the game starts. what the title's own code draws, its
//! layouts, text and effects or a pass it sets up itself, is left to be built
//! as the game needs it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::rom::Title;
use crate::shaders::{self, Shbin};

pub const HEADER: &str = "pocket-native precache 1";

const STAGES: [usize; 6] = [0xC0, 0xC8, 0xD0, 0xD8, 0xF0, 0xF8];
const UNIT_PARAMS: [usize; 3] = [0x83, 0x93, 0x9B];
/// the coordinates units 2 and 3 read, for configs 0120, 0110, 0111, 0112,
/// 0121 and 0122.
const COORDINATES: [u32; 6] = [0, 0x2000, 0x2100, 0x2200, 0x100, 0x200];
/// the lookup tables, reflection red, green and blue, distribution 0 and 1
/// and fresnel: each one's field in the input, scale and absolute registers,
/// and the bit that turns it off.
const FIELDS: [u32; 6] = [6, 5, 4, 0, 1, 3];
const DISABLE: [u32; 6] = [20, 21, 22, 16, 17, 19];
/// the scene's fog, which the titles set from their own code.
const FOG_MODES: [u32; 2] = [0, 5];
const ETC1A4: u32 = 13;
const FRAGMENT_LIGHT: u32 = 0x4000_00A2;

/// a lookup table's input and scale, and its set's name and its own.
type Table = (u32, u32, (String, String));

struct Material {
    regs: BTreeMap<usize, u32>,
    flags: u32,
    coordinates: u32,
    texcoords: u32,
    depth_flags: u32,
    mappers: [bool; 3],
    textures: [Option<String>; 3],
    lighting: [u32; 6],
    tables: [Option<Table>; 6],
}

#[derive(Default)]
struct Found {
    materials: Vec<Material>,
    formats: HashMap<String, u32>,
    absolute: HashMap<(String, String), BTreeSet<bool>>,
    shaders: Vec<(String, Shbin)>,
    /// how many fragment lights the title's light sets have.
    light_counts: BTreeSet<u32>,
}

/// writes precache.txt into dir, each line a vertex shader of the list
/// write_vertex_list made from found, file:entry, and a setup as the
/// engine's own list keeps them. how many lines, none for a title without
/// NintendoWare models, which writes nothing.
pub fn write(title: &Title, found: &[Shbin], dir: &Path) -> std::io::Result<usize> {
    let mut seen = Found::default();
    shaders::files(title, &mut |data, source| {
        if data.starts_with(b"CGFX") {
            read(data, source, &mut seen);
        }
    });
    let named: HashSet<Vec<u8>> = title.code().map(|code| code.split(|&byte| byte == 0).map(<[u8]>::to_vec).collect()).unwrap_or_default();
    let lines = lines(&seen, found, &named);
    if lines.is_empty() {
        return Ok(0);
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("precache.txt"), std::iter::once(HEADER.to_owned()).chain(lines.iter().cloned()).collect::<Vec<_>>().join("\n") + "\n")?;
    Ok(lines.len())
}

fn lines(seen: &Found, found: &[Shbin], named: &HashSet<Vec<u8>>) -> BTreeSet<String> {
    // the shaders the title's code binds by name, else every one
    let file = |shbin: &Shbin| found.iter().position(|other| other.code == shbin.code && other.descriptors == shbin.descriptors);
    let bound: Vec<usize> = seen.shaders.iter().filter(|(name, _)| named.contains(name.as_bytes())).filter_map(|(_, shbin)| file(shbin)).collect();
    let files = if bound.is_empty() { seen.shaders.iter().filter_map(|(_, shbin)| file(shbin)).collect() } else { bound };
    let mut variants: HashMap<u32, Vec<String>> = HashMap::new();
    for n in files {
        for entry in found[n].entries.iter().filter(|entry| !entry.geometry) {
            let semantics: Vec<u32> = entry.outmap().2.iter().flat_map(|word| (0..4).map(move |byte| word >> (byte * 8) & 0x1F)).collect();
            let texcoords = [0xC, 0xE, 0x16].iter().filter(|kind| semantics.contains(kind)).count() as u32;
            variants.entry(texcoords).or_default().push(format!("{n}:{:x}", entry.main));
        }
    }
    let mut lines = BTreeSet::new();
    for material in &seen.materials {
        let Some(vertices) = variants.get(&material.texcoords) else { continue };
        let Some(&coordinates) = COORDINATES.get(material.coordinates as usize) else { continue };
        let r = |register: usize| material.regs.get(&register).copied().unwrap_or(0);
        let mut out = BTreeMap::new();
        for base in STAGES {
            for offset in [0, 1, 2, 4] {
                out.insert(base + offset, r(base + offset));
            }
        }
        let enabled = (0..3).filter(|&unit| material.mappers[unit]).fold(0, |bits, unit| bits | 1 << unit);
        out.insert(0x80, 0x11000 | enabled | coordinates);
        for (_, register) in UNIT_PARAMS.into_iter().enumerate().filter(|&(unit, _)| material.mappers[unit]) {
            out.insert(register, r(register));
        }
        for register in [0x100, 0x101, 0x102, 0x104] {
            out.insert(register, r(register));
        }
        out.insert(0x107, r(0x107) & 0xFF | 0xF << 8 | (material.depth_flags & 2) << 11);
        out.insert(0x113, 0xF);
        let lit = material.flags & 1;
        let split = (0..3).filter(|&unit| material.textures[unit].as_ref().and_then(|name| seen.formats.get(name)) == Some(&ETC1A4)).fold(0, |bits, unit| bits | 1 << unit);
        let flags = enabled | split << 4 | 2 << 12 | lit << 17;
        let counts = if seen.light_counts.is_empty() { BTreeSet::from([1, 2]) } else { seen.light_counts.clone() };
        let lightings = if lit != 0 { lighting(material, &seen.absolute, &counts) } else { vec![BTreeMap::new()] };
        for light in &lightings {
            for fog in FOG_MODES {
                out.insert(0xE0, r(0xE0) & 0xFF00 | fog);
                let regs: Vec<String> = out.iter().chain(light).filter(|&(&register, &value)| value != 0 && register < 0x300).map(|(register, value)| format!("{register:x}={value:x}")).collect();
                for vertex in vertices {
                    lines.insert(format!("{vertex} {flags:x} {}", regs.join(" ")));
                }
            }
        }
    }
    lines
}

/// the lighting the library sets up for a material: its tables, fresnel
/// and bump, under as many directional lights as the title's scenes have.
fn lighting(material: &Material, absolute: &HashMap<(String, String), BTreeSet<bool>>, counts: &BTreeSet<u32>) -> Vec<BTreeMap<usize, u32>> {
    let [flags, layer, fresnel, bump_unit, bump, renormalize] = material.lighting;
    let used = [flags >> 5 & 1 != 0, flags >> 5 & 1 != 0, flags >> 5 & 1 != 0, flags >> 1 & 1 != 0, flags >> 2 & 1 != 0, fresnel != 0];
    let mut config0 = 0x8000_0400 | (layer & 0xF) << 4 | (fresnel & 1) << 2 | (fresnel >> 1 & 1) << 3 | (flags & 1) << 27 | (bump_unit & 3) << 22 | (bump & 3) << 28;
    if bump != 0 && renormalize == 0 {
        config0 |= 1 << 30;
    }
    let (mut config1, mut inputs, mut scales) = (0xFF04_FFFFu32, 0u32, 0u32);
    let mut absolutes = vec![0u32];
    for (table, found) in material.tables.iter().enumerate() {
        let Some((input, scale, names)) = found.as_ref().filter(|_| used[table]) else {
            config1 |= 1 << DISABLE[table];
            continue;
        };
        let field = FIELDS[table] * 4;
        inputs |= (input & 7) << field;
        scales |= (scale & 7) << field;
        let both = BTreeSet::from([false, true]);
        let seen = absolute.get(names).unwrap_or(&both);
        absolutes = absolutes.iter().flat_map(|&bits| seen.iter().map(move |&on| if on { bits } else { bits | 2 << field })).collect();
    }
    let light = 1 | (flags >> 3 & 1) << 2 | (flags >> 4 & 1) << 3;
    let mut out = Vec::new();
    for &count in counts {
        for &bits in &absolutes {
            let slots = (0..count).fold(0, |slots, slot| slots | slot << (4 * slot));
            let mut regs = BTreeMap::from([(0x8F, 1), (0x1C2, count - 1), (0x1C3, config0), (0x1C4, config1), (0x1D0, bits), (0x1D1, inputs), (0x1D2, scales), (0x1D9, slots)]);
            for slot in 0..8 {
                regs.insert(0x149 + 0x10 * slot, if slot < count as usize { light } else { 1 });
            }
            out.push(regs);
        }
    }
    out
}

/// a CGFX's materials, image textures' formats, lookup tables and shaders.
fn read(d: &[u8], source: &str, seen: &mut Found) {
    let mut at = 0;
    while let Some(next) = d[at..].windows(4).position(|window| window == b"MTOB") {
        at += next + 4;
        let base = at - 8;
        if u32_at(d, base) == Some(0x0800_0000) {
            seen.materials.extend(material(d, base));
        }
    }
    let mut at = 0;
    while let Some(next) = d[at..].windows(4).position(|window| window == b"TXOB") {
        at += next + 4;
        let base = at - 8;
        if u32_at(d, base) == Some(0x2000_0011) {
            if let (Some(name), Some(format)) = (rel(d, base + 0xC).and_then(|name| cstr(d, name)), u32_at(d, base + 0x34)) {
                seen.formats.insert(name, format);
            }
        }
    }
    let Some(data) = u16_at(d, 6).map(usize::from).filter(|&data| d.get(data..data + 4) == Some(b"DATA")) else { return };
    for (set, at) in dict(d, rel(d, data + 0x1C)) {
        for (table, sampler) in dict(d, rel(d, at + 0x1C)) {
            if let Some(word) = u32_at(d, sampler + 8) {
                seen.absolute.entry((set.clone(), table)).or_default().insert(word & 1 != 0);
            }
        }
    }
    let lights = dict(d, rel(d, data + 0x3C)).iter().filter(|&&(_, at)| u32_at(d, at) == Some(FRAGMENT_LIGHT)).count() as u32;
    if lights > 0 {
        seen.light_counts.insert(lights.min(8));
    }
    for (name, at) in dict(d, rel(d, data + 0x2C)) {
        if let Some(shbin) = rel(d, at + 0x1C).and_then(|binary| d.get(binary..)).filter(|binary| binary.starts_with(b"DVLB")).and_then(|binary| shaders::dvlb(binary, source)) {
            seen.shaders.push((name, shbin));
        }
    }
}

fn material(d: &[u8], base: usize) -> Option<Material> {
    let mut regs = BTreeMap::new();
    commands(d, base + 0x110, 2, &mut regs)?; // culling
    commands(d, base + 0x11C, 4, &mut regs)?; // depth
    commands(d, base + 0x140, 6, &mut regs)?; // blending
    commands(d, base + 0x158, 4, &mut regs)?; // stencil
    let mappers: [Option<usize>; 3] = std::array::from_fn(|unit| rel(d, base + 0x274 + 4 * unit));
    let mut textures = [None, None, None];
    for (unit, mapper) in mappers.iter().enumerate() {
        let Some(mapper) = *mapper else { continue };
        if u32_at(d, mapper)? == 0x8000_0000 {
            commands(d, mapper + 0x10, 14, &mut regs)?;
            textures[unit] = rel(d, mapper + 8).and_then(|reference| rel(d, reference + 0x18)).and_then(|name| cstr(d, name));
        }
    }
    let fragment = rel(d, base + 0x288)?;
    for stage in 0..6 {
        commands(d, fragment + 0x2C + 28 * stage + 4, 6, &mut regs)?;
    }
    commands(d, fragment + 0xD4, 2, &mut regs)?; // alpha test
    commands(d, fragment + 0xDC, 6, &mut regs)?; // buffer colour and updates
    let lighting = [0, 1, 2, 3, 4, 5].map(|word| u32_at(d, fragment + 0x10 + 4 * word));
    let tables_at = rel(d, fragment + 0x28);
    let tables = std::array::from_fn(|table| {
        let lut = rel(d, tables_at? + 4 * table)?;
        let reference = rel(d, lut + 8)?;
        Some((u32_at(d, lut)?, u32_at(d, lut + 4)?, (cstr(d, rel(d, reference + 4)?)?, cstr(d, rel(d, reference + 8)?)?)))
    });
    Some(Material {
        regs,
        flags: u32_at(d, base + 0x18)?,
        coordinates: u32_at(d, base + 0x1C)?,
        texcoords: u32_at(d, base + 0x168)?,
        depth_flags: u32_at(d, base + 0x118)?,
        mappers: mappers.map(|mapper| mapper.is_some()),
        textures,
        lighting: [lighting[0]?, lighting[1]?, lighting[2]?, lighting[3]?, lighting[4]?, lighting[5]?],
        tables,
    })
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

/// where a self-relative offset at at points, None for none.
fn rel(d: &[u8], at: usize) -> Option<usize> {
    let offset = u32_at(d, at)? as i32;
    let to = at.checked_add_signed(offset as isize).filter(|_| offset != 0)?;
    (to < d.len()).then_some(to)
}

fn cstr(d: &[u8], at: usize) -> Option<String> {
    let end = d.get(at..)?.iter().position(|&byte| byte == 0)?;
    Some(d[at..at + end].iter().map(|&byte| byte as char).collect())
}

/// a CGFX dictionary's names and what each points at.
fn dict(d: &[u8], at: Option<usize>) -> Vec<(String, usize)> {
    let Some(at) = at.filter(|&at| d.get(at..at + 4) == Some(b"DICT")) else { return Vec::new() };
    let count = u32_at(d, at + 8).unwrap_or(0) as usize;
    (0..count.min(65536)).filter_map(|k| Some((cstr(d, rel(d, at + 0x1C + 0x10 * k + 8)?)?, rel(d, at + 0x1C + 0x10 * k + 12)?))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lit_material_sets_up_lighting_as_the_library_does() {
        // Mario Kart 7's kart bodies: both distributions, geometric factor 1,
        // layer config 2, the two tables absolute
        let names = |table: &str| ("body_LUT".to_owned(), table.to_owned());
        let material = Material {
            regs: BTreeMap::new(),
            flags: 1,
            coordinates: 0,
            texcoords: 1,
            depth_flags: 0,
            mappers: [true, false, false],
            textures: [None, None, None],
            lighting: [0x16, 2, 0, 0, 0, 0],
            tables: [None, None, None, Some((0, 0, names("body_D0"))), Some((2, 1, names("body_D1"))), None],
        };
        let absolute = HashMap::from([(names("body_D0"), BTreeSet::from([true])), (names("body_D1"), BTreeSet::from([true]))]);
        let setups = lighting(&material, &absolute, &BTreeSet::from([1, 2]));
        assert_eq!(setups.len(), 2);
        let two = &setups[1];
        let expected = [(0x1C2, 1), (0x1C3, 0x8000_0420), (0x1C4, 0xFF7C_FFFF), (0x1D0, 0), (0x1D1, 0x20), (0x1D2, 0x10), (0x1D9, 0x10), (0x149, 9), (0x159, 9)];
        for (register, value) in expected {
            assert_eq!(two[&register], value, "{register:x}");
        }
    }

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
