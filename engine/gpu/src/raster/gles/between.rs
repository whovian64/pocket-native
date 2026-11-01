//! frame interpolation: for a game drawing 30 or fewer pictures a second on
//! the 60 Hz panel, a picture halfway between the top screen's last two goes
//! up the tick before the newer one. motion is found for blocks of 16 pixels
//! by matching quarter size copies of both pictures from the halfway point
//! each way, and a block no motion explains, or a pixel its block's motion
//! does not, is a blend of the two.

use glow::HasContext;

use super::{link, PACK_VERTEX_SOURCE};

/// the two pictures' brightness at a quarter of their size, the earlier in
/// red, the newer in green.
const LUMA_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D earlier;
uniform highp sampler2D newer;
layout(location = 0) out vec4 color;
float luma(highp sampler2D picture, ivec2 at) {
    float sum = 0.0;
    for (int y = 0; y < 4; y++) {
        for (int x = 0; x < 4; x++) {
            sum += dot(texelFetch(picture, at + ivec2(x, y), 0).rgb, vec3(0.299, 0.587, 0.114));
        }
    }
    return sum / 16.0;
}
void main() {
    ivec2 at = ivec2(gl_FragCoord.xy) * 4;
    color = vec4(luma(earlier, at), luma(newer, at), 0.0, 1.0);
}
"#;

/// a block's motion halfway, the step each way that matches the earlier
/// picture behind the newer one ahead best, in full size pixels from -16 to
/// 16 kept as 0 to 1, none where nothing matches well.
const SEARCH_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D luma;
layout(location = 0) out vec4 color;
const int REACH = 4;
void main() {
    ivec2 size = textureSize(luma, 0) - 1;
    ivec2 origin = ivec2(gl_FragCoord.xy) * 4;
    float best = 1e9;
    ivec2 step = ivec2(0);
    for (int dy = -REACH; dy <= REACH; dy++) {
        for (int dx = -REACH; dx <= REACH; dx++) {
            ivec2 d = ivec2(dx, dy);
            float cost = 0.0;
            for (int y = 0; y < 4; y++) {
                for (int x = 0; x < 4; x++) {
                    ivec2 at = origin + ivec2(x, y);
                    float a = texelFetch(luma, clamp(at - d, ivec2(0), size), 0).r;
                    float b = texelFetch(luma, clamp(at + d, ivec2(0), size), 0).g;
                    cost += abs(a - b);
                }
            }
            // standing still wins a tie, a flat block matches anywhere
            cost += 0.02 * float(abs(dx) + abs(dy));
            if (cost < best) {
                best = cost;
                step = d;
            }
        }
    }
    // a chance match among the 81 steps stays well above this
    vec2 motion = best / 16.0 < 0.025 ? vec2(step) * 4.0 : vec2(0.0);
    color = vec4(motion / 32.0 + 0.5, 0.0, 1.0);
}
"#;

/// the halfway picture over the panel's area for the top screen.
const BLEND_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D earlier;
uniform highp sampler2D newer;
uniform highp sampler2D motion;
uniform ivec4 area;
layout(location = 0) out vec4 color;
void main() {
    vec2 size = vec2(textureSize(earlier, 0));
    vec2 pixel = (gl_FragCoord.xy - vec2(area.xy)) / vec2(area.zw) * size;
    vec2 d = (texture(motion, pixel / (vec2(textureSize(motion, 0)) * 16.0)).xy - 0.5) * 32.0;
    vec3 a = texture(earlier, (pixel - d) / size).rgb;
    vec3 b = texture(newer, (pixel + d) / size).rgb;
    // where the two disagree the block's motion is not this pixel's, a
    // blend in place ghosts there rather than tearing
    vec3 still = mix(texture(earlier, pixel / size).rgb, texture(newer, pixel / size).rgb, 0.5);
    float trust = 1.0 - smoothstep(0.08, 0.25, length(a - b));
    color = vec4(mix(still, mix(a, b, 0.5), trust), 1.0);
}
"#;

#[derive(Clone, Copy)]
struct Target {
    texture: glow::Texture,
    framebuffer: glow::Framebuffer,
}

/// what a present of the top screen shows.
pub(super) enum Step {
    /// the picture itself, kept as the earlier one when it is new.
    Plain { keep: bool },
    /// the halfway picture, the new one kept as the newer first.
    Halfway,
}

