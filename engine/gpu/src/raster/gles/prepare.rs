//! the programs a start prepares before the game runs: those the game
//! built on earlier runs, the kit's vertex list and its precache list, this
//! last one handed out a little each frame for the start's screen.

use super::*;

/// the precache list a start hands out: the vertex list's shaders by
/// file:entry, as read, translated and as stages, the lines left, and where
/// to note it built.
pub(super) struct Precache {
    dir: std::path::PathBuf,
    listed: HashMap<String, Vec<String>>,
    units: HashMap<String, Option<ShaderUnit>>,
    stages: HashMap<String, Option<u64>>,
    lines: std::collections::VecDeque<String>,
    stamp: Option<std::path::PathBuf>,
    count: usize,
    started: std::time::Instant,
}

/// a setup as the manifest keeps it, flags and the registers its program
/// comes from: the program's fragment constants, whether it writes depth
/// and its lighting's shape, None for lighting that is off.
fn setup<'a>(mut fields: impl Iterator<Item = &'a str>) -> Option<([u32; FRAGMENT_CONSTANTS], bool, Vec<u32>)> {
    let flags = u32::from_str_radix(fields.next()?, 16).ok()?;
    let mut r = [0u32; 0x300];
    for (register, value) in fields.filter_map(|field| field.split_once('=')) {
        if let (Ok(register @ 0..0x300), Ok(value)) = (usize::from_str_radix(register, 16), u32::from_str_radix(value, 16)) {
            r[register] = value;
        }
    }
    let mut packed = Vec::new();
    if flags & 1 << 17 != 0 {
        crate::lighting::Lighting::read(&r)?.pack(&mut packed);
    }
    let (writes_depth, sources) = (flags & 1 << 16 != 0, std::array::from_fn(|unit| flags & 1 << (8 + unit) != 0));
    let (constants, lighting) = fragment_constants(&r, flags & 7, flags >> 4 & 7, sources, flags >> 12 & 3, writes_depth, (flags & 1 << 17 != 0).then_some(&packed[..]));
    Some((constants, writes_depth, lighting))
}

