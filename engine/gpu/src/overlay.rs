//! what is drawn over the screens.

/// what is drawn over the screens, a user interface, as textured triangles
/// in window pixels.
#[derive(Default)]
pub struct Overlay {
    /// textures to make or change before drawing.
    pub textures: Vec<OverlayTexture>,
    pub meshes: Vec<OverlayMesh>,
    /// textures that go away once this frame is drawn.
    pub free: Vec<u64>,
    /// in the layout's size, over all of it, rather than in the menu's
    /// 640x480 in the middle.
    pub whole: bool,
    /// over the bottom screen, wherever it is shown, its 640x480 twice the
    /// screen's size, rather than in the middle.
    pub bottom: bool,
}

/// where 640x480 sits fitted into an area of the layout, centred: left, top
/// and how many times over.
pub fn fitted([left, top, width, height]: [i32; 4]) -> (f32, f32, f32) {
    let scale = (width as f32 / 640.0).min(height as f32 / 480.0);
    (left as f32 + (width as f32 - 640.0 * scale) / 2.0, top as f32 + (height as f32 - 480.0 * scale) / 2.0, scale)
}

/// pixels for a texture of the overlay.
pub struct OverlayTexture {
    pub id: u64,
    /// where the pixels go in a texture that exists, none to make it anew at
    /// this size.
    pub offset: Option<[u32; 2]>,
    pub size: [u32; 2],
    /// RGBA, premultiplied by alpha, in sRGB.
    pub pixels: Vec<u8>,
    /// filtered when scaled, rather than nearest.
    pub linear: bool,
}

/// one corner of an overlay triangle.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct OverlayVertex {
    /// window pixels from the top left.
    pub position: [f32; 2],
    pub uv: [f32; 2],
    /// premultiplied sRGB.
    pub color: [u8; 4],
}

/// triangles of the overlay that share a texture and a clip.
pub struct OverlayMesh {
    pub texture: u64,
    /// the part of the window they may draw in, left, top, right, bottom.
    pub clip: [u32; 4],
    pub vertices: Vec<OverlayVertex>,
    pub indices: Vec<u32>,
}
