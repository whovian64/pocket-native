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

/// files up to this size are read whole and searched.
const LARGEST: u64 = 2 << 30;

pub fn find(title: &Title) -> Vec<Shbin> {
    let mut found = Vec::new();
    if let Ok(code) = title.code() {
        search(&code, "code", &mut found, 0);
    }
    if let Some(romfs) = &title.romfs {
        if let Ok(root) = romfs.root() {
            walk(title, &root, String::new(), &mut found);
        }
    }
    let mut seen = HashSet::new();
    found.retain(|shbin| seen.insert((shbin.code.clone(), shbin.descriptors.clone())));
    found
}

fn walk(title: &Title, dir: &DirEntry, path: String, found: &mut Vec<Shbin>) {
    let Some(romfs) = &title.romfs else { return };
    for (_, file) in romfs.files(dir) {
        if file.data_size > LARGEST {
            continue;
        }
        if let Some(data) = title.read_romfs(&file, 0, file.data_size as usize) {
            search(&data, &format!("{path}/{}", file.name), found, 0);
        }
    }
    for (_, sub) in romfs.subdirs(dir) {
        let name = format!("{path}/{}", sub.name);
        walk(title, &sub, name, found);
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn search(data: &[u8], source: &str, found: &mut Vec<Shbin>, depth: u32) {
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
        search(&plain, &format!("{source} ({how})"), found, depth + 1);
        return;
    }
    let members = match &data[0..4] {
        b"SARC" => sarc(data),
        b"darc" => darc(data),
        b"CRAG" => garc(data),
        _ => mini(data),
    };
    if let Some(members) = members {
        for (index, (start, end)) in members.into_iter().enumerate() {
            if let Some(member) = data.get(start..end) {
                search(member, &format!("{source}#{index}"), found, depth + 1);
            }
        }
        return;
    }
    let mut at = 0;
    while let Some(next) = data[at..].windows(4).position(|window| window == b"DVLB") {
        if let Some(shbin) = dvlb(&data[at + next..], source) {
            found.push(shbin);
        }
        at += next + 4;
    }
}

fn dvlb(data: &[u8], source: &str) -> Option<Shbin> {
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
    fn lz11_unpacks_literals_and_copies() {
        // "abcabcabc": three literals, then a copy of six from three back
        let packed = [0x11, 9, 0, 0, 0b0001_0000, b'a', b'b', b'c', 0x50, 0x02];
        assert_eq!(lz11(&packed).as_deref(), Some(&b"abcabcabc"[..]));
    }
}
