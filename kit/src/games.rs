//! the games the kit finds: a file's title id, its name from the kit's
//! profiles or the game's own icon data, its icon, and what the
//! compatibility list says about it.

use std::path::{Path, PathBuf};

use zakuro_fs::{Cia, ExeFs, NcchHeader, RomImage};

/// the files the build takes.
pub const EXTENSIONS: [&str; 6] = ["3ds", "cci", "cia", "zcci", "zcia", "cxi"];

pub struct Game {
    pub path: PathBuf,
    pub id: Option<u64>,
    pub name: String,
    pub size: u64,
    /// the compatibility list's status for it, "Playable" and the like.
    pub status: Option<String>,
    /// encrypted, which the build decrypts with the console's aes_keys.txt.
    pub encrypted: bool,
    /// 48x48 RGBA, from a decrypted game's icon data.
    pub icon: Option<Vec<u8>>,
    pub selected: bool,
}

pub fn is_game(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// every game file in a folder and the folders under it, a few deep.
pub fn scan(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && depth > 0 {
                walk(&path, depth - 1, found);
            } else if is_game(&path) {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, 3, &mut found);
    found.sort();
    found
}

/// what the kit knows about a game file.
pub fn detect(path: &Path, kit: &Path, list: &[(String, String)]) -> Game {
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    let compressed = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("zcci") || e.eq_ignore_ascii_case("zcia"));
    let (id, ncch) = if compressed { (None, None) } else { header(path) };
    let encrypted = ncch.as_ref().is_some_and(|(header, _)| !header.is_decrypted());
    let smdh = ncch.as_ref().filter(|(header, _)| header.is_decrypted()).and_then(|(header, offset)| smdh(path, header, *offset));
    let profile = id.and_then(|id| profile_name(kit, id));
    let name = profile
        .or_else(|| smdh.as_ref().map(|(name, _)| name.clone()))
        .unwrap_or_else(|| path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned()));
    let status = status_of(&name, list);
    Game { path: path.to_path_buf(), id, name, size, status, encrypted, icon: smdh.map(|(_, icon)| icon), selected: true }
}

/// the title id and the executable's header with where it starts, for a
/// cartridge dump or a .cia.
fn header(path: &Path) -> (Option<u64>, Option<(NcchHeader, u64)>) {
    let Ok(image) = RomImage::open(path) else { return (None, None) };
    let data = image.data();
    if zakuro_fs::ncsd::Ncsd::detect(data) {
        let Ok(ncsd) = zakuro_fs::ncsd::Ncsd::parse(data) else { return (None, None) };
        let ncch = ncsd.executable_partition().ok().and_then(|part| {
            let at = part.offset as usize;
            data.get(at..).and_then(|rest| NcchHeader::parse(rest).ok()).map(|header| (header, part.offset))
        });
        (Some(ncsd.media_id), ncch)
    } else if Cia::detect(data) {
        let Ok(cia) = Cia::parse(data) else { return (None, None) };
        let at = cia.executable.0 as usize;
        let ncch = data.get(at..).and_then(|rest| NcchHeader::parse(rest).ok()).map(|header| (header, cia.executable.0));
        (Some(cia.title_id), ncch)
    } else if NcchHeader::detect(data) {
        let ncch = NcchHeader::parse(data).ok();
        (ncch.as_ref().map(|header| header.program_id), ncch.map(|header| (header, 0)))
    } else {
        (None, None)
    }
}

/// the English name and the big icon in a decrypted game's icon data.
fn smdh(path: &Path, header: &NcchHeader, offset: u64) -> Option<(String, Vec<u8>)> {
    let image = RomImage::open(path).ok()?;
    let data = image.data();
    let exefs_at = (offset + header.exefs_offset) as usize;
    let exefs = ExeFs::parse(data.get(exefs_at..)?).ok()?;
    let entry = exefs.find("icon")?;
    let start = exefs_at + 0x200 + entry.offset as usize;
    let icon = data.get(start..start + 0x36C0)?;
    if &icon[0..4] != b"SMDH" {
        return None;
    }
    // English is the second of the sixteen titles, its short name first
    let short = &icon[0x8 + 0x200..0x8 + 0x200 + 0x80];
    let units: Vec<u16> = short.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    let name = String::from_utf16_lossy(&units).replace('\n', " ");
    Some((name, decode_icon(&icon[0x24C0..0x24C0 + 48 * 48 * 2])))
}

