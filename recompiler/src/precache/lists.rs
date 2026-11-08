//! titles' shader lists, learned from what a run of the game built: each
//! line a setup and its vertex program, named by the key of the title's
//! shader binary it's in, or by where its words are in the title's files.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use super::u32_at;
use crate::rom::Title;
use crate::shaders::{self, Shbin};

/// a title's list: setups a run of the game used, each with its vertex
/// shader by the binary's key and where the shader starts in it.
pub const TITLE_HEADER: &str = "pocket-native title shaders 1";

/// what goes through a title's files, each with where it came from.
pub type Files<'a> = dyn Fn(&mut dyn FnMut(&[u8], &str)) + 'a;

/// the lines of lists whose vertex programs the title has into lines, the
/// first list being the title's own: its lines first, then the other
/// titles'.
pub(super) fn add(title: &Title, found: &[Shbin], dir: &Path, lists: &[&Path], lines: &mut Vec<(String, usize)>) -> std::io::Result<()> {
    let files: HashMap<String, usize> = found.iter().enumerate().map(|(n, shbin)| (format!("{:016x}", shbin.key()), n)).collect();
    let raw = raw_programs(title, found.len(), dir, lists)?;
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
                Some((key, rest)) if key.starts_with('r') => raw.get(key).map(|n| format!("{n}:{rest}")),
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
    Ok(())
}

/// the lists' programs the title holds with no DVLB, each read from where
/// its line says and checked against its key, written into dir after
/// found's as the vertex list writes those: by line key, its file number.
fn raw_programs(title: &Title, found: usize, dir: &Path, lists: &[&Path]) -> std::io::Result<HashMap<String, usize>> {
    let mut wanted: BTreeMap<String, (String, usize, usize, usize, usize, u64)> = BTreeMap::new();
    for path in lists {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for line in text.lines().skip(1) {
            let Some(key) = line.split([' ', ':']).next().filter(|key| key.starts_with('r')) else { continue };
            let mut parts = key.split('@');
            let (Some(hash), Some(source), Some(code), Some(descriptors)) = (parts.next(), parts.next(), parts.next(), parts.next()) else { continue };
            let span = |field: &str| field.split_once('+').and_then(|(at, count)| Some((usize::from_str_radix(at, 16).ok()?, usize::from_str_radix(count, 16).ok()?)));
            let (Some((code, length)), Some((at, count)), Ok(hash)) = (span(code), span(descriptors), u64::from_str_radix(&hash[1..], 16)) else { continue };
            wanted.insert(key.to_owned(), (source.replace("%20", " ").replace("%25", "%"), code, length.min(4096), at, count.min(128), hash));
        }
    }
    let mut out = HashMap::new();
    if wanted.is_empty() {
        return Ok(out);
    }
    let sources: HashSet<String> = wanted.values().map(|(source, ..)| source.clone()).collect();
    let mut programs: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    shaders::files(title, &mut |data, source| {
        if !sources.contains(source) {
            return;
        }
        for (key, (from, code, length, at, count, hash)) in &wanted {
            let words = |at: usize, count: usize| (0..count).map(|k| u32_at(data, at + 4 * k)).collect::<Option<Vec<u32>>>();
            if from != source || programs.contains_key(key) {
                continue;
            }
            let (Some(code_words), Some(descriptor_words)) = (words(*code, *length), words(*at, *count)) else { continue };
            if raw_key(&code_words, &descriptor_words) == *hash {
                let mut memory = vec![0u32; 4096 + 128];
                memory[..code_words.len()].copy_from_slice(&code_words);
                memory[4096..4096 + descriptor_words.len()].copy_from_slice(&descriptor_words);
                programs.insert(key.clone(), memory);
            }
        }
    });
    for (key, memory) in programs {
        let n = found + out.len();
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(format!("{n}.pica")), memory.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<u8>>())?;
        out.insert(key, n);
    }
    Ok(out)
}

