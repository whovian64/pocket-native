//! NintendoWare's models (CGFX): each material's setup as the library makes
//! it, with the shader it names or the one the title's code binds instead.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::{commands, cstr, light_regs, u16_at, u32_at, Found, COORDINATES, DISABLE, ETC1A4, FOG_MODES, STAGES, UNIT_PARAMS};
use crate::shaders::{self, Shbin};

/// the lookup tables, reflection red, green and blue, distribution 0 and 1
/// and fresnel: each one's field in the input, scale and absolute registers.
const FIELDS: [u32; 6] = [6, 5, 4, 0, 1, 3];
const FRAGMENT_LIGHT: u32 = 0x4000_00A2;
const CAMERA: u32 = 0x4000_000A;

/// a lookup table's input and scale, and its set's name and its own.
type Table = (u32, u32, (String, String));
/// a shader's entries in the vertex list, by how many texture coordinates
/// each hands out.
type Variants = HashMap<u32, Vec<String>>;

pub(super) struct Material {
    source: String,
    shader: String,
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
pub(super) struct Models {
    materials: Vec<Material>,
    absolute: HashMap<(String, String), BTreeSet<bool>>,
    shaders: Vec<(String, String, Shbin)>,
    /// how many fragment lights the title's light sets have.
    light_counts: BTreeSet<u32>,
    /// a camera of the title's with a w scale, which draws with the depth
    /// the fragment shader writes.
    w_buffer: bool,
}

/// each line, and how many materials give it.
pub(super) fn lines(seen: &Found, found: &[Shbin], named: &HashSet<Vec<u8>>) -> HashMap<String, usize> {
    let models = &seen.cgfx;
    let mut shaders: Vec<(&str, &str, Variants)> = Vec::new();
    for (name, source, shbin) in &models.shaders {
        let Some(n) = found.iter().position(|other| other.code == shbin.code && other.descriptors == shbin.descriptors) else { continue };
        let mut variants = Variants::new();
        for entry in found[n].entries.iter().filter(|entry| !entry.geometry) {
            let semantics: Vec<u32> = entry.outmap().2.iter().flat_map(|word| (0..4).map(move |byte| word >> (byte * 8) & 0x1F)).collect();
            let texcoords = [0xC, 0xE, 0x16].iter().filter(|kind| semantics.contains(kind)).count() as u32;
            variants.entry(texcoords).or_default().push(format!("{n}:{:x}", entry.main));
        }
        shaders.push((name, source, variants));
    }
    // what a title's code binds in place of the library's default shader:
    // one it names, that no material does, made like the default, a variant
    // for each count of texture coordinates
    let referenced: HashSet<&str> = models.materials.iter().map(|material| material.shader.as_str()).collect();
    let substitutes: Vec<usize> = (0..shaders.len()).filter(|&k| named.contains(shaders[k].0.as_bytes()) && !referenced.contains(shaders[k].0) && shaders[k].2.len() >= 3).collect();
    let mut paired: BTreeMap<&str, usize> = BTreeMap::new();
    let bind = |material: &Material| -> Vec<usize> {
        if material.shader == "DefaultShader" && !substitutes.is_empty() {
            return substitutes.clone();
        }
        // the one by its name, in its own file else its archive else any
        let named: Vec<usize> = (0..shaders.len()).filter(|&k| shaders[k].0 == material.shader).collect();
        let archive = |source: &str| source.rsplit_once('#').map_or(source, |(archive, _)| archive).to_owned();
        let rank = |k: usize| if shaders[k].1 == material.source { 0 } else if archive(shaders[k].1) == archive(&material.source) { 1 } else { 2 };
        let nearest = named.iter().map(|&k| rank(k)).min();
        named.into_iter().filter(|&k| Some(rank(k)) == nearest).collect()
    };
    let mut lines = HashMap::new();
    for material in &models.materials {
        let bound = bind(material);
        for &k in &bound {
            *paired.entry(shaders[k].0).or_default() += 1;
        }
        let vertices: Vec<&String> = bound.iter().filter_map(|&k| shaders[k].2.get(&material.texcoords)).flatten().collect();
        if vertices.is_empty() {
            continue;
        }
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
        let flags = enabled | split << 4 | lit << 17;
        // shaded, and with the w buffer where a camera has one
        let depths: &[u32] = if models.w_buffer { &[2 << 12, 3 << 12 | 1 << 16] } else { &[2 << 12] };
        // one light, which titles set up themselves as often as not, and two
        // where their light sets have two; sets of more are scenes' lights a
        // draw takes one or two of
        let counts: BTreeSet<u32> = if models.light_counts.is_empty() { BTreeSet::from([1, 2]) } else { models.light_counts.iter().copied().filter(|&count| count <= 2).chain([1]).collect() };
        let lightings = if lit != 0 { lighting(material, &models.absolute, &counts) } else { vec![BTreeMap::new()] };
        let mut own = HashSet::new();
        for light in &lightings {
            for fog in FOG_MODES {
                out.insert(0xE0, r(0xE0) & 0xFF00 | fog);
                let regs: Vec<String> = out.iter().chain(light).filter(|&(&register, &value)| value != 0 && register < 0x300).map(|(register, value)| format!("{register:x}={value:x}")).collect();
                for vertex in &vertices {
                    for depth in depths {
                        own.insert(format!("{vertex} {:x} {}", flags | depth, regs.join(" ")));
                    }
                }
            }
        }
        for line in own {
            *lines.entry(line).or_default() += 1;
        }
    }
    println!("{} NintendoWare materials, their shaders {paired:?}, light sets of {:?}{}", models.materials.len(), models.light_counts, if models.w_buffer { ", a w buffer" } else { "" });
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
            out.push(light_regs(config0, config1, [bits, inputs, scales], light, count));
        }
    }
    out
}

