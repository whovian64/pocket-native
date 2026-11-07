//! the shader binaries a title carries, DVLB, in its code and its RomFS:
//! loose, compressed with LZ11, or inside SARC, darc and GARC archives.

use std::collections::HashSet;

use crate::rom::{DirEntry, Title};

/// one DVLB: its program, its operand descriptors and the shaders that
/// start in it.
pub struct Shbin {
    pub source: String,
    pub code: Vec<u32>,
    pub descriptors: Vec<u32>,
    pub entries: Vec<Entry>,
}

pub struct Entry {
    pub geometry: bool,
    /// where main starts and ends, in words.
    pub main: u32,
    pub end: u32,
    /// the outputs: kind, register and component mask.
    pub outputs: Vec<(u16, u16, u32)>,
}

impl Shbin {
    /// what a title's list knows the binary by, the same in every copy of
    /// the game: FNV-1a over its program and its descriptors.
    pub fn key(&self) -> u64 {
        self.code.iter().chain(&self.descriptors).flat_map(|word| word.to_le_bytes()).fold(0xCBF2_9CE4_8422_2325, |hash, byte| (hash ^ byte as u64).wrapping_mul(0x100_0000_01B3))
    }
}

impl Entry {
    /// the output map a game sets for the entry, worked out from its table
    /// as libctru's shader programs do: the output registers used, how many,
    /// and each one's four semantics, 0x1F where none.
    pub fn outmap(&self) -> (u32, u32, [u32; 7]) {
        let mut map = [0x1F1F_1F1Fu32; 7];
        let mut mask = 0u32;
        for &(kind, register, components) in &self.outputs {
            let register = register as usize;
            if register >= 7 {
                continue;
            }
            mask |= 1 << register;
            let (mut semantic, mut left) = match kind {
                0 => (0x00, 4),
                1 => (0x04, 4),
                2 => (0x08, 4),
                3 => (0x0C, 2),
                4 => (0x10, 1),
                5 => (0x0E, 2),
                6 => (0x16, 2),
                8 => (0x12, 3),
                _ => continue,
            };
            for component in 0..4 {
                if left > 0 && components & (1 << component) != 0 {
                    map[register] = (map[register] & !(0xFF << (component * 8))) | semantic << (component * 8);
                    semantic += 1;
                    left -= 1;
                }
            }
        }
        (mask, mask.count_ones(), map)
    }
}

/// the vertex shaders found, for the engine to build at a start: each
/// binary's program and descriptors as the GPU's shader memory holds them,
/// and a list of where each shader starts and its output map.
pub fn write_vertex_list(found: &[Shbin], dir: &std::path::Path) -> std::io::Result<usize> {
    // none, no list: a build copies the list with its shaders, which an
    // empty one has none of
    if !found.iter().any(|shbin| shbin.entries.iter().any(|entry| !entry.geometry)) {
        return Ok(0);
    }
    std::fs::create_dir_all(dir)?;
    let mut list = String::from("pocket-native vertex 1\n");
    let mut count = 0;
    for (n, shbin) in found.iter().enumerate() {
        let mut words = vec![0u32; 4096 + 128];
        words[..shbin.code.len()].copy_from_slice(&shbin.code);
        words[4096..4096 + shbin.descriptors.len()].copy_from_slice(&shbin.descriptors);
        std::fs::write(dir.join(format!("{n}.pica")), words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>())?;
        for entry in shbin.entries.iter().filter(|entry| !entry.geometry) {
            let (mask, total, map) = entry.outmap();
            list += &format!("{n} {:x} {mask:x} {total} {}\n", entry.main, map.map(|word| format!("{word:x}")).join(" "));
            count += 1;
        }
    }
    std::fs::write(dir.join("vertex.txt"), list)?;
    Ok(count)
}

/// files up to this size are read whole and searched.
const LARGEST: u64 = 2 << 30;

