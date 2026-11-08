//! the SDK's H3D models (BCH): each material's setup from its own commands,
//! with every shader of the name it gives.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::{commands, cstr, light_regs, u16_at, u32_at, Found, COORDINATES, DISABLE, ETC1A4, FOG_MODES, STAGES, UNIT_PARAMS};
use crate::shaders::{self, Shbin};

/// an H3D material: the registers its commands set, its flags, the
/// textures it names with their samplers (type, wrap u, wrap v) and its
/// shader reference, program index @ shader name.
pub(super) struct Material {
    shader: String,
    regs: BTreeMap<usize, u32>,
    flags: u32,
    fragment: u32,
    layer: u32,
    fresnel: u32,
    bump: u32,
    bump_texture: u32,
    textures: [Option<String>; 3],
    samplers: [Option<[u32; 3]>; 3],
}

#[derive(Default)]
pub(super) struct Models {
    materials: Vec<Material>,
    /// shaders by name, without the program index.
    shaders: Vec<(String, Shbin)>,
    light_counts: BTreeSet<u32>,
}

/// the materials' lines into lines, each with every shader of the name it
/// gives, which the game binds as it loads them.
pub(super) fn lines(seen: &Found, found: &[Shbin], lines: &mut HashMap<String, usize>) {
    let models = &seen.bch;
    let counts: BTreeSet<u32> = models.light_counts.iter().copied().filter(|&count| count <= 2).chain([1]).collect();
    let (mut paired, mut unpaired) = (0, 0);
    for material in &models.materials {
        let Some((index, name)) = material.shader.split_once('@') else { continue };
        let Ok(index) = index.parse::<usize>() else { continue };
        let vertices: BTreeSet<String> = models
            .shaders
            .iter()
            .filter(|(base, _)| base == name)
            .filter_map(|(_, shbin)| {
                let n = found.iter().position(|other| other.code == shbin.code && other.descriptors == shbin.descriptors)?;
                let entry = found[n].entries.get(index).filter(|entry| !entry.geometry)?;
                Some(format!("{n}:{:x}", entry.main))
            })
            .collect();
        if vertices.is_empty() {
            unpaired += 1;
            continue;
        }
        paired += 1;
        let r = |register: usize| material.regs.get(&register).copied().unwrap_or(0);
        let mut out = BTreeMap::new();
        for base in STAGES {
            for offset in [0, 1, 2, 4] {
                out.insert(base + offset, r(base + offset));
            }
        }
        let used = |unit: usize| material.textures[unit].is_some() && material.samplers[unit].is_some_and(|[kind, ..]| kind != 5);
        let enabled = (0..3).filter(|&unit| used(unit)).fold(0, |bits, unit| bits | 1 << unit);
        out.insert(0x80, 0x11000 | enabled | COORDINATES.get(material.layer as usize >> 4).copied().unwrap_or(0));
        for (unit, register) in UNIT_PARAMS.into_iter().enumerate().filter(|&(unit, _)| used(unit)) {
            let [kind, wrap_u, wrap_v] = material.samplers[unit].unwrap_or_default();
            out.insert(register, wrap_v << 8 | wrap_u << 12 | (kind & 7) << 28);
        }
        for register in [0x100, 0x101, 0x102, 0x104, 0x107, 0x113] {
            out.insert(register, r(register));
        }
        let lit = material.flags & 1;
        let split = (0..3).filter(|&unit| used(unit) && material.textures[unit].as_ref().and_then(|name| seen.formats.get(name)) == Some(&ETC1A4)).fold(0, |bits, unit| bits | 1 << unit);
        let flags = enabled | split << 4 | 2 << 12 | lit << 17;
        let lightings = if lit != 0 { lighting(material, &counts) } else { vec![BTreeMap::new()] };
        // the fog the material asks for, which the scene's may be or not
        let fogs: &[u32] = if material.flags & 0x10 != 0 { &FOG_MODES } else { &[0] };
        let mut own = HashSet::new();
        for light in &lightings {
            for &fog in fogs {
                out.insert(0xE0, r(0xE0) & 0xFFFF8 | fog);
                let regs: Vec<String> = out.iter().chain(light).filter(|&(&register, &value)| value != 0 && register < 0x300).map(|(register, value)| format!("{register:x}={value:x}")).collect();
                for vertex in &vertices {
                    own.insert(format!("{vertex} {flags:x} {}", regs.join(" ")));
                }
            }
        }
        for line in own {
            *lines.entry(line).or_default() += 1;
        }
    }
    if !models.materials.is_empty() {
        println!("{} H3D materials, {paired} with their shaders, {unpaired} without", models.materials.len());
    }
}