#[derive(Default)]
pub(super) struct Between {
    size: (i32, i32),
    /// the earlier and the newer picture, upright at their own size.
    pictures: [Option<Target>; 2],
    luma: Option<Target>,
    motion: Option<Target>,
    programs: Option<[glow::Program; 3]>,
    /// the picture shown last, by its surface and version.
    shown: Option<(glow::Texture, u64)>,
    /// presents the picture shown last has been up, the halfway one before
    /// it counted.
    held: u32,
    /// a halfway picture is up, the newer one goes up next.
    pending: bool,
    /// the earlier slot holds the picture shown last.
    kept: bool,
}

impl Between {
    /// forgets the pictures, while interpolation is off.
    pub(super) fn reset(&mut self) {
        (self.shown, self.held, self.pending, self.kept) = (None, 0, false, false);
    }

    /// what to show for the top screen's picture, by its surface and version.
    pub(super) fn step(&mut self, key: (glow::Texture, u64)) -> Step {
        let new = self.shown != Some(key);
        self.shown = Some(key);
        if self.pending {
            self.pending = false;
            if !new {
                self.pictures.swap(0, 1);
                self.held = 2;
                return Step::Plain { keep: false };
            }
        } else if !new {
            self.held += 1;
            return Step::Plain { keep: false };
        } else if self.held >= 2 && self.kept {
            (self.pending, self.held) = (true, 1);
            return Step::Halfway;
        }
        self.held = 1;
        Step::Plain { keep: true }
    }