/// a 48x48 RGB565 icon in 8x8 tiles, each in Z order, as RGBA.
fn decode_icon(pixels: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 48 * 48 * 4];
    for (index, color) in pixels.chunks_exact(2).enumerate() {
        let color = u16::from_le_bytes([color[0], color[1]]);
        let (tile, inside) = (index / 64, index % 64);
        let x = (tile % 6) * 8 + (inside & 1 | (inside >> 1) & 2 | (inside >> 2) & 4);
        let y = (tile / 6) * 8 + ((inside >> 1) & 1 | (inside >> 2) & 2 | (inside >> 3) & 4);
        let at = (y * 48 + x) * 4;
        let (r, g, b) = ((color >> 11) & 0x1F, (color >> 5) & 0x3F, color & 0x1F);
        out[at..at + 4].copy_from_slice(&[(r << 3 | r >> 2) as u8, (g << 2 | g >> 4) as u8, (b << 3 | b >> 2) as u8, 255]);
    }
    out
}

/// the name in the kit's profile for the title.
fn profile_name(kit: &Path, id: u64) -> Option<String> {
    let text = std::fs::read_to_string(kit.join("titles").join(format!("{id:016X}")).join("profile.toml")).ok()?;
    text.lines().find_map(|line| line.strip_prefix("name = \"")?.strip_suffix('"').map(str::to_owned))
}

/// the compatibility list's games and their status, lowercased names first.
pub fn compatibility(kit: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(kit.join("docs").join("compatibility.md")) else { return Vec::new() };
    text.lines()
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            (cells.len() > 3 && !cells[1].is_empty() && cells[1] != "Game" && !cells[1].starts_with("---")).then(|| (cells[1].to_lowercase(), cells[2].to_owned()))
        })
        .collect()
}

/// a game's status, by its name, or its name with Pokémon left off, in a
/// row that names it among others ("Pokémon Sun and Moon").
fn status_of(name: &str, list: &[(String, String)]) -> Option<String> {
    let name = name.to_lowercase();
    let short = name.strip_prefix("pokémon ").unwrap_or(&name);
    list.iter().find(|(game, _)| *game == name).or_else(|| list.iter().find(|(game, _)| game.split(" and ").any(|part| part.trim_start_matches("pokémon ") == short))).map(|(_, status)| status.clone())
}

pub fn size_text(size: u64) -> String {
    if size >= 1 << 30 {
        format!("{:.1} GB", size as f64 / (1u64 << 30) as f64)
    } else {
        format!("{} MB", size >> 20)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_finds_its_row_among_others() {
        let list = vec![("pokémon sun and moon".to_owned(), "Playable".to_owned()), ("pokémon ultra sun and ultra moon".to_owned(), "Starts".to_owned()), ("mario kart 7".to_owned(), "Playable".to_owned())];
        assert_eq!(status_of("Pokémon Moon", &list).as_deref(), Some("Playable"));
        assert_eq!(status_of("Pokémon Ultra Moon", &list).as_deref(), Some("Starts"));
        assert_eq!(status_of("Mario Kart 7", &list).as_deref(), Some("Playable"));
        assert_eq!(status_of("Tetris", &list), None);
    }

    #[test]
    fn an_icon_tile_unfolds_in_z_order() {
        let mut pixels = vec![0u8; 48 * 48 * 2];
        // the fourth texel of the first tile is (1, 1)
        pixels[3 * 2..3 * 2 + 2].copy_from_slice(&0xF800u16.to_le_bytes());
        let icon = decode_icon(&pixels);
        assert_eq!(&icon[(48 + 1) * 4..(48 + 1) * 4 + 4], &[255, 0, 0, 255]);
    }
}