/// what the runtime sets up for a material's lighting, its tables' inputs,
/// scales and absolutes being its own commands'.
fn lighting(material: &Material, counts: &BTreeSet<u32>) -> Vec<BTreeMap<usize, u32>> {
    let fragment = material.fragment;
    let reflection = fragment >> 4 & 1 != 0;
    let used = [reflection, reflection, reflection, fragment >> 2 & 1 != 0, fragment >> 3 & 1 != 0, material.fresnel != 0];
    let mut config0 = 0x8000_0400 | (material.layer & 0xF) << 4 | (material.fresnel & 1) << 2 | (material.fresnel >> 1 & 1) << 3 | (fragment >> 1 & 1) << 27 | (material.bump_texture & 3) << 22 | (material.bump & 3) << 28;
    if material.bump != 0 && fragment & 1 == 0 {
        config0 |= 1 << 30;
    }
    let config1 = (0..6).filter(|&table| !used[table]).fold(0xFF04_FFFFu32, |config, table| config | 1 << DISABLE[table]);
    let light = 1 | (fragment >> 5 & 1) << 2 | (fragment >> 6 & 1) << 3;
    let given = [0x1D0, 0x1D1, 0x1D2].map(|register| material.regs.get(&register).copied().unwrap_or(0));
    counts.iter().map(|&count| light_regs(config0, config1, given, light, count)).collect()
}

/// a BCH's materials, textures' formats and shaders, its pointers made
/// absolute first.
pub(super) fn read(d: &[u8], source: &str, seen: &mut Found) {
    let version = d.get(4).copied().unwrap_or(0);
    let Some(d) = relocate(d) else { return };
    let d = &d[..];
    // before 0x21 there's no raw ext section, and a model's material holds
    // its three samplers in place of a pointer to them
    let sections = if version >= 0x21 { 6 } else { 5 };
    let (Some(contents), Some(length)) = (u32_at(d, 8), u32_at(d, 8 + 4 * sections)) else { return };
    let (contents, length) = (contents as usize, length as usize);
    let (size, names, samplers_inline) = if version >= 0x21 { (44, 28, false) } else { (88, 72, true) };
    // and before 7 the fragment commands' pointer comes once, not twice
    let references = if version < 7 { 256 } else { 260 };
    let dict = |index: usize| -> Vec<usize> {
        let (Some(at), Some(count)) = (u32_at(d, contents + 12 * index), u32_at(d, contents + 12 * index + 4)) else { return Vec::new() };
        (0..count.min(1 << 16) as usize).filter_map(|k| u32_at(d, at as usize + 4 * k)).map(|value| value as usize).collect()
    };
    let string = |at: usize| u32_at(d, at).filter(|&pointer| pointer != 0).and_then(|pointer| cstr(d, pointer as usize));
    for texture in dict(3) {
        if let (Some(name), Some(&format)) = (string(texture + 0x1C), d.get(texture + 0x18)) {
            seen.formats.insert(name, format as u32);
        }
    }
    let models = &mut seen.bch;
    for shader in dict(2) {
        let (Some(program), Some(size)) = (u32_at(d, shader), u32_at(d, shader + 4)) else { continue };
        let Some(binary) = d.get(program as usize..program as usize + size as usize).filter(|binary| binary.starts_with(b"DVLB")) else { continue };
        if let (Some(name), Some(shbin)) = (string(shader + 40), shaders::dvlb(binary, source)) {
            let base = name.split_once('@').map_or(name.as_str(), |(_, base)| base).to_owned();
            models.shaders.push((base, shbin));
        }
    }
    let lights = dict(5).len() as u32;
    if lights > 0 {
        models.light_counts.insert(lights.min(8));
    }
    // each model's materials sit inline in its dictionary: the material's
    // parameters, three textures by pointer, the texture commands, the
    // samplers, the three textures' names and its name
    let params: HashSet<usize> = dict(1).into_iter().collect();
    let list = u32_at(d, contents + 12).unwrap_or(0) as usize..u32_at(d, contents + 12).unwrap_or(0) as usize + 4 * params.len();
    let mut done = HashSet::new();
    for at in (contents..(contents + length).saturating_sub(size)).step_by(4) {
        let Some(p) = u32_at(d, at).map(|p| p as usize).filter(|p| params.contains(p) && !list.contains(&at)) else { continue };
        let reference = string(p + references + 4).unwrap_or_default();
        if string(at + names + 12).is_none_or(|name| reference.split('@').next() != Some(name.as_str())) || !done.insert(p) {
            continue;
        }
        let mut regs = BTreeMap::new();
        if commands(d, p + 168, 6, &mut regs).is_none() {
            continue;
        }
        if let (Some(commands_at), Some(words)) = (u32_at(d, p + 200), u32_at(d, p + 204)) {
            let _ = commands(d, commands_at as usize, words.min(4096) as usize, &mut regs);
        }
        let samplers_at = if samplers_inline { at + 24 } else { u32_at(d, at + 24).unwrap_or(0) as usize };
        let byte = |at: usize| d.get(at).map(|&byte| byte as u32);
        models.materials.push(Material {
            shader: string(p + references).unwrap_or_default(),
            regs,
            flags: u16_at(d, p + 4).unwrap_or(0) as u32,
            fragment: byte(p + 6).unwrap_or(0),
            layer: byte(p + 164).unwrap_or(0),
            fresnel: byte(p + 165).unwrap_or(0),
            bump: byte(p + 166).unwrap_or(0),
            bump_texture: byte(p + 167).unwrap_or(0),
            textures: [0, 1, 2].map(|unit| string(at + names + 4 * unit)),
            samplers: [0, 1, 2].map(|unit| (samplers_at != 0).then(|| Some([byte(samplers_at + 16 * unit)?, byte(samplers_at + 16 * unit + 1)?, byte(samplers_at + 16 * unit + 2)?])).flatten()),
        });
    }
}