impl Gles {
    pub(super) fn preload(&mut self) {
        let Some(path) = manifest_path() else { return };
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        if text.lines().next() != Some(MANIFEST_HEADER) {
            log::info!("{} is from another version, starting it over", path.display());
            let _ = std::fs::remove_file(&path);
            let _ = vertex_path(&path, 0).parent().map(std::fs::remove_dir_all);
            return;
        }
        let started = std::time::Instant::now();
        self.preloading = true;
        // the compiler threads take the list, the GL thread left free for
        // the frontend to show them building, which it waits on before the
        // game starts
        let queue = self.compiler.is_some();
        let (mut translating, mut programs) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        let mut units: HashMap<u64, Option<ShaderUnit>> = HashMap::new();
        let mut count = 0;
        for line in text.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let vertex = match fields.next() {
                Some("0") => 0,
                Some(field) => {
                    let Ok(parts) = field.split(':').map(|part| u64::from_str_radix(part, 16)).collect::<Result<Vec<u64>, _>>() else { continue };
                    let Some((&[fingerprint, entry], semantics)) = parts.split_first_chunk::<2>() else { continue };
                    let Ok(semantics) = <[u32; SEMANTICS]>::try_from(semantics.iter().map(|&value| value as u32).collect::<Vec<u32>>()) else { continue };
                    let unit = units.entry(fingerprint).or_insert_with(|| {
                        let bytes = std::fs::read(vertex_path(&path, fingerprint)).ok().filter(|bytes| bytes.len() == (PROGRAM_SIZE + DESCRIPTOR_SIZE) * 4)?;
                        let mut unit = ShaderUnit::new();
                        for (word, chunk) in unit.program.iter_mut().chain(unit.descriptors.iter_mut()).zip(bytes.as_chunks::<4>().0) {
                            *word = u32::from_le_bytes(*chunk);
                        }
                        unit.prepare();
                        Some(unit)
                    });
                    let Some(unit) = unit else { continue };
                    unit.entry_point = entry as u32;
                    let began = std::time::Instant::now();
                    let stage = self.vertex_stage(unit, semantics);
                    translating += began.elapsed();
                    let Ok(hash) = stage else { continue };
                    hash
                }
                None => continue,
            };
            let Some((mut constants, writes_depth, mut lighting)) = setup(fields) else { continue };
            // generic, the draws take the generic program with their vertex
            // program, not the one their setup would have
            if self.generic {
                constants = generic_constants(&constants);
                lighting.clear();
            }
            let began = std::time::Instant::now();
            match self.program(&constants, writes_depth, &lighting, vertex, !queue, None) {
                Ok(_) => count += 1,
                Err(error) => log::warn!("could not prepare a shader from {}, {error}", path.display()),
            }
            programs += began.elapsed();
        }
        self.preloading = false;
        log::info!(
            "{count} shaders from {} to prepare, handed out in {:.1} s: {:.1} s translating vertex programs, {:.1} s handing out programs",
            path.display(),
            started.elapsed().as_secs_f64(),
            translating.as_secs_f64(),
            programs.as_secs_f64(),
        );
    }

    /// generic, the vertex shaders the kit found in the game's files, each
    /// with every generic program, the list ZAKURO_VERTEX_LIST names
    /// (3dsrecomp shaders writes it), so that nothing is built in game.
    pub(super) fn preload_vertex_list(&mut self) {
        let Some(list) = std::env::var_os("ZAKURO_VERTEX_LIST").map(std::path::PathBuf::from).filter(|_| self.generic) else { return };
        let Ok(text) = std::fs::read_to_string(&list) else { return };
        if text.lines().next() != Some("pocket-native vertex 1") {
            return;
        }
        let started = std::time::Instant::now();
        let dir = list.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
        let queue = self.compiler.is_some();
        let mut units: HashMap<String, Option<ShaderUnit>> = HashMap::new();
        let (mut shaders, mut count) = (0, 0);
        self.preloading = true;
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let Some((vertex, lit)) = self.listed_vertex(&dir, &mut units, &fields) else { continue };
            shaders += 1;
            for writes_depth in [false, true] {
                for unbordered in [0, 0x400] {
                    for lighting in if lit { &[0, 1][..] } else { &[0][..] } {
                        let lighting = *lighting;
                        let mut constants = [0u32; FRAGMENT_CONSTANTS];
                        constants[25] = unbordered;
                        constants[26] = lighting;
                        constants[28] = if writes_depth { WRITES_DEPTH } else { 0 };
                        if self.program(&generic_constants(&constants), writes_depth, &[], vertex, !queue, None).is_ok() {
                            count += 1;
                        }
                    }
                }
            }
        }
        self.preloading = false;
        log::info!("{count} generic programs for {shaders} vertex shaders from {} to prepare, handed out in {:.1} s", list.display(), started.elapsed().as_secs_f64());
    }

    /// the shaders the kit worked out from the game's files, the list beside
    /// the vertex list (3dsrecomp shaders with precache writes it): each line
    /// one of the vertex list's shaders, file:entry, and a setup as the
    /// manifest keeps them. what it missed is built as the game needs it.
    pub(super) fn preload_precache(&mut self) {
        let prewarm = std::env::var_os("ZAKURO_SHADER_MANIFEST").is_some();
        let Some(list) = std::env::var_os("ZAKURO_VERTEX_LIST").map(std::path::PathBuf::from).filter(|_| prewarm && !self.generic) else { return };
        let dir = list.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
        let (Ok(text), Ok(vertices)) = (std::fs::read_to_string(dir.join("precache.txt")), std::fs::read_to_string(&list)) else { return };
        if text.lines().next() != Some("pocket-native precache 1") || vertices.lines().next() != Some("pocket-native vertex 1") {
            return;
        }
        // built at an earlier start, its programs saved: the manifest has
        // those the game used, the rest load as it needs them
        let stamp = program_cache_path(&self.name, "precache", &text).map(|path| path.with_extension("precached"));
        if stamp.as_ref().is_some_and(|path| path.exists()) {
            log::info!("the precache list was built at an earlier start, its shaders load as the game needs them");
            return;
        }
        let listed = vertices
            .lines()
            .skip(1)
            .filter_map(|line| {
                let fields: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
                Some((format!("{}:{}", fields.first()?, fields.get(1)?), fields))
            })
            .collect();
        let lines: std::collections::VecDeque<String> = text.lines().skip(1).map(str::to_owned).collect();
        PRECACHE_LEFT.store(lines.len(), std::sync::atomic::Ordering::Relaxed);
        self.precache = Some(Precache { dir, listed, units: HashMap::new(), stages: HashMap::new(), lines, stamp, count: 0, started: std::time::Instant::now() });
    }

    /// hands out the precache list's lines for a while, each one of the
    /// vertex list's shaders, file:entry, and a setup as the manifest keeps
    /// them. what it missed, or a skip left, is built as the game needs it.
    pub(super) fn precache_some(&mut self, budget: std::time::Duration) {
        let Some(mut precache) = self.precache.take() else { return };
        if SKIP.load(std::sync::atomic::Ordering::Relaxed) {
            log::info!("precaching skipped with {} of the list's shaders not handed out", precache.lines.len());
            PRECACHE_LEFT.store(0, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let began = std::time::Instant::now();
        let queue = self.compiler.is_some();
        (self.preloading, self.precaching) = (true, true);
        while began.elapsed() < budget {
            let Some(line) = precache.lines.pop_front() else { break };
            PRECACHE_LEFT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            let mut fields = line.split_whitespace();
            let Some(at) = fields.next() else { continue };
            let stage = match precache.stages.get(at) {
                _ if at == "0" => Some(0),
                Some(&stage) => stage,
                None => {
                    // a program the kit read from the game's words, with the
                    // output map the run that found it saw
                    let parts: Vec<&str> = at.split(':').collect();
                    let stage = if let [file, entry, semantics @ ..] = &parts[..] {
                        let semantics: Option<Vec<u32>> = semantics.iter().map(|value| u32::from_str_radix(value, 16).ok()).collect();
                        match (u32::from_str_radix(entry, 16), semantics.and_then(|semantics| <[u32; SEMANTICS]>::try_from(semantics).ok())) {
                            (Ok(entry), Some(semantics)) => Self::listed_unit(&precache.dir, &mut precache.units, file).and_then(|unit| {
                                unit.entry_point = entry;
                                self.vertex_stage(unit, semantics).ok()
                            }),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    let stage = stage.or_else(|| {
                        let found = precache.listed.get(at).map(|fields| fields.iter().map(String::as_str).collect::<Vec<&str>>());
                        found.and_then(|fields| self.listed_vertex(&precache.dir, &mut precache.units, &fields)).map(|(vertex, _)| vertex)
                    });
                    precache.stages.insert(at.to_owned(), stage);
                    stage
                }
            };
            let rest = fields.clone().collect::<Vec<_>>().join(" ");
            // a line that builds nothing new comes off what the start shows
            let fewer = || _ = PREPARING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            let (Some(vertex), Some((constants, writes_depth, lighting))) = (stage, setup(fields)) else {
                fewer();
                continue;
            };
            // the manifest's own, or an earlier line's
            let key = (constants, writes_depth, lighting, vertex);
            if self.programs.contains_key(&key) || self.precached.contains_key(&key) || self.compiler.as_ref().is_some_and(|compiler| compiler.pending.contains(&key)) {
                fewer();
                continue;
            }
            match self.program(&key.0, key.1, &key.2, key.3, !queue, None) {
                Ok(_) => {
                    precache.count += 1;
                    self.precached.insert(key, rest);
                }
                Err(error) => log::warn!("could not prepare a shader from {}, {error}", precache.dir.join("precache.txt").display()),
            }
        }
        (self.preloading, self.precaching) = (false, false);
        if !precache.lines.is_empty() {
            self.precache = Some(precache);
            return;
        }
        log::info!("{} precached shaders from {} handed out in {:.1} s", precache.count, precache.dir.join("precache.txt").display(), precache.started.elapsed().as_secs_f64());
        if queue {
            self.precache_stamp = precache.stamp;
        } else if let Some(path) = precache.stamp {
            let _ = std::fs::write(path, "");
        }
    }

    /// a program of the kit's vertex list by its file, read once.
    fn listed_unit<'a>(dir: &std::path::Path, units: &'a mut HashMap<String, Option<ShaderUnit>>, file: &str) -> Option<&'a mut ShaderUnit> {
        units
            .entry(file.to_owned())
            .or_insert_with(|| {
                let bytes = std::fs::read(dir.join(format!("{file}.pica"))).ok().filter(|bytes| bytes.len() == (PROGRAM_SIZE + DESCRIPTOR_SIZE) * 4)?;
                let mut unit = ShaderUnit::new();
                for (word, chunk) in unit.program.iter_mut().chain(unit.descriptors.iter_mut()).zip(bytes.as_chunks::<4>().0) {
                    *word = u32::from_le_bytes(*chunk);
                }
                unit.prepare();
                Some(unit)
            })
            .as_mut()
    }

    /// a shader of the kit's vertex list, its file, entry, output mask, total
    /// and map, and whether it hands out the surface's orientation, which lit
    /// draws need.
    fn listed_vertex(&mut self, dir: &std::path::Path, units: &mut HashMap<String, Option<ShaderUnit>>, fields: &[&str]) -> Option<(u64, bool)> {
        let [file, entry, mask, total, map @ ..] = fields else { return None };
        let hex = |field: &str| u32::from_str_radix(field, 16).ok();
        let (Some(entry), Some(mask), Ok(total)) = (hex(entry), hex(mask), total.parse::<u32>()) else { return None };
        if map.len() != 7 {
            return None;
        }
        let unit = Self::listed_unit(dir, units, file)?;
        unit.entry_point = entry;
        let mut registers = [0u32; 0x300];
        registers[crate::registers::REG_SHADER_OUTPUT_TOTAL] = total;
        registers[crate::registers::REG_VS_OUTPUT_MASK] = mask;
        for (register, word) in map.iter().enumerate() {
            registers[crate::registers::REG_SHADER_OUTPUT_MAP + register] = hex(word).unwrap_or(0x1F1F_1F1F);
        }
        let vertex = self.vertex_stage(unit, crate::raster::output_semantics(&registers)).ok()?;
        let lit = map.iter().filter_map(|word| hex(word)).any(|word| word.to_le_bytes().iter().any(|&semantic| (4..=7).contains(&(semantic & 0x1F))));
        Some((vertex, lit))
    }
}