    /// the targets and programs for pictures of this size.
    unsafe fn prepare(&mut self, gl: &glow::Context, (width, height): (i32, i32)) -> Result<(), String> {
        if self.programs.is_none() {
            self.programs = Some([
                link(gl, PACK_VERTEX_SOURCE, LUMA_SOURCE)?,
                link(gl, PACK_VERTEX_SOURCE, SEARCH_SOURCE)?,
                link(gl, PACK_VERTEX_SOURCE, BLEND_SOURCE)?,
            ]);
        }
        if self.size == (width, height) {
            return Ok(());
        }
        for target in self.pictures.iter_mut().chain([&mut self.luma, &mut self.motion]).filter_map(Option::take) {
            gl.delete_texture(target.texture);
            gl.delete_framebuffer(target.framebuffer);
        }
        let target = |format: u32, (width, height): (i32, i32), filter: u32| -> Result<Target, String> {
            let texture = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_storage_2d(glow::TEXTURE_2D, 1, format, width, height);
            for (parameter, value) in [(glow::TEXTURE_MIN_FILTER, filter), (glow::TEXTURE_MAG_FILTER, filter), (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE), (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE)] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, value as i32);
            }
            let framebuffer = gl.create_framebuffer()?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0);
            Ok(Target { texture, framebuffer })
        };
        self.pictures = [Some(target(glow::RGBA8, (width, height), glow::NEAREST)?), Some(target(glow::RGBA8, (width, height), glow::NEAREST)?)];
        self.luma = Some(target(glow::RG8, (width / 4, height / 4), glow::NEAREST)?);
        self.motion = Some(target(glow::RGBA8, ((width + 15) / 16, (height + 15) / 16), glow::LINEAR)?);
        (self.size, self.kept) = ((width, height), false);
        Ok(())
    }

    /// keeps the picture upright at its own size in a slot, 0 the earlier,
    /// 1 the newer, drawn with the panel's program as the panel would show
    /// it. the caller binds the panel's framebuffer and program again.
    pub(super) unsafe fn keep(&mut self, gl: &glow::Context, present: glow::Program, texture: glow::Texture, screen: [i32; 4], slot: usize) -> Result<(), String> {
        let size = (screen[0], screen[1]);
        self.prepare(gl, size)?;
        let target = self.pictures[slot].ok_or("no picture to keep into")?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
        gl.viewport(0, 0, size.0, size.1);
        gl.use_program(Some(present));
        gl.uniform_4_i32(gl.get_uniform_location(present, "area").as_ref(), 0, 0, size.0, size.1);
        gl.uniform_4_i32(gl.get_uniform_location(present, "screen").as_ref(), screen[0], screen[1], screen[2], screen[3]);
        gl.uniform_1_i32(gl.get_uniform_location(present, "upright").as_ref(), 0);
        gl.uniform_1_i32(gl.get_uniform_location(present, "smooth_scale").as_ref(), 0);
        gl.bind_sampler(0, None);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.draw_arrays(glow::TRIANGLES, 0, 3);
        self.kept |= slot == 0;
        Ok(())
    }

    /// draws the halfway picture between the two kept into an area of the
    /// panel's framebuffer, from its bottom left as GL counts, only the part
    /// of it in clip when given, filtered when linear is given. the caller
    /// binds the panel's program again.
    pub(super) unsafe fn draw(&mut self, gl: &glow::Context, panel: Option<glow::Framebuffer>, area: [i32; 4], clip: Option<[i32; 4]>, linear: Option<glow::Sampler>) -> Result<(), String> {
        let ([luma_program, search, blend], Some(luma), Some(motion), [Some(earlier), Some(newer)]) = (self.programs.ok_or("no programs")?, self.luma, self.motion, self.pictures) else {
            return Err("nothing to interpolate between".into());
        };
        let (width, height) = self.size;
        let bind = |unit: u32, texture: glow::Texture| {
            gl.active_texture(glow::TEXTURE0 + unit);
            gl.bind_sampler(unit, None);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        };
        let sampler = |program: glow::Program, name: &str, unit: i32| gl.uniform_1_i32(gl.get_uniform_location(program, name).as_ref(), unit);

        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(luma.framebuffer));
        gl.viewport(0, 0, width / 4, height / 4);
        gl.use_program(Some(luma_program));
        bind(0, earlier.texture);
        bind(1, newer.texture);
        sampler(luma_program, "earlier", 0);
        sampler(luma_program, "newer", 1);
        gl.draw_arrays(glow::TRIANGLES, 0, 3);

        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(motion.framebuffer));
        gl.viewport(0, 0, (width + 15) / 16, (height + 15) / 16);
        gl.use_program(Some(search));
        bind(0, luma.texture);
        sampler(search, "luma", 0);
        gl.draw_arrays(glow::TRIANGLES, 0, 3);

        gl.bind_framebuffer(glow::FRAMEBUFFER, panel);
        gl.viewport(area[0], area[1], area[2], area[3]);
        gl.use_program(Some(blend));
        bind(0, earlier.texture);
        bind(1, newer.texture);
        bind(2, motion.texture);
        gl.bind_sampler(0, linear);
        gl.bind_sampler(1, linear);
        sampler(blend, "earlier", 0);
        sampler(blend, "newer", 1);
        sampler(blend, "motion", 2);
        gl.uniform_4_i32(gl.get_uniform_location(blend, "area").as_ref(), area[0], area[1], area[2], area[3]);
        if let Some([x, y, width, height]) = clip {
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(x, y, width, height);
        }
        gl.draw_arrays(glow::TRIANGLES, 0, 3);
        gl.disable(glow::SCISSOR_TEST);
        for unit in [2, 1] {
            gl.active_texture(glow::TEXTURE0 + unit);
            gl.bind_sampler(unit, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        gl.active_texture(glow::TEXTURE0);
        gl.bind_sampler(0, None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Gles;
    use super::*;

    const WIDTH: i32 = 128;
    const HEIGHT: i32 = 64;

    fn noise(seed: u32) -> Vec<u8> {
        let mut state = seed;
        (0..WIDTH * HEIGHT * 4)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if i % 4 == 3 { 255 } else { (state >> 24) as u8 }
            })
            .collect()
    }

    /// the picture each pixel of which is another's that far to its left.
    fn shifted(picture: &[u8], by: i32) -> Vec<u8> {
        let mut out = picture.to_vec();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let from = ((y * WIDTH + (x - by).clamp(0, WIDTH - 1)) * 4) as usize;
                let to = ((y * WIDTH + x) * 4) as usize;
                out[to..to + 4].copy_from_slice(&picture[from..from + 4]);
            }
        }
        out
    }

    /// the halfway picture between two, drawn on the GL thread.
    fn halfway(earlier: Vec<u8>, newer: Vec<u8>) -> Option<Vec<u8>> {
        halfway_sized(earlier, newer, (WIDTH, HEIGHT))
    }

    pub(super) fn halfway_sized(earlier: Vec<u8>, newer: Vec<u8>, (width, height): (i32, i32)) -> Option<Vec<u8>> {
        let gles = Gles::new().ok()?;
        let gl = gles.gl;
        Some(gles.worker.run(move || unsafe {
            let mut between = Between::default();
            between.prepare(&gl, (width, height)).unwrap();
            for (slot, picture) in [earlier, newer].iter().enumerate() {
                gl.bind_texture(glow::TEXTURE_2D, Some(between.pictures[slot].unwrap().texture));
                gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, width, height, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(picture)));
            }
            let output = gl.create_texture().unwrap();
            gl.bind_texture(glow::TEXTURE_2D, Some(output));
            gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, width, height);
            let framebuffer = gl.create_framebuffer().unwrap();
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(output), 0);
            let empty = gl.create_vertex_array().unwrap();
            gl.bind_vertex_array(Some(empty));
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            between.draw(&gl, Some(framebuffer), [0, 0, width, height], None, None).unwrap();
            let mut pixels = vec![0u8; (width * height * 4) as usize];
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.read_pixels(0, 0, width, height, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut pixels)));
            pixels
        }))
    }

    /// pixels of the middle that differ, two blocks from the edges, where
    /// the shifted picture repeats its first column.
    fn differing(a: &[u8], b: &[u8]) -> usize {
        (8..HEIGHT - 8).flat_map(|y| (32..WIDTH - 32).map(move |x| ((y * WIDTH + x) * 4) as usize)).filter(|&at| (0..3).any(|c| a[at + c].abs_diff(b[at + c]) > 2)).count()
    }

    #[test]
    fn a_picture_moving_comes_out_halfway() {
        let earlier = noise(1);
        let Some(between) = halfway(earlier.clone(), shifted(&earlier, 8)) else { return };
        assert_eq!(differing(&between, &shifted(&earlier, 4)), 0);
    }

    #[test]
    fn pictures_nothing_moved_between_are_blended() {
        let (earlier, newer) = (noise(1), noise(2));
        let Some(between) = halfway(earlier.clone(), newer.clone()) else { return };
        let blend: Vec<u8> = earlier.iter().zip(&newer).map(|(&a, &b)| (a as u32 + b as u32).div_ceil(2) as u8).collect();
        assert_eq!(differing(&between, &blend), 0);
    }
}