/// a run's shaders.txt, with the vertex programs it kept beside it, as a
/// title's list: each line's vertex program found among the title's shader
/// binaries and named by its key, else among the words of its files, named by
/// where they are and a key of them, with the output map the run saw. the
/// lines and how many had a program neither is.
pub fn learn(files: &Files, found: &[Shbin], recorded: &Path) -> std::io::Result<(BTreeSet<String>, usize)> {
    let text = std::fs::read_to_string(recorded)?;
    let mut vertex_dir = recorded.as_os_str().to_owned();
    vertex_dir.push(".vertex");
    let vertex_dir = std::path::PathBuf::from(vertex_dir);
    let mut keys: HashMap<String, Option<u64>> = HashMap::new();
    let mut memories: HashMap<String, (Vec<u32>, Vec<u32>)> = HashMap::new();
    let mut raw: Vec<(String, String, String)> = Vec::new();
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
            let number = u64::from_str_radix(fingerprint, 16).ok()?;
            let bytes = std::fs::read(vertex_dir.join(format!("{number:016x}.pica"))).ok()?;
            let words: Vec<u32> = bytes.as_chunks::<4>().0.iter().map(|chunk| u32::from_le_bytes(*chunk)).collect();
            let (program, descriptors) = words.split_at_checked(4096)?;
            let key = found.iter().filter(|shbin| program.starts_with(&shbin.code) && descriptors.starts_with(&shbin.descriptors)).max_by_key(|shbin| shbin.code.len()).map(Shbin::key);
            if key.is_none() {
                memories.insert(fingerprint.to_owned(), (program.to_vec(), descriptors.to_vec()));
            }
            key
        });
        match key {
            Some(key) => _ = lines.insert(format!("{key:016x}:{entry} {setup}")),
            None if memories.contains_key(fingerprint) => raw.push((fingerprint.to_owned(), vertex.split_once(':').map_or("", |(_, rest)| rest).to_owned(), setup.to_owned())),
            None => unknown += 1,
        }
    }
    // programs the title uploads from its code or files with no DVLB around
    // them: where their words are, found once for all of them
    let mut located: HashMap<String, String> = HashMap::new();
    if !raw.is_empty() {
        files(&mut |data, source| {
            for (fingerprint, (program, descriptors)) in &memories {
                if located.contains_key(fingerprint) {
                    continue;
                }
                let words = |at: usize, count: usize| (0..count).map(|k| u32_at(data, at + 4 * k)).collect::<Option<Vec<u32>>>();
                let head: Vec<u8> = program[..8].iter().flat_map(|word| word.to_le_bytes()).collect();
                let Some(code) = data.windows(32).position(|window| window == head.as_slice()) else { continue };
                let length = (0..4096).take_while(|&k| u32_at(data, code + 4 * k) == Some(program[k])).count();
                let head: Vec<u8> = descriptors[..4].iter().flat_map(|word| word.to_le_bytes()).collect();
                let Some(at) = data.windows(16).position(|window| window == head.as_slice()) else { continue };
                let count = (0..128).take_while(|&k| u32_at(data, at + 4 * k) == Some(descriptors[k])).count();
                let (Some(code_words), Some(descriptor_words)) = (words(code, length), words(at, count)) else { continue };
                let key = raw_key(&code_words, &descriptor_words);
                located.insert(fingerprint.clone(), format!("r{key:016x}@{}@{code:x}+{length:x}@{at:x}+{count:x}", source.replace('%', "%25").replace(' ', "%20")));
            }
        });
    }
    for (fingerprint, rest, setup) in raw {
        match located.get(&fingerprint) {
            Some(at) => _ = lines.insert(format!("{at}:{rest} {setup}")),
            None => unknown += 1,
        }
    }
    Ok((lines, unknown))
}

/// a key of a program's words and its descriptors, as Shbin::key makes.
pub(crate) fn raw_key(code: &[u32], descriptors: &[u32]) -> u64 {
    code.iter().chain(descriptors).flat_map(|word| word.to_le_bytes()).fold(0xCBF2_9CE4_8422_2325, |hash, byte| (hash ^ byte as u64).wrapping_mul(0x100_0000_01B3))
}
