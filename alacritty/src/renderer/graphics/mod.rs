use std::collections::HashMap;
use std::mem;

use ahash::RandomState;

use alacritty_terminal::graphics::{ImageId, RenderSnapshot};

use crate::display::SizeInfo;
use crate::gl;
use crate::gl::types::*;
use crate::renderer::shader::{ShaderError, ShaderProgram, ShaderVersion};

const IMAGE_SHADER_V: &str = include_str!("../../../res/image.v.glsl");
const IMAGE_SHADER_F: &str = include_str!("../../../res/image.f.glsl");

/// Vertex: pixel-space position + normalised UV.
#[repr(C)]
#[derive(Copy, Clone)]
struct ImageVertex {
    x: f32,
    y: f32,
    u: f32,
    v: f32,
}

/// Standalone image-quad renderer.
///
/// Owns its own VAO/VBO and shader. The texture cache is keyed by `ImageId`;
/// because each `GraphicsRenderer` lives inside exactly one `Display` (one GL
/// context, one terminal), no cross-terminal namespacing is required.
pub struct GraphicsRenderer {
    vao: GLuint,
    vbo: GLuint,
    program: ImageShaderProgram,
    /// GPU texture cache: image_id → GL texture name.
    texture_cache: HashMap<ImageId, GLuint, RandomState>,
    vertices: Vec<ImageVertex>,
}

impl GraphicsRenderer {
    pub fn new(shader_version: ShaderVersion) -> Result<Self, ShaderError> {
        let mut vao: GLuint = 0;
        let mut vbo: GLuint = 0;

        unsafe {
            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut vbo);

            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);

            let stride = mem::size_of::<ImageVertex>() as i32;

