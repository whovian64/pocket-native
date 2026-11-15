//! the menu's text, from the atlas tools/bake_font.py makes, and the overlay
//! it is drawn into.

use std::collections::HashMap;
use std::sync::Arc;

use zakuro_gpu::{Overlay, OverlayMesh, OverlayTexture, OverlayVertex};

const FONT: &[u8] = include_bytes!("../../assets/fonts/menu.font");
const LOGO: &[u8] = include_bytes!("../../assets/logo/mono.alpha");
/// the overlay's textures: one white texel for the boxes, the atlas, and a
/// white disc for round buttons.
const WHITE: u64 = 1;
const ATLAS: u64 = 2;
const DISC: u64 = 3;
const DISC_SIZE: u32 = 64;
const LOGO_ID: u64 = 4;

#[derive(Clone, Copy)]
struct Glyph {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    left: i16,
    top: i16,
    advance: u16,
}

pub struct Face {
    glyphs: HashMap<char, Glyph>,
    pub ascent: f32,
}

pub struct Font {
    pub faces: Vec<Face>,
    size: [u32; 2],
    /// the atlas as premultiplied white.
    pixels: Arc<Vec<u8>>,
    /// the disc as premultiplied white, its edge smoothed.
    disc: Arc<Vec<u8>>,
}

fn read<const N: usize>(data: &[u8], at: &mut usize) -> [u8; N] {
    let bytes = data[*at..*at + N].try_into().unwrap();
    *at += N;
    bytes
}

impl Font {
    pub fn load() -> Font {
        assert_eq!(&FONT[..4], b"PNF1", "the menu font is not one tools/bake_font.py made");
        let mut at = 4;
        let u16_at = |at: &mut usize| u16::from_le_bytes(read(FONT, at));
        let (width, height, count) = (u16_at(&mut at), u16_at(&mut at), u16_at(&mut at));
        let mut faces = Vec::new();
        for _ in 0..count {
            let (_, _, ascent, glyphs) = (u16_at(&mut at), u16_at(&mut at), u16_at(&mut at), u16_at(&mut at));
            let mut table = HashMap::new();
            for _ in 0..glyphs {
                let code = u32::from_le_bytes(read(FONT, &mut at));
                let [x, y, w, h] = [0; 4].map(|_| u16_at(&mut at));
                let (left, top) = (i16::from_le_bytes(read(FONT, &mut at)), i16::from_le_bytes(read(FONT, &mut at)));
                let advance = u16_at(&mut at);
                if let Some(char) = char::from_u32(code) {
                    table.insert(char, Glyph { x, y, width: w, height: h, left, top, advance });
                }
            }
            faces.push(Face { glyphs: table, ascent: ascent as f32 });
        }
        let pixels = FONT[at..at + width as usize * height as usize].iter().flat_map(|&alpha| [alpha; 4]).collect();
        let middle = DISC_SIZE as f32 / 2.0;
        let disc = (0..DISC_SIZE * DISC_SIZE)
            .flat_map(|at| {
                let (x, y) = ((at % DISC_SIZE) as f32 + 0.5 - middle, (at / DISC_SIZE) as f32 + 0.5 - middle);
                [((middle - x.hypot(y)).clamp(0.0, 1.0) * 255.0) as u8; 4]
            })
            .collect();
        Font { faces, size: [width as u32, height as u32], pixels: Arc::new(pixels), disc: Arc::new(disc) }
    }
}

impl Face {
    pub fn width(&self, text: &str) -> f32 {
        text.chars().map(|char| self.glyphs.get(&char).map_or(0.0, |glyph| glyph.advance as f32)).sum()
    }
}

/// boxes and text over the screens, in panel pixels from the top left.
pub struct Canvas<'a> {
    font: &'a Font,
    boxes: OverlayMesh,
    discs: OverlayMesh,
    text: OverlayMesh,
    logo: Option<OverlayMesh>,
}

fn quad(mesh: &mut OverlayMesh, [left, top, right, bottom]: [f32; 4], [u0, v0, u1, v1]: [f32; 4], color: [u8; 4]) {
    let first = mesh.vertices.len() as u32;
    for (position, uv) in [([left, top], [u0, v0]), ([right, top], [u1, v0]), ([right, bottom], [u1, v1]), ([left, bottom], [u0, v1])] {
        mesh.vertices.push(OverlayVertex { position, uv, color });
    }
    mesh.indices.extend([first, first + 1, first + 2, first, first + 2, first + 3]);
}

