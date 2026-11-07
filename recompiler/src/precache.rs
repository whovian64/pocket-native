//! the shaders a title's models need, worked out from their materials as the
//! library sets the GPU up for them, NintendoWare's (CGFX) and the SDK's H3D
//! (BCH), for the engine to build before the game starts. what the title's
//! own code draws, its layouts, text and effects or a pass it sets up itself,
//! is left to be built as the game needs it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::rom::Title;
use crate::shaders::{self, Shbin};

pub const HEADER: &str = "pocket-native precache 1";
/// a title's list: setups a run of the game used, each with its vertex
/// shader by the binary's key and where the shader starts in it.
pub const TITLE_HEADER: &str = "pocket-native title shaders 1";

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
const CAMERA: u32 = 0x4000_000A;
/// at most this many, those the most materials share first: a Mali-G31 takes
/// about a third of a second a program the first time.
const MOST: usize = 2000;

/// a lookup table's input and scale, and its set's name and its own.
type Table = (u32, u32, (String, String));
/// a shader's entries in the vertex list, by how many texture coordinates
/// each hands out.
type Variants = HashMap<u32, Vec<String>>;

struct Material {
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

/// an H3D material: the registers its commands set, its flags, the
/// textures it names with their samplers (type, wrap u, wrap v) and its
/// shader reference, program index @ shader name.
struct BchMaterial {
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
struct Found {
    materials: Vec<Material>,
    bch: Vec<BchMaterial>,
    /// H3D shaders by name, without the program index.
    bch_shaders: Vec<(String, Shbin)>,
    bch_light_counts: BTreeSet<u32>,
    formats: HashMap<String, u32>,
    absolute: HashMap<(String, String), BTreeSet<bool>>,
    shaders: Vec<(String, String, Shbin)>,
    /// how many fragment lights the title's light sets have.
    light_counts: BTreeSet<u32>,
    /// a camera of the title's with a w scale, which draws with the depth
    /// the fragment shader writes.
    w_buffer: bool,
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
            read(data, source, &mut seen);
        } else if data.starts_with(b"BCH\0") {
            read_bch(data, source, &mut seen);
        }
    });
    let named: HashSet<Vec<u8>> = title.code().map(|code| code.split(|&byte| byte == 0).map(<[u8]>::to_vec).collect()).unwrap_or_default();
    let mut lines: Vec<(String, usize)> = lines(&seen, found, &named).into_iter().collect();
    let files: HashMap<String, usize> = found.iter().enumerate().map(|(n, shbin)| (format!("{:016x}", shbin.key()), n)).collect();
    let mut listed = HashSet::new();
    for (k, path) in lists.iter().enumerate() {
        let Some(text) = std::fs::read_to_string(path).ok().filter(|text| text.lines().next() == Some(TITLE_HEADER)) else { continue };
        // a line with no vertex program is the title's own alone
        let own = k == 0;
        let mut known = 0;
        for line in text.lines().skip(1) {
            let Some((vertex, setup)) = line.split_once(' ') else { continue };
            let at = match vertex.split_once(':') {
                None if vertex == "0" && own => Some("0".to_owned()),
                Some((key, entry)) => files.get(key).map(|n| format!("{n}:{entry}")),
                None => None,
            };
            if let Some(line) = at.map(|at| format!("{at} {setup}")).filter(|line| listed.insert(line.clone())) {
                known += 1;
                lines.push((line, if own { usize::MAX } else { usize::MAX - 1 }));
            }
        }
        if known > 0 {
            println!("{known} shaders from {}", if own { "the title's list".to_owned() } else { path.display().to_string() });
        }
    }
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

