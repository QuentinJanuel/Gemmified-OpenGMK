//! WebGL2 (via `glow`) renderer implementation, used only on `wasm32`. This is a faithful port of
//! `render/opengl.rs` (the desktop GL 3.3 renderer, via raw `gl_generator` bindings) — same struct
//! layout, same render-state tracking, same atlas/framebuffer/savestate logic. Only the GL call
//! *mechanics* differ:
//!
//! - No function-pointer loading: `glow::Context::from_webgl2_context` just wraps the JS context
//!   object directly, so there's no `native_gl::PlatformImpl`-equivalent loader step at all.
//! - No explicit buffer swap: the browser composites the canvas automatically once this task
//!   returns control to the event loop, so anywhere `opengl.rs` calls `self.imp.swap_buffers()`
//!   is simply omitted here.
//! - `set_vsync`/`get_vsync`/`wait_vsync` are simple state-only no-ops: there is no vsync toggle in
//!   a browser (frame pacing is the host's job).
//! - Typed `glow::Texture`/`glow::Buffer`/`glow::Program`/`glow::Shader`/`glow::Framebuffer`/
//!   `glow::VertexArray`/`glow::UniformLocation` handles replace raw `GLuint`s. Note these are the
//!   backend-agnostic *type aliases* (`glow::Texture`, not `glow::NativeTexture`) — the `Native*`
//!   names only exist when glow's desktop backend is compiled, which never happens on wasm32.
//! - WebGL2/GLES has no `glGetTexLevelParameteriv`, `glGetIntegerv(*_BINDING)`, or `glGetTexImage`
//!   equivalent at all (confirmed: none of these exist in glow's `HasContext` trait on any
//!   backend). `opengl.rs` leans on all three:
//!     - Texture/framebuffer width & height are tracked ourselves in Rust fields (`Framebuffer`'s
//!       `width`/`height`, and `atlas_dims` parallel to `texture_ids`) instead of being queried
//!       back from the driver.
//!     - Binding-point "save old binding / do a temporary bind / restore" patterns (used
//!       constantly in `opengl.rs` for texture/framebuffer bindings) are replicated using our own
//!       `Cell`-tracked mirror of what's currently bound (`current_texture_2d`, `current_read_fbo`,
//!       `current_draw_fbo`) rather than a GL query — we're the sole client of this context, so the
//!       mirror is always accurate. `Cell` (not a plain field) is required because several
//!       `RendererTrait` methods that need to flip a binding temporarily (e.g. `get_pixels`,
//!       `dump_sprite_part`, `set_vsync`) take `&self`, not `&mut self`.
//!     - Texture-level pixel readback (`glGetTexImage`, used by savestate dump/restore for the
//!       colour AND depth planes) has no WebGL2 analogue for depth; colour is read back by
//!       temporarily binding the texture to a throwaway FBO and using `read_pixels` instead
//!       (framebuffer-based readback, not texture-based). Depth-plane readback is simply
//!       unsupported (see `stored_zbuffer`) — savestates are pruned entirely from the wasm build
//!       so this is dead code in practice, not a real user-facing gap.
//! - WebGL2's `tex_image_2d`/`tex_sub_image_2d` `format` parameter does not accept `GL_BGRA`
//!   (unlike desktop GL, which `opengl.rs` relies on on for uploading the packed sprite atlas).
//!   Pixel data is byte-swizzled (R/B swapped) on the CPU before upload instead (`bgra_to_rgba`).
//! - There's no `glProvokingVertex` in GLES/WebGL2 (`opengl.rs` calls
//!   `gl.ProvokingVertex(gl::FIRST_VERTEX_CONVENTION)` to match Direct3D's flat-shading vertex
//!   convention; glow doesn't even expose a method for this on any backend). Flat-shaded lighting
//!   (`flat out vec4 frag_blend_flat` in the shared shader, used only for non-Gouraud lit
//!   primitives) will therefore pick its provoking vertex according to the WebGL2-mandated
//!   convention rather than `opengl.rs`'s chosen one. This is a deliberately-accepted, purely
//!   cosmetic difference restricted to a rarely-used 3D lighting corner case.
//! - WebGL2 *rejects* a draw that samples a texture which is also an attachment of the current draw
//!   framebuffer ("feedback loop"): the draw is dropped and `INVALID_OPERATION` is raised. Desktop
//!   GL leaves the result undefined but issues no error, so `opengl.rs` never had to care. GM8
//!   games do hit this — drawing a surface onto itself while it's the render target is a real
//!   idiom, and D3D8 let it through — so `draw_buffer` breaks the loop by sampling a snapshot
//!   instead (see `feedback_copy`).

use crate::{
    render::{
        BlendType, DepthFunc, Fog, IndexData, Light, PrimitiveBuilder, PrimitiveShape, PrimitiveType, RendererOptions,
        RendererTrait, SavedTexture, Scaling, ShaderRef, ShaderStage, TexcoordSource, Vertex, VertexBuffer,
        VertexStream,
        atlas::{AtlasBuilder, AtlasRect, AtlasRef},
        mat4mult,
    },
    types::Colour,
    window::{connection::Connection, window::Window},
};
use glow::HasContext;
use rect_packer::DensePacker;
use std::{
    any::Any,
    cell::Cell,
    collections::HashMap,
    f64::consts::PI,
    mem::{offset_of, size_of},
};

static VERTEX_SHADER_SOURCE: &str = include_str!("glsl/vertex_webgl.glsl");
static FRAGMENT_SHADER_SOURCE: &str = include_str!("glsl/fragment_webgl.glsl");

#[derive(Clone, Copy, PartialEq)]
#[repr(u32)]
enum GLBool {
    False = 0,
    True = 1,
}

impl From<GLBool> for bool {
    fn from(b: GLBool) -> Self {
        match b {
            GLBool::False => false,
            GLBool::True => true,
        }
    }
}

impl From<bool> for GLBool {
    fn from(b: bool) -> Self {
        if b { GLBool::True } else { GLBool::False }
    }
}

#[derive(Clone, Copy, PartialEq)]
#[repr(C)]
struct GLLight {
    pos: [f32; 4],
    colour: [f32; 4],
    enabled: GLBool,
    is_point: GLBool,
    range: f32,
    padding: u32, // must pad to size of vec4
}

#[derive(Clone, PartialEq)]
#[repr(C)]
struct RenderState {
    // vertex shader
    model_matrix: [f32; 16],
    viewproj_matrix: [f32; 16],
    lights: [GLLight; 8],
    ambient_colour: [f32; 4],
    lighting: GLBool,
    gouraud: GLBool,
    texture_blend: GLBool, // emulates glTexEnv(): GL_MODULATE if true, GL_REPLACE if false
    // frag shader
    texture_repeat: GLBool,
    interpolate_pixels: GLBool,
    depth_test: GLBool,
    fog_enabled: GLBool,
    fog_begin: f32,
    fog_end: f32,
    // std140 requires vec4 members (fog_colour, next) to start at a 16-byte-aligned offset. Rust's
    // `#[repr(C)]` only naturally aligns `[f32; 4]` to 4 bytes (the element type's alignment), not
    // 16, so without this explicit padding fog_colour lands 12 bytes short of where the shader
    // actually expects it -- desktop GL drivers apparently tolerate the resulting undersized
    // buffer upload silently, but WebGL2/ANGLE validates it strictly and raises
    // GL_INVALID_OPERATION at draw time ("uniform buffer that is too small").
    _pad_fog_colour: [u32; 3],
    fog_colour: [f32; 4],
    // The view matrix on its own, which the vertex shader needs only for `texcoord_source`: every
    // generated coordinate is a function of *camera* space, and `viewproj_matrix` cannot be taken
    // apart again. It is also deliberately the unflipped one -- `update_matrix`'s vertical flip
    // corrects GL's screen space against D3D's and is no part of camera space, so leaving it out is
    // what makes a generated coordinate match what the real runner's D3D9 computes.
    view_matrix: [f32; 16],
    // `#[repr(i32)]`, so this is the small integer the shader switches on. Padded because std140
    // rounds a block up to a multiple of 16 and the CPU-side upload length has to agree.
    texcoord_source: TexcoordSource,
    _pad_texcoord_source: [u32; 3],
    // end of shader stuff
    end_of_uniform: (),
    proj_matrix: [f32; 16],
    alpha_blending: bool,
    blend_mode: (BlendType, BlendType),
    // Separate factors for the alpha channel, or None to blend it with `blend_mode` like colour.
    // Only GM8.2's `draw_set_blend_alphamode` sets this.
    blend_mode_alpha: Option<(BlendType, BlendType)>,
    write_depth: bool,
    culling: bool,
    cull_clockwise: bool,
    colour_mask: (bool, bool, bool, bool),
    depth_func: DepthFunc,
    depth_bias: f32,
    depth_range: (f32, f32),
}

impl Default for RenderState {
    fn default() -> Self {
        // Create identity matrix to initialize MVP matrices with
        #[rustfmt::skip]
        let identity_matrix: [f32; 16] = [
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
        ];
        Self {
            alpha_blending: true,
            blend_mode: (BlendType::SrcAlpha, BlendType::InvSrcAlpha),
            interpolate_pixels: GLBool::False,
            texture_repeat: GLBool::False,
            texture_blend: GLBool::True,
            model_matrix: identity_matrix,
            view_matrix: identity_matrix,
            proj_matrix: identity_matrix,
            depth_test: false.into(),
            write_depth: false,
            culling: false,
            cull_clockwise: false,
            blend_mode_alpha: None,
            colour_mask: (true, true, true, true),
            // D3D's default, which is also what this renderer has always put GL into. These fields
            // exist to be diffed against what a draw asks for, so a default that disagreed with the
            // state GL actually starts in would swallow a change as "already set" -- which is why
            // `apply_render_state_forced` now seeds GL from here rather than the two being written
            // out independently and drifting.
            depth_func: DepthFunc::LessEqual,
            depth_bias: 0.0,
            depth_range: (0.0, 1.0),
            fog_enabled: GLBool::False,
            fog_begin: 0.0,
            fog_end: 0.0,
            lighting: GLBool::False,
            gouraud: GLBool::True,
            ambient_colour: [0.0; 4],
            lights: [GLLight {
                pos: [0.0; 4],
                colour: [0.0; 4],
                enabled: GLBool::False,
                is_point: GLBool::False,
                range: 0.0,
                padding: 0,
            }; 8],
            viewproj_matrix: identity_matrix,
            _pad_fog_colour: [0; 3],
            fog_colour: [0.0; 4],
            texcoord_source: TexcoordSource::Vertex,
            _pad_texcoord_source: [0; 3],
            end_of_uniform: (),
        }
    }
}

impl RenderState {
    /// Same maths as `opengl.rs`'s `update_matrix`, but takes the current viewport dimensions as
    /// plain arguments instead of querying `GL_VIEWPORT` back from the driver (WebGL2/glow has no
    /// such query; `RendererImpl` tracks its own viewport in `self.viewport` instead).
    fn update_matrix(&mut self, viewport_w: i32, viewport_h: i32) {
        let offset_x = 1.0 / f64::from(viewport_w);
        let offset_y = 1.0 / f64::from(viewport_h);
        // build viewproj matrix
        #[rustfmt::skip]
        let viewproj = mat4mult(
            mat4mult(self.view_matrix, self.proj_matrix),
            // flip vertically because GL textures are flipped vertically vs DX
            // also GL's screen space is offset half a pixel vs DX so shift it
            [
                1.0,             0.0,             0.0, 0.0,
                0.0,             -1.0,            0.0, 0.0,
                0.0,             0.0,             1.0, 0.0,
                offset_x as f32, offset_y as f32, 0.0, 1.0,
            ],
        );
        self.viewproj_matrix = viewproj;
    }
}

/// Byte-view of a `#[repr(C)]` POD struct, used to feed `glow`'s safe slice-based UBO upload
/// functions (`buffer_data_u8_slice`) in place of `opengl.rs`'s raw-pointer `glBufferSubData` cast.
unsafe fn as_u8_slice<T>(v: &T, len: usize) -> &[u8] {
    std::slice::from_raw_parts((v as *const T).cast::<u8>(), len)
}

/// WebGL2 has no `GL_BGRA` texture format (unlike desktop GL, where `opengl.rs` uploads the packed
/// sprite atlas — sourced from GM8's BGRA-ish asset pixel data — directly as `GL_BGRA`). Swap the
/// R/B byte of every pixel instead, so it can be uploaded as plain `RGBA`.
fn bgra_to_rgba(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    out
}

fn make_view_matrix(x: f64, y: f64, z: f64, w: f64, h: f64, angle: f64) -> [f32; 16] {
    // Note: sin is negated because it's the same as negating the angle, which is how GM8 does view angles
    let angle = angle.to_radians();
    let sin_angle = -angle.sin() as f32;
    let cos_angle = angle.cos() as f32;

    #[rustfmt::skip]
    let view_matrix: [f32; 16] = {
        // source rectangle's center coordinates aka -(x + w/2) and -(y + h/2)
        let scx = -((x as f32) + (w as f32 / 2.0));
        let scy = -((y as f32) + (h as f32 / 2.0));
        let scz = -z as f32;
        mat4mult(
            // Place camera at (scx, scy, scz)
            [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                scx, scy, scz, 1.0,
            ],
            // Rotate to view_angle
            [
                cos_angle,  sin_angle, 0.0, 0.0,
                -sin_angle, cos_angle, 0.0, 0.0,
                0.0,        0.0,       1.0, 0.0,
                0.0,        0.0,       0.0, 1.0,
            ]
        )
    };

    view_matrix
}

fn split_colour(rgb: i32, alpha: f64) -> [f32; 4] {
    [
        ((rgb & 0xFF) as f32) / 255.0,
        (((rgb >> 8) & 0xFF) as f32) / 255.0,
        (((rgb >> 16) & 0xFF) as f32) / 255.0,
        alpha.max(0.0).min(1.0) as f32,
    ]
}

// TODO: probably put this in render.rs instead (same TODO as opengl.rs -- this impl is
// duplicated between the two renderer backends because `gml/kernel.rs` calls it regardless of
// which backend is active, and it's cheap enough to not be worth threading through the trait).
impl VertexBuffer {
    pub fn swap_colour(&mut self, old: (i32, f64), new: (i32, f64)) {
        let old = split_colour(old.0, old.1);
        let new = split_colour(new.0, new.1);
        for vert in self.points.iter_mut().chain(&mut self.lines).chain(&mut self.tris) {
            if vert.blend == old {
                vert.blend = new;
            }
        }
    }
}

impl From<AtlasRect> for [f32; 4] {
    fn from(ar: AtlasRect) -> Self {
        [ar.x as f32, ar.y as f32, ar.w as f32, ar.h as f32]
    }
}

impl From<BlendType> for u32 {
    fn from(bt: BlendType) -> Self {
        match bt {
            BlendType::Zero => glow::ZERO,
            BlendType::One => glow::ONE,
            BlendType::SrcColour => glow::SRC_COLOR,
            BlendType::InvSrcColour => glow::ONE_MINUS_SRC_COLOR,
            BlendType::SrcAlpha => glow::SRC_ALPHA,
            BlendType::InvSrcAlpha => glow::ONE_MINUS_SRC_ALPHA,
            BlendType::DestAlpha => glow::DST_ALPHA,
            BlendType::InvDestAlpha => glow::ONE_MINUS_DST_ALPHA,
            BlendType::DestColour => glow::DST_COLOR,
            BlendType::InvDestColour => glow::ONE_MINUS_DST_COLOR,
            BlendType::SrcAlphaSaturate => glow::SRC_ALPHA_SATURATE,
        }
    }
}

impl From<DepthFunc> for u32 {
    fn from(func: DepthFunc) -> Self {
        match func {
            DepthFunc::Never => glow::NEVER,
            DepthFunc::Less => glow::LESS,
            DepthFunc::Equal => glow::EQUAL,
            DepthFunc::LessEqual => glow::LEQUAL,
            DepthFunc::Greater => glow::GREATER,
            DepthFunc::NotEqual => glow::NOTEQUAL,
            DepthFunc::GreaterEqual => glow::GEQUAL,
            DepthFunc::Always => glow::ALWAYS,
        }
    }
}

impl From<PrimitiveShape> for u32 {
    fn from(shape: PrimitiveShape) -> Self {
        match shape {
            PrimitiveShape::Point => glow::POINTS,
            PrimitiveShape::Line => glow::LINES,
            PrimitiveShape::Triangle => glow::TRIANGLES,
        }
    }
}

/// The GL primitive for a `PrimitiveType`, *keeping* strips and fans.
///
/// `PrimitiveShape` deliberately collapses those three into `Triangle`, and that is right for the
/// immediate-mode queue: it unpacks a strip into independent triangles as it builds, so that by the
/// time anything is drawn the distinction is gone. A game-supplied vertex buffer is the opposite --
/// it goes to the GPU exactly as the game laid it out, so the primitive type has to survive.
///
/// Losing it drew renex engine's 13-index shadow-volume strip as four separate triangles, which is
/// four of the eleven the strip means -- so every block's shadow was missing whole wedges and each
/// light came out with a starburst of light spilling through its own casters.
fn gl_primitive(primitive: PrimitiveType) -> u32 {
    match primitive {
        PrimitiveType::PointList => glow::POINTS,
        PrimitiveType::LineList => glow::LINES,
        PrimitiveType::LineStrip => glow::LINE_STRIP,
        PrimitiveType::TriList => glow::TRIANGLES,
        PrimitiveType::TriStrip => glow::TRIANGLE_STRIP,
        PrimitiveType::TriFan => glow::TRIANGLE_FAN,
    }
}

/// A builder to be used for building basic shapes.
struct ShapeBuilder {
    primitive: PrimitiveBuilder,
    outline: bool,
    depth: f32,
    alpha: f64,
}

impl ShapeBuilder {
    fn new(outline: bool, atlas_ref: AtlasRect, alpha: f64, depth: f32) -> Self {
        Self {
            primitive: PrimitiveBuilder::new(
                atlas_ref,
                if outline { PrimitiveType::LineStrip } else { PrimitiveType::TriFan },
                false,
            ),
            outline,
            depth,
            alpha,
        }
    }

    /// Shortcut for basic shapes.
    fn push_point(&mut self, x: f64, y: f64, colour: i32) -> &mut Self {
        self.primitive.push_vertex([x as f32, y as f32, self.depth], [0.0, 0.0], split_colour(colour, self.alpha), [
            0.0, 0.0, 0.0,
        ]);
        self
    }

    /// Should only be called once. This is only used for basic shapes, so it's fine for it to be *possible* to
    /// call it multiple times, as that makes things easier elsewhere.
    fn build(&mut self) -> &PrimitiveBuilder {
        if self.outline {
            let vertices = self.primitive.get_vertices();
            if vertices.len() > 2 {
                let vertex = vertices[0];
                self.primitive.push_vertex_raw(vertex);
            }
        }
        &self.primitive
    }
}

/// Whether to check for GL errors around every draw call -- see `check_gl_error_hot`. Read from the
/// page rather than a build flag so a slow or broken run can be looked at on the deployed site, and
/// cached rather than read per call: `localStorage` is not free at the rate a draw path runs.
fn gl_debug() -> bool {
    thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(false) };
        static COUNTDOWN: Cell<u32> = const { Cell::new(1) };
    }
    let due = COUNTDOWN.with(|c| {
        let next = c.get().saturating_sub(1);
        c.set(next);
        next == 0
    });
    if due {
        COUNTDOWN.with(|c| c.set(600));
        // Two places, because a game runs in a throwaway `<iframe>`: the console's `globalThis` is
        // not the one the engine sees, but `localStorage` is shared across the origin.
        //
        // The global is the one that works in every realm, and so the only one in a worker: there
        // is no `Window` there to reach `localStorage` through, and the API is main-thread-only
        // regardless. `web_sys::window()` returning `None` is therefore a realm without the
        // persistent flag rather than a failure -- whoever wants the flag set in a worker sets it
        // on that worker's own global.
        let key = wasm_bindgen::JsValue::from_str("__emuGlDebug");
        let global = js_sys::Reflect::get(&js_sys::global(), &key).map(|v| v.is_truthy()).unwrap_or(false);
        let stored = web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("__emuGlDebug").ok().flatten())
            .is_some_and(|v| !v.is_empty() && v != "0" && v != "false");
        ENABLED.with(|e| e.set(global || stored));
    }
    ENABLED.with(Cell::get)
}