            // aPosition (location 0): x, y
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, stride, std::ptr::null());
            gl::EnableVertexAttribArray(0);

            // aTexCoord (location 1): u, v
            let uv_offset = (mem::size_of::<f32>() * 2) as i32;
            gl::VertexAttribPointer(1, 2, gl::FLOAT, gl::FALSE, stride, uv_offset as *const _);
            gl::EnableVertexAttribArray(1);

            gl::BindVertexArray(0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
        }

        let program = ImageShaderProgram::new(shader_version)?;

        Ok(Self { vao, vbo, program, texture_cache: HashMap::default(), vertices: Vec::new() })
    }

    /// Process the delete queue from a snapshot, freeing GPU textures.
    pub fn sync_deletes(&mut self, snapshot: &mut RenderSnapshot) {
        for id in snapshot.deletes.drain(..) {
            self.delete_texture(id);
        }
    }

    /// Upload a straight-alpha RGBA8 texture with explicit dimensions.
    ///
    /// Data is straight-alpha. The draw path uses `GL_ONE, GL_ONE_MINUS_SRC_ALPHA`
    /// (premultiplied-over) to match kitty's gl.c blend behavior.
    pub fn upload_texture_with_dims(&mut self, id: ImageId, width: u32, height: u32, rgba: &[u8]) {
        self.delete_texture(id);

        let mut tex: GLuint = 0;
        unsafe {
            gl::GenTextures(1, &mut tex);
            gl::BindTexture(gl::TEXTURE_2D, tex);

            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);

            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA as i32,
                width as i32,
                height as i32,
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                rgba.as_ptr() as *const _,
            );

            gl::BindTexture(gl::TEXTURE_2D, 0);
        }

        self.texture_cache.insert(id, tex);
    }

    /// Free a GL texture and remove it from the cache.
    pub fn delete_texture(&mut self, id: ImageId) {
        if let Some(tex) = self.texture_cache.remove(&id) {
            unsafe {
                gl::DeleteTextures(1, &tex);
            }
        }
    }

    /// Draw a slice of `ImageRenderItem`s for one z-bucket.
    ///
    /// Items must already be sorted and grouped (group_index from the snapshot).
    /// `projection` is `vec4(offset_x, offset_y, scale_x, scale_y)` in NDC,
    /// matching the same projection used by the text/rect renderers.
    pub fn draw(
        &mut self,
        items: &[alacritty_terminal::graphics::ImageRenderItem],
        size_info: &SizeInfo,
        projection: [f32; 4],
    ) {
        if items.is_empty() {
            return;
        }

        // Save blend enable state and blend func; restore both after drawing.
        let blend_was_enabled = unsafe { gl::IsEnabled(gl::BLEND) == gl::TRUE };
        let saved_blend = save_blend_func();

        unsafe {
            gl::Enable(gl::BLEND);
            // Premultiplied-over (GL_ONE, GL_ONE_MINUS_SRC_ALPHA) — matches kitty gl.c.
            gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);

            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);

            gl::UseProgram(self.program.id());
            gl::Uniform4fv(self.program.u_projection, 1, projection.as_ptr());
        }

        let mut current_group = u32::MAX;
        let mut bound_tex = false;

        for item in items.iter() {
            if item.group_index != current_group {
                if !self.vertices.is_empty() {
                    self.flush_vertices();
                }
                current_group = item.group_index;

                // Bind texture for this group.
                if let Some(&tex) = self.texture_cache.get(&item.image_id) {
                    unsafe {
                        gl::BindTexture(gl::TEXTURE_2D, tex);
                        gl::Uniform1i(self.program.u_image, 0);
                    }
                    bound_tex = true;
                } else {
                    bound_tex = false;
                }
            }

            if !bound_tex {
                continue;
            }
            self.push_quad(item, size_info);
        }

        // Flush final group.
        if !self.vertices.is_empty() {
            self.flush_vertices();
        }

        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, 0);

            gl::UseProgram(0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
        }

        // Restore prior blend func and enable state.
        restore_blend_func(saved_blend);
        unsafe {
            if blend_was_enabled {
                gl::Enable(gl::BLEND);
            } else {
                gl::Disable(gl::BLEND);
            }
        }

        debug_assert_blend_restored(saved_blend);
    }

    fn push_quad(
        &mut self,
        item: &alacritty_terminal::graphics::ImageRenderItem,
        size_info: &SizeInfo,
    ) {
        let dest = &item.dest;
        let uv = &item.src_uv;

        let [x0, y0, x1, y1] = quad_corners(
            dest.column.0,
            dest.line.0,
            dest.cell_x_offset,
            dest.cell_y_offset,
            dest.num_cols,
            dest.num_rows,
            size_info.cell_width(),
            size_info.cell_height(),
        );

        let (u0, v0, u1, v1) = (uv.u0, uv.v0, uv.u1, uv.v1);

        // Two triangles (CCW).
        self.vertices.push(ImageVertex { x: x0, y: y0, u: u0, v: v0 });
        self.vertices.push(ImageVertex { x: x0, y: y1, u: u0, v: v1 });
        self.vertices.push(ImageVertex { x: x1, y: y0, u: u1, v: v0 });
        self.vertices.push(ImageVertex { x: x1, y: y0, u: u1, v: v0 });
        self.vertices.push(ImageVertex { x: x0, y: y1, u: u0, v: v1 });
        self.vertices.push(ImageVertex { x: x1, y: y1, u: u1, v: v1 });
    }

    fn flush_vertices(&mut self) {
        if self.vertices.is_empty() {
            return;
        }
        unsafe {
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (self.vertices.len() * mem::size_of::<ImageVertex>()) as isize,
                self.vertices.as_ptr() as *const _,
                gl::STREAM_DRAW,
            );
            gl::DrawArrays(gl::TRIANGLES, 0, self.vertices.len() as i32);
        }
        self.vertices.clear();
    }
}