/// each line, and how many materials give it.
fn lines(seen: &Found, found: &[Shbin], named: &HashSet<Vec<u8>>) -> HashMap<String, usize> {
    let mut shaders: Vec<(&str, &str, Variants)> = Vec::new();
    for (name, source, shbin) in &seen.shaders {
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
    let referenced: HashSet<&str> = seen.materials.iter().map(|material| material.shader.as_str()).collect();
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
    for material in &seen.materials {
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
        let depths: &[u32] = if seen.w_buffer { &[2 << 12, 3 << 12 | 1 << 16] } else { &[2 << 12] };
        // one light, which titles set up themselves as often as not, and two
        // where their light sets have two; sets of more are scenes' lights a
        // draw takes one or two of
        let counts: BTreeSet<u32> = if seen.light_counts.is_empty() { BTreeSet::from([1, 2]) } else { seen.light_counts.iter().copied().filter(|&count| count <= 2).chain([1]).collect() };
        let lightings = if lit != 0 { lighting(material, &seen.absolute, &counts) } else { vec![BTreeMap::new()] };
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
    println!("{} NintendoWare materials, their shaders {paired:?}, light sets of {:?}{}", seen.materials.len(), seen.light_counts, if seen.w_buffer { ", a w buffer" } else { "" });
    bch_lines(seen, found, &mut lines);
    lines
}

/// H3D materials' lines into lines, each with every shader of the name it
/// gives, which the game binds as it loads them.
fn bch_lines(seen: &Found, found: &[Shbin], lines: &mut HashMap<String, usize>) {
    let counts: BTreeSet<u32> = seen.bch_light_counts.iter().copied().filter(|&count| count <= 2).chain([1]).collect();
    let (mut paired, mut unpaired) = (0, 0);
    for material in &seen.bch {
        let Some((index, name)) = material.shader.split_once('@') else { continue };
        let Ok(index) = index.parse::<usize>() else { continue };
        let vertices: BTreeSet<String> = seen
            .bch_shaders
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
        let lightings = if lit != 0 { bch_lighting(material, &counts) } else { vec![BTreeMap::new()] };
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
    if !seen.bch.is_empty() {
        println!("{} H3D materials, {paired} with their shaders, {unpaired} without", seen.bch.len());
    }
}

/// what the runtime sets up for an H3D material's lighting, its tables'
/// inputs, scales and absolutes being its own commands'.
fn bch_lighting(material: &BchMaterial, counts: &BTreeSet<u32>) -> Vec<BTreeMap<usize, u32>> {
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

/// a run's shaders.txt, with the vertex programs it kept beside it, as a
/// title's list: each line's vertex program found among the title's shader
/// binaries and named by its key. the lines and how many had a program none
/// of found's is.
pub fn learn(found: &[Shbin], recorded: &Path) -> std::io::Result<(BTreeSet<String>, usize)> {
    let text = std::fs::read_to_string(recorded)?;
    let mut vertex_dir = recorded.as_os_str().to_owned();
    vertex_dir.push(".vertex");
    let vertex_dir = std::path::PathBuf::from(vertex_dir);
    let mut keys: HashMap<String, Option<u64>> = HashMap::new();
    let (mut lines, mut unknown) = (BTreeSet::new(), 0);
    for line in text.lines().skip(1) {
        let Some((vertex, setup)) = line.split_once(' ') else { continue };
        if vertex == "0" {
            lines.insert(line.to_owned());
            continue;
        }
        let mut parts = vertex.split(':');
        let (Some(fingerprint), Some(entry)) = (parts.next(), parts.next()) else { continue };
        // the program memory the run saw: the binary at its start, and what
        // an earlier, longer one left after it
        let key = *keys.entry(fingerprint.to_owned()).or_insert_with(|| {
            let fingerprint = u64::from_str_radix(fingerprint, 16).ok()?;
            let bytes = std::fs::read(vertex_dir.join(format!("{fingerprint:016x}.pica"))).ok()?;
            let words: Vec<u32> = bytes.as_chunks::<4>().0.iter().map(|chunk| u32::from_le_bytes(*chunk)).collect();
            let (program, descriptors) = words.split_at_checked(4096)?;
            found.iter().filter(|shbin| program.starts_with(&shbin.code) && descriptors.starts_with(&shbin.descriptors)).max_by_key(|shbin| shbin.code.len()).map(Shbin::key)
        });
        match key {
            Some(key) => _ = lines.insert(format!("{key:016x}:{entry} {setup}")),
            None => unknown += 1,
        }
    }
    Ok((lines, unknown))
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

/// a BCH's (H3D) materials, textures' formats and shaders, its pointers
/// made absolute first.
fn read_bch(d: &[u8], source: &str, seen: &mut Found) {
    let version = d.get(4).copied().unwrap_or(0);
    let Some(d) = relocate_bch(d) else { return };
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
    for shader in dict(2) {
        let (Some(program), Some(size)) = (u32_at(d, shader), u32_at(d, shader + 4)) else { continue };
        let Some(binary) = d.get(program as usize..program as usize + size as usize).filter(|binary| binary.starts_with(b"DVLB")) else { continue };
        if let (Some(name), Some(shbin)) = (string(shader + 40), shaders::dvlb(binary, source)) {
            let base = name.split_once('@').map_or(name.as_str(), |(_, base)| base).to_owned();
            seen.bch_shaders.push((base, shbin));
        }
    }
    let lights = dict(5).len() as u32;
    if lights > 0 {
        seen.bch_light_counts.insert(lights.min(8));
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
        seen.bch.push(BchMaterial {
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
fn relocate_bch(d: &[u8]) -> Option<Vec<u8>> {
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

/// a CGFX's materials, image textures' formats, lookup tables and shaders.
fn read(d: &[u8], source: &str, seen: &mut Found) {
    let mut at = 0;
    while let Some(next) = d[at..].windows(4).position(|window| window == b"MTOB") {
        at += next + 4;
        let base = at - 8;
        if u32_at(d, base) == Some(0x0800_0000) {
            seen.materials.extend(material(d, base).map(|material| Material { source: source.to_owned(), ..material }));
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
    seen.w_buffer |= dict(d, rel(d, data + 0x34)).iter().any(|&(_, at)| u32_at(d, at) == Some(CAMERA) && u32_at(d, at + 0xC4).is_some_and(|scale| f32::from_bits(scale) != 0.0));
    let lights = dict(d, rel(d, data + 0x3C)).iter().filter(|&&(_, at)| u32_at(d, at) == Some(FRAGMENT_LIGHT)).count() as u32;
    if lights > 0 {
        seen.light_counts.insert(lights.min(8));
    }
    for (name, at) in dict(d, rel(d, data + 0x2C)) {
        if let Some(shbin) = rel(d, at + 0x1C).and_then(|binary| d.get(binary..)).filter(|binary| binary.starts_with(b"DVLB")).and_then(|binary| shaders::dvlb(binary, source)) {
            seen.shaders.push((name, source.to_owned(), shbin));
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
        let relocated = relocate_bch(&d).unwrap();
        assert_eq!(u32_at(&relocated, 0x48), Some(0x64));
        assert_eq!(u32_at(&relocated, 0x4C), Some(0x52));
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