/// How many `c#` constant registers each stage keeps. Shader model 3 gives a vertex shader 256 and
/// a pixel shader 224; allocating the full file up front costs 16KB of ordinary memory and means a
/// game can write any register at any time without the engine having to grow anything mid-frame.
const SHADER_CONSTANT_REGISTERS: usize = 256;

/// How many sampler stages of each kind are tracked. D3D9 gives a pixel shader sixteen and a
/// vertex shader four; eight each is more than any of these games uses and keeps both kinds inside
/// the sixteen combined texture units GLES 3.0 guarantees.
const SHADER_SAMPLER_STAGES: usize = 8;

/// The texture unit a sampler stage reads from. The two kinds have separate registers in D3D9, so
/// they need separate units here or a vertex `s0` would fight a pixel `s0`.
fn sampler_texture_unit(kind: ShaderStage, stage: u32) -> u32 {
    let stage = stage.min(SHADER_SAMPLER_STAGES as u32 - 1);
    match kind {
        ShaderStage::Pixel => stage,
        ShaderStage::Vertex => SHADER_SAMPLER_STAGES as u32 + stage,
    }
}

/// Per-stage sampler state, as `texture_set_stage_interpolation`/`_repeat` set it. `None` means the
/// game has not said, and the draw's own settings apply.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct SamplerState {
    interpolate: Option<bool>,
    repeat: Option<(bool, bool)>,
}

impl SamplerState {
    fn is_default(self) -> bool {
        self.interpolate.is_none() && self.repeat.is_none()
    }
}

/// What a caller knows about the texture of a whole vertex buffer, when it knows it for all of it.
#[derive(Clone, Copy)]
struct DrawTexture {
    rect: AtlasRect,
    /// False when the draw has no texture at all. The vertices' own coordinates then mean nothing,
    /// and the built-in program wants NaN -- its signal for "untextured".
    textured: bool,
}

/// The uniform-block binding point the shared `RenderState` lives on. Both the built-in program and
/// any custom program built from `DEFAULT_VERTEX_GLSL` have to agree on it.
const RENDER_STATE_BINDING: u32 = 1;

/// Stand-ins for a stage a game leaves unset, so a lone pixel shader still draws through the custom
/// path. Between them they reproduce what the built-in program does for an untextured-or-textured
/// 2D draw, which is what D3D's fixed-function stage would have supplied.
///
/// The vertex half reads `model` and `viewproj` out of the same `RenderState` uniform block the
/// built-in program uses. Declaring only the leading members is deliberate and safe: the block is
/// `std140`, so the offsets of the first two are fixed regardless of what follows, and there is no
/// point restating a whole light array this never looks at.
///
/// **Its inputs are named the way `sm3` names a translated shader's**, and that is load-bearing:
/// `discover_attributes` reads those names back out of the GLSL to work out which of this
/// renderer's vertex fields feeds each location, and `bind_custom_attributes` enables *only* what
/// it found. Under any other spelling it found nothing, so every attribute array stayed disabled
/// and every vertex arrived as GL's default `(0, 0, 0, 1)` -- collapsing the whole draw to one
/// point. A lone pixel shader therefore drew nothing at all, silently.
const DEFAULT_VERTEX_GLSL: &str = r#"#version 300 es
precision highp float;

layout(std140) uniform RenderState {
    mat4 model;
    mat4 viewproj;
};

layout(location = 0) in vec4 vs_in_position0;
layout(location = 1) in vec4 vs_in_color0;
layout(location = 2) in vec4 vs_in_texcoord0;

out vec4 vary_texcoord0;
out vec4 vary_color0;

void main() {
    // An untextured draw arrives with NaN texture coordinates -- that is how this renderer's queue
    // says "no texture", and the built-in vertex shader tests for it too. Sampling at NaN is
    // undefined in GL, and these draws do sample: they are pointed at the 1x1 white pixel.
    vec2 tex_coord = vs_in_texcoord0.xy;
    vary_texcoord0 = vec4(any(isnan(tex_coord)) ? vec2(0.0) : tex_coord, 0.0, 1.0);
    vary_color0 = vs_in_color0;
    gl_Position = viewproj * (model * vec4(vs_in_position0.xyz, 1.0));
}
"#;

/// The pixel half: sample stage 0 and modulate by the vertex colour, which is what the fixed
/// function pipeline does for a textured draw.
const DEFAULT_PIXEL_GLSL: &str = r#"#version 300 es
precision highp float;

uniform sampler2D ps_s0;

in vec4 vary_texcoord0;
in vec4 vary_color0;

layout(location = 0) out vec4 ps_colour0;

void main() {
    ps_colour0 = texture(ps_s0, vary_texcoord0.xy) * vary_color0;
}
"#;

/// One compiled stage, kept alive until the game destroys it.
struct CompiledShader {
    stage: ShaderStage,
    shader: glow::Shader,
    /// The declared vertex inputs, as `(location, vf_usage, semantic index)`, recovered from the
    /// GLSL by name. Vertex stage only. Two consumers want different things from it: an
    /// immediate-mode draw can only offer four semantics (`bind_custom_attributes`), while a
    /// game-supplied vertex buffer declares its own and needs the whole list.
    attribute_bindings: Vec<(u32, u8, u8)>,
}

/// The renderer's fixed vertex layout, which is what an immediate-mode draw has to offer a
/// game-supplied vertex shader.
///
/// A custom shader declares its inputs by D3D semantic (`dcl_position v0`), and those semantics are
/// exactly the four things this vertex format already carries -- so a shader that asks for
/// `position`, `texcoord0`, `color0` or `normal` can be fed from the same buffer every other draw
/// uses. Anything else it declares is left disabled, which GL defines as `(0, 0, 0, 1)`, and is the
/// honest answer: an immediate-mode draw has no blend weights to give it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BuiltinAttribute {
    Position,
    Colour,
    TexCoord,
    Normal,
}

impl BuiltinAttribute {
    /// The field of `Vertex` this reads, as (component count, byte offset).
    fn layout(self) -> (i32, i32) {
        match self {
            Self::Position => (3, offset_of!(Vertex, pos) as i32),
            Self::Colour => (4, offset_of!(Vertex, blend) as i32),
            Self::TexCoord => (2, offset_of!(Vertex, tex_coord) as i32),
            Self::Normal => (3, offset_of!(Vertex, normal) as i32),
        }
    }
}

/// A linked (vertex, pixel) pair, plus the uniform locations worth not looking up again.
struct ShaderProgram {
    program: glow::Program,
    constant_locations: [Option<glow::UniformLocation>; 2],
    /// Which sampler registers the program declares, and where to write their texture unit.
    sampler_locations: Vec<(ShaderStage, u32, glow::UniformLocation)>,
    attribute_bindings: Vec<(u32, u8, u8)>,
}

#[derive(Clone, Copy)]
struct Framebuffer {
    texture: Option<glow::Texture>,
    zbuf: Option<glow::Texture>,
    fbo: Option<glow::Framebuffer>,
    // NEW vs. opengl.rs: WebGL2/glow has no `glGetTexLevelParameteriv` equivalent, so dimensions
    // are tracked here instead of being queried back from the driver whenever needed.
    width: i32,
    height: i32,
}

pub struct RendererImpl {
    gl: glow::Context,
    // Kept alive for the renderer's lifetime but never read again after setup (the VAO's attrib
    // bindings and the VBO's ARRAY_BUFFER binding persist implicitly; the program stays the only
    // one ever `use_program`'d) -- matches opengl.rs, which doesn't store these as fields at all.
    _program: glow::Program,
    _vao: glow::VertexArray,
    _commands_vbo: glow::Buffer,

    // -- game-supplied shaders. All inert until a game creates one; see `active_program`. --
    /// Compiled stages by handle. A `None` slot is a destroyed shader, so handles are never reused
    /// and a game holding a stale one gets nothing rather than someone else's shader.
    shaders: Vec<Option<CompiledShader>>,
    /// Linked programs by the (vertex, pixel) pair that produced them. D3D9 binds the two stages
    /// independently and GL links them together, so the program only exists once a draw needs a
    /// particular combination -- and is kept, because a game switching between two effects would
    /// otherwise relink on every switch.
    programs: HashMap<(i32, i32), ShaderProgram>,
    /// The stages the game has set, which is not the same as a program: either may be `None`, in
    /// which case `active_shader_pair` fills it with a stand-in.
    bound_shaders: [Option<ShaderRef>; 2],
    /// Those stand-ins, compiled on first use. Indexed by `ShaderStage`.
    default_shaders: [Option<ShaderRef>; 2],
    shader_constants: [Vec<[f32; 4]>; 2],
    shader_constants_dirty: [bool; 2],
    /// Indexed by `ShaderStage` then by stage number.
    shader_samplers: [[Option<AtlasRef>; SHADER_SAMPLER_STAGES]; 2],
    shader_sampler_state: [[SamplerState; SHADER_SAMPLER_STAGES]; 2],
    /// GL sampler objects, created only for a stage the game has actually set state on. They
    /// override the texture's own parameters for one unit, which is the whole point: D3D's sampler
    /// state is per stage, and two stages reading the same texture may filter it differently.
    sampler_objects: [[Option<glow::Sampler>; SHADER_SAMPLER_STAGES]; 2],
    /// Standalone copies of atlas sub-rects, so a game's shader sees a texture whose 0..1 is the
    /// sprite instead of the whole packed page. Keyed by `(page, x, y, w, h)`; see `sampler_view`.
    sampler_views: HashMap<(u32, i32, i32, i32, i32), glow::Texture>,
    /// Distinct diagnostic lines already logged, so `debug_note` says each thing once. Bounded so
    /// a game cannot flood the console.
    debug_seen: std::collections::HashSet<String>,
    /// The vertex array the custom path draws through. Separate from `_vao` so the built-in
    /// program's attribute setup is never rewritten -- a custom shader binds its own layout here
    /// and the fixed-function path cannot be disturbed by it.
    custom_vao: glow::VertexArray,
    /// Which program `custom_vao` is currently configured for, so it is only rewired on a change.
    custom_vao_program: Option<(i32, i32)>,
    /// The vertex array a game-supplied vertex buffer draws through, and the GL buffers its streams
    /// are uploaded into. Separate again from `custom_vao`, whose layout is this renderer's own.
    stream_vao: glow::VertexArray,
    stream_buffers: Vec<glow::Buffer>,
    index_buffer: Option<glow::Buffer>,
    /// Which program was last made current. Uniform values in GL belong to a program, not to the
    /// context, so switching programs invalidates the constants just as surely as a game writing
    /// one does -- and a freshly linked program starts with all of them at zero.
    active_custom_program: Option<(i32, i32)>,

    atlas_packers: Vec<DensePacker>,
    texture_ids: Vec<Option<glow::Texture>>,
    zbuf_ids: Vec<Option<glow::Texture>>,
    fbo_ids: Vec<Option<glow::Framebuffer>>,
    // NEW vs. opengl.rs (see the `Framebuffer` comment above): per-atlas-id (i.e. per-page /
    // per-surface) texture dimensions, parallel to `texture_ids`/`zbuf_ids`/`fbo_ids`.
    atlas_dims: Vec<(i32, i32)>,
    texture_rects: Vec<Option<AtlasRect>>,
    stock_texture_count: usize,
    stock_atlas_count: u32,
    current_atlas: u32,
    framebuffer: Framebuffer,
    stored_framebuffer: Option<Framebuffer>,
    zbuf_internal_format: i32,
    zbuf_type: u32,
    zbuf_trashed: bool,
    white_pixel: AtlasRect,
    vertex_queue: Vec<Vertex>,
    queue_type: PrimitiveShape,
    queue_render_state: RenderState,
    next_render_state: RenderState,
    render_state_updated: bool,
    circle_precision: i32,
    using_3d: bool,
    perspective: bool,
    depth: f32,
    primitive_2d: PrimitiveBuilder,
    primitive_3d: PrimitiveBuilder,

    // No vsync toggle exists in a browser (frame pacing is requestAnimationFrame's job); this is
    // simple state so `Renderer::state()`/`set_state()` (used by savestates, pruned on wasm
    // anyway) round-trip something sane.
    vsync: Cell<bool>,

    // GL binding-point tracking. WebGL2/glow has no way to query a currently-bound
    // texture/framebuffer back as a usable handle (unlike desktop GL's
    // `glGetIntegerv(GL_*_BINDING)`, which opengl.rs relies on constantly for "save the current
    // binding, do something temporarily, restore it" patterns) -- so we mirror the bindings we
    // make ourselves. `Cell` (not a plain field) because several trait methods that need this
    // (e.g. `get_pixels`, `dump_sprite_part`, `stored_pixels`) take `&self`.
    current_texture_2d: Cell<Option<glow::Texture>>,
    current_read_fbo: Cell<Option<glow::Framebuffer>>,
    current_draw_fbo: Cell<Option<glow::Framebuffer>>,
    viewport: Cell<(i32, i32, i32, i32)>,
    // Smallest depth difference this framebuffer can resolve, which `set_depth_bias` converts a
    // D3D-style normalized bias into a count of.
    depth_unit: f32,

    // NEW vs. opengl.rs: scratch texture used to break framebuffer feedback loops (see the module
    // doc comment and `resolve_draw_texture`). Lazily created on the first self-referential draw --
    // most games never make one -- and reallocated only when the target's size changes, so a game
    // that does this every frame at a fixed size pays one allocation total. `feedback_copy_dims` is
    // the size it's currently allocated at; the fragment shader derives texel coordinates from
    // `textureSize(tex, 0)`, so the copy has to match the texture it stands in for exactly.
    feedback_copy: Option<glow::Texture>,
    feedback_copy_fbo: Option<glow::Framebuffer>,
    feedback_copy_dims: (i32, i32),

    loc_gm81_normalize: Option<glow::UniformLocation>, // uniform bool gm81_normalize
    loc_tex: Option<glow::UniformLocation>,            // uniform sampler2D tex
    buf_state: glow::Buffer,                           // uniform RenderState state
}

impl RendererImpl {
    pub fn new(
        options: &RendererOptions,
        _connection: &Connection,
        window: &Window,
        clear_colour: Colour,
    ) -> Result<Self, String> {
        let gl = glow::Context::from_webgl2_context(window.webgl2_context());

        unsafe {
            web_sys::console::log_1(
                &format!(
                    "creating WebGL2 graphics context\n  > GL_VERSION: \"{}\"\n  > GL_VENDOR: \"{}\"",
                    gl.get_parameter_string(glow::VERSION),
                    gl.get_parameter_string(glow::VENDOR),
                )
                .into(),
            );

            let (zbuf_internal_format, zbuf_type) = if options.zbuf_24 {
                (glow::DEPTH_COMPONENT24 as i32, glow::UNSIGNED_INT)
            } else {
                (glow::DEPTH_COMPONENT16 as i32, glow::UNSIGNED_SHORT)
            };

            // Compile vertex shader
            let vertex_shader = gl.create_shader(glow::VERTEX_SHADER)?;
            gl.shader_source(vertex_shader, VERTEX_SHADER_SOURCE);
            gl.compile_shader(vertex_shader);
            if !gl.get_shader_compile_status(vertex_shader) {
                return Err(format!(
                    "Failed to compile vertex shader, compiler output:\n{}",
                    gl.get_shader_info_log(vertex_shader)
                ))
            }

            // Compile fragment shader
            let fragment_shader = gl.create_shader(glow::FRAGMENT_SHADER)?;
            gl.shader_source(fragment_shader, FRAGMENT_SHADER_SOURCE);
            gl.compile_shader(fragment_shader);
            if !gl.get_shader_compile_status(fragment_shader) {
                return Err(format!(
                    "Failed to compile fragment shader, compiler output:\n{}",
                    gl.get_shader_info_log(fragment_shader)
                ))
            }

            // Link shaders
            let program = gl.create_program()?;
            gl.attach_shader(program, vertex_shader);
            gl.attach_shader(program, fragment_shader);
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(format!("Failed to link shaders, compiler output:\n{}", gl.get_program_info_log(program)))
            }
            gl.delete_shader(vertex_shader);
            gl.delete_shader(fragment_shader);

            // set up vertex array
            let vao = gl.create_vertex_array()?;
            gl.bind_vertex_array(Some(vao));
            // A second one, left empty for now, is what game-supplied shaders draw through -- see
            // `custom_vao`. Created here so the field is never an `Option` nothing checks.
            let custom_vao = gl.create_vertex_array()?;
            let stream_vao = gl.create_vertex_array()?;

            // Scissor is the one piece of GL state `RenderState` does not model -- it is always on,
            // and `set_gl_viewport` keeps its box equal to the viewport. Everything else is seeded
            // from the tracked defaults by `apply_render_state_forced` once the renderer exists,
            // so there is exactly one description of the starting state; see its doc comment.
            gl.enable(glow::SCISSOR_TEST);

            // Note: no `glProvokingVertex` equivalent exists in GLES/WebGL2 (see the module-level
            // doc comment) -- opengl.rs's `gl.ProvokingVertex(gl::FIRST_VERTEX_CONVENTION)` call
            // is simply not portable here.

            // Create and bind the VBO once
            let commands_vbo = gl.create_buffer()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(commands_vbo));