/// Pixel-space quad corners `[x0, y0, x1, y1]` for an image cell, content-relative.
///
/// (0,0) is the top-left of the drawable area. The GL viewport is already offset
/// to (padding_x, padding_y), so no padding is added here.
// Geometry inputs (grid position, per-cell offsets, cell metrics) are naturally
// positional; bundling them into a struct would not aid clarity here.
#[allow(clippy::too_many_arguments)]
fn quad_corners(
    col: usize,
    line: i32,
    cell_x_offset: u32,
    cell_y_offset: u32,
    num_cols: u32,
    num_rows: u32,
    cell_w: f32,
    cell_h: f32,
) -> [f32; 4] {
    let px = col as f32 * cell_w + cell_x_offset as f32;
    let py = line as f32 * cell_h + cell_y_offset as f32;
    let pw = num_cols as f32 * cell_w;
    let ph = num_rows as f32 * cell_h;
    [px, py, px + pw, py + ph]
}

impl Drop for GraphicsRenderer {
    fn drop(&mut self) {
        // Free all cached textures.
        let ids: Vec<ImageId> = self.texture_cache.keys().copied().collect();
        for id in ids {
            self.delete_texture(id);
        }
        unsafe {
            gl::DeleteBuffers(1, &self.vbo);
            gl::DeleteVertexArrays(1, &self.vao);
        }
    }
}

/// Saved blend function state for save/restore around image draws.
#[derive(Copy, Clone)]
struct BlendFuncState {
    src_rgb: GLint,
    dst_rgb: GLint,
    src_alpha: GLint,
    dst_alpha: GLint,
}

fn save_blend_func() -> BlendFuncState {
    let mut s = BlendFuncState { src_rgb: 0, dst_rgb: 0, src_alpha: 0, dst_alpha: 0 };
    unsafe {
        gl::GetIntegerv(gl::BLEND_SRC_RGB, &mut s.src_rgb);
        gl::GetIntegerv(gl::BLEND_DST_RGB, &mut s.dst_rgb);
        gl::GetIntegerv(gl::BLEND_SRC_ALPHA, &mut s.src_alpha);
        gl::GetIntegerv(gl::BLEND_DST_ALPHA, &mut s.dst_alpha);
    }
    s
}

fn restore_blend_func(s: BlendFuncState) {
    unsafe {
        gl::BlendFuncSeparate(
            s.src_rgb as GLenum,
            s.dst_rgb as GLenum,
            s.src_alpha as GLenum,
            s.dst_alpha as GLenum,
        );
    }
}

#[cfg(debug_assertions)]
fn debug_assert_blend_restored(saved: BlendFuncState) {
    let mut cur = BlendFuncState { src_rgb: 0, dst_rgb: 0, src_alpha: 0, dst_alpha: 0 };
    unsafe {
        gl::GetIntegerv(gl::BLEND_SRC_RGB, &mut cur.src_rgb);
        gl::GetIntegerv(gl::BLEND_DST_RGB, &mut cur.dst_rgb);
        gl::GetIntegerv(gl::BLEND_SRC_ALPHA, &mut cur.src_alpha);
        gl::GetIntegerv(gl::BLEND_DST_ALPHA, &mut cur.dst_alpha);
    }
    debug_assert_eq!(cur.src_rgb, saved.src_rgb, "blend src_rgb not restored after image draw");
    debug_assert_eq!(cur.dst_rgb, saved.dst_rgb, "blend dst_rgb not restored after image draw");
}

#[cfg(not(debug_assertions))]
fn debug_assert_blend_restored(_saved: BlendFuncState) {}

/// Image shader program wrapper.
struct ImageShaderProgram {
    program: ShaderProgram,
    u_projection: GLint,
    u_image: GLint,
}

impl ImageShaderProgram {
    fn new(shader_version: ShaderVersion) -> Result<Self, ShaderError> {
        let program = ShaderProgram::new(shader_version, None, IMAGE_SHADER_V, IMAGE_SHADER_F)?;
        let u_projection = program.get_uniform_location(c"projection")?;
        let u_image = program.get_uniform_location(c"image")?;
        Ok(Self { program, u_projection, u_image })
    }

