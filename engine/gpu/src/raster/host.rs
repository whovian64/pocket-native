//! what the software rasterizer hands the GPU renderer: draws, the vertices
//! it shades and display transfers.

use super::{BoundTexture, DepthMap, Screen};
use crate::format::ColorFormat;
use crate::lighting::{Lighting, Tables};
use crate::proctex;
use crate::shader::{ShaderUnit, Vec4, INPUT_REGISTERS};

/// a semantic no output register carries, which takes its default.
pub(super) const MISSING: u32 = u32::MAX;
/// a semantic in an output attribute past the enabled registers, zero.
pub(super) const ZERO: u32 = u32::MAX - 1;

/// what a draw needs from the rasterizer.
pub(super) struct Draw<'a> {
    pub(super) registers: &'a [u32],
    pub(super) target: u32,
    pub(super) format: ColorFormat,
    pub(super) width: u32,
    pub(super) height: u32,
    /// the pixels the draw may touch, left, bottom, right and top in window
    /// coordinates, y up.
    pub(super) scissor: [i32; 4],
    /// the depth and stencil buffer and its bytes per sample, when the draw
    /// uses one.
    pub(super) depth: Option<(u32, u32)>,
    pub(super) depth_map: DepthMap,
    pub(super) geometry: Geometry<'a>,
    pub(super) textures: &'a [Option<BoundTexture>; 3],
    pub(super) lighting: Option<&'a Lighting>,
    pub(super) tables: &'a Tables,
    /// whether the procedural texture is on, and its tables.
    pub(super) proctex: bool,
    pub(super) proctex_tables: &'a proctex::Tables,
}

/// the triangles of a draw.
#[derive(Clone, Copy)]
pub(super) enum Geometry<'a> {
    /// shaded, clipped and culled on the CPU.
    Triangles(&'a [[Screen; 3]]),
    /// for the GPU to shade.
    Shaded(&'a Shading<'a>),
}

/// vertices the GPU runs the vertex shader over.
pub(super) struct Shading<'a> {
    pub(super) unit: &'a ShaderUnit,
    /// each vertex's input registers.
    pub(super) inputs: &'a [[Vec4; INPUT_REGISTERS]],
    /// three inputs a triangle.
    pub(super) indices: &'a [u32],
    /// where each varying is in the output registers.
    pub(super) semantics: [u32; 24],
    /// left, bottom, width and height.
    pub(super) viewport: (f32, f32, f32, f32),
    /// the face culling register.
    pub(super) cull: u32,
    pub(super) fetched: Option<&'a Fetched>,
    /// the indices as the bytes a GPU reads and the highest, kept with the
    /// mesh, for a list fetched whole.
    pub(super) prepared: Option<(&'a std::sync::Arc<[u8]>, u32)>,
}

#[derive(Clone, Copy)]
pub(super) enum Input {
    Constant(Vec4),
    Array { array: usize, offset: u32, ty: u32, count: u32 },
}

pub(super) struct FetchedArray {
    pub(super) bytes: std::sync::Arc<[u8]>,
    pub(super) stride: u32,
}

pub(super) struct Fetched {
    pub(super) arrays: Vec<FetchedArray>,
    pub(super) inputs: [Input; INPUT_REGISTERS],
    pub(super) first: u32,
}

/// a display transfer's registers, taken apart.
#[derive(Clone)]
pub(crate) struct Transfer {
    pub(crate) input: u32,
    pub(crate) output: u32,
    pub(crate) input_width: u32,
    pub(crate) input_height: u32,
    pub(crate) output_width: u32,
    pub(crate) output_height: u32,
    /// the pixels written, width and height.
    pub(crate) copy: (u32, u32),
    /// the input pixels averaged into each output pixel, across and down.
    pub(crate) scale: (u32, u32),
    pub(crate) flip: bool,
    pub(crate) input_linear: bool,
    pub(crate) output_tiled: bool,
    pub(crate) input_format: ColorFormat,
    pub(crate) output_format: ColorFormat,
    /// the input pixels skipped at the left of each row, a texture copy of
    /// part of a picture's width.
    pub(crate) column: u32,
}