            // layout (location = 0) in vec3 pos;
            // layout (location = 1) in vec4 blend;
            // layout (location = 2) in vec2 tex_coord;
            // layout (location = 3) in vec3 normal;
            // layout (location = 4) in vec4 atlas_xywh;
            let stride = size_of::<Vertex>() as i32;
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, stride, offset_of!(Vertex, pos) as i32);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 4, glow::FLOAT, false, stride, offset_of!(Vertex, blend) as i32);
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(2, 2, glow::FLOAT, false, stride, offset_of!(Vertex, tex_coord) as i32);
            gl.enable_vertex_attrib_array(3);
            gl.vertex_attrib_pointer_f32(3, 3, glow::FLOAT, false, stride, offset_of!(Vertex, normal) as i32);
            gl.enable_vertex_attrib_array(4);
            gl.vertex_attrib_pointer_f32(4, 4, glow::FLOAT, false, stride, offset_of!(Vertex, atlas_xywh) as i32);

            // Use program
            gl.use_program(Some(program));

            // Configure read_pixels() to align to 1 byte
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);

            // Create framebuffer
            let framebuffer_texture = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(framebuffer_texture));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                options.size.0 as i32,
                options.size.1 as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                None,
            );
            let framebuffer_zbuf = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(framebuffer_zbuf));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                zbuf_internal_format,
                options.size.0 as i32,
                options.size.1 as i32,
                0,
                glow::DEPTH_COMPONENT,
                zbuf_type,
                None,
            );
            let framebuffer_fbo = gl.create_framebuffer()?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer_fbo));
            gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(framebuffer_texture),
                0,
            );
            gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::DEPTH_ATTACHMENT,
                glow::TEXTURE_2D,
                Some(framebuffer_zbuf),
                0,
            );

            // setup uniform buffer for render state
            let loc_state = RENDER_STATE_BINDING;
            let ind = gl
                .get_uniform_block_index(program, "RenderState")
                .expect("RenderState uniform block not found in shader program");
            gl.uniform_block_binding(program, ind, loc_state);
            let buf_state = gl.create_buffer()?;
            gl.bind_buffer_base(glow::UNIFORM_BUFFER, loc_state, Some(buf_state));
            assert_eq!(gl.get_error(), 0);

            let loc_gm81_normalize = gl.get_uniform_location(program, "gm81_normalize");
            let loc_tex = gl.get_uniform_location(program, "tex");

            // Create Renderer
            let mut renderer = Self {
                _program: program,
                _vao: vao,
                _commands_vbo: commands_vbo,

                shaders: vec![],
                programs: HashMap::new(),
                bound_shaders: [None, None],
                default_shaders: [None, None],
                shader_constants: [vec![[0.0; 4]; SHADER_CONSTANT_REGISTERS], vec![
                    [0.0; 4];
                    SHADER_CONSTANT_REGISTERS
                ]],
                shader_constants_dirty: [true, true],
                shader_samplers: [[None; SHADER_SAMPLER_STAGES]; 2],
                shader_sampler_state: [[SamplerState::default(); SHADER_SAMPLER_STAGES]; 2],
                sampler_objects: [[None; SHADER_SAMPLER_STAGES]; 2],
                sampler_views: HashMap::new(),
                debug_seen: std::collections::HashSet::new(),
                custom_vao,
                custom_vao_program: None,
                stream_vao,
                stream_buffers: Vec::new(),
                index_buffer: None,
                active_custom_program: None,

                atlas_packers: vec![],
                texture_ids: vec![],
                zbuf_ids: vec![],
                fbo_ids: vec![],
                atlas_dims: vec![],
                texture_rects: vec![],
                stock_texture_count: 0,
                stock_atlas_count: 0,
                current_atlas: 0,
                framebuffer: Framebuffer {
                    texture: Some(framebuffer_texture),
                    zbuf: Some(framebuffer_zbuf),
                    fbo: Some(framebuffer_fbo),
                    width: options.size.0 as i32,
                    height: options.size.1 as i32,
                },
                stored_framebuffer: None,
                zbuf_internal_format,
                zbuf_type,
                zbuf_trashed: false,
                white_pixel: Default::default(),
                vertex_queue: Vec::with_capacity(1536),
                queue_type: PrimitiveShape::Triangle,
                queue_render_state: RenderState {
                    interpolate_pixels: options.interpolate_pixels.into(),
                    ..Default::default()
                },
                next_render_state: RenderState {
                    interpolate_pixels: options.interpolate_pixels.into(),
                    ..Default::default()
                },
                render_state_updated: false,
                circle_precision: 24,
                using_3d: false,
                perspective: false,
                depth: 0.0,
                primitive_2d: PrimitiveBuilder::new(Default::default(), PrimitiveType::PointList, false),
                primitive_3d: PrimitiveBuilder::new(Default::default(), PrimitiveType::PointList, false),

                vsync: Cell::new(options.vsync),

                current_texture_2d: Cell::new(Some(framebuffer_zbuf)), // matches last BindTexture above
                current_read_fbo: Cell::new(Some(framebuffer_fbo)),
                current_draw_fbo: Cell::new(Some(framebuffer_fbo)),
                viewport: Cell::new((0, 0, 0, 0)),
                depth_unit: if options.zbuf_24 { 1.0 / 16777216.0 } else { 1.0 / 65536.0 },

                feedback_copy: None,
                feedback_copy_fbo: None,
                feedback_copy_dims: (0, 0),

                loc_gm81_normalize,
                loc_tex,
                buf_state,

                gl,
            };
            assert_eq!(renderer.gl.get_error(), 0);

            // Put GL into the state `queue_render_state` claims it is in, before anything draws.
            renderer.apply_render_state_forced();

            // default uniform values
            renderer.gl.uniform_1_i32(renderer.loc_gm81_normalize.as_ref(), options.normalize_normals as i32);
            renderer.gl.uniform_1_i32(renderer.loc_tex.as_ref(), 0);
            renderer.gl.bind_buffer(glow::UNIFORM_BUFFER, Some(renderer.buf_state));
            let bytes = as_u8_slice(&renderer.queue_render_state, offset_of!(RenderState, end_of_uniform));
            renderer.gl.buffer_data_u8_slice(glow::UNIFORM_BUFFER, bytes, glow::STATIC_DRAW);
            assert_eq!(renderer.gl.get_error(), 0);

            // Start first frame
            renderer.setup_frame(clear_colour);

            // verify it actually worked
            match renderer.gl.get_error() {
                0 => Ok(renderer),
                err => Err(format!("Failed to fully initialize WebGL2! (GL code {})", err)),
            }
        }
    }

    fn setup_frame(&mut self, clear_colour: Colour) {
        let (width, height) = (self.framebuffer.width, self.framebuffer.height);
        self.set_view(0, 0, width, height, 0.0, 0, 0, width, height);
        self.clear_view(clear_colour, 1.0);
    }

    fn get_rect_mut(&mut self, id: AtlasRef) -> Option<&mut AtlasRect> {
        id.0.try_into()
            .ok()
            .and_then(move |id: usize| self.texture_rects.get_mut(id))
            .and_then(|o: &mut Option<AtlasRect>| o.as_mut())
    }

    /// Binds a texture to `TEXTURE_2D` and updates our own binding-point mirror (see the doc
    /// comment on `current_texture_2d`).
    /// Drains the GL error queue and reports anything in it, naming the operation that raised it.
    ///
    /// NEW vs. opengl.rs, which asserts these away instead. WebGL2 rejects a number of things
    /// desktop GL waves through (feedback loops, same-framebuffer overlapping blits, stricter
    /// format matching), so a faithful port necessarily meets errors the desktop renderer never
    /// could — and aborting the whole game over one is the wrong trade in a browser. The offending
    /// call has already been dropped by the driver and everything else still draws, so log which
    /// operation raised it and carry on. The browser prints its own message saying *why* right
    /// next to ours, which together are usually enough to pinpoint the call.

    /// The same check, from a place that runs once per *draw call* rather than once per operation
    /// that can plausibly fail.
    ///
    /// `glGetError` is the one call in the draw path that is synchronous: WebGL is a command queue
    /// the browser hands to the GPU process asynchronously, and asking whether an error occurred
    /// forces it to flush that queue and wait for the answer. The cost is per call, not per
    /// command, so it scales with how many times a frame is *interrupted* -- and a game that draws
    /// from many textures interrupts constantly, since a texture change ends the batch. On a game
    /// with a few hundred draw calls a frame this can cost more than everything it is checking.
    ///
    /// So these are opt-in: set `globalThis.__emuGlDebug = true` to get them back. The checks
    /// around resource creation, framebuffer setup and pixel readback stay unconditional -- those
    /// are the ones that actually catch bugs, and they run at a rate nobody can measure.
    fn check_gl_error_hot(&self, during: &str) {
        if gl_debug() {
            self.check_gl_error(during);
        }
    }

    fn check_gl_error(&self, during: &str) {
        // Bounded rather than `while`: a lost context can keep yielding CONTEXT_LOST_WEBGL, and
        // hanging the frame loop would be a worse failure than the one we're reporting.
        for _ in 0..8 {
            let err = unsafe { self.gl.get_error() };
            if err == 0 {
                return
            }
            let name = match err {
                glow::INVALID_ENUM => "INVALID_ENUM",
                glow::INVALID_VALUE => "INVALID_VALUE",
                glow::INVALID_OPERATION => "INVALID_OPERATION",
                glow::INVALID_FRAMEBUFFER_OPERATION => "INVALID_FRAMEBUFFER_OPERATION",
                glow::OUT_OF_MEMORY => "OUT_OF_MEMORY",
                _ => "unrecognised error",
            };
            web_sys::console::warn_1(&format!("WebGL2: {} ({}) raised during {}", name, err, during).into());
        }
    }

    fn bind_texture_2d(&self, tex: Option<glow::Texture>) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, tex);
        }
        self.current_texture_2d.set(tex);
    }

    fn bind_read_fbo(&self, fbo: Option<glow::Framebuffer>) {
        unsafe {
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, fbo);
        }
        self.current_read_fbo.set(fbo);
    }

    fn bind_draw_fbo(&self, fbo: Option<glow::Framebuffer>) {
        unsafe {
            self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, fbo);
        }
        self.current_draw_fbo.set(fbo);
    }

    /// Binds a framebuffer to both the read and draw targets at once (equivalent to binding
    /// `GL_FRAMEBUFFER`).
    fn bind_fbo(&self, fbo: Option<glow::Framebuffer>) {
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, fbo);
        }
        self.current_draw_fbo.set(fbo);
        self.current_read_fbo.set(fbo);
    }

    fn set_gl_viewport(&self, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            self.gl.viewport(x, y, w, h);
            self.gl.scissor(x, y, w, h);
        }
        self.viewport.set((x, y, w, h));
    }

    /// Updates the renderer state.
    /// Call this before doing any drawing.
    fn update_render_state(&mut self) {
        if self.render_state_updated && self.queue_render_state != self.next_render_state {
            self.flush_queue();
            // update the viewproj matrix before doing any cloning
            if self.queue_render_state.view_matrix != self.next_render_state.view_matrix
                || self.queue_render_state.proj_matrix != self.next_render_state.proj_matrix
            {
                let (_, _, vw, vh) = self.viewport.get();
                self.next_render_state.update_matrix(vw, vh);
            }

            let mut old_render_state = self.next_render_state.clone();
            std::mem::swap(&mut old_render_state, &mut self.queue_render_state);
            self.render_state_updated = false;
            self.emit_render_state(Some(&old_render_state));
        }
    }

    /// Puts GL into the state `queue_render_state` describes, at construction, so the two provably
    /// agree before anything draws.
    ///
    /// Needed because `emit_render_state` only issues a GL call for a field that *changed*. Any
    /// field whose tracked default disagrees with the context's actual starting state is therefore
    /// invisible until a game happens to set it to something else -- and worse, a game setting it
    /// to the tracked default is silently dropped as "already set". Two fields were exactly that:
    /// `depth_func` was tracked as `Less` while this constructor had put GL into `LEQUAL` by hand,
    /// and `write_depth` was tracked as `false` while GL kept its own default of *enabled*, so
    /// `d3d_set_zwriteenable(false)` could never take effect. Seeding from the tracked state kills
    /// the whole class rather than those two instances.
    fn apply_render_state_forced(&mut self) {
        self.emit_render_state(None);
    }

    /// Issues the GL calls for `queue_render_state`. With `old`, only the fields that differ from
    /// it; with `None`, all of them.
    fn emit_render_state(&mut self, old: Option<&RenderState>) {
        {
            let changed = |f: &dyn Fn(&RenderState) -> bool| old.map_or(true, |o| f(o));
            let alpha_blending = self.next_render_state.alpha_blending;
            let blend_mode = self.next_render_state.blend_mode;
            let blend_mode_alpha = self.next_render_state.blend_mode_alpha;
            let depth_test = self.next_render_state.depth_test;
            let write_depth = self.next_render_state.write_depth;
            let culling = self.next_render_state.culling;
            let cull_clockwise = self.next_render_state.cull_clockwise;
            let colour_mask = self.next_render_state.colour_mask;
            let depth_func = self.next_render_state.depth_func;
            let depth_bias = self.next_render_state.depth_bias;
            let depth_range = self.next_render_state.depth_range;
            unsafe {
                if changed(&|o| o.alpha_blending != alpha_blending) {
                    if alpha_blending {
                        self.gl.enable(glow::BLEND);
                    } else {
                        self.gl.disable(glow::BLEND);
                    }
                }
                if changed(&|o| o.blend_mode != blend_mode || o.blend_mode_alpha != blend_mode_alpha) {
                    let (src, dst) = blend_mode;
                    match blend_mode_alpha {
                        Some((src_alpha, dst_alpha)) => {
                            self.gl.blend_func_separate(src.into(), dst.into(), src_alpha.into(), dst_alpha.into())
                        },
                        None => self.gl.blend_func(src.into(), dst.into()),
                    }
                }
                if changed(&|o| o.depth_test != depth_test) {
                    if bool::from(depth_test) {
                        self.gl.enable(glow::DEPTH_TEST);
                    } else {
                        self.gl.disable(glow::DEPTH_TEST);
                    }
                }
                if changed(&|o| o.culling != culling) {
                    if culling {
                        self.gl.enable(glow::CULL_FACE);
                    } else {
                        self.gl.disable(glow::CULL_FACE);
                    }
                }
                if changed(&|o| o.cull_clockwise != cull_clockwise) {
                    // GL culls back faces, so which winding is *front* decides which one goes.
                    // The viewproj matrix flips vertically (see `update_matrix`), which reverses
                    // screen-space winding, so GL's default front face is D3D's clockwise one --
                    // that is, `cull_clockwise == false` is already D3D's `D3DCULL_CCW`, the mode
                    // stock GM8's `d3d_set_culling` means.
                    self.gl.front_face(if cull_clockwise { glow::CW } else { glow::CCW });
                }
                if changed(&|o| o.colour_mask != colour_mask) {
                    let (r, g, b, a) = colour_mask;
                    self.gl.color_mask(r, g, b, a);
                }
                if changed(&|o| o.depth_func != depth_func) {
                    self.gl.depth_func(depth_func.into());
                }
                if changed(&|o| o.depth_bias != depth_bias) {
                    if depth_bias == 0.0 {
                        self.gl.disable(glow::POLYGON_OFFSET_FILL);
                    } else {
                        self.gl.enable(glow::POLYGON_OFFSET_FILL);
                        // D3D's DEPTHBIAS is a constant offset in normalized depth; GL counts the
                        // same offset in units of the smallest difference the depth buffer can
                        // resolve. Same quantity, different scale.
                        self.gl.polygon_offset(0.0, depth_bias / self.depth_unit);
                    }
                }
                if changed(&|o| o.depth_range != depth_range) {
                    let (near, far) = depth_range;
                    self.gl.depth_range_f32(near, far);
                }
                if changed(&|o| o.write_depth != write_depth) {
                    self.gl.depth_mask(write_depth);
                }

                self.gl.bind_buffer(glow::UNIFORM_BUFFER, Some(self.buf_state));
                let bytes = as_u8_slice(&self.queue_render_state, offset_of!(RenderState, end_of_uniform));
                self.gl.buffer_data_u8_slice(glow::UNIFORM_BUFFER, bytes, glow::STATIC_DRAW);
                self.check_gl_error_hot("update_render_state");
            }
        }
    }

    fn setup_queue(&mut self, atlas_id: u32, queue_type: PrimitiveShape) {
        self.update_render_state();
        if atlas_id != self.current_atlas || self.queue_type != queue_type {
            self.flush_queue();
            self.current_atlas = atlas_id;
            self.queue_type = queue_type;
        }
    }

    fn push_primitive(&mut self, builder: &PrimitiveBuilder) {
        self.setup_queue(builder.get_atlas_id(), builder.get_shape());
        self.vertex_queue.extend_from_slice(builder.get_vertices());
    }

    /// Returns the texture to sample when drawing from `atlas_id`.
    ///
    /// NEW vs. opengl.rs. Normally that's just `texture_ids[atlas_id]`, but if that texture is the
    /// colour attachment of the framebuffer we're currently drawing *into*, sampling it directly is
    /// a feedback loop: WebGL2 raises `INVALID_OPERATION` and drops the draw entirely (desktop GL
    /// merely leaves the sampled texels undefined). This happens for real — a GM8 game doing
    /// `surface_set_target(s); draw_surface(s, ...)` is drawing a surface onto itself, which D3D8
    /// permitted, reading the contents as they were before the draw. So we sample a snapshot taken
    /// at exactly that moment, which both keeps the draw alive and matches what the game expects.
    fn resolve_draw_texture(&mut self, atlas_id: u32) -> Option<glow::Texture> {
        let texture = self.texture_ids.get(atlas_id as usize).copied().flatten();
        let fbo = self.fbo_ids.get(atlas_id as usize).copied().flatten();
        // `fbo.is_some()` matters: an atlas with no FBO and the canvas (the default framebuffer)
        // are both `None`, and those two must not be mistaken for each other.
        if texture.is_none() || fbo.is_none() || fbo != self.current_draw_fbo.get() {
            return texture
        }

        let (width, height) = self.atlas_dims[atlas_id as usize];
        // Min/mag filters are (re)set by `draw_buffer` on whatever we hand back, so not here.
        match self.snapshot_fbo(fbo, width, height) {
            Some((copy, _)) => Some(copy),
            // Nothing sane to do if the GPU won't give us the scratch; sampling the target directly
            // draws nothing, but that beats taking the whole game down.
            None => texture,
        }
    }

    /// The texture a game's own shader should see for a draw that uses `rect`.
    ///
    /// NEW vs. opengl.rs, and the reason 3D geometry came out wearing the whole texture atlas. Every
    /// sprite is packed into a shared page, and the built-in program copes because each vertex
    /// carries its sub-rect (`atlas_xywh`) and the fragment shader maps 0..1 into it. A game's
    /// shader knows nothing about any of that: on real D3D9 its `s0` was a texture of its own, so
    /// `tex2d(s0, uv)` with uv across 0..1 has to mean *the sprite*, not the page it shares with two
    /// hundred others.
    ///
    /// Whole-page rects -- every surface, and every sprite made at runtime, since both get a page to
    /// themselves -- are handed straight back. Anything else is copied out into a texture of its own
    /// the first time it is sampled and kept: only the pages the atlas builder packed at load can
    /// hold a sub-rect, and those never change afterwards.
    fn sampler_view(&mut self, rect: AtlasRect) -> Option<glow::Texture> {
        let (atlas_id, x, y, w, h) = (rect.atlas_id, rect.x, rect.y, rect.w, rect.h);
        let (page_w, page_h) = *self.atlas_dims.get(atlas_id as usize)?;
        if (x, y, w, h) == (0, 0, page_w, page_h) {
            return self.resolve_draw_texture(atlas_id)
        }
        if let Some(&view) = self.sampler_views.get(&(atlas_id, x, y, w, h)) {
            return Some(view)
        }

        // Copying out reads through a framebuffer, which `push_atlases` gave every packed page. A
        // page without one is not one of those, so there is nothing to do but sample it whole.
        let source_fbo = self.fbo_ids.get(atlas_id as usize).copied().flatten();
        let view = match (source_fbo, w > 0 && h > 0) {
            (Some(fbo), true) => unsafe { self.gl.create_texture().ok().map(|view| (view, fbo)) },
            _ => None,
        };
        let Some((view, source_fbo)) = view else { return self.resolve_draw_texture(atlas_id) };

        let prev_read_fbo = self.current_read_fbo.get();
        let prev_tex2d = self.current_texture_2d.get();
        unsafe {
            self.bind_texture_2d(Some(view));
            // WRAP is D3D9's default addressing mode, and what the pages themselves already carry.
            // Filters are set per draw by `bind_custom_program`, so they don't matter here.
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::REPEAT as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::REPEAT as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            // Allocated first and filled second, rather than `copy_tex_image_2d` doing both: that is
            // the pattern `snapshot_fbo` uses and the one this renderer is known to get right.
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                w,
                h,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                None,
            );
            self.bind_read_fbo(Some(source_fbo));
            self.gl.copy_tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, x, y, w, h);
        }
        self.check_gl_error("copying a sprite out of its atlas page for a shader");

        self.bind_read_fbo(prev_read_fbo);
        self.bind_texture_2d(prev_tex2d);

        self.sampler_views.insert((atlas_id, x, y, w, h), view);
        Some(view)
    }

    /// Copies the colour plane of `fbo` into the scratch texture and returns it, along with an FBO
    /// it's attached to so callers can blit *out* of it as well as sample it.
    ///
    /// NEW vs. opengl.rs — the shared machinery behind both places WebGL2's stricter aliasing rules
    /// bite (see `resolve_draw_texture` and `copy_surface`). The scratch is reallocated only when
    /// the requested size changes, so a game doing this every frame pays one allocation in total.
    fn snapshot_fbo(
        &mut self,
        fbo: Option<glow::Framebuffer>,
        width: i32,
        height: i32,
    ) -> Option<(glow::Texture, glow::Framebuffer)> {
        let (copy, copy_fbo) = match (self.feedback_copy, self.feedback_copy_fbo) {
            (Some(t), Some(f)) => (t, f),
            _ => unsafe {
                let (t, f) = (self.gl.create_texture().ok()?, self.gl.create_framebuffer().ok()?);
                self.feedback_copy = Some(t);
                self.feedback_copy_fbo = Some(f);
                self.feedback_copy_dims = (0, 0);
                (t, f)
            },
        };

        let prev_read_fbo = self.current_read_fbo.get();
        self.bind_texture_2d(Some(copy));
        unsafe {
            if self.feedback_copy_dims != (width, height) {
                // Allocate exactly like `create_surface` does, so the copy below is a plain pixel
                // transfer with no format conversion to get wrong.
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA as i32,
                    width,
                    height,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    None,
                );
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::REPEAT as i32);
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::REPEAT as i32);
                self.bind_read_fbo(Some(copy_fbo));
                self.gl.framebuffer_texture_2d(
                    glow::READ_FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(copy),
                    0,
                );
                self.feedback_copy_dims = (width, height);
            }
            self.bind_read_fbo(fbo);
            self.gl.copy_tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, 0, 0, width, height);
        }
        self.bind_read_fbo(prev_read_fbo);
        self.check_gl_error("snapshot_fbo");

        Some((copy, copy_fbo))
    }

    /// The (vertex, pixel) pair to draw with, or `None` for the built-in program.
    ///
    /// D3D9 sets the two stages independently and pairs whichever is missing with its
    /// fixed-function equivalent -- and games lean on that hard: this game calls `shader_pixel_set`
    /// sixty times against ten `shader_vertex_set`, because a screen-space effect only needs the
    /// pixel half. Requiring both meant all sixty fell back to the built-in program and did
    /// nothing at all, which is what a whole rendering pass silently not happening looks like.
    ///
    /// So a missing half is filled by a stand-in that does what this renderer's fixed-function
    /// stage does -- see `DEFAULT_VERTEX_GLSL` and `DEFAULT_PIXEL_GLSL`. They are created once, on
    /// demand, and live in the same table as a game's own shaders so the program cache needs no
    /// special case.
    fn active_shader_pair(&mut self) -> Option<(i32, i32)> {
        let (vertex, pixel) = (self.bound_shaders[0], self.bound_shaders[1]);
        if vertex.is_none() && pixel.is_none() {
            return None
        }
        let vertex = match vertex {
            Some(shader) => shader,
            None => self.default_stage(ShaderStage::Vertex)?,
        };
        let pixel = match pixel {
            Some(shader) => shader,
            None => self.default_stage(ShaderStage::Pixel)?,
        };
        Some((vertex.0, pixel.0))
    }

    /// The stand-in for a stage a game has not set. Compiled on first use and kept.
    fn default_stage(&mut self, stage: ShaderStage) -> Option<ShaderRef> {
        let index = stage as usize;
        if let Some(shader) = self.default_shaders[index] {
            return Some(shader)
        }
        let source = match stage {
            ShaderStage::Vertex => DEFAULT_VERTEX_GLSL,
            ShaderStage::Pixel => DEFAULT_PIXEL_GLSL,
        };
        match self.create_shader(stage, source) {
            Ok(shader) => {
                self.default_shaders[index] = Some(shader);
                Some(shader)
            },
            Err(log) => {
                web_sys::console::warn_1(&format!("default {:?} shader failed to compile: {}", stage, log).into());
                None
            },
        }
    }

    /// Scans translated GLSL for the vertex inputs `sm3` emitted, and works out which of this
    /// renderer's own attributes can feed each one.
    ///
    /// Reading the generated text rather than being told is deliberate: the alternative is to
    /// thread `sm3`'s attribute list through the renderer trait, which would put a shader-format
    /// detail into an interface `opengl.rs` also implements. The names are this engine's own
    /// output, in a fixed shape (`layout(location = N) in vec4 vs_in_<semantic><index>;`), so
    /// there is nothing here that another compiler's whitespace could break.
    fn discover_attributes(glsl: &str) -> Vec<(u32, u8, u8)> {
        let mut bindings = Vec::new();
        for line in glsl.lines().map(str::trim) {
            let Some(rest) = line.strip_prefix("layout(location = ") else { continue };
            let Some((location, rest)) = rest.split_once(')') else { continue };
            let Ok(location) = location.trim().parse::<u32>() else { continue };
            let Some(name) = rest.trim().strip_prefix("in vec4 vs_in_").and_then(|n| n.strip_suffix(';')) else {
                continue
            };
            // The name is `<semantic><index>`; split the trailing digits back off. These are the
            // `D3DDECLUSAGE` numbers, which `vf_usage_*` uses verbatim -- so a game's vertex
            // declaration and its shader's inputs are already speaking the same language.
            let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
            let (base, index) = name.split_at(name.len() - digits.max(1).min(name.len()));
            let Ok(index) = index.parse::<u8>() else { continue };
            let usage = match base {
                "position" => 0,
                "blendweight" => 1,
                "blendindices" => 2,
                "normal" => 3,
                "psize" => 4,
                "texcoord" => 5,
                "tangent" => 6,
                "binormal" => 7,
                "tessfactor" => 8,
                "positiont" => 9,
                "color" => 10,
                "fog" => 11,
                "depth" => 12,
                _ => continue,
            };
            bindings.push((location, usage, index));
        }
        bindings
    }

    /// Which of this renderer's own vertex fields can feed a declared input. Only four semantics
    /// have a source in the immediate-mode vertex format, and only at index 0 -- a second texture
    /// coordinate has nothing behind it, and feeding it the first would be inventing data.
    fn builtin_for_semantic(usage: u8, index: u8) -> Option<BuiltinAttribute> {
        if index != 0 {
            return None
        }
        Some(match usage {
            0 => BuiltinAttribute::Position,
            3 => BuiltinAttribute::Normal,
            5 => BuiltinAttribute::TexCoord,
            10 => BuiltinAttribute::Colour,
            _ => return None,
        })
    }

    /// Links a (vertex, pixel) pair, or returns the one already linked for it.
    fn shader_program(&mut self, pair: (i32, i32)) -> Option<&ShaderProgram> {
        if !self.programs.contains_key(&pair) {
            let program = self.link_shader_pair(pair)?;
            self.programs.insert(pair, program);
        }
        self.programs.get(&pair)
    }

    fn link_shader_pair(&mut self, (vertex, pixel): (i32, i32)) -> Option<ShaderProgram> {
        let vertex_shader = self.shaders.get(vertex as usize)?.as_ref()?;
        let attribute_bindings = vertex_shader.attribute_bindings.clone();
        let vertex_shader = vertex_shader.shader;
        let pixel_shader = self.shaders.get(pixel as usize)?.as_ref()?.shader;

        unsafe {
            let program = self.gl.create_program().ok()?;
            self.gl.attach_shader(program, vertex_shader);
            self.gl.attach_shader(program, pixel_shader);
            self.gl.link_program(program);
            if !self.gl.get_program_link_status(program) {
                web_sys::console::warn_1(
                    &format!("custom shader program failed to link: {}", self.gl.get_program_info_log(program)).into(),
                );
                self.gl.delete_program(program);
                return None
            }
            self.gl.detach_shader(program, vertex_shader);
            self.gl.detach_shader(program, pixel_shader);

            // The stand-in vertex shader reads the shared `RenderState` block, so a program built
            // from it has to be pointed at the same binding the built-in program uses. Translated
            // shaders never declare it and simply have no such block.
            if let Some(block) = self.gl.get_uniform_block_index(program, "RenderState") {
                self.gl.uniform_block_binding(program, block, RENDER_STATE_BINDING);
            }

            let constant_locations =
                [self.gl.get_uniform_location(program, "vs_c"), self.gl.get_uniform_location(program, "ps_c")];
            // Samplers are named after the register they came from, so the stage a game sets with
            // `texture_set_stage` is the number in the name -- and the `vs`/`ps` prefix is which
            // kind, since the two have separate registers.
            let mut sampler_locations = Vec::new();
            for kind in [ShaderStage::Vertex, ShaderStage::Pixel] {
                let prefix = match kind {
                    ShaderStage::Vertex => "vs",
                    ShaderStage::Pixel => "ps",
                };
                for stage in 0..SHADER_SAMPLER_STAGES as u32 {
                    let name = format!("{}_s{}", prefix, stage);
                    if let Some(location) = self.gl.get_uniform_location(program, &name) {
                        sampler_locations.push((kind, stage, location));
                    }
                }
            }
            Some(ShaderProgram { program, constant_locations, sampler_locations, attribute_bindings })
        }
    }

    /// Points the custom vertex array at this renderer's vertex buffer, for whichever attributes
    /// the program declared. Only redone when the program changes.
    fn bind_custom_attributes(&mut self, pair: (i32, i32), bindings: &[(u32, u8, u8)]) {
        if self.custom_vao_program == Some(pair) {
            return
        }
        let stride = size_of::<Vertex>() as i32;
        unsafe {
            // Everything a previous program enabled has to go: a leftover enabled array pointing at
            // a field this shader does not declare would feed whatever attribute now sits there.
            for location in 0..8 {
                self.gl.disable_vertex_attrib_array(location);
            }
            self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self._commands_vbo));
            for &(location, usage, index) in bindings {
                let Some(attribute) = Self::builtin_for_semantic(usage, index) else { continue };
                let (size, offset) = attribute.layout();
                self.gl.enable_vertex_attrib_array(location);
                self.gl.vertex_attrib_pointer_f32(location, size, glow::FLOAT, false, stride, offset);
            }
        }
        self.custom_vao_program = Some(pair);
    }

    /// Uploads whatever the custom path needs and leaves the program bound. Returns false if the
    /// pair could not be linked, in which case the caller falls back to the built-in program.
    fn use_custom_program(&mut self, pair: (i32, i32), draw_texture: Option<AtlasRect>) -> bool {
        let attribute_bindings = match self.shader_program(pair) {
            Some(program) => program.attribute_bindings.clone(),
            None => return false,
        };
        unsafe {
            self.gl.bind_vertex_array(Some(self.custom_vao));
        }
        self.bind_custom_attributes(pair, &attribute_bindings);
        self.bind_custom_program(pair, draw_texture)
    }

    /// Makes the program current and uploads its constants and samplers. Says nothing about where
    /// the vertices come from -- an immediate-mode draw and a game-supplied vertex buffer share all
    /// of this and differ only in the vertex array bound around it.
    fn bind_custom_program(&mut self, pair: (i32, i32), draw_texture: Option<AtlasRect>) -> bool {
        let Some(program) = self.shader_program(pair) else { return false };
        let (gl_program, constant_locations) = (program.program, program.constant_locations.clone());
        let sampler_locations = program.sampler_locations.clone();
        let switched = self.active_custom_program != Some(pair);
        unsafe {
            self.gl.use_program(Some(gl_program));

            for stage in 0..2 {
                if let Some(location) = &constant_locations[stage] {
                    if switched || self.shader_constants_dirty[stage] {
                        // One flat upload of the whole register file. Tracking which registers a
                        // game touched would save bandwidth that is not scarce here, and would have
                        // to be right about every path that writes one.
                        let flat: Vec<f32> = self.shader_constants[stage].iter().flatten().copied().collect();
                        self.gl.uniform_4_f32_slice(Some(location), &flat);
                    }
                }
            }
            self.shader_constants_dirty = [false, false];
        }
        self.active_custom_program = Some(pair);

        // **Stage 0 is the draw's own texture, always** -- a `texture_set_stage(0, ...)` does not
        // survive to the next draw and must not be consulted here.
        //
        // Measured: a probe on the real runner stages a texture on 0
        // and then draws something untextured, and the real runner samples *white*, not the staged
        // texture. GM sets stage 0 itself before every draw it issues, so whatever a game staged
        // there is overwritten immediately -- which makes `texture_set_stage(0, ...)` inert on real
        // hardware, and reading it here a pure invention.
        //
        // It was not a harmless one. A stage never being cleared meant that once any game called
        // `texture_set_stage(0, x)` even once, *every shaded draw for the rest of the run* sampled
        // `x` instead of its own texture. In the probe run that turned every subsequent shader pass
        // -- a 16x16 coordinate sprite, a 1x1 white pixel, an untextured quad -- into the 100x60
        // probe card, stretched to fit. A game that composes through shaders would render one
        // texture over everything and look exactly like a broken atlas.
        //
        // *Every* stage goes through `resolve_draw_texture`, not just that fallback. It is what
        // notices a texture is also the framebuffer being drawn into and hands back a snapshot
        // instead: WebGL2 calls that a feedback loop and drops the whole draw, where D3D9 allowed
        // it and read the contents from before the draw. A multi-pass compose does this constantly
        // -- `texture_set_stage` a surface, then draw into it -- and binding the texture directly
        // meant those draws silently did not happen at all.
        //
        // Resolved before any of it is bound, because taking a snapshot needs `&mut self` and
        // cannot happen half-way through a loop already holding the GL context.
        let mut resolved = Vec::with_capacity(sampler_locations.len());
        for (kind, stage, location) in sampler_locations {
            let index = kind as usize;
            let rect = if kind == ShaderStage::Pixel && stage == 0 {
                draw_texture
            } else {
                self.shader_samplers[index][(stage as usize).min(SHADER_SAMPLER_STAGES - 1)]
                    .and_then(|atlas_ref| self.get_rect(atlas_ref).copied())
            };
            // A sprite packed into a shared page becomes a texture of its own here, because that is
            // what the shader was written against -- see `sampler_view`.
            let texture = rect.and_then(|rect| self.sampler_view(rect));
            self.debug_note(match rect {
                Some(r) => {
                    let dims = self.atlas_dims.get(r.atlas_id as usize).copied().unwrap_or((-1, -1));
                    format!(
                        "sampler {:?}{} of shader {:?}: page {} rect {},{} {}x{} of {}x{} -> {} (repeat {}, lerp {})",
                        kind,
                        stage,
                        pair,
                        r.atlas_id,
                        r.x,
                        r.y,
                        r.w,
                        r.h,
                        dims.0,
                        dims.1,
                        match texture {
                            Some(_) if (r.x, r.y, r.w, r.h) == (0, 0, dims.0, dims.1) => "the page itself",
                            Some(_) => "a copy of that rect",
                            None => "NOTHING (samples black)",
                        },
                        bool::from(self.queue_render_state.texture_repeat),
                        bool::from(self.queue_render_state.interpolate_pixels),
                    )
                },
                None => format!("sampler {:?}{} of shader {:?}: nothing staged, samples black", kind, stage, pair),
            });
            let sampler = self.sampler_object(kind, stage);
            resolved.push((kind, stage, location, texture, sampler));
        }

        let filter = if self.queue_render_state.interpolate_pixels.into() { glow::LINEAR } else { glow::NEAREST };
        // `texture_set_repeat` is global render state, and the built-in fragment shader honours it by
        // hand because it samples out of a shared page. Here the sampler really is one texture, so
        // the wrap mode can do it -- and has to, or a tiled floor stops tiling under a shader.
        let wrap = if self.queue_render_state.texture_repeat.into() { glow::REPEAT } else { glow::CLAMP_TO_EDGE };
        unsafe {
            for (kind, stage, location, texture, sampler) in resolved {
                let unit = sampler_texture_unit(kind, stage);
                self.gl.active_texture(glow::TEXTURE0 + unit);
                self.gl.bind_texture(glow::TEXTURE_2D, texture);
                // A sampler object, where the game set per-stage state, overrides everything below
                // for this unit. Where it did not, unbind so the texture's own parameters apply --
                // leaving a previous draw's sampler bound would silently filter this one.
                self.gl.bind_sampler(unit, sampler);
                // `glTexParameter` acts on the texture bound to the target, so with nothing bound
                // it is an INVALID_OPERATION rather than a no-op -- and a shader draw with no
                // texture (a shadow-volume pass, an untextured quad) is perfectly ordinary. Left in,
                // it raised a GL error every frame that later surfaced against whatever unrelated
                // call happened to poll for one.
                if sampler.is_none() && texture.is_some() {
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, wrap as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, wrap as i32);
                }
                self.gl.uniform_1_i32(Some(&location), unit as i32);
            }
            // Back to unit 0, which is the only one the built-in path knows about.
            self.gl.active_texture(glow::TEXTURE0);
        }
        true
    }

    /// `glClear` with `DEPTH_BUFFER_BIT`, forcing the depth write mask on for the duration.
    ///
    /// **A depth clear is gated by `glDepthMask`.** GL treats it as a write like any other, so with
    /// the mask off it silently clears nothing -- no error, no hint. This engine turns depth writes
    /// off for 2D drawing, which is every frame's starting state, so every depth clear it issued was
    /// doing nothing at all: `draw_view`'s per-frame one and `d3d_clear_depth_buffer` alike.
    ///
    /// The symptom is confined to games that actually depth-test, which is why it survived this
    /// long. renex engine's teapot boss depth-tested against a buffer still holding *the previous
    /// frame's* depths and came out with pieces of itself missing, in shapes that moved as it
    /// turned. Restoring the mask afterwards is what keeps `write_depth`'s own tracking honest --
    /// `queue_render_state` is the state GL is actually in, not `next_render_state`.
    unsafe fn clear_including_depth(&mut self, mask: u32) {
        let restore = self.queue_render_state.write_depth;
        if !restore {
            self.gl.depth_mask(true);
        }
        self.gl.clear(mask);
        if !restore {
            self.gl.depth_mask(false);
        }
    }

    /// Logs `line` the first time it is seen, up to a cap, behind `localStorage.__emuGlDebug`.
    ///
    /// Which texture each shaded draw actually binds: the diagnostic that finds a stale stage 0 or
    /// a leaked sampler object. Deduplicated because a game issues the
    /// same draw sixty times a second and the interesting part is the *set* of distinct bindings.
    fn debug_note(&mut self, line: String) {
        if !gl_debug() {
            return
        }
        if self.debug_seen.len() < 150 && self.debug_seen.insert(line.clone()) {
            web_sys::console::warn_1(&format!("emu-diag: {}", line).into());
        }
    }

    /// The GL sampler object for a stage, created on first use and updated to match the state the
    /// game has set. `None` when it has set none, which leaves the texture's own parameters alone.
    fn sampler_object(&mut self, kind: ShaderStage, stage: u32) -> Option<glow::Sampler> {
        let (index, slot) = (kind as usize, (stage as usize).min(SHADER_SAMPLER_STAGES - 1));
        let state = self.shader_sampler_state[index][slot];
        if state.is_default() {
            return None
        }
        let sampler = match self.sampler_objects[index][slot] {
            Some(sampler) => sampler,
            None => {
                let sampler = unsafe { self.gl.create_sampler().ok()? };
                self.sampler_objects[index][slot] = Some(sampler);
                sampler
            },
        };
        // Re-applied rather than cached: the fallbacks come from render state the game can change
        // between draws without touching the stage at all.
        let interpolate = state.interpolate.unwrap_or_else(|| self.queue_render_state.interpolate_pixels.into());
        let filter = if interpolate { glow::LINEAR } else { glow::NEAREST } as i32;
        let (wrap_h, wrap_v) = state.repeat.unwrap_or_else(|| {
            let repeat = self.queue_render_state.texture_repeat.into();
            (repeat, repeat)
        });
        let wrap = |on: bool| if on { glow::REPEAT } else { glow::CLAMP_TO_EDGE } as i32;
        unsafe {
            self.gl.sampler_parameter_i32(sampler, glow::TEXTURE_MIN_FILTER, filter);
            self.gl.sampler_parameter_i32(sampler, glow::TEXTURE_MAG_FILTER, filter);
            self.gl.sampler_parameter_i32(sampler, glow::TEXTURE_WRAP_S, wrap(wrap_h));
            self.gl.sampler_parameter_i32(sampler, glow::TEXTURE_WRAP_T, wrap(wrap_v));
        }
        Some(sampler)
    }

    /// How a `vf_type_*` is read: component count, GL type, and whether it is normalised on the way
    /// in. `None` for the ones this has no mapping for, which are then simply not fed.
    fn vertex_element_format(kind: i32) -> Option<(i32, u32, bool)> {
        Some(match kind {
            0 => (1, glow::FLOAT, false), // float1
            1 => (2, glow::FLOAT, false), // float2
            2 => (3, glow::FLOAT, false), // float3
            3 => (4, glow::FLOAT, false), // float4
            // d3dcolor is BGRA in memory, and GL has no BGRA vertex format -- so `draw_vertex_streams`
            // swaps the bytes before upload and this reads them as four normalised bytes in order.
            4 => (4, glow::UNSIGNED_BYTE, true),   // d3dcolor
            5 => (4, glow::UNSIGNED_BYTE, false),  // ubyte4
            6 => (2, glow::SHORT, false),          // short2
            7 => (4, glow::SHORT, false),          // short4
            8 => (4, glow::UNSIGNED_BYTE, true),   // ubyte4n
            9 => (2, glow::SHORT, true),           // short2n
            10 => (4, glow::SHORT, true),          // short4n
            11 => (2, glow::UNSIGNED_SHORT, true), // ushort2n
            12 => (4, glow::UNSIGNED_SHORT, true), // ushort4n
            15 => (2, glow::HALF_FLOAT, false),    // float16_2
            16 => (4, glow::HALF_FLOAT, false),    // float16_4
            _ => return None,
        })
    }

    /// Puts back the built-in program and its vertex array, after a custom-shader draw.
    fn use_builtin_program(&mut self) {
        if self.active_custom_program.is_none() {
            return
        }
        self.active_custom_program = None;
        unsafe {
            self.gl.bind_vertex_array(Some(self._vao));
            self.gl.use_program(Some(self._program));
            // Unit 0 is shared: it is the only one this path binds a texture to, and it is also
            // where the custom path puts pixel sampler 0. A sampler object left bound there
            // overrides the texture's own filter and wrap for *every* draw that follows, and
            // nothing else ever unbinds it -- so a game that called
            // `texture_set_stage_interpolation` once turned interpolation off for the rest of the
            // run. Measured: a 2x sprite blit with `texture_set_interpolation(true)` came back
            // hard-edged here and smooth on the real runner.
            self.gl.bind_sampler(0, None);
        }
        // `bind_texture_2d`'s mirror is only meaningful for unit 0, which the custom path restores
        // before returning, so the tracked binding is still whatever a custom draw left there.
        self.bind_texture_2d(self.current_texture_2d.get());
    }

    /// `draw_texture` is the one texture the whole buffer uses, when the caller knows it.
    ///
    /// A baked model is the reason it exists. `d3d_model_draw` takes its texture *at draw time*, but
    /// GM8.1 caches the model's vertices on the first draw -- so every vertex carries the rect of
    /// whichever texture that first draw happened to use, and every later draw with a different one
    /// pairs a stale rect with a page it does not belong to. The diagnostics caught this as a 1x1
    /// white-pixel rect at (7241,4864) being read out of a 512x512 page: nowhere near it, so it read
    /// nothing at all. When the caller knows the rect, it is the truth and the per-vertex one is not.
    fn draw_buffer(
        &mut self,
        atlas_id: u32,
        shape: PrimitiveShape,
        buffer: &[Vertex],
        draw_texture: Option<DrawTexture>,
    ) {
        if buffer.is_empty() {
            return
        }
        // if something else broke check here just in case
        self.check_gl_error_hot("an earlier operation (reported on entry to draw_buffer)");

        // A game-supplied shader replaces the built-in program for this draw, and nothing else --
        // the queue, the vertex data, the render state and the framebuffer are all the same ones
        // the fixed-function path uses. When no shader is set, or the pair fails to link, this is
        // skipped entirely and the path below is what it always was.
        //
        // The queue batches by page, but a shader has to be told *which sprite on that page* it is
        // sampling (see `sampler_view`), and that is per vertex. So under a shader the batch is cut
        // wherever the sub-rect changes. Cuts land on primitive boundaries because every vertex of a
        // primitive comes from one builder with one rect, and the ordinary cases -- one sprite, or a
        // whole pass reading one surface -- are still a single draw.
        let pair = self.active_shader_pair();
        let mut start = 0;
        while start < buffer.len() {
            let xywh = buffer[start].atlas_xywh;
            let end = match (pair, draw_texture) {
                // One known texture for the whole buffer: nothing to split on.
                (_, Some(_)) | (None, _) => buffer.len(),
                (Some(_), None) => start + buffer[start..].iter().take_while(|v| v.atlas_xywh == xywh).count(),
            };
            let rect = draw_texture.map(|t| t.rect).unwrap_or(AtlasRect {
                atlas_id,
                x: xywh[0] as i32,
                y: xywh[1] as i32,
                w: xywh[2] as i32,
                h: xywh[3] as i32,
                origin_x: 0.0,
                origin_y: 0.0,
            });

            let custom = match pair {
                Some(pair) => self.use_custom_program(pair, Some(rect)),
                None => false,
            };
            if !custom {
                self.use_builtin_program();
                let texture = self.resolve_draw_texture(atlas_id);
                unsafe {
                    self.bind_texture_2d(texture);
                    let filter_mode =
                        if self.queue_render_state.interpolate_pixels.into() { glow::LINEAR } else { glow::NEAREST };
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter_mode as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter_mode as i32);
                    // The built-in program reads both of these per vertex, so a draw that knows them
                    // for the whole buffer has to replace the attributes -- disabled, an attribute
                    // takes the constant value set here. Re-enabled below, since the array state
                    // lives in the vertex array and every other draw still wants it.
                    if let Some(texture) = draw_texture {
                        self.gl.disable_vertex_attrib_array(4);
                        self.gl.vertex_attrib_4_f32(4, rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
                        if !texture.textured {
                            self.gl.disable_vertex_attrib_array(2);
                            self.gl.vertex_attrib_4_f32(2, f32::NAN, f32::NAN, 0.0, 0.0);
                        }
                    }
                }
            }

            let run = &buffer[start..end];
            unsafe {
                let bytes = std::slice::from_raw_parts(run.as_ptr().cast::<u8>(), size_of::<Vertex>() * run.len());
                self.gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
                self.check_gl_error_hot("draw_buffer's vertex upload");

                self.gl.draw_arrays(shape.into(), 0, run.len() as i32);

                // https://www.khronos.org/opengl/wiki/Buffer_Object_Streaming#Buffer_re-specification
                // https://www.khronos.org/opengl/wiki/Buffer_Object#Invalidation
                self.gl.buffer_data_size(
                    glow::ARRAY_BUFFER,
                    (size_of::<Vertex>() * run.len()) as i32,
                    glow::STREAM_DRAW,
                );
                if let (false, Some(texture)) = (custom, draw_texture) {
                    self.gl.enable_vertex_attrib_array(4);
                    if !texture.textured {
                        self.gl.enable_vertex_attrib_array(2);
                    }
                }
            }
            start = end;
        }
    }
}