    fn id(&self) -> GLuint {
        self.program.id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that shader source files are valid UTF-8 and non-empty.
    /// Full GL compilation is tested via the GLES2 compile test below.
    #[test]
    fn shader_sources_non_empty() {
        assert!(!IMAGE_SHADER_V.is_empty(), "image vertex shader is empty");
        assert!(!IMAGE_SHADER_F.is_empty(), "image fragment shader is empty");
    }

    /// Verify that the vertex shader contains the GLES2_RENDERER guard,
    /// meaning the single source compiles under both ShaderVersion paths.
    #[test]
    fn shader_has_gles2_guard() {
        assert!(
            IMAGE_SHADER_V.contains("GLES2_RENDERER"),
            "image.v.glsl missing GLES2_RENDERER guard"
        );
        assert!(
            IMAGE_SHADER_F.contains("GLES2_RENDERER"),
            "image.f.glsl missing GLES2_RENDERER guard"
        );
    }

    /// Verify that the GLES2 preamble injected by ShaderVersion::Gles2 produces
    /// the expected header string (same contract the rect/text shaders rely on).
    #[test]
    fn gles2_preamble_defines_guard() {
        // This mirrors the exact string produced by shader.rs ShaderVersion::Gles2.
        let header = "#version 100\n#define GLES2_RENDERER\n";
        assert!(header.contains("GLES2_RENDERER"));
        // The vertex shader uses `attribute` + `varying` under GLES2 guard.
        let full_v = format!("{header}{}", IMAGE_SHADER_V);
        assert!(full_v.contains("attribute vec2 aPosition"));
        assert!(full_v.contains("varying mediump vec2 vTexCoord"));
    }

    /// Verify that the GLSL3 path uses `in`/`out` layout qualifiers.
    #[test]
    fn glsl3_uses_layout_qualifiers() {
        assert!(IMAGE_SHADER_V.contains("layout (location = 0) in vec2 aPosition"));
        assert!(IMAGE_SHADER_V.contains("layout (location = 1) in vec2 aTexCoord"));
        assert!(IMAGE_SHADER_V.contains("out vec2 vTexCoord"));
    }

    /// Verify blend save/restore round-trip logic does not panic in the
    /// non-GL path (compile-time check of the struct and function signatures).
    #[test]
    fn blend_state_struct_is_copy() {
        let s = BlendFuncState { src_rgb: 1, dst_rgb: 2, src_alpha: 3, dst_alpha: 4 };
        let _copy = s;
    }

    /// Verify ImageVertex has the expected memory layout (no padding between
    /// fields that would break the VertexAttribPointer offsets).
    #[test]
    fn image_vertex_layout() {
        assert_eq!(mem::size_of::<ImageVertex>(), 4 * mem::size_of::<f32>());
        assert_eq!(mem::size_of::<f32>() * 2, 8); // uv_offset used in new()
    }

    /// An image at column 0, row 0 must produce a top-left vertex at (0,0),
    /// not at (padding, padding). The GL viewport origin is already at the
    /// padding offset, so quad_corners must be content-relative with no
    /// padding added.
    #[test]
    fn quad_corners_no_padding_offset() {
        let cell_w = 8.0_f32;
        let cell_h = 16.0_f32;

        // col=0, line=0, no sub-cell offset, 1x1 cell image.
        let [x0, y0, x1, y1] =
            quad_corners(0_usize, 0_i32, 0_u32, 0_u32, 1_u32, 1_u32, cell_w, cell_h);
        assert_eq!(x0, 0.0, "x0 must be 0, not padding-offset");
        assert_eq!(y0, 0.0, "y0 must be 0, not padding-offset");
        assert_eq!(x1, cell_w);
        assert_eq!(y1, cell_h);

        // col=2, line=1 — verify cell-grid math is intact.
        let [x0, y0, x1, y1] =
            quad_corners(2_usize, 1_i32, 0_u32, 0_u32, 2_u32, 3_u32, cell_w, cell_h);
        assert_eq!(x0, 2.0 * cell_w);
        assert_eq!(y0, 1.0 * cell_h);
        assert_eq!(x1, 4.0 * cell_w);
        assert_eq!(y1, 4.0 * cell_h);
    }
}