/// a BCH with its relocation table applied: each entry adds the base of the
/// section it points into to the pointer at its place, a string's place
/// counted in bytes and the rest in words.
fn relocate(d: &[u8]) -> Option<Vec<u8>> {
    let mut b = d.to_vec();
    let version = *d.get(4)?;
    let sections = if version >= 0x21 { 6 } else { 5 };
    let address = |section: usize| u32_at(d, 8 + 4 * section);
    let (contents, strings, commands, raw) = (address(0)?, address(1)?, address(2)?, address(3)?);
    let raw_ext = if sections == 6 { address(4)? } else { raw };
    let (table, table_length) = (address(sections - 1)? as usize, u32_at(d, 8 + 4 * (2 * sections - 1))? as usize);
    // older converters numbered the sections past the commands one higher,
    // and those past raw data's vertices lower
    let section = |value: u32| {
        if version < 7 && value >= 3 {
            value + 1
        } else if version > 7 && version < 0x21 && value >= 6 {
            value - 1
        } else {
            value
        }
    };
    let base = |section: u32| match section {
        0 => contents,
        1 => strings,
        2 | 3 => commands,
        4..=8 => raw,
        9..=13 => raw_ext,
        _ => 0,
    };
    for k in 0..table_length / 4 {
        let entry = u32_at(d, table + 4 * k)?;
        let (place, target, source) = (entry & 0x1FF_FFFF, section(entry >> 25 & 0xF), section(entry >> 29));
        let at = base(source) as usize + place as usize * if target == 1 { 1 } else { 4 };
        if let Some(value) = u32_at(&b, at) {
            b[at..at + 4].copy_from_slice(&value.wrapping_add(base(target)).to_le_bytes());
        }
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bch_s_pointers_are_placed_by_their_section() {
        // version 0x21: contents at 0x44, strings at 0x50, commands at 0x60,
        // raw data at 0x70, raw ext at 0x80, the relocation table at 0x90
        let mut d = vec![0u8; 0x98];
        d[..4].copy_from_slice(b"BCH\0");
        d[4] = 0x21;
        for (k, value) in [0x44u32, 0x50, 0x60, 0x70, 0x80, 0x90].into_iter().enumerate() {
            d[8 + 4 * k..12 + 4 * k].copy_from_slice(&value.to_le_bytes());
        }
        d[0x34..0x38].copy_from_slice(&8u32.to_le_bytes());
        // a pointer into the commands at contents word 1, and one into the
        // strings at contents byte 8, each holding its offset in that section
        d[0x48..0x4C].copy_from_slice(&4u32.to_le_bytes());
        d[0x4C..0x50].copy_from_slice(&2u32.to_le_bytes());
        d[0x90..0x94].copy_from_slice(&(1u32 | 2 << 25).to_le_bytes());
        d[0x94..0x98].copy_from_slice(&(8u32 | 1 << 25).to_le_bytes());
        let relocated = relocate(&d).unwrap();
        assert_eq!(u32_at(&relocated, 0x48), Some(0x64));
        assert_eq!(u32_at(&relocated, 0x4C), Some(0x52));
    }
}