/// how far from the real picture the halfway one comes, against holding the
/// earlier one and blending the two, for three pictures in a row of a game
/// drawing 60 a second: ZAKURO_BETWEEN_FRAMES=earlier.ppm,middle.ppm,newer.ppm
#[cfg(test)]
mod measure {
    use super::tests::halfway_sized;

    fn ppm(path: &str) -> (Vec<u8>, i32, i32) {
        let bytes = std::fs::read(path).unwrap();
        let header: Vec<&[u8]> = bytes.splitn(5, |&b| b == b'\n' || b == b' ').collect();
        let (width, height): (i32, i32) = (std::str::from_utf8(header[1]).unwrap().parse().unwrap(), std::str::from_utf8(header[2]).unwrap().parse().unwrap());
        let rgb = &bytes[bytes.len() - (width * height * 3) as usize..];
        // bottom row first, as GL keeps it, and cropped to whole blocks
        let (w, h) = (width / 16 * 16, height / 16 * 16);
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in (0..h).rev() {
            for x in 0..w {
                let at = ((y * width + x) * 3) as usize;
                out.extend_from_slice(&[rgb[at], rgb[at + 1], rgb[at + 2], 255]);
            }
        }
        (out, w, h)
    }

    #[test]
    #[ignore = "needs ZAKURO_BETWEEN_FRAMES"]
    fn halfway_against_the_real_picture() {
        let Ok(frames) = std::env::var("ZAKURO_BETWEEN_FRAMES") else { return };
        let paths: Vec<&str> = frames.split(',').collect();
        let [(earlier, w, h), (middle, ..), (newer, ..)] = [ppm(paths[0]), ppm(paths[1]), ppm(paths[2])];
        let between = halfway_sized(earlier.clone(), newer.clone(), (w, h)).unwrap();
        let error = |a: &[u8]| a.iter().zip(&middle).enumerate().filter(|(i, _)| i % 4 != 3).map(|(_, (&x, &y))| x.abs_diff(y) as f64).sum::<f64>() / (w * h * 3) as f64;
        let blend: Vec<u8> = earlier.iter().zip(&newer).map(|(&a, &b)| (a as u32 + b as u32).div_ceil(2) as u8).collect();
        println!("hold {:.2} blend {:.2} halfway {:.2}", error(&earlier), error(&blend), error(&between));
        // ZAKURO_BETWEEN_OUT=file keeps the halfway picture
        if let Ok(out) = std::env::var("ZAKURO_BETWEEN_OUT") {
            let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
            for row in between.chunks((w * 4) as usize).rev() {
                ppm.extend(row.chunks(4).flat_map(|pixel| &pixel[..3]));
            }
            std::fs::write(out, ppm).unwrap();
        }
    }
}