impl RendererTrait for RendererImpl {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn max_texture_size(&self) -> u32 {
        let size = unsafe {
            let size = self.gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE);
            self.check_gl_error("max_texture_size");
            size
        };
        // Note: opengl.rs works around an Intel driver bug here (16384 reported but
        // GL_OUT_OF_MEMORY on allocation) by checking GL_VENDOR == "Intel". That's a native GL
        // driver quirk specific to desktop Intel OpenGL drivers, not something that applies to a
        // browser's WebGL2 implementation, so it's intentionally not replicated here.
        size.max(0) as u32
    }

    fn push_atlases(&mut self, mut atl: AtlasBuilder) -> Result<(), String> {
        assert!(self.atlas_packers.is_empty(), "atlases should be initialized only once");

        let white_pixel_ref =
            atl.texture(1, 1, 0, 0, Box::new([0xFF, 0xFF, 0xFF, 0xFF])).ok_or("Couldn't pack white_pixel")?;
        let (packers, mut sprites) = atl.into_inner();
        self.white_pixel = sprites[white_pixel_ref.0 as usize].0;

        // update primitive buffers with white pixel
        self.reset_primitive_2d(PrimitiveType::PointList, None);
        self.reset_primitive_3d(PrimitiveType::PointList, None);

        let mut textures: Vec<glow::Texture> = Vec::with_capacity(packers.len());
        let mut dims: Vec<(i32, i32)> = Vec::with_capacity(packers.len());
        unsafe {
            for (i, packer) in packers.iter().enumerate() {
                let (width, height) = packer.size();

                let tex_id =
                    self.gl.create_texture().map_err(|e| format!("Failed to allocate texture on GPU: {}", e))?;
                self.bind_texture_2d(Some(tex_id));
                self.current_atlas = i as u32;

                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::REPEAT as i32);
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::REPEAT as i32);
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,  // target
                    0,                 // level
                    glow::RGBA as i32, // internalformat
                    width,             // width
                    height,            // height
                    0,                 // border ("must be 0")
                    glow::RGBA,        // format (note: RGBA not BGRA, see module doc comment)
                    glow::UNSIGNED_BYTE,
                    None,
                );
                textures.push(tex_id);
                dims.push((width, height));
            }

            // verify it actually worked
            match self.gl.get_error() {
                0 => (),
                err => return Err(format!("Failed to allocate texture on GPU! (WebGL2 code {})", err)),
            }

            // upload textures
            for (atl_ref, pixels) in &sprites {
                if self.current_atlas != atl_ref.atlas_id {
                    self.bind_texture_2d(Some(textures[atl_ref.atlas_id as usize]));
                    self.current_atlas = atl_ref.atlas_id;
                }

                let rgba = bgra_to_rgba(pixels);
                self.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D, // target
                    0,                // level
                    atl_ref.x,        // xoffset
                    atl_ref.y,        // yoffset
                    atl_ref.w,        // width
                    atl_ref.h,        // height
                    glow::RGBA,       // format
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(&rgba),
                );
            }

            // verify it actually worked
            match self.gl.get_error() {
                0 => (),
                err => return Err(format!("Failed to upload textures to GPU! (WebGL2 code {})", err)),
            }

            // generate framebuffers
            let mut fbo_ids = Vec::with_capacity(textures.len());
            for tex_id in &textures {
                let fbo = self.gl.create_framebuffer().map_err(|e| format!("Failed to generate framebuffer: {}", e))?;
                self.bind_read_fbo(Some(fbo));
                self.gl.framebuffer_texture_2d(
                    glow::READ_FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(*tex_id),
                    0,
                );
                fbo_ids.push(Some(fbo));
            }
            self.bind_read_fbo(self.framebuffer.fbo);

            // verify it actually worked
            match self.gl.get_error() {
                0 => (),
                err => return Err(format!("Failed to generate framebuffers! (WebGL2 code {})", err)),
            }

            // store opengl texture handles
            self.texture_ids = textures.iter().map(|t| Some(*t)).collect();
            self.zbuf_ids.resize(self.texture_ids.len(), None);
            self.fbo_ids = fbo_ids;
            self.atlas_dims = dims;
            self.stock_atlas_count = textures.len() as u32 + 2; // TODO remove +2 when -o works
        }

        // store packers, discard pixeldata
        self.atlas_packers = packers;
        self.texture_rects = sprites.drain(..).map(|(ar, _)| Some(ar)).collect();
        self.stock_texture_count = self.texture_rects.len();

        Ok(())
    }

    fn upload_sprite(
        &mut self,
        data: Box<[u8]>,
        width: i32,
        height: i32,
        origin_x: i32,
        origin_y: i32,
    ) -> Result<AtlasRef, String> {
        let atlas_ref = self.create_surface(width, height, false)?;
        if let Some(rect) = self.get_rect_mut(atlas_ref) {
            rect.origin_x = origin_x as f32 / width as f32;
            rect.origin_y = origin_y as f32 / height as f32;
            let rect = *self.get_rect(atlas_ref).unwrap();
            let prev_tex2d = self.current_texture_2d.get();
            unsafe {
                self.bind_texture_2d(self.texture_ids[rect.atlas_id as usize]);
                self.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D, // target
                    0,                // level
                    rect.x,           // xoffset
                    rect.y,           // yoffset
                    rect.w,           // width
                    rect.h,           // height
                    glow::RGBA,       // format
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(&data),
                );

                // verify it actually worked
                match self.gl.get_error() {
                    0 => (),
                    err => return Err(format!("Failed to upload texture to GPU! (WebGL2 code {})", err)),
                }
            }
            self.bind_texture_2d(prev_tex2d);
        }
        Ok(atlas_ref)
    }

    fn duplicate_sprite(&mut self, atlas_ref: AtlasRef) -> Result<AtlasRef, String> {
        if let Some(rect) = self.get_rect(atlas_ref).cloned() {
            let sprite = self.create_surface(rect.w, rect.h, false)?;
            let new_rect = self.get_rect_mut(sprite).unwrap();
            new_rect.origin_x = rect.origin_x;
            new_rect.origin_y = rect.origin_y;
            let new_rect = *self.get_rect(sprite).unwrap();

            let prev_read_fbo = self.current_read_fbo.get();
            let prev_tex2d = self.current_texture_2d.get();
            unsafe {
                self.bind_read_fbo(self.fbo_ids[rect.atlas_id as usize]);
                self.bind_texture_2d(self.texture_ids[new_rect.atlas_id as usize]);

                self.gl.copy_tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA, rect.x, rect.y, rect.w, rect.h, 0);

                self.bind_read_fbo(prev_read_fbo);
                self.bind_texture_2d(prev_tex2d);

                // verify it actually worked
                match self.gl.get_error() {
                    0 => (),
                    err => return Err(format!("Failed to duplicate texture! (WebGL2 code {})", err)),
                }
            }
            Ok(sprite)
        } else {
            Ok(AtlasRef(-1))
        }
    }

    fn set_sprite_origin(&mut self, atlas_ref: AtlasRef, origin_x: i32, origin_y: i32) {
        if let Some(rect) = self.get_rect_mut(atlas_ref) {
            // Same normalization `upload_sprite` applies. A zero-sized rect has no meaningful
            // origin and would divide by zero, so leave it alone.
            if rect.w != 0 && rect.h != 0 {
                rect.origin_x = origin_x as f32 / rect.w as f32;
                rect.origin_y = origin_y as f32 / rect.h as f32;
            }
        }
    }

    fn delete_sprite(&mut self, atlas_ref: AtlasRef) {
        // this only deletes sprites created with upload_sprite
        self.flush_queue();
        if let Some(rect) = atlas_ref
            .0
            .try_into()
            .ok()
            .and_then(|id: usize| self.texture_rects.get_mut(id))
            .and_then(|o: &mut Option<AtlasRect>| o.take())
        {
            // A shader may have been handed a copy of this sprite; the rect is about to stop meaning
            // what that copy holds, so it must not be found again.
            if let Some(view) = self.sampler_views.remove(&(rect.atlas_id, rect.x, rect.y, rect.w, rect.h)) {
                unsafe { self.gl.delete_texture(view) };
            }
            if rect.atlas_id >= self.stock_atlas_count {
                unsafe {
                    if let Some(tex_id) = self.texture_ids[rect.atlas_id as usize].take() {
                        self.gl.delete_texture(tex_id);
                    }
                    if let Some(zbuf) = self.zbuf_ids.get_mut(rect.atlas_id as usize).and_then(Option::take) {
                        self.gl.delete_texture(zbuf);
                    }
                    if let Some(fbo) = self.fbo_ids.get_mut(rect.atlas_id as usize).and_then(Option::take) {
                        self.gl.delete_framebuffer(fbo);
                    }
                    self.check_gl_error("delete_sprite");
                }
            }
        }
    }

    fn set_vsync(&self, vsync: bool) {
        self.vsync.set(vsync);
    }

    fn get_vsync(&self) -> bool {
        self.vsync.get()
    }

    fn wait_vsync(&self) {
        // No vsync concept in WebGL2 -- the browser paces frames via requestAnimationFrame
        // (the host's concern), not this renderer.
    }

    fn create_sprite_colour(&mut self, width: i32, height: i32, col: Colour) -> Result<AtlasRef, String> {
        let atlas_ref = self.create_surface(width, height, false)?;
        if let Some(rect) = self.get_rect(atlas_ref) {
            let fbo = self.fbo_ids[rect.atlas_id as usize];
            let prev_read_fbo = self.current_read_fbo.get();
            self.bind_read_fbo(fbo);
            unsafe {
                self.gl.clear_color(col.r as f32, col.g as f32, col.b as f32, 1.0);
                self.gl.clear(glow::COLOR_BUFFER_BIT);
            }
            self.bind_read_fbo(prev_read_fbo);
        }
        Ok(atlas_ref)
    }

    fn create_surface(&mut self, width: i32, height: i32, has_zbuffer: bool) -> Result<AtlasRef, String> {
        let atlas_id = if let Some(id) = self.texture_ids.iter().position(|x| x.is_none()) {
            id as u32
        } else {
            self.texture_ids.push(None);
            self.zbuf_ids.push(None);
            self.fbo_ids.push(None);
            self.atlas_dims.push((0, 0));
            self.texture_ids.len() as u32 - 1
        };

        let prev_tex2d = self.current_texture_2d.get();
        let prev_fbo = self.current_read_fbo.get();
        unsafe {
            // generate new
            let tex_id = self.gl.create_texture().map_err(|e| format!("Failed to allocate texture on GPU: {}", e))?;
            self.bind_texture_2d(Some(tex_id));

            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::REPEAT as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::REPEAT as i32);

            self.gl.tex_image_2d(
                glow::TEXTURE_2D,  // target
                0,                 // level
                glow::RGBA as i32, // internalformat
                width,             // width
                height,            // height
                0,                 // border ("must be 0")
                glow::RGBA,        // format
                glow::UNSIGNED_BYTE,
                None,
            );

            // verify it actually worked
            match self.gl.get_error() {
                0 => (),
                err => return Err(format!("Failed to allocate texture on GPU! (WebGL2 code {})", err)),
            }

            // generate fbo
            let fbo = self.gl.create_framebuffer().map_err(|e| format!("Failed to generate framebuffer: {}", e))?;
            self.bind_read_fbo(Some(fbo));
            self.gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex_id),
                0,
            );
            match self.gl.get_error() {
                0 => (),
                err => return Err(format!("Failed to generate framebuffer! (WebGL2 code {})", err)),
            }

            // generate zbuffer if applicable
            if has_zbuffer {
                let zbuf_id =
                    self.gl.create_texture().map_err(|e| format!("Failed to allocate depth texture on GPU: {}", e))?;
                self.bind_texture_2d(Some(zbuf_id));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,          // target
                    0,                         // level
                    self.zbuf_internal_format, // internalformat
                    width,                     // width
                    height,                    // height
                    0,                         // border ("must be 0")
                    glow::DEPTH_COMPONENT,     // format
                    self.zbuf_type,
                    None,
                );
                self.gl.framebuffer_texture_2d(
                    glow::READ_FRAMEBUFFER,
                    glow::DEPTH_ATTACHMENT,
                    glow::TEXTURE_2D,
                    Some(zbuf_id),
                    0,
                );
                match self.gl.get_error() {
                    0 => (),
                    err => return Err(format!("Failed to generate depth buffer! (WebGL2 code {})", err)),
                }
                // store handle
                self.zbuf_ids[atlas_id as usize] = Some(zbuf_id);
            }

            // store opengl texture handles
            self.texture_ids[atlas_id as usize] = Some(tex_id);
            self.fbo_ids[atlas_id as usize] = Some(fbo);
            self.atlas_dims[atlas_id as usize] = (width, height);

            // cleanup
            self.bind_texture_2d(prev_tex2d);
            self.bind_read_fbo(prev_fbo);
            self.check_gl_error("create_surface");
        }
        let id = self.texture_rects.len() as i32;
        self.texture_rects.push(Some(AtlasRect {
            atlas_id,
            x: 0,
            y: 0,
            w: width,
            h: height,
            origin_x: 0.0,
            origin_y: 0.0,
        }));
        Ok(AtlasRef(id))
    }

    fn set_target(&mut self, atlas_ref: AtlasRef) {
        self.flush_queue();
        let target = self
            .get_rect(atlas_ref)
            .and_then(|rect| self.fbo_ids.get(rect.atlas_id as usize).copied().flatten().map(|fbo| (*rect, fbo)));
        if let Some((AtlasRect { x, y, w, h, .. }, fbo_id)) = target {
            self.bind_draw_fbo(Some(fbo_id));
            // set viewport here since set_view doesn't
            self.set_gl_viewport(x, y, w, h);
            self.set_view(x, y, w, h, 0.0, x, y, w, h);
        }
    }

    fn reset_target(&mut self) {
        self.flush_queue();
        self.bind_draw_fbo(self.framebuffer.fbo);

        // reset view
        let (fb_width, fb_height) = (self.framebuffer.width, self.framebuffer.height);
        self.set_view(0, 0, fb_width, fb_height, 0.0, 0, 0, fb_width, fb_height);
    }

    fn copy_surface(
        &mut self,
        dest: AtlasRef,
        mut dest_x: i32,
        mut dest_y: i32,
        src: AtlasRef,
        mut src_x: i32,
        mut src_y: i32,
        mut width: i32,
        mut height: i32,
    ) {
        let (src_rect, dest_rect) = match (self.get_rect(src), self.get_rect(dest)) {
            (Some(src), Some(dest)) => (src, dest),
            _ => return,
        };
        // correct coordinates
        if src_x < 0 {
            dest_x -= src_x;
            width += src_x;
            src_x = 0;
        }
        if src_y < 0 {
            dest_y -= src_y;
            height += src_y;
            src_y = 0;
        }
        if src_x + width > src_rect.w {
            width = src_rect.w - src_x;
        }
        if src_y + height > src_rect.h {
            height = dest_rect.h - src_y;
        }
        if dest_x < 0 {
            src_x -= dest_x;
            width += dest_x;
            dest_x = 0;
        }
        if dest_y < 0 {
            src_y -= dest_y;
            height += dest_y;
            dest_y = 0;
        }
        if dest_x + width > dest_rect.w {
            width = dest_rect.w - dest_x;
        }
        if dest_y + height > dest_rect.h {
            height = dest_rect.h - dest_y;
        }
        if width > 0 && height > 0 {
            // actual copy
            if let (Some(Some(src_id)), Some(Some(dst_id))) =
                (self.fbo_ids.get(src_rect.atlas_id as usize), self.fbo_ids.get(dest_rect.atlas_id as usize))
            {
                let (mut src_id, dst_id) = (*src_id, *dst_id);
                unsafe {
                    // On Intel, glBlitFrameBuffer just does nothing if the scissor box is too big, which it
                    // very well could be. So just disable the scissor test for now.
                    self.gl.disable(glow::SCISSOR_TEST);
                }

                // NEW vs. opengl.rs: blitting a framebuffer onto *itself* with overlapping
                // rectangles is undefined in desktop GL and rejected outright by WebGL2, so a game
                // scrolling a surface with `surface_copy(s, x, y, s)` would silently corrupt or
                // lose the copy. Read out of a snapshot instead, which is well-defined and is what
                // D3D8 gave GM8. Non-overlapping copies are legal, so leave those on the fast path.
                let overlaps = src_x < dest_x + width
                    && dest_x < src_x + width
                    && src_y < dest_y + height
                    && dest_y < src_y + height;
                if src_id == dst_id && overlaps {
                    let (w, h) = self.atlas_dims[src_rect.atlas_id as usize];
                    match self.snapshot_fbo(Some(src_id), w, h) {
                        // The snapshot is a whole-atlas copy, so the source rect keeps its
                        // coordinates -- only the framebuffer we read out of changes.
                        Some((_, copy_fbo)) => src_id = copy_fbo,
                        // Couldn't get the scratch. Skipping the copy leaves the surface as it was,
                        // which beats issuing a blit whose result we already know is garbage.
                        None => {
                            unsafe { self.gl.enable(glow::SCISSOR_TEST) };
                            return
                        },
                    }
                }

                // Remember old framebuffer so we can rebind it after we're done
                let fb_old = self.current_draw_fbo.get();

                self.bind_draw_fbo(Some(dst_id));
                self.bind_read_fbo(Some(src_id));
                unsafe {
                    self.gl.blit_framebuffer(
                        src_x,
                        src_y,
                        src_x + width,
                        src_y + height,
                        dest_x,
                        dest_y,
                        dest_x + width,
                        dest_y + height,
                        glow::COLOR_BUFFER_BIT,
                        glow::NEAREST,
                    );
                }
                self.bind_draw_fbo(fb_old);

                unsafe {
                    self.gl.enable(glow::SCISSOR_TEST);
                    self.check_gl_error("copy_surface");
                }
            }
        }
    }

    fn set_zbuf_trashed(&mut self, trashed: bool) {
        if trashed != self.zbuf_trashed {
            self.flush_queue();
            self.zbuf_trashed = trashed;
            let prev_fbo = self.current_read_fbo.get();
            self.bind_read_fbo(self.framebuffer.fbo);
            unsafe {
                self.check_gl_error("set_zbuf_trashed");
                self.gl.framebuffer_texture_2d(
                    glow::READ_FRAMEBUFFER,
                    glow::DEPTH_ATTACHMENT,
                    glow::TEXTURE_2D,
                    if trashed { None } else { self.framebuffer.zbuf },
                    0,
                );
                self.check_gl_error("set_zbuf_trashed");
            }
            self.bind_read_fbo(prev_fbo);
        }
    }

    fn get_zbuf_trashed(&self) -> bool {
        self.zbuf_trashed
    }

    fn resize_framebuffer(&mut self, width: u32, height: u32, store: bool) {
        self.flush_queue();
        let (width, height) = (width as i32, height as i32);
        let old = self.framebuffer;
        unsafe {
            // set up new texture
            let new_tex = self.gl.create_texture().expect("Failed to allocate texture on GPU");
            self.bind_texture_2d(Some(new_tex));
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                width,
                height,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                None,
            );
            self.check_gl_error("resize_framebuffer");

            // set up new zbuffer
            let new_zbuf = self.gl.create_texture().expect("Failed to allocate depth texture on GPU");
            self.bind_texture_2d(Some(new_zbuf));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                self.zbuf_internal_format,
                width,
                height,
                0,
                glow::DEPTH_COMPONENT,
                self.zbuf_type,
                None,
            );
            self.check_gl_error("resize_framebuffer");

            // set up new fbo
            let new_fbo = self.gl.create_framebuffer().expect("Failed to generate framebuffer");
            self.bind_fbo(Some(new_fbo));
            self.gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(new_tex),
                0,
            );
            self.gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::DEPTH_ATTACHMENT,
                glow::TEXTURE_2D,
                Some(new_zbuf),
                0,
            );
            self.check_gl_error("resize_framebuffer");

            // draw old fb onto new
            self.bind_read_fbo(old.fbo);
            let copy_width = width.min(old.width);
            let copy_height = height.min(old.height);
            self.gl.blit_framebuffer(
                0,
                0,
                copy_width,
                copy_height,
                0,
                0,
                copy_width,
                copy_height,
                glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT,
                glow::NEAREST,
            );
            self.check_gl_error("resize_framebuffer");

            self.framebuffer =
                Framebuffer { texture: Some(new_tex), zbuf: Some(new_zbuf), fbo: Some(new_fbo), width, height };

            if store {
                // store old texture and fbo, delete previous stored one
                if let Some(framebuffer) = self.stored_framebuffer {
                    if let Some(t) = framebuffer.texture {
                        self.gl.delete_texture(t);
                    }
                    if let Some(z) = framebuffer.zbuf {
                        self.gl.delete_texture(z);
                    }
                    if let Some(f) = framebuffer.fbo {
                        self.gl.delete_framebuffer(f);
                    }
                }
                self.stored_framebuffer = Some(old);
            } else {
                // delete old texture and fbo
                if let Some(t) = old.texture {
                    self.gl.delete_texture(t);
                }
                if let Some(z) = old.zbuf {
                    self.gl.delete_texture(z);
                }
                if let Some(f) = old.fbo {
                    self.gl.delete_framebuffer(f);
                }
            }
            self.check_gl_error("resize_framebuffer");
        }
    }

    fn get_texture_id(&mut self, atl_ref: AtlasRef) -> i32 {
        atl_ref.0
    }

    fn get_texture_from_id(&self, id: i32) -> Option<AtlasRef> {
        Some(AtlasRef(id))
    }

    fn get_texture_rects(&self) -> Vec<Option<AtlasRect>> {
        self.texture_rects[self.stock_texture_count..].to_vec()
    }

    fn set_texture_rects(&mut self, rects: &[Option<AtlasRect>]) {
        self.texture_rects.truncate(self.stock_texture_count);
        self.texture_rects.extend_from_slice(rects);
    }

    fn dump_sprite_part(&self, atlas_ref: AtlasRef, part_x: i32, part_y: i32, part_w: i32, part_h: i32) -> Box<[u8]> {
        let rect = match self.get_rect(atlas_ref) {
            Some(rect) => AtlasRect { x: rect.x + part_x, y: rect.y + part_y, w: part_w, h: part_h, ..*rect },
            None => return Box::new([]),
        };

        let prev_read_fbo = self.current_read_fbo.get();
        let fbo = self.fbo_ids[rect.atlas_id as usize].expect("Trying to dump nonexistent sprite");
        self.bind_read_fbo(Some(fbo));

        let len = (rect.w * rect.h * 4).max(0) as usize;
        let mut data: Vec<u8> = vec![0u8; len];
        unsafe {
            self.gl.read_pixels(
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(&mut data),
            );
            self.check_gl_error("dump_sprite_part");
        }

        self.bind_read_fbo(prev_read_fbo);
        data.into_boxed_slice()
    }

    fn get_pixels(&self, x: i32, y: i32, w: i32, h: i32) -> Box<[u8]> {
        self.bind_read_fbo(self.framebuffer.fbo);
        let len = (w * h * 4).max(0) as usize;
        let mut data: Vec<u8> = vec![0u8; len];
        unsafe {
            self.gl.read_pixels(x, y, w, h, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(&mut data));
            self.check_gl_error("get_pixels");
        }
        data.into_boxed_slice()
    }

    fn stored_pixels(&self) -> Box<[u8]> {
        let fb = self.stored_framebuffer.unwrap_or(self.framebuffer);
        let (width, height) = self.stored_size();
        self.bind_read_fbo(fb.fbo);
        let len = (width * height * 4) as usize;
        let mut data: Vec<u8> = vec![0u8; len];
        unsafe {
            self.gl.read_pixels(
                0,
                0,
                width as i32,
                height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(&mut data),
            );
            self.check_gl_error("stored_pixels");
        }
        data.into_boxed_slice()
    }

    fn stored_zbuffer(&self) -> Box<[f32]> {
        // WebGL2/GLES has no `glGetTexImage` equivalent -- there is no way to read an arbitrary
        // texture's texels back directly at all (only framebuffer *colour* attachments, via
        // read_pixels). This method only serves savestates, which are pruned entirely on wasm
        // so it can never actually be called in practice; return a
        // correctly-sized, best-effort all-zero buffer rather than attempting an unsupported
        // readback or panicking.
        let fb = self.stored_framebuffer.unwrap_or(self.framebuffer);
        vec![0.0f32; (fb.width * fb.height).max(0) as usize].into_boxed_slice()
    }

    fn set_stored(&mut self, rgba: Box<[u8]>, zbuf: Box<[f32]>, fb_w: u32, fb_h: u32) {
        // Depth data can't be meaningfully restored either (see `stored_zbuffer`); this whole path
        // is only reachable from savestate loading, which is pruned on wasm.
        let _ = zbuf;
        let (fb_w, fb_h) = (fb_w as i32, fb_h as i32);
        unsafe {
            let texture = self.gl.create_texture().ok();
            let zbuffer = self.gl.create_texture().ok();
            let fbo = self.gl.create_framebuffer().ok();

            if let Some(texture) = texture {
                self.bind_texture_2d(Some(texture));
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA as i32,
                    fb_w,
                    fb_h,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    Some(&rgba),
                );
            }

            if let Some(zbuffer) = zbuffer {
                self.bind_texture_2d(Some(zbuffer));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    self.zbuf_internal_format,
                    fb_w,
                    fb_h,
                    0,
                    glow::DEPTH_COMPONENT,
                    self.zbuf_type,
                    None,
                );
            }

            if let Some(fbo) = fbo {
                self.bind_fbo(Some(fbo));
                self.gl.framebuffer_texture_2d(
                    glow::READ_FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    texture,
                    0,
                );
                self.gl.framebuffer_texture_2d(
                    glow::READ_FRAMEBUFFER,
                    glow::DEPTH_ATTACHMENT,
                    glow::TEXTURE_2D,
                    zbuffer,
                    0,
                );
            }

            if let Some(framebuffer) = self.stored_framebuffer {
                if let Some(t) = framebuffer.texture {
                    self.gl.delete_texture(t);
                }
                if let Some(z) = framebuffer.zbuf {
                    self.gl.delete_texture(z);
                }
                if let Some(f) = framebuffer.fbo {
                    self.gl.delete_framebuffer(f);
                }
            }

            self.stored_framebuffer = Some(Framebuffer { fbo, texture, zbuf: zbuffer, width: fb_w, height: fb_h });

            self.check_gl_error("set_stored");
        }
    }

    fn dump_dynamic_textures(&self) -> Vec<Option<SavedTexture>> {
        // Colour data is read back for real (via a throwaway FBO + read_pixels -- WebGL2/GLES has
        // no direct texel readback). Depth data is NOT included (`zbuf: None`): there is no way at
        // all to read an arbitrary depth texture's texels back under WebGL2. This whole method
        // only serves savestates, which are pruned entirely on wasm, so it
        // should never actually run, but it's implemented for real here rather than stubbed,
        // since the colour half is genuinely possible and cheap.
        let mut textures = Vec::with_capacity(self.texture_ids.len().saturating_sub(self.stock_atlas_count as usize));
        for (i, tex_id) in self.texture_ids.iter().copied().enumerate().skip(self.stock_atlas_count as usize) {
            textures.push(match tex_id {
                Some(tex_id) => {
                    let (width, height) = self.atlas_dims[i];
                    let prev_read_fbo = self.current_read_fbo.get();
                    let mut pixels = vec![0u8; (width * height * 4).max(0) as usize];
                    unsafe {
                        if let Ok(fbo) = self.gl.create_framebuffer() {
                            self.bind_read_fbo(Some(fbo));
                            self.gl.framebuffer_texture_2d(
                                glow::READ_FRAMEBUFFER,
                                glow::COLOR_ATTACHMENT0,
                                glow::TEXTURE_2D,
                                Some(tex_id),
                                0,
                            );
                            self.gl.read_pixels(
                                0,
                                0,
                                width,
                                height,
                                glow::RGBA,
                                glow::UNSIGNED_BYTE,
                                glow::PixelPackData::Slice(&mut pixels),
                            );
                            self.gl.delete_framebuffer(fbo);
                        }
                    }
                    self.bind_read_fbo(prev_read_fbo);
                    Some(SavedTexture { width, height, pixels: pixels.into_boxed_slice(), zbuf: None })
                },
                None => None,
            });
        }
        textures
    }

    fn upload_dynamic_textures(&mut self, textures: &[Option<SavedTexture>]) {
        unsafe {
            for tex_id in self.texture_ids.iter_mut().skip(self.stock_atlas_count as usize) {
                if let Some(tex_id) = tex_id.take() {
                    self.gl.delete_texture(tex_id);
                }
            }
            self.texture_ids.resize(self.stock_atlas_count as usize + textures.len(), None);
            for fbo_id in self.fbo_ids.iter_mut().skip(self.stock_atlas_count as usize) {
                if let Some(fbo_id) = fbo_id.take() {
                    self.gl.delete_framebuffer(fbo_id);
                }
            }
            self.fbo_ids.resize(self.stock_atlas_count as usize + textures.len(), None);
            for zbuf_id in self.zbuf_ids.iter_mut().skip(self.stock_atlas_count as usize) {
                if let Some(zbuf_id) = zbuf_id.take() {
                    self.gl.delete_texture(zbuf_id);
                }
            }
            self.zbuf_ids.resize(self.stock_atlas_count as usize + textures.len(), None);
            self.atlas_dims.resize(self.stock_atlas_count as usize + textures.len(), (0, 0));

            for (i, tex) in textures.iter().enumerate() {
                let i = i + self.stock_atlas_count as usize;
                if let Some(tex) = tex.as_ref() {
                    if let Ok(tex_id) = self.gl.create_texture() {
                        self.bind_texture_2d(Some(tex_id));

                        self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
                        self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
                        self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::REPEAT as i32);
                        self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::REPEAT as i32);

                        self.gl.tex_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            glow::RGBA as i32,
                            tex.width,
                            tex.height,
                            0,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            Some(&tex.pixels),
                        );
                        self.texture_ids[i] = Some(tex_id);
                        self.atlas_dims[i] = (tex.width, tex.height);

                        if let Ok(fbo_id) = self.gl.create_framebuffer() {
                            self.bind_read_fbo(Some(fbo_id));
                            self.gl.framebuffer_texture_2d(
                                glow::READ_FRAMEBUFFER,
                                glow::COLOR_ATTACHMENT0,
                                glow::TEXTURE_2D,
                                Some(tex_id),
                                0,
                            );
                            self.fbo_ids[i] = Some(fbo_id);
                        }

                        // Depth data isn't preserved by dump_dynamic_textures (see there), so
                        // there's nothing meaningful to re-upload even if `tex.zbuf` is `Some`.
                    }
                }
            }
            self.bind_read_fbo(self.framebuffer.fbo);
            self.check_gl_error("upload_dynamic_textures");
        }
    }

    fn get_rect(&self, id: AtlasRef) -> Option<&AtlasRect> {
        id.0.try_into()
            .ok()
            .and_then(|id: usize| self.texture_rects.get(id))
            .and_then(|o: &Option<AtlasRect>| o.as_ref())
    }

    fn draw_sprite_general(
        &mut self,
        texture: AtlasRef,
        part_x: f64,
        part_y: f64,
        part_w: f64,
        part_h: f64,
        x: f64,
        y: f64,
        xscale: f64,
        yscale: f64,
        angle: f64,
        col1: i32,
        col2: i32,
        col3: i32,
        col4: i32,
        alpha: f64,
        use_origin: bool,
    ) {
        let atlas_ref = match self.get_rect(texture) {
            Some(rect) => *rect,
            None => return,
        };

        self.set_texture_repeat(false);

        // get angle
        let angle = -angle.to_radians();
        let angle_sin = angle.sin();
        let angle_cos = angle.cos();

        // get real width of drawn sprite
        let width: f64 = xscale * f64::from(part_w);
        let height: f64 = yscale * f64::from(part_h);
        // calculate pre-rotation corner offsets from sprite origin
        // incl. subtraction 0.5 from left and top (GM does this in an attempt to combat the DX half-pixel offset)
        let (left, top): (f64, f64) = if use_origin {
            (-width * f64::from(atlas_ref.origin_x) - 0.5, -height * f64::from(atlas_ref.origin_y) - 0.5)
        } else {
            (-0.5, -0.5)
        };
        let right: f64 = left + width;
        let bottom: f64 = top + height;

        // get texture corners
        let tex_left = f64::from(part_x) / f64::from(atlas_ref.w);
        let tex_top = f64::from(part_y) / f64::from(atlas_ref.h);
        let tex_right = tex_left + f64::from(part_w) / f64::from(atlas_ref.w);
        let tex_bottom = tex_top + f64::from(part_h) / f64::from(atlas_ref.h);

        let (tex_left, tex_top, tex_right, tex_bottom) =
            (tex_left as f32, tex_top as f32, tex_right as f32, tex_bottom as f32);

        let normal = [0.0, 0.0, 0.0];
        let depth = self.depth;

        // rotate around draw origin
        let rotate = |xoff, yoff| {
            [(x + xoff * angle_cos - yoff * angle_sin) as f32, (y + yoff * angle_cos + xoff * angle_sin) as f32, depth]
        };

        // push the vertices
        self.push_primitive(
            PrimitiveBuilder::new(atlas_ref, PrimitiveType::TriFan, true)
                .push_vertex(rotate(left, top), [tex_left, tex_top], split_colour(col1, alpha), normal)
                .push_vertex(rotate(right, top), [tex_right, tex_top], split_colour(col2, alpha), normal)
                .push_vertex(rotate(right, bottom), [tex_right, tex_bottom], split_colour(col3, alpha), normal)
                .push_vertex(rotate(left, bottom), [tex_left, tex_bottom], split_colour(col4, alpha), normal),
        );
    }

    fn draw_sprite_pos(
        &mut self,
        texture: AtlasRef,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
        x4: f64,
        y4: f64,
        alpha: f64,
    ) {
        let atlas_ref = match self.get_rect(texture) {
            Some(rect) => *rect,
            None => return,
        };

        self.set_texture_repeat(false);

        // get texture corners
        let tex_left = 0.0;
        let tex_top = 0.0;
        let tex_right = tex_left + 1.0;
        let tex_bottom = tex_top + 1.0;

        let (tex_left, tex_top, tex_right, tex_bottom) =
            (tex_left as f32, tex_top as f32, tex_right as f32, tex_bottom as f32);

        let normal = [0.0, 0.0, 0.0];
        let depth = self.depth;

        // correct for gm offset
        let correct = |xoff, yoff| [(xoff - 0.5) as f32, (yoff - 0.5) as f32, depth];

        // push the vertices
        self.push_primitive(
            PrimitiveBuilder::new(atlas_ref, PrimitiveType::TriFan, true)
                .push_vertex(correct(x1, y1), [tex_left, tex_top], split_colour(0xffffff, alpha), normal)
                .push_vertex(correct(x2, y2), [tex_right, tex_top], split_colour(0xffffff, alpha), normal)
                .push_vertex(correct(x3, y3), [tex_right, tex_bottom], split_colour(0xffffff, alpha), normal)
                .push_vertex(correct(x4, y4), [tex_left, tex_bottom], split_colour(0xffffff, alpha), normal),
        );
    }

    fn draw_rectangle(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, colour: i32, alpha: f64) {
        let (x1, x2) = if x2 < x1 { (x2, x1) } else { (x1, x2) };
        let (y1, y2) = if y2 < y1 { (y2, y1) } else { (y1, y2) };
        let x2 = if x2 == x2.floor() { x2 + 0.01 } else { x2 };
        let y2 = if y2 == y2.floor() { y2 + 0.01 } else { y2 };
        self.push_primitive(
            ShapeBuilder::new(false, self.white_pixel, alpha, self.depth)
                .push_point(x1, y1, colour)
                .push_point(x2, y1, colour)
                .push_point(x2, y2, colour)
                .push_point(x1, y2, colour)
                .build(),
        );
    }

    fn draw_rectangle_outline(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, colour: i32, alpha: f64) {
        let (x1, x2) = if x2 < x1 { (x2, x1) } else { (x1, x2) };
        let (y1, y2) = if y2 < y1 { (y2, y1) } else { (y1, y2) };
        let x2 = if x2 == x2.floor() { x2 + 0.01 } else { x2 };
        let y2 = if y2 == y2.floor() { y2 + 0.01 } else { y2 };
        self.push_primitive(
            ShapeBuilder::new(true, self.white_pixel, alpha, self.depth)
                .push_point(x1, y1, colour)
                .push_point(x2, y1, colour)
                .push_point(x2, y2, colour)
                .push_point(x1, y2, colour)
                .build(),
        );
    }

    fn draw_rectangle_gradient(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        c1: i32,
        c2: i32,
        c3: i32,
        c4: i32,
        alpha: f64,
        outline: bool,
    ) {
        let (x1, x2) = if x2 < x1 { (x2, x1) } else { (x1, x2) };
        let (y1, y2) = if y2 < y1 { (y2, y1) } else { (y1, y2) };
        let x2 = if x2 == x2.floor() { x2 + 0.01 } else { x2 };
        let y2 = if y2 == y2.floor() { y2 + 0.01 } else { y2 };
        self.push_primitive(
            ShapeBuilder::new(outline, self.white_pixel, alpha, self.depth)
                .push_point(x1, y1, c1)
                .push_point(x2, y1, c2)
                .push_point(x2, y2, c3)
                .push_point(x1, y2, c4)
                .build(),
        );
    }

    fn draw_point(&mut self, x: f64, y: f64, colour: i32, alpha: f64) {
        self.setup_queue(self.white_pixel.atlas_id, PrimitiveShape::Point);
        self.vertex_queue.push(Vertex {
            pos: [x as f32, y as f32, self.depth],
            tex_coord: [f32::NAN; 2],
            blend: split_colour(colour, alpha),
            atlas_xywh: self.white_pixel.into(),
            normal: [0.0, 0.0, 0.0],
        });
    }

    fn draw_line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, width: Option<f64>, c1: i32, c2: i32, alpha: f64) {
        if let Some(width) = width {
            let length = (x2 - x1).hypot(y2 - y1);
            // on the off chance that they're in different points but the length is still somehow 0, check length
            if length != 0.0 {
                // calculate corners
                let width_x = (y2 - y1) * (width / 2.0) / length;
                let width_y = (x2 - x1) * (width / 2.0) / length;
                // actually push the rectangle
                self.push_primitive(
                    ShapeBuilder::new(false, self.white_pixel, alpha, self.depth)
                        .push_point(x1 - width_x, y1 + width_y, c1)
                        .push_point(x1 + width_x, y1 - width_y, c1)
                        .push_point(x2 + width_x, y2 - width_y, c2)
                        .push_point(x2 - width_x, y2 + width_y, c2)
                        .build(),
                );
            }
        } else {
            self.push_primitive(
                ShapeBuilder::new(true, self.white_pixel, alpha, self.depth)
                    .push_point(x1, y1, c1)
                    .push_point(x2, y2, c2)
                    .build(),
            );
        }
    }

    fn draw_triangle(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
        c1: i32,
        c2: i32,
        c3: i32,
        alpha: f64,
        outline: bool,
    ) {
        self.push_primitive(
            ShapeBuilder::new(outline, self.white_pixel, alpha, self.depth)
                .push_point(x1, y1, c1)
                .push_point(x2, y2, c2)
                .push_point(x3, y3, c3)
                .build(),
        );
    }

    fn draw_ellipse(&mut self, x: f64, y: f64, rad_x: f64, rad_y: f64, c1: i32, c2: i32, alpha: f64, outline: bool) {
        let mut builder = ShapeBuilder::new(outline, self.white_pixel, alpha, self.depth);
        if !outline {
            builder.push_point(x, y, c1);
        }
        for i in 0..=self.circle_precision {
            let angle = f64::from(i) * 2.0 * PI / f64::from(self.circle_precision);
            builder.push_point(x + rad_x * angle.cos(), y + rad_y * angle.sin(), c2);
        }
        self.push_primitive(builder.build());
    }

    fn draw_roundrect(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, c1: i32, c2: i32, alpha: f64, outline: bool) {
        let x2 = if x2 == x2.floor() { x2 + 0.01 } else { x2 };
        let y2 = if y2 == y2.floor() { y2 + 0.01 } else { y2 };
        let xcenter = (x1 + x2) / 2.0;
        let ycenter = (y1 + y2) / 2.0;
        let width = (x2 - x1).abs();
        let height = (y2 - y1).abs();
        let rad_x = width.min(10.0) / 2.0;
        let rad_y = height.min(10.0) / 2.0;
        let rect_half_w = (width / 2.0 - rad_x).max(0.0);
        let rect_half_h = (height / 2.0 - rad_y).max(0.0);
        let mut builder = ShapeBuilder::new(outline, self.white_pixel, alpha, self.depth);
        if !outline {
            builder.push_point(xcenter, ycenter, c1);
        }
        let quarter_circle = self.circle_precision / 4;
        for quad in 0..4 {
            let circle_x = xcenter + if quad == 0 || quad == 3 { rect_half_w } else { -rect_half_w };
            let circle_y = ycenter + if quad < 2 { rect_half_h } else { -rect_half_h };
            for i in quarter_circle * quad..=quarter_circle * (quad + 1) {
                let angle = f64::from(i) * 2.0 * PI / f64::from(self.circle_precision);
                builder.push_point(circle_x + rad_x * angle.cos(), circle_y + rad_y * angle.sin(), c2);
            }
        }
        self.push_primitive(builder.push_point(xcenter + rect_half_w + rad_x, ycenter + rect_half_h, c2).build());
    }

    fn set_circle_precision(&mut self, prec: i32) {
        self.circle_precision = (prec.max(4).min(64) >> 2) << 2;
    }

    fn get_circle_precision(&self) -> i32 {
        self.circle_precision
    }

    fn reset_primitive_2d(&mut self, ptype: PrimitiveType, atlas_ref: Option<AtlasRef>) {
        let ar = atlas_ref.and_then(|ar| self.get_rect(ar).copied());
        self.primitive_2d = PrimitiveBuilder::new(ar.unwrap_or(self.white_pixel), ptype, ar.is_some());
    }

    fn vertex_2d(&mut self, x: f64, y: f64, xtex: f64, ytex: f64, col: i32, alpha: f64) {
        self.primitive_2d.push_vertex(
            [x as f32, y as f32, self.depth],
            [xtex as f32, ytex as f32],
            split_colour(col, alpha),
            [0.0, 0.0, 0.0],
        );
    }

    fn draw_primitive_2d(&mut self) {
        // I would use push_primitive but that causes borrowing issues.
        self.setup_queue(self.primitive_2d.get_atlas_id(), self.primitive_2d.get_shape());
        self.vertex_queue.extend_from_slice(self.primitive_2d.get_vertices());
    }

    fn get_primitive_2d(&self) -> PrimitiveBuilder {
        self.primitive_2d.clone()
    }

    fn set_primitive_2d(&mut self, prim: PrimitiveBuilder) {
        self.primitive_2d = prim;
    }

    fn reset_primitive_3d(&mut self, ptype: PrimitiveType, atlas_ref: Option<AtlasRef>) {
        let ar = atlas_ref.and_then(|ar| self.get_rect(ar).copied());
        self.primitive_3d = PrimitiveBuilder::new(ar.unwrap_or(self.white_pixel), ptype, ar.is_some());
    }

    fn vertex_3d(
        &mut self,
        x: f64,
        y: f64,
        z: f64,
        nx: f64,
        ny: f64,
        nz: f64,
        xtex: f64,
        ytex: f64,
        col: i32,
        alpha: f64,
    ) {
        self.primitive_3d.push_vertex(
            [x as f32, y as f32, z as f32],
            [xtex as f32, ytex as f32],
            split_colour(col, alpha),
            [nx as f32, ny as f32, nz as f32],
        );
    }

    fn draw_primitive_3d(&mut self) {
        // See draw_primitive_2d.
        self.setup_queue(self.primitive_3d.get_atlas_id(), self.primitive_3d.get_shape());
        self.vertex_queue.extend_from_slice(self.primitive_3d.get_vertices());
    }

    fn get_primitive_3d(&self) -> PrimitiveBuilder {
        self.primitive_3d.clone()
    }

    fn set_primitive_3d(&mut self, prim: PrimitiveBuilder) {
        self.primitive_3d = prim;
    }

    fn extend_buffers(&self, buf: &mut VertexBuffer) {
        let verts = self.primitive_3d.get_vertices();
        match self.primitive_3d.get_shape() {
            PrimitiveShape::Point => buf.points.extend_from_slice(verts),
            PrimitiveShape::Line => buf.lines.extend_from_slice(&verts[..verts.len() / 2 * 2]),
            PrimitiveShape::Triangle => buf.tris.extend_from_slice(&verts[..verts.len() / 3 * 3]),
        }
    }

    fn draw_buffers(&mut self, atlas_ref: Option<AtlasRef>, buf: &VertexBuffer) {
        // TODO: bench this method vs copying the buffer onto the draw queue
        self.flush_queue();
        self.update_render_state();
        // The texture belongs to this draw, not to the cached vertices -- see `draw_buffer`.
        let rect = atlas_ref.and_then(|ar| self.get_rect(ar).copied());
        let texture = DrawTexture { rect: rect.unwrap_or(self.white_pixel), textured: rect.is_some() };
        let atlas_id = texture.rect.atlas_id;
        self.draw_buffer(atlas_id, PrimitiveShape::Point, &buf.points, Some(texture));
        self.draw_buffer(atlas_id, PrimitiveShape::Line, &buf.lines, Some(texture));
        self.draw_buffer(atlas_id, PrimitiveShape::Triangle, &buf.tris, Some(texture));
    }

    fn get_alpha_blending(&self) -> bool {
        self.next_render_state.alpha_blending
    }

    fn set_alpha_blending(&mut self, alphablend: bool) {
        self.next_render_state.alpha_blending = alphablend;
        self.render_state_updated = true;
    }

    fn get_colour_blending(&self) -> bool {
        self.next_render_state.texture_blend.into()
    }

    fn set_colour_blending(&mut self, modulate: bool) {
        self.next_render_state.texture_blend = modulate.into();
        self.render_state_updated = true;
    }

    fn get_blend_mode(&self) -> (BlendType, BlendType) {
        self.next_render_state.blend_mode
    }

    fn set_blend_mode(&mut self, src: BlendType, dst: BlendType) {
        self.next_render_state.blend_mode = (src, dst);
        self.render_state_updated = true;
    }

    fn set_blend_mode_alpha(&mut self, modes: Option<(BlendType, BlendType)>) {
        self.next_render_state.blend_mode_alpha = modes;
        self.render_state_updated = true;
    }

    fn set_colour_mask(&mut self, r: bool, g: bool, b: bool, a: bool) {
        self.next_render_state.colour_mask = (r, g, b, a);
        self.render_state_updated = true;
    }

    fn set_depth_func(&mut self, func: DepthFunc) {
        self.next_render_state.depth_func = func;
        self.render_state_updated = true;
    }

    fn set_depth_bias(&mut self, bias: f64) {
        self.next_render_state.depth_bias = bias as f32;
        self.render_state_updated = true;
    }

    fn set_depth_range(&mut self, near: f64, far: f64) {
        self.next_render_state.depth_range = (near as f32, far as f32);
        self.render_state_updated = true;
    }

    fn get_pixel_interpolation(&self) -> bool {
        self.next_render_state.interpolate_pixels.into()
    }

    fn set_pixel_interpolation(&mut self, lerping: bool) {
        // in DX (and therefore GM) this is set per texture unit, but in GL it's per texture
        // therefore, we need to apply the setting before every draw call
        self.next_render_state.interpolate_pixels = lerping.into();
        self.render_state_updated = true;
    }

    fn get_texture_repeat(&self) -> bool {
        self.next_render_state.texture_repeat.into()
    }

    fn set_texture_repeat(&mut self, repeat: bool) {
        self.next_render_state.texture_repeat = repeat.into();
        self.render_state_updated = true;
    }

    /// Does anything that's queued to be done.
    fn flush_queue(&mut self) {
        // move the queue out of self to satisfy the borrow checker
        let mut queue = std::mem::take(&mut self.vertex_queue);
        // The queue's own rects are trustworthy: `setup_queue` flushes whenever the page changes, so
        // every vertex in here belongs to `current_atlas`.
        self.draw_buffer(self.current_atlas, self.queue_type, &queue, None);
        // clear it and put it back so we can reuse the memory
        queue.clear();
        self.vertex_queue = queue;
    }

    fn set_view_matrix(&mut self, view: [f32; 16]) {
        self.next_render_state.view_matrix = view;
        self.render_state_updated = true;
    }

    fn set_viewproj_matrix(&mut self, view: [f32; 16], proj: [f32; 16]) {
        self.next_render_state.view_matrix = view;
        self.next_render_state.proj_matrix = proj;
        self.render_state_updated = true;
    }

    fn get_view_matrix(&self) -> [f32; 16] {
        self.next_render_state.view_matrix
    }

    fn get_proj_matrix(&self) -> [f32; 16] {
        self.next_render_state.proj_matrix
    }

    fn get_model_matrix(&self) -> [f32; 16] {
        self.next_render_state.model_matrix
    }

    fn set_model_matrix(&mut self, model: [f32; 16]) {
        self.next_render_state.model_matrix = model;
        self.render_state_updated = true;
    }

    fn mult_model_matrix(&mut self, model: [f32; 16]) {
        self.next_render_state.model_matrix = mat4mult(self.next_render_state.model_matrix, model);
        self.render_state_updated = true;
    }

    fn set_projection_ortho(&mut self, x: f64, y: f64, w: f64, h: f64, angle: f64) {
        #[rustfmt::skip]
        let proj_matrix: [f32; 16] = {
            // Squish to screen, flip vertically, and constrain z to range 1 - 32000
            [
                2.0 / w as f32, 0.0,             0.0,            0.0,
                0.0,            -2.0 / h as f32, 0.0,            0.0,
                0.0,            0.0,             1.0 / 31999.0,  0.0,
                0.0,            0.0,             -1.0 / 31999.0, 1.0,
            ]
        };

        self.set_viewproj_matrix(make_view_matrix(x, y, -16000.0, w, h, angle), proj_matrix);
    }

    fn set_projection_perspective(&mut self, x: f64, y: f64, w: f64, h: f64, angle: f64) {
        #[rustfmt::skip]
        let proj_matrix: [f32; 16] = {
            // Squish to screen, flip vertically, and constrain z to range 1 - 32000
            [
                2.0, 0.0,                  0.0,                0.0,
                0.0, 2.0 * (w / h) as f32, 0.0,                0.0,
                0.0, 0.0,                  32000.0 / 31999.0,  1.0,
                0.0, 0.0,                  -32000.0 / 31999.0, 0.0,
            ]
        };

        self.set_viewproj_matrix(make_view_matrix(x, y, -w, w, h, angle), proj_matrix);
    }

    fn set_view(
        &mut self,
        src_x: i32,
        src_y: i32,
        src_w: i32,
        src_h: i32,
        src_angle: f64,
        port_x: i32,
        port_y: i32,
        port_w: i32,
        port_h: i32,
    ) {
        self.flush_queue();
        // DX8's viewport function doesn't do anything if a surface is set as the draw target, so emulate that
        // (uses our own tracked binding-point mirror instead of glGetIntegerv, see the module doc comment)
        if self.current_draw_fbo.get() == self.framebuffer.fbo {
            // Set viewport
            if port_x >= 0 && port_y >= 0 && port_w >= 0 && port_h >= 0 {
                self.set_gl_viewport(port_x, port_y, port_w, port_h);
            }
        }
        if self.using_3d && self.perspective {
            self.set_projection_perspective(src_x.into(), src_y.into(), src_w.into(), src_h.into(), src_angle);
        } else {
            self.set_projection_ortho(src_x.into(), src_y.into(), src_w.into(), src_h.into(), src_angle);
        }
    }

    fn get_viewport(&self) -> (i32, i32, i32, i32) {
        self.viewport.get()
    }

    fn set_viewport(&mut self, x: i32, y: i32, w: i32, h: i32) {
        // Anything already queued was drawn under the old viewport, so it has to go out first --
        // unlike the `RenderState` fields, this is applied immediately rather than at the next
        // state update, since the viewport is also what `update_matrix` reads.
        self.flush_queue();
        self.set_gl_viewport(x, y, w, h);
    }

    fn clear_view(&mut self, colour: Colour, alpha: f64) {
        self.flush_queue();
        unsafe {
            self.gl.clear_color(colour.r as f32, colour.g as f32, colour.b as f32, alpha as f32);
            self.clear_including_depth(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            self.check_gl_error("clear_view");
        }
    }

    fn clear_view_no_zbuf(&mut self, colour: Colour, alpha: f64) {
        self.flush_queue();
        unsafe {
            self.gl.clear_color(colour.r as f32, colour.g as f32, colour.b as f32, alpha as f32);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.check_gl_error("clear_view_no_zbuf");
        }
    }

    /// Clears the depth buffer, **unconditionally**.
    ///
    /// Not gated on `using_3d`, which asks the wrong question. `using_3d` is only true
    /// between `d3d_start()` and `d3d_end()`, and a game is free to call both inside a *draw event*
    /// -- renex engine's teapot boss does exactly that. Its room's per-frame clear (`draw_view`,
    /// where `background_showcolor = 0` selects the depth-only branch) runs with 3D off, and with
    /// that gate would clear nothing, so the boss would depth-test against the previous frame's
    /// depths and come out with holes punched through it. GM8's own runner has no such condition here.
    ///
    /// The cost is one `glClear` per view per frame for 2D games. `clear_view` -- the other branch
    /// of that same choice -- has always cleared depth unconditionally, so this only makes the two
    /// agree.
    fn clear_zbuf(&mut self) {
        self.flush_queue();
        unsafe {
            self.clear_including_depth(glow::DEPTH_BUFFER_BIT);
            self.check_gl_error("clear_zbuf");
        }
    }

    fn get_3d(&self) -> bool {
        self.using_3d
    }

    fn set_3d(&mut self, use_3d: bool) {
        self.using_3d = use_3d;
        self.set_depth_test(use_3d);
        // `d3d_start` is hidden-surface removal, which in D3D is ZENABLE *and* ZWRITEENABLE; only
        // the first was being set here. It went unnoticed because the tracked `write_depth` default
        // disagreed with GL's, so depth writing was unconditionally on anyway -- now that GL is
        // seeded from the tracked state, 2D correctly writes no depth and this has to turn it back
        // on, or nothing in a 3D scene would ever occlude anything.
        self.set_write_depth(use_3d);
        self.set_perspective(use_3d);
    }

    fn get_depth(&self) -> f32 {
        self.depth
    }

    fn set_depth(&mut self, depth: f32) {
        self.depth = if self.using_3d { depth.max(-16000.0).min(16000.0) } else { 0.0 };
    }

    fn get_depth_test(&self) -> bool {
        self.next_render_state.depth_test.into()
    }

    fn set_depth_test(&mut self, depth_test: bool) {
        let depth_test = depth_test && self.using_3d;
        self.next_render_state.depth_test = depth_test.into();
        self.render_state_updated = true;
    }

    fn get_write_depth(&self) -> bool {
        self.next_render_state.write_depth
    }

    fn set_write_depth(&mut self, write_depth: bool) {
        self.next_render_state.write_depth = write_depth;
        self.render_state_updated = true;
    }

    fn get_culling(&self) -> bool {
        self.next_render_state.culling
    }

    fn set_cull_clockwise(&mut self, clockwise: bool) {
        self.next_render_state.cull_clockwise = clockwise;
        self.render_state_updated = true;
    }

    fn set_culling(&mut self, culling: bool) {
        self.next_render_state.culling = culling;
        self.render_state_updated = true;
    }

    fn get_perspective(&self) -> bool {
        self.perspective
    }

    fn set_perspective(&mut self, perspective: bool) {
        // don't need to flush_queue for this because this only affects set_view
        self.perspective = perspective;
    }

    fn get_fog(&self) -> Option<Fog> {
        let c = self.next_render_state.fog_colour;
        bool::from(self.next_render_state.fog_enabled).then(|| Fog {
            colour: u32::from(Colour::from((f64::from(c[0]), c[1].into(), c[2].into()))) as i32,
            begin: self.next_render_state.fog_begin,
            end: self.next_render_state.fog_end,
        })
    }

    fn set_fog(&mut self, fog: Option<Fog>) {
        self.next_render_state.fog_enabled = fog.is_some().into();
        if let Some(fog) = fog {
            self.next_render_state.fog_colour = split_colour(fog.colour, 1.0);
            self.next_render_state.fog_begin = fog.begin;
            self.next_render_state.fog_end = fog.end;
        }
        self.render_state_updated = true;
    }

    fn get_gouraud(&self) -> bool {
        self.next_render_state.gouraud.into()
    }

    fn set_gouraud(&mut self, gouraud: bool) {
        self.next_render_state.gouraud = gouraud.into();
        self.render_state_updated = true;
    }

    fn get_lighting_enabled(&self) -> bool {
        self.next_render_state.lighting.into()
    }

    fn set_lighting_enabled(&mut self, enabled: bool) {
        self.next_render_state.lighting = enabled.into();
        self.render_state_updated = true;
    }

    fn get_ambient_colour(&self) -> i32 {
        let col = &self.next_render_state.ambient_colour[0..3];
        u32::from(Colour::from((f64::from(col[0]), col[1].into(), col[2].into()))) as _
    }

    fn set_ambient_colour(&mut self, colour: i32) {
        self.next_render_state.ambient_colour = split_colour(colour, 1.0);
        self.render_state_updated = true;
    }

    fn get_lights(&self) -> [(bool, Light); 8] {
        let mut iter = self.next_render_state.lights.iter().map(|l| {
            let position = [l.pos[0], l.pos[1], l.pos[2]];
            let colour =
                u32::from(Colour::from((f64::from(l.colour[0]), l.colour[1].into(), l.colour[2].into()))) as i32;
            (
                l.enabled.into(),
                if l.is_point.into() {
                    Light::Point { position, range: l.range, colour }
                } else {
                    Light::Directional { direction: position, colour }
                },
            )
        });
        [
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
            iter.next().unwrap(),
        ]
    }

    fn set_lights(&mut self, lights: [(bool, Light); 8]) {
        lights.iter().enumerate().for_each(|(i, &(enabled, light))| {
            self.set_light_enabled(i, enabled);
            self.set_light(i, light);
        })
    }

    fn set_light_enabled(&mut self, id: usize, enabled: bool) {
        self.next_render_state.lights[id].enabled = enabled.into();
        self.render_state_updated = true;
    }

    fn set_light(&mut self, id: usize, light: Light) {
        match light {
            Light::Directional { direction, colour } => {
                self.next_render_state.lights[id].is_point = false.into();
                self.next_render_state.lights[id].pos[0..3].copy_from_slice(&direction);
                self.next_render_state.lights[id].colour = split_colour(colour, 1.0);
            },
            Light::Point { position, range, colour } => {
                self.next_render_state.lights[id].is_point = true.into();
                self.next_render_state.lights[id].pos[0..3].copy_from_slice(&position);
                self.next_render_state.lights[id].colour = split_colour(colour, 1.0);
                self.next_render_state.lights[id].range = range;
            },
        }
        self.render_state_updated = true;
    }

    fn get_texcoord_source(&self) -> TexcoordSource {
        self.next_render_state.texcoord_source
    }

    fn set_texcoord_source(&mut self, source: TexcoordSource) {
        self.next_render_state.texcoord_source = source;
        self.render_state_updated = true;
    }

    // -- game-supplied shaders -------------------------------------------------------------------
    //
    // Everything below flushes the queue first. Anything already queued was built to be drawn with
    // the shader, constants and textures in force when it was queued, so letting a later change
    // reach it would silently redraw it under someone else's state -- the same reason
    // `set_viewport` flushes.

    fn create_shader(&mut self, stage: ShaderStage, glsl: &str) -> Result<ShaderRef, String> {
        let kind = match stage {
            ShaderStage::Vertex => glow::VERTEX_SHADER,
            ShaderStage::Pixel => glow::FRAGMENT_SHADER,
        };
        unsafe {
            let shader = self.gl.create_shader(kind)?;
            self.gl.shader_source(shader, glsl);
            self.gl.compile_shader(shader);
            if !self.gl.get_shader_compile_status(shader) {
                let log = self.gl.get_shader_info_log(shader);
                self.gl.delete_shader(shader);
                return Err(log)
            }
            let attribute_bindings = match stage {
                ShaderStage::Vertex => Self::discover_attributes(glsl),
                ShaderStage::Pixel => Vec::new(),
            };
            // Handles are never reused: a game that destroys a shader and keeps drawing with the
            // old id gets nothing, rather than whatever was created next.
            self.shaders.push(Some(CompiledShader { stage, shader, attribute_bindings }));
            Ok(ShaderRef(self.shaders.len() as i32 - 1))
        }
    }

    fn delete_shader(&mut self, shader: ShaderRef) {
        self.flush_queue();
        let Some(slot) = self.shaders.get_mut(shader.0.max(0) as usize) else { return };
        let Some(compiled) = slot.take() else { return };
        unsafe {
            self.gl.delete_shader(compiled.shader);
        }
        // Every program built from it is now unlinkable, so drop them rather than leave a cache
        // entry pointing at a deleted stage.
        let doomed: Vec<(i32, i32)> =
            self.programs.keys().copied().filter(|(v, p)| *v == shader.0 || *p == shader.0).collect();
        for pair in doomed {
            if let Some(program) = self.programs.remove(&pair) {
                unsafe {
                    self.gl.delete_program(program.program);
                }
            }
            if self.active_custom_program == Some(pair) {
                self.active_custom_program = None;
            }
            if self.custom_vao_program == Some(pair) {
                self.custom_vao_program = None;
            }
        }
        for bound in self.bound_shaders.iter_mut() {
            if *bound == Some(shader) {
                *bound = None;
            }
        }
    }

    fn set_shader_stage(&mut self, stage: ShaderStage, shader: Option<ShaderRef>) {
        self.flush_queue();
        // A handle for the wrong half would link into a program that compiles and draws nothing
        // recognisable, so check it here where there is still something to say no to.
        let shader = shader
            .filter(|s| self.shaders.get(s.0.max(0) as usize).and_then(Option::as_ref).map(|c| c.stage) == Some(stage));
        self.bound_shaders[stage as usize] = shader;
    }

    fn get_shader_stage(&self, stage: ShaderStage) -> Option<ShaderRef> {
        self.bound_shaders[stage as usize]
    }

    fn set_shader_constant(&mut self, stage: ShaderStage, register: u32, value: [f32; 4]) {
        let Some(slot) = self.shader_constants[stage as usize].get_mut(register as usize) else { return };
        if *slot == value {
            return
        }
        self.flush_queue();
        self.shader_constants[stage as usize][register as usize] = value;
        self.shader_constants_dirty[stage as usize] = true;
    }

    fn set_shader_sampler(&mut self, kind: ShaderStage, stage: u32, texture: Option<AtlasRef>) {
        if stage as usize >= SHADER_SAMPLER_STAGES {
            return
        }
        self.flush_queue();
        self.shader_samplers[kind as usize][stage as usize] = texture;
    }

    fn set_shader_sampler_state(
        &mut self,
        kind: ShaderStage,
        stage: u32,
        interpolate: Option<bool>,
        repeat: Option<(bool, bool)>,
    ) {
        if stage as usize >= SHADER_SAMPLER_STAGES {
            return
        }
        let slot = &mut self.shader_sampler_state[kind as usize][stage as usize];
        let updated = SamplerState { interpolate: interpolate.or(slot.interpolate), repeat: repeat.or(slot.repeat) };
        if *slot == updated {
            return
        }
        self.flush_queue();
        self.shader_sampler_state[kind as usize][stage as usize] = updated;
    }

    fn draw_vertex_streams(
        &mut self,
        streams: &[VertexStream],
        indices: Option<IndexData>,
        primitive: PrimitiveType,
        vertex_count: usize,
        instance_count: usize,
        texture: Option<AtlasRef>,
    ) -> bool {
        // Without a shader there is nothing here that could read a game's own vertex declaration,
        // so the caller unpacks the buffer into immediate-mode calls instead.
        let Some(pair) = self.active_shader_pair() else { return false };
        if vertex_count == 0 || instance_count == 0 {
            return true
        }
        // Anything queued was built for the immediate-mode layout and must go out under it.
        self.flush_queue();
        // ...and then GL has to be put into the state the game has *since* asked for. Render state
        // is only pushed when an immediate-mode draw needs it, so a path that draws without going
        // through the queue inherits whatever the last one left behind. renex engine's lighting
        // alternates a shadow-volume stream draw with an immediate-mode quad under `bm_subtract`,
        // and the leftover subtract made every shadow pass after the first draw black-on-white into
        // nothing at all -- one light cast shadows and the other five did not.
        self.update_render_state();

        let draw_texture = texture.and_then(|t| self.get_rect(t).copied());
        if !self.bind_custom_program(pair, draw_texture) {
            return false
        }
        let bindings = match self.shader_program(pair) {
            Some(program) => program.attribute_bindings.clone(),
            None => return false,
        };

        unsafe {
            self.gl.bind_vertex_array(Some(self.stream_vao));
            // Whatever a previous draw left enabled has to go: a stale array still pointing into a
            // freed buffer is a crash, and a stale divisor silently turns per-vertex data into
            // per-instance data.
            for location in 0..16 {
                self.gl.disable_vertex_attrib_array(location);
                self.gl.vertex_attrib_divisor(location, 0);
            }

            // One GL buffer per stream, grown as needed and reused between draws.
            for (index, stream) in streams.iter().enumerate() {
                while self.stream_buffers.len() <= index {
                    let Ok(buffer) = self.gl.create_buffer() else { return false };
                    self.stream_buffers.push(buffer);
                }
                self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.stream_buffers[index]));
                // D3DCOLOR is `0xAARRGGBB`, so B, G, R, A in memory, and GLES has no BGRA vertex
                // format -- reading it as four normalised bytes gives the right values in the wrong
                // order. The bytes are therefore swapped on the way to the GPU, which is the only
                // place that can do it: `sm3` deliberately knows nothing about the vertex
                // declaration, and the same shader may be drawn with several. Measured on the real
                // runner: a vertex buffer's d3dcolor reaches a shader with red in red.
                //
                // Only paid for by a stream that actually declares one, since it costs a copy.
                let swapped;
                let mut data = stream.data;
                let colours: Vec<usize> =
                    stream.elements.iter().filter(|(k, ..)| *k == 4).map(|&(_, _, _, o)| o).collect();
                if !colours.is_empty() && stream.stride >= 4 {
                    let mut bytes = stream.data.to_vec();
                    for vertex in bytes.chunks_mut(stream.stride) {
                        for &offset in &colours {
                            if offset + 4 <= vertex.len() {
                                vertex.swap(offset, offset + 2);
                            }
                        }
                    }
                    swapped = bytes;
                    data = &swapped;
                }
                self.gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, data, glow::STREAM_DRAW);
                for &(kind, usage, usage_index, offset) in stream.elements {
                    // Match the declaration to the shader by semantic *and* index. An element the
                    // shader never declared is simply not fed, which is what D3D does too.
                    let Some(&(location, _, _)) =
                        bindings.iter().find(|(_, u, i)| i32::from(*u) == usage && *i == usage_index)
                    else {
                        continue
                    };
                    let Some((size, gl_type, normalise)) = Self::vertex_element_format(kind) else { continue };
                    self.gl.enable_vertex_attrib_array(location);
                    self.gl.vertex_attrib_pointer_f32(
                        location,
                        size,
                        gl_type,
                        normalise,
                        stream.stride as i32,
                        offset as i32,
                    );
                    self.gl.vertex_attrib_divisor(location, stream.divisor);
                }
            }

            let shape = gl_primitive(primitive);
            match indices {
                Some(index_data) => {
                    let buffer = match self.index_buffer {
                        Some(buffer) => buffer,
                        None => {
                            let Ok(buffer) = self.gl.create_buffer() else { return false };
                            self.index_buffer = Some(buffer);
                            buffer
                        },
                    };
                    self.gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(buffer));
                    self.gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, index_data.data, glow::STREAM_DRAW);
                    let (element_type, element_size) =
                        if index_data.wide { (glow::UNSIGNED_INT, 4) } else { (glow::UNSIGNED_SHORT, 2) };
                    let count = (index_data.data.len() / element_size) as i32;
                    self.gl.draw_elements_instanced(shape, count, element_type, 0, instance_count as i32);
                },
                None => self.gl.draw_arrays_instanced(shape, 0, vertex_count as i32, instance_count as i32),
            }
        }
        self.check_gl_error_hot("draw_vertex_streams");
        // The stream vertex array is only ever configured here, so nothing else has to undo it --
        // but the next immediate-mode draw must not inherit it.
        self.custom_vao_program = None;
        true
    }

    fn get_viewproj_matrix(&self) -> [f32; 16] {
        // Recomputed rather than read out of `queue_render_state`, whose copy is only refreshed
        // when a draw forces a state update -- a game that sets a matrix and immediately asks for
        // it would otherwise get the previous one.
        let mut state = self.next_render_state.clone();
        let (_, _, width, height) = self.viewport.get();
        state.update_matrix(width, height);
        state.viewproj_matrix
    }

    fn present(&mut self, window_width: u32, window_height: u32, scaling: Scaling) {
        // Drain and report whatever the frame's draws raised. `draw_buffer` does not check per
        // call (see `check_gl_error_hot`), so without this the queue carries an error forward until
        // some unrelated `check_gl_error` picks it up and blames itself -- which is how a dropped
        // draw would turn into "surface_create: Failed to allocate texture on GPU". One
        // synchronous call per frame buys back an honest attribution.
        self.check_gl_error("the frame just drawn");
        if window_width == 0 || window_height == 0 {
            // if we continue, intel will dereference a null pointer
            return
        }
        // Finish drawing frame
        self.flush_queue();

        // Get framebuffer size (tracked in Rust, see the `Framebuffer` doc comment)
        let (fb_width, fb_height) = (self.framebuffer.width, self.framebuffer.height);

        // yeah i know but they need to be converted anyway
        let (window_width, window_height) = (window_width as i32, window_height as i32);

        // Scaling
        let (w_x, w_y, w_w, w_h) = match scaling {
            Scaling::Fixed(scale) => {
                let w = (f64::from(fb_width) * scale) as i32;
                let h = (f64::from(fb_height) * scale) as i32;
                ((window_width - w) / 2, (window_height - h) / 2, w, h)
            },
            Scaling::Aspect(_) => {
                if fb_width > 0 && fb_height > 0 {
                    let fixed_width = window_height * fb_width / fb_height;
                    if fixed_width < window_width {
                        // window is too wide
                        ((window_width - fixed_width) / 2, 0, fixed_width, window_height)
                    } else {
                        // window is too tall
                        let fixed_height = window_width * fb_height / fb_width;
                        (0, (window_height - fixed_height) / 2, window_width, fixed_height)
                    }
                } else {
                    // can never be too careful
                    (0, 0, fb_width, fb_height)
                }
            },
            Scaling::Full => (0, 0, window_width, window_height),
        };

        unsafe {
            // On Intel, glBlitFrameBuffer just does nothing if the scissor box is too big, which it
            // very well could be. So just disable the scissor test for now.
            self.gl.disable(glow::SCISSOR_TEST);
        }

        // Remember old framebuffer so we can rebind it after we're done
        let fb_old = self.current_draw_fbo.get();

        // Draw framebuffer to screen (None = the canvas itself)
        self.bind_draw_fbo(None);
        self.clear_view((0.0, 0.0, 0.0).into(), 1.0); // to avoid weird strobe lights (???)
        self.bind_read_fbo(self.framebuffer.fbo);
        unsafe {
            self.gl.blit_framebuffer(
                0,
                fb_height,
                fb_width,
                0,
                w_x,
                w_y,
                w_x + w_w,
                w_y + w_h,
                glow::COLOR_BUFFER_BIT,
                if bool::from(self.next_render_state.interpolate_pixels) { glow::LINEAR } else { glow::NEAREST },
            );
        }
        self.bind_draw_fbo(fb_old);

        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
        }

        // Note: no `self.imp.swap_buffers()` call here -- WebGL2 has no explicit presentation
        // step. The browser composites the canvas automatically once this task returns control to
        // the event loop (see the module doc comment).
    }

    fn draw_stored(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return
        }

        let framebuffer = match self.stored_framebuffer {
            Some(f) => f,
            None => return,
        };

        self.flush_queue();

        let (width, height) = (w as i32, h as i32);

        // Remember old framebuffer so we can rebind it after we're done
        let fb_old = self.current_draw_fbo.get();

        // Draw framebuffer to screen
        self.bind_draw_fbo(self.framebuffer.fbo);
        self.bind_read_fbo(framebuffer.fbo);
        unsafe {
            self.gl.blit_framebuffer(
                0,
                0,
                framebuffer.width,
                framebuffer.height,
                x,
                y,
                x + width,
                y + height,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
        }
        self.bind_draw_fbo(fb_old);
    }

    fn stored_size(&self) -> (u32, u32) {
        let framebuffer = self.stored_framebuffer.unwrap_or(self.framebuffer);
        (framebuffer.width.max(0) as u32, framebuffer.height.max(0) as u32)
    }

    fn finish(&mut self, window_width: u32, window_height: u32, clear_colour: Colour) {
        // Present screen
        self.present(window_width, window_height, Scaling::Fixed(1.0));

        // Start next frame
        self.setup_frame(clear_colour)
    }
}