/// a color with its alpha, premultiplied as the overlay takes them.
pub fn rgba([r, g, b]: [u8; 3], alpha: u8) -> [u8; 4] {
    let scale = |c: u8| (c as u32 * alpha as u32 / 255) as u8;
    [scale(r), scale(g), scale(b), alpha]
}

impl<'a> Canvas<'a> {
    pub fn new(font: &'a Font, (width, height): (u32, u32)) -> Canvas<'a> {
        let mesh = |texture| OverlayMesh { texture, clip: [0, 0, width, height], vertices: Vec::new(), indices: Vec::new() };
        Canvas { font, boxes: mesh(WHITE), discs: mesh(DISC), text: mesh(ATLAS), logo: None }
    }

    pub fn measure(&self, face: usize, text: &str) -> f32 {
        self.font.faces[face].width(text)
    }

    pub fn ascent(&self, face: usize) -> f32 {
        self.font.faces[face].ascent
    }

    pub fn disc(&mut self, [x, y]: [f32; 2], radius: f32, color: [u8; 4]) {
        quad(&mut self.discs, [x - radius, y - radius, x + radius, y + radius], [0.0, 0.0, 1.0, 1.0], color);
    }

    pub fn fill(&mut self, area: [f32; 4], color: [u8; 4]) {
        quad(&mut self.boxes, area, [0.0, 0.0, 1.0, 1.0], color);
    }

    /// a frame width pixels wide inside the area.
    pub fn outline(&mut self, [left, top, right, bottom]: [f32; 4], width: f32, color: [u8; 4]) {
        for side in [[left, top, right, top + width], [left, bottom - width, right, bottom], [left, top, left + width, bottom], [right - width, top, right, bottom]] {
            self.fill(side, color);
        }
    }

    /// text from x, its top at y, in face size, right aligned at x when
    /// right is set.
    pub fn text(&mut self, face: usize, text: &str, x: f32, y: f32, color: [u8; 4], right: bool) {
        let face = &self.font.faces[face];
        let [width, height] = self.font.size.map(|size| size as f32);
        let mut pen = if right { x - face.width(text) } else { x };
        let baseline = (y + face.ascent).round();
        for char in text.chars() {
            let Some(&glyph) = face.glyphs.get(&char) else { continue };
            if glyph.width > 0 {
                let left = (pen + glyph.left as f32).round();
                let top = baseline + glyph.top as f32;
                let (u, v) = (glyph.x as f32 / width, glyph.y as f32 / height);
                let (du, dv) = (glyph.width as f32 / width, glyph.height as f32 / height);
                quad(&mut self.text, [left, top, left + glyph.width as f32, top + glyph.height as f32], [u, v, u + du, v + dv], color);
            }
            pen += glyph.advance as f32;
        }
    }

    /// the monochrome logo's size, in panel pixels as drawn.
    pub fn logo_size() -> [f32; 2] {
        [u16::from_le_bytes([LOGO[0], LOGO[1]]) as f32, u16::from_le_bytes([LOGO[2], LOGO[3]]) as f32]
    }

    /// the monochrome logo in color, its top left at x, y.
    pub fn logo(&mut self, x: f32, y: f32, color: [u8; 4]) {
        let [width, height] = Canvas::logo_size();
        let clip = self.boxes.clip;
        let mesh = self.logo.get_or_insert_with(|| OverlayMesh { texture: LOGO_ID, clip, vertices: Vec::new(), indices: Vec::new() });
        quad(mesh, [x, y, x + width, y + height], [0.0, 0.0, 1.0, 1.0], color);
    }

    pub fn finish(self) -> Overlay {
        let mut textures = vec![
            OverlayTexture { id: WHITE, offset: None, size: [1, 1], pixels: vec![255; 4], linear: false },
            OverlayTexture { id: ATLAS, offset: None, size: self.font.size, pixels: self.font.pixels.to_vec(), linear: false },
            OverlayTexture { id: DISC, offset: None, size: [DISC_SIZE; 2], pixels: self.font.disc.to_vec(), linear: true },
        ];
        let mut meshes = vec![self.boxes, self.discs, self.text];
        if let Some(logo) = self.logo {
            let [width, height] = Canvas::logo_size().map(|side| side as u32);
            textures.push(OverlayTexture { id: LOGO_ID, offset: None, size: [width, height], pixels: LOGO[4..].iter().flat_map(|&alpha| [alpha; 4]).collect(), linear: false });
            meshes.push(logo);
        }
        Overlay { textures, meshes, free: Vec::new(), whole: false, bottom: false }
    }
}