/// a CGFX's materials, image textures' formats, lookup tables and shaders.
pub(super) fn read(d: &[u8], source: &str, seen: &mut Found) {
    let mut at = 0;
    while let Some(next) = d[at..].windows(4).position(|window| window == b"MTOB") {
        at += next + 4;
        let base = at - 8;
        if u32_at(d, base) == Some(0x0800_0000) {
            seen.cgfx.materials.extend(material(d, base).map(|material| Material { source: source.to_owned(), ..material }));
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
    let models = &mut seen.cgfx;
    let Some(data) = u16_at(d, 6).map(usize::from).filter(|&data| d.get(data..data + 4) == Some(b"DATA")) else { return };
    for (set, at) in dict(d, rel(d, data + 0x1C)) {
        for (table, sampler) in dict(d, rel(d, at + 0x1C)) {
            if let Some(word) = u32_at(d, sampler + 8) {
                models.absolute.entry((set.clone(), table)).or_default().insert(word & 1 != 0);
            }
        }
    }
    models.w_buffer |= dict(d, rel(d, data + 0x34)).iter().any(|&(_, at)| u32_at(d, at) == Some(CAMERA) && u32_at(d, at + 0xC4).is_some_and(|scale| f32::from_bits(scale) != 0.0));
    let lights = dict(d, rel(d, data + 0x3C)).iter().filter(|&&(_, at)| u32_at(d, at) == Some(FRAGMENT_LIGHT)).count() as u32;
    if lights > 0 {
        models.light_counts.insert(lights.min(8));
    }
    for (name, at) in dict(d, rel(d, data + 0x2C)) {
        if let Some(shbin) = rel(d, at + 0x1C).and_then(|binary| d.get(binary..)).filter(|binary| binary.starts_with(b"DVLB")).and_then(|binary| shaders::dvlb(binary, source)) {
            models.shaders.push((name, source.to_owned(), shbin));
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
        source: String::new(),
        shader: rel(d, base + 0x284).and_then(|reference| rel(d, reference + 0x18)).and_then(|name| cstr(d, name)).unwrap_or_default(),
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

/// where a self-relative offset at at points, None for none.
fn rel(d: &[u8], at: usize) -> Option<usize> {
    let offset = u32_at(d, at)? as i32;
    let to = at.checked_add_signed(offset as isize).filter(|_| offset != 0)?;
    (to < d.len()).then_some(to)
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
            source: String::new(),
            shader: "DefaultShader".to_owned(),
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
}