pub fn find(title: &Title) -> Vec<Shbin> {
    let mut found = Vec::new();
    files(title, &mut |data, source| {
        let mut at = 0;
        while let Some(next) = data[at..].windows(4).position(|window| window == b"DVLB") {
            if let Some(shbin) = dvlb(&data[at + next..], source) {
                found.push(shbin);
            }
            at += next + 4;
        }
    });
    let mut seen = HashSet::new();
    found.retain(|shbin| seen.insert((shbin.code.clone(), shbin.descriptors.clone())));
    found
}

/// every file a title has, its code first, each unpacked from the
/// compressions and archives titles keep them in, with where it came from.
pub fn files(title: &Title, file: &mut dyn FnMut(&[u8], &str)) {
    if let Ok(code) = title.code() {
        visit(&code, "code", file, 0);
    }
    if let Some(romfs) = &title.romfs {
        if let Ok(root) = romfs.root() {
            walk(title, &root, String::new(), file);
        }
    }
}

fn walk(title: &Title, dir: &DirEntry, path: String, file: &mut dyn FnMut(&[u8], &str)) {
    let Some(romfs) = &title.romfs else { return };
    for (_, entry) in romfs.files(dir) {
        if entry.data_size > LARGEST {
            continue;
        }
        if let Some(data) = title.read_romfs(&entry, 0, entry.data_size as usize) {
            visit(&data, &format!("{path}/{}", entry.name), file, 0);
        }
    }
    for (_, sub) in romfs.subdirs(dir) {
        let name = format!("{path}/{}", sub.name);
        walk(title, &sub, name, file);
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn visit(data: &[u8], source: &str, file: &mut dyn FnMut(&[u8], &str), depth: u32) {
    if depth > 4 || data.len() < 8 {
        return;
    }
    let plain = match data[0] {
        0x10 => lz10(data).map(|plain| (plain, "lz10")),
        0x11 => lz11(data).map(|plain| (plain, "lz11")),
        b'Y' if &data[0..4] == b"Yaz0" => yaz0(data).map(|plain| (plain, "yaz0")),
        _ => None,
    };
    if let Some((plain, how)) = plain {
        visit(&plain, &format!("{source} ({how})"), file, depth + 1);
        return;
    }
    let members = match &data[0..4] {
        b"SARC" => sarc(data),
        b"darc" => darc(data),
        b"CRAG" => garc(data),
        b"NARC" => narc(data),
        _ => mini(data),
    };
    if let Some(members) = members {
        for (index, (start, end)) in members.into_iter().enumerate() {
            if let Some(member) = data.get(start..end) {
                visit(member, &format!("{source}#{index}"), file, depth + 1);
            }
        }
        return;
    }
    file(data, source);
}

pub(crate) fn dvlb(data: &[u8], source: &str) -> Option<Shbin> {
    let count = u32_at(data, 4)? as usize;
    if count == 0 || count > 64 {
        return None;
    }
    let dvlp = 8 + 4 * count;
    if data.get(dvlp..dvlp + 4)? != b"DVLP" {
        return None;
    }
    let (code_at, words) = (dvlp + u32_at(data, dvlp + 8)? as usize, u32_at(data, dvlp + 0xC)? as usize);
    let (descriptors_at, descriptor_count) = (dvlp + u32_at(data, dvlp + 0x10)? as usize, u32_at(data, dvlp + 0x14)? as usize);
    if words == 0 || words > 4096 || descriptor_count > 128 {
        return None;
    }
    let code = (0..words).map(|i| u32_at(data, code_at + i * 4)).collect::<Option<Vec<u32>>>()?;
    let descriptors = (0..descriptor_count).map(|i| u32_at(data, descriptors_at + i * 8)).collect::<Option<Vec<u32>>>()?;
    let mut entries = Vec::new();
    for i in 0..count {
        let dvle = u32_at(data, 8 + 4 * i)? as usize;
        if data.get(dvle..dvle + 4)? != b"DVLE" {
            return None;
        }
        let (outputs_at, output_count) = (dvle + u32_at(data, dvle + 0x28)? as usize, u32_at(data, dvle + 0x2C)? as usize);
        let outputs = (0..output_count.min(16)).filter_map(|o| Some((u16_at(data, outputs_at + o * 8)?, u16_at(data, outputs_at + o * 8 + 2)?, u32_at(data, outputs_at + o * 8 + 4)?))).collect();
        entries.push(Entry { geometry: *data.get(dvle + 6)? != 0, main: u32_at(data, dvle + 8)?, end: u32_at(data, dvle + 0xC)?, outputs });
    }
    Some(Shbin { source: source.to_owned(), code, descriptors, entries })
}

/// LZ11 as Nintendo's tools pack it, None for what only starts like it.
fn lz11(data: &[u8]) -> Option<Vec<u8>> {
    let mut size = (u32_at(data, 0)? >> 8) as usize;
    let mut at = 4;
    if size == 0 {
        size = u32_at(data, 4)? as usize;
        at = 8;
    }
    if size == 0 || size > 512 << 20 {
        return None;
    }
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        let flags = *data.get(at)?;
        at += 1;
        for bit in (0..8).rev() {
            if out.len() >= size {
                break;
            }
            if flags & (1 << bit) == 0 {
                out.push(*data.get(at)?);
                at += 1;
                continue;
            }
            let first = *data.get(at)? as usize;
            let (length, distance) = match first >> 4 {
                0 => {
                    let (b1, b2) = (*data.get(at + 1)? as usize, *data.get(at + 2)? as usize);
                    at += 3;
                    (((first & 0xF) << 4 | b1 >> 4) + 0x11, ((b1 & 0xF) << 8 | b2) + 1)
                }
                1 => {
                    let (b1, b2, b3) = (*data.get(at + 1)? as usize, *data.get(at + 2)? as usize, *data.get(at + 3)? as usize);
                    at += 4;
                    (((first & 0xF) << 12 | b1 << 4 | b2 >> 4) + 0x111, ((b2 & 0xF) << 8 | b3) + 1)
                }
                high => {
                    let b1 = *data.get(at + 1)? as usize;
                    at += 2;
                    (high + 1, ((first & 0xF) << 8 | b1) + 1)
                }
            };
            if distance > out.len() {
                return None;
            }
            for _ in 0..length {
                out.push(out[out.len() - distance]);
            }
        }
    }
    out.truncate(size);
    Some(out)
}

/// LZ10, the older of the two, None for what only starts like it.
fn lz10(data: &[u8]) -> Option<Vec<u8>> {
    let size = (u32_at(data, 0)? >> 8) as usize;
    if size == 0 || size > 512 << 20 {
        return None;
    }
    let (mut out, mut at) = (Vec::with_capacity(size), 4);
    while out.len() < size {
        let flags = *data.get(at)?;
        at += 1;
        for bit in (0..8).rev() {
            if out.len() >= size {
                break;
            }
            if flags & (1 << bit) == 0 {
                out.push(*data.get(at)?);
                at += 1;
            } else {
                let (b0, b1) = (*data.get(at)? as usize, *data.get(at + 1)? as usize);
                at += 2;
                let (length, distance) = ((b0 >> 4) + 3, ((b0 & 0xF) << 8 | b1) + 1);
                if distance > out.len() {
                    return None;
                }
                for _ in 0..length {
                    out.push(out[out.len() - distance]);
                }
            }
        }
    }
    out.truncate(size);
    Some(out)
}

/// Yaz0, as .szs files are packed.
fn yaz0(data: &[u8]) -> Option<Vec<u8>> {
    let size = u32::from_be_bytes(data.get(4..8)?.try_into().ok()?) as usize;
    if size == 0 || size > 512 << 20 {
        return None;
    }
    let (mut out, mut at) = (Vec::with_capacity(size), 16);
    while out.len() < size {
        let flags = *data.get(at)?;
        at += 1;
        for bit in (0..8).rev() {
            if out.len() >= size {
                break;
            }
            if flags & (1 << bit) != 0 {
                out.push(*data.get(at)?);
                at += 1;
            } else {
                let (b0, b1) = (*data.get(at)? as usize, *data.get(at + 1)? as usize);
                at += 2;
                let distance = ((b0 & 0xF) << 8 | b1) + 1;
                let length = if b0 >> 4 == 0 {
                    at += 1;
                    *data.get(at - 1)? as usize + 0x12
                } else {
                    (b0 >> 4) + 2
                };
                if distance > out.len() {
                    return None;
                }
                for _ in 0..length {
                    out.push(out[out.len() - distance]);
                }
            }
        }
    }
    out.truncate(size);
    Some(out)
}

/// a SARC's members, where each starts and ends.
fn sarc(data: &[u8]) -> Option<Vec<(usize, usize)>> {
    let header = u16_at(data, 4)? as usize;
    let base = u32_at(data, 0xC)? as usize;
    if data.get(header..header + 4)? != b"SFAT" {
        return None;
    }
    let nodes = u16_at(data, header + 6)? as usize;
    (0..nodes).map(|n| Some((base + u32_at(data, header + 0xC + n * 16 + 8)? as usize, base + u32_at(data, header + 0xC + n * 16 + 12)? as usize))).collect()
}

/// a NARC's files, from its allocation table and where its image starts.
fn narc(data: &[u8]) -> Option<Vec<(usize, usize)>> {
    let (mut at, mut table, mut image) = (u16_at(data, 0xC)? as usize, None, None);
    for _ in 0..u16_at(data, 0xE)? {
        match data.get(at..at + 4)? {
            b"BTAF" => table = Some(at),
            b"GMIF" => image = Some(at + 8),
            _ => {}
        }
        at += u32_at(data, at + 4)? as usize;
    }
    let (table, image) = (table?, image?);
    (0..u16_at(data, table + 8)? as usize).map(|n| Some((image + u32_at(data, table + 12 + n * 8)? as usize, image + u32_at(data, table + 16 + n * 8)? as usize))).collect()
}

/// a darc's files, the folders left out.
fn darc(data: &[u8]) -> Option<Vec<(usize, usize)>> {
    let table = u32_at(data, 0x10)? as usize;
    // the root's next is how many entries there are
    let count = u32_at(data, table + 8)? as usize;
    let mut members = Vec::new();
    for entry in 0..count.min(100_000) {
        let at = table + entry * 12;
        if u32_at(data, at)? & 0x0100_0000 != 0 {
            continue;
        }
        let (offset, size) = (u32_at(data, at + 4)? as usize, u32_at(data, at + 8)? as usize);
        members.push((offset, offset + size));
    }
    Some(members)
}

/// Game Freak's small packs inside GARCs: two capital letters, a count,
/// and one more offset than files, the last the pack's end.
fn mini(data: &[u8]) -> Option<Vec<(usize, usize)>> {
    if !data[0].is_ascii_uppercase() || !data[1].is_ascii_uppercase() {
        return None;
    }
    let count = u16_at(data, 2)? as usize;
    if count == 0 || count > 4096 {
        return None;
    }
    let offsets = (0..=count).map(|i| u32_at(data, 4 + i * 4).map(|o| o as usize)).collect::<Option<Vec<usize>>>()?;
    let sane = offsets[0] >= 4 + (count + 1) * 4 && offsets.windows(2).all(|pair| pair[0] <= pair[1]) && offsets[count] <= data.len();
    sane.then(|| offsets.windows(2).map(|pair| (pair[0], pair[1])).collect())
}

/// a GARC's files: FATO's count, FATB's entries, from the data's start.
fn garc(data: &[u8]) -> Option<Vec<(usize, usize)>> {
    let header = u32_at(data, 4)? as usize;
    let base = u32_at(data, 0x10)? as usize;
    if data.get(header..header + 4)? != b"OTAF" {
        return None;
    }
    let fatb = header + u32_at(data, header + 4)? as usize;
    if data.get(fatb..fatb + 4)? != b"BTAF" {
        return None;
    }
    let files = u32_at(data, fatb + 8)? as usize;
    let mut members = Vec::new();
    let mut at = fatb + 12;
    for _ in 0..files.min(100_000) {
        let mut mask = u32_at(data, at)?;
        at += 4;
        while mask != 0 {
            let (start, end) = (u32_at(data, at)? as usize, u32_at(data, at + 4)? as usize);
            members.push((base + start, base + end));
            at += 12;
            mask &= mask - 1;
        }
    }
    Some(members)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_shaders_found_leaves_no_list() {
        let dir = std::env::temp_dir().join(format!("pn-vertex-list-{}", std::process::id()));
        assert_eq!(write_vertex_list(&[], &dir).unwrap(), 0);
        // a build copies the list's shaders with it, an empty list has none
        assert!(!dir.join("vertex.txt").exists());
    }

    #[test]
    fn an_entry_maps_its_outputs_as_libctru_does() {
        // position in o0, color in o1, texcoord0 in o2's xy
        let entry = Entry { geometry: false, main: 0, end: 0, outputs: vec![(0, 0, 0xF), (2, 1, 0xF), (3, 2, 0x3)] };
        assert_eq!(entry.outmap(), (0b111, 3, [0x0302_0100, 0x0B0A_0908, 0x1F1F_0D0C, 0x1F1F_1F1F, 0x1F1F_1F1F, 0x1F1F_1F1F, 0x1F1F_1F1F]));
    }

    #[test]
    fn a_run_s_vertex_program_is_found_by_its_binary() {
        let shbin = |code: Vec<u32>| Shbin { source: String::new(), code, descriptors: vec![7], entries: Vec::new() };
        let found = [shbin(vec![1, 2]), shbin(vec![1, 2, 3])];
        let dir = std::env::temp_dir().join(format!("pn-learn-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("shaders.txt.vertex")).unwrap();
        // the program memory holds the longer binary and what was left after it
        let mut words = vec![0u32; 4096 + 128];
        words[..4].copy_from_slice(&[1, 2, 3, 9]);
        words[4096] = 7;
        std::fs::write(dir.join("shaders.txt.vertex").join("00000000000000ab.pica"), words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
        std::fs::write(dir.join("shaders.txt"), "zakuro-gles 5\nab:1c:0:1 1 c0=f\ncd:0:0:1 1 c0=e\n0 2 c0=d\n").unwrap();
        let (lines, unknown) = crate::precache::learn(&found, &dir.join("shaders.txt")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(unknown, 1);
        assert_eq!(lines.into_iter().collect::<Vec<_>>(), vec!["0 2 c0=d".to_owned(), format!("{:016x}:1c 1 c0=f", found[1].key())]);
    }

    #[test]
    fn a_narc_gives_its_files() {
        // two files, "ab" and "cde", after a name table with nothing in it
        let mut data = b"NARC\xFE\xFF\x00\x01\0\0\0\0\x10\0\x03\0".to_vec();
        data.extend(b"BTAF\x1C\0\0\0\x02\0\0\0");
        data.extend([0u32, 2, 2, 5].iter().flat_map(|word| word.to_le_bytes()));
        data.extend(b"BTNF\x08\0\0\0GMIF\x0D\0\0\0abcde");
        let image = data.len() - 5;
        assert_eq!(narc(&data), Some(vec![(image, image + 2), (image + 2, image + 5)]));
    }

    #[test]
    fn lz11_unpacks_literals_and_copies() {
        // "abcabcabc": three literals, then a copy of six from three back
        let packed = [0x11, 9, 0, 0, 0b0001_0000, b'a', b'b', b'c', 0x50, 0x02];
        assert_eq!(lz11(&packed).as_deref(), Some(&b"abcabcabc"[..]));
    }
}
