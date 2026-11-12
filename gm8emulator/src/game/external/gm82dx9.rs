//! Reimplementation of `gm82dx9.dll`, the graphics half of GameMaker 8.2's bundled extensions
//! (<https://github.com/GM82Project>). Like the rest of the family it ships with the IDE, so every
//! GM8.2 game links it whether or not it draws anything unusual.
//!
//! What it adds to GM8's fixed-function `d3d_*` surface is roughly "the parts of GameMaker Studio
//! that GM8 never got": a full matrix stack, render-state control D3D always had but GM never
//! exposed (colour masks, cull winding, z comparison, separate alpha blending), vertex and index
//! buffers, and shaders. The original does all of it by talking to the GM8 runner's own
//! `IDirect3DDevice9` -- which is why `d3d_get_fog_color` can simply ask the device -- and by
//! patching the runner in memory at hardcoded addresses.
//!
//! Here, "the device" is `render/webgl.rs`. Most of these map onto it cleanly; where they do, this
//! module is a translation layer and nothing more. Three groups do not, and are inert on purpose:
//!
//! - **Shaders.** `shader_pixel_create_file` takes *compiled Direct3D 9 shader bytecode* (a `.ps3`
//!   file, or the same bytes base64'd into the game), which WebGL cannot execute and which nothing
//!   here can turn into GLSL without writing a shader-model-3 decompiler. Creation answers `-1`,
//!   the extension's own not-a-shader value, so a game's own error paths see a shader that failed
//!   to load rather than one that silently does nothing. See `resolve`'s tail.
//! - **Multiple render targets and instancing.** WebGL2 has both, but this renderer's whole drawing
//!   path is one target and one stream; wiring them up is not a mapping, it is a renderer feature.
//! - **Device-level settings** -- exclusive fullscreen, backbuffer format and size. The browser
//!   canvas is sized by the page, not by the game.
//!
//! Transcribed from the extension's own C++ (`source/gm82dx9.cpp`, `transform.cpp`,
//! `vertex_buffers.cpp`), which like gm82core is zlib-licensed and explicitly permits alteration.
//! Several of these are easy to get subtly wrong by guessing -- the rotation combinators multiply
//! *into* the current matrix rather than replacing it, and their angles are negated -- so where a
//! definition looks odd it is because the original is.

use crate::{
    game::Game,
    gml::{self, Context, Function, Value},
};

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use crate::render::{
        BlendType, DepthFunc, Light, PrimitiveType, ShaderRef, ShaderStage, TexcoordSource, atlas::AtlasRef,
    };
    use std::cell::{Cell, RefCell};

    fn ok_num(n: f64) -> gml::Result<Value> {
        Ok(Value::Real(n.into()))
    }

    fn arg_f64(v: &Value) -> f64 {
        match v {
            Value::Real(r) => (*r).into(),
            Value::Str(_) => crate::game::decode_gml_string(<&Value as Into<&[u8]>>::into(v)).parse().unwrap_or(0.0),
        }
    }

    /// Argument `i` as a number, defaulting to 0 -- extension functions are called through GML
    /// wrappers that always pass the full list, but a missing one must not panic.
    fn a(args: &[Value], i: usize) -> f64 {
        args.get(i).map(arg_f64).unwrap_or(0.0)
    }

    fn ai(args: &[Value], i: usize) -> i32 {
        a(args, i) as i32
    }

    /// Argument as a GML string. Shader functions take file names and uniform names, which are the
    /// only strings this extension passes.
    fn arg_str(v: &Value) -> std::borrow::Cow<'_, str> {
        crate::game::decode_gml_string(<&Value as Into<&[u8]>>::into(v))
    }

    /// The extension's own truthiness for flag arguments, which is `>= 0.5` rather than `!= 0`.
    fn ab(args: &[Value], i: usize) -> bool {
        a(args, i) >= 0.5
    }

    thread_local! {
        static DX9_STARTED: Cell<bool> = Cell::new(false);
        // Result of the last transform/project/eye-position call. The DLL returns only x and keeps
        // y and z in a file-static vector for `__gm82dx9_getvertexy`/`getvertexz` to fetch, because
        // a GM8 external can only return one value.
        static VERTEX: Cell<(f64, f64, f64)> = Cell::new((0.0, 0.0, 0.0));
        static VERTEX_FORMATS: RefCell<Registry<VertexFormat>> = RefCell::new(Registry::new());
        static VERTEX_BUFFERS: RefCell<Registry<VertexBuffer>> = RefCell::new(Registry::new());
        static INDEX_BUFFERS: RefCell<Registry<IndexBuffer>> = RefCell::new(Registry::new());
        // The format being assembled between `vertex_format_begin` and `vertex_format_end`.
        static FORMAT_BUILDING: RefCell<VertexFormat> = RefCell::new(VertexFormat::default());
        // Set by `__gm82dx9_index_buffer_set` and consumed by the next indexed draw, exactly like
        // the `SetIndices` it stands in for.
        static INDEX_BOUND: Cell<i32> = Cell::new(-1);
        // D3D9's stream state, which is what instancing is built out of. `vertex_bind_buffer` is
        // `SetStreamSource(slot, ...)` and the two frequency setters are `SetStreamSourceFreq`, so
        // these mirror the device rather than the one GML wrapper that happens to drive them --
        // `vertex_instance_set` is a *script* in the extension, not an export, and it reaches us as
        // three separate calls.
        //
        // `STREAM_SOURCES[slot]` is the buffer bound there (-1 for none); slot 0 is overridden by
        // whatever `vertex_buffer_draw` is handed. `STREAM_DIVISOR[slot]` is that stream's
        // `D3DSTREAMSOURCE_INSTANCEDATA` step rate, 0 meaning it advances per vertex as usual.
        static STREAM_SOURCES: Cell<[i32; 16]> = const { Cell::new([-1; 16]) };
        static STREAM_DIVISOR: Cell<[u32; 16]> = const { Cell::new([0; 16]) };
        // `SetStreamSourceFreq(0, D3DSTREAMSOURCE_INDEXEDDATA | count)`: how many instances the
        // next draw makes. 1 is "not instanced", which is also the device's own reset value.
        static INSTANCE_COUNT: Cell<usize> = const { Cell::new(1) };
    }

    /// Ids handed out to GML start at 1: 0 is `false`, and this extension family is written as if
    /// that matters (gm82core's initializer leaks one of every `ds_*` so real ones start at 1).
    struct Registry<T> {
        items: std::collections::HashMap<i32, T>,
        next: i32,
    }

    impl<T> Registry<T> {
        fn new() -> Self {
            Self { items: std::collections::HashMap::new(), next: 1 }
        }

        fn insert(&mut self, item: T) -> i32 {
            let id = self.next;
            self.next += 1;
            self.items.insert(id, item);
            id
        }
    }

    // -- handshake -------------------------------------------------------------------------------

    fn dllcheck(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(super::super::gm82::GM82_VERSION)
    }

    /// Inverted relative to `__gm82core_checkstart`, which returns 1 on its first call: this one
    /// returns **0** the first time, because `__gm82dx9_init` reads it as `if (checkstart()) exit`.
    /// Getting it backwards makes init bail before installing the app-surface compose hooks, which
    /// presents as a black screen with nothing in the log.
    fn checkstart(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(u8::from(DX9_STARTED.replace(true))))
    }

    /// Only reached when a game loads the mutually-exclusive gm82dx8 extension alongside dx9, in
    /// which case the original pops a message box and hard-exits the process. Nothing we load can
    /// reach it, so returning inertly beats killing the page.
    fn abort(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(0.0)
    }

    /// `surface_get`'s out-of-memory guard: it only consults this *after* `surface_create` has
    /// already failed, to decide between "out of memory" and a generic error. Reporting a large
    /// figure keeps that diagnostic honest -- if a surface fails here it will not be for want of
    /// video memory.
    fn free_video_memory(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(256.0 * 1024.0 * 1024.0)
    }

    /// The raw `IDirect3DSurface9*` behind a surface, which WebGL has no equivalent of. Used by
    /// `surface_get` purely as an identity check: it stores this alongside a cached surface and
    /// re-compares later to notice one that was destroyed and recreated underneath it. Distinct per
    /// surface and stable while that id is, which is all the check needs; a single constant made
    /// every surface compare equal and the check silently never fired. Offset well clear of 0,
    /// which the extension treats as "no surface".
    fn surface_ptr(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        ok_num(0x1000_0000u32 as f64 + a(args, 0))
    }

    // -- colour packing --------------------------------------------------------------------------
    //
    // GM stores a colour as 0xBBGGRR and alpha separately; D3D wants one 0xAARRGGBB word, which is
    // also what a surface's pixels are. These four convert between the two and are pure functions
    // of their arguments, so they are transcribed exactly.

    fn gm_colour(v: f64) -> u32 {
        v as i64 as u32 & 0x00ff_ffff
    }

    /// Swaps the red and blue bytes, which is the whole difference between the two orders.
    fn swap_rb(col: u32) -> u32 {
        ((col & 0xff0000) >> 16) + ((col & 0x0000ff) << 16) + (col & 0x00ff00)
    }

    fn argb_get_color(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(swap_rb(gm_colour(a(args, 0)))))
    }

    fn argb_get_alpha(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from((a(args, 0) as i64 as u32 & 0xff00_0000) >> 24) / 255.0)
    }

    fn argb_make_color(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let alpha = (a(args, 1) * 255.0) as u32;
        ok_num(f64::from((alpha << 24) | swap_rb(gm_colour(a(args, 0)))))
    }

    /// The same packing as `argb_make_color`; it exists separately because it is what a game writes
    /// into a vertex buffer, where the surface-pixel spelling would read as a different function.
    fn color_to_d3dcolor(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let alpha = (a(args, 1) * 255.0) as u32;
        ok_num(f64::from((alpha << 24) | swap_rb(gm_colour(a(args, 0)))))
    }

    // -- matrices --------------------------------------------------------------------------------

    /// Row-vector transform of a point, `v * m`, with the perspective divide -- D3D's
    /// `XMVector3TransformCoord`. Matrices are laid out the way `render.rs` and D3D both store
    /// them: row-major, translation in elements 12..14.
    fn transform_coord(v: (f64, f64, f64), m: &[f32; 16]) -> (f64, f64, f64) {
        let m = |i: usize| f64::from(m[i]);
        let (x, y, z) = v;
        let out = (
            x * m(0) + y * m(4) + z * m(8) + m(12),
            x * m(1) + y * m(5) + z * m(9) + m(13),
            x * m(2) + y * m(6) + z * m(10) + m(14),
        );
        let w = x * m(3) + y * m(7) + z * m(11) + m(15);
        if w == 0.0 || w == 1.0 { out } else { (out.0 / w, out.1 / w, out.2 / w) }
    }

    fn determinant(m: &[f32; 16]) -> f64 {
        let m = |r: usize, c: usize| f64::from(m[r * 4 + c]);
        // Expanded along the first row, with the 2x2 minors of the bottom two rows shared.
        let s0 = m(2, 0) * m(3, 1) - m(2, 1) * m(3, 0);
        let s1 = m(2, 0) * m(3, 2) - m(2, 2) * m(3, 0);
        let s2 = m(2, 0) * m(3, 3) - m(2, 3) * m(3, 0);
        let s3 = m(2, 1) * m(3, 2) - m(2, 2) * m(3, 1);
        let s4 = m(2, 1) * m(3, 3) - m(2, 3) * m(3, 1);
        let s5 = m(2, 2) * m(3, 3) - m(2, 3) * m(3, 2);
        m(0, 0) * (m(1, 1) * s5 - m(1, 2) * s4 + m(1, 3) * s3) - m(0, 1) * (m(1, 0) * s5 - m(1, 2) * s2 + m(1, 3) * s1)
            + m(0, 2) * (m(1, 0) * s4 - m(1, 1) * s2 + m(1, 3) * s0)
            - m(0, 3) * (m(1, 0) * s3 - m(1, 1) * s1 + m(1, 2) * s0)
    }

    /// Sets the world matrix from sixteen components, in D3D's `_11.._44` order -- which is the
    /// order `render.rs` stores them in too, so this is a straight copy.
    fn transform_set_matrix(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let mut m = [0.0f32; 16];
        for (i, slot) in m.iter_mut().enumerate() {
            *slot = a(args, i) as f32;
        }
        game.renderer.set_model_matrix(m);
        ok_num(0.0)
    }

    /// Multiplies the matrix on top of GM8's own transform stack into the world matrix, without
    /// popping it. The original reaches into the runner's stack at a fixed address; here it is
    /// `Game`'s own stack, whose top is the matrix a `d3d_transform_stack_push` last saved.
    fn transform_add_stack_top(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        if let Some(top) = game.model_matrix_stack.last().copied() {
            game.renderer.mult_model_matrix(top);
        }
        ok_num(0.0)
    }

    fn transform_get_determinant(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(determinant(&game.renderer.get_model_matrix()))
    }

    fn projection_get_determinant(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(determinant(&game.renderer.get_proj_matrix()))
    }

    // The rotation matrices GM8 itself builds in `kernel.rs`'s `d3d_transform_add_rotation_*`,
    // repeated here rather than reached for so the combinators below cannot drift from them. The
    // sine is negated, which is GM's y-down screen convention and matches the extension's own
    // `rot(deg / -180 * pi)`.

    fn rotation_x(deg: f64) -> [f32; 16] {
        let rad = deg.to_radians();
        let (sin, cos) = (-rad.sin() as f32, rad.cos() as f32);
        #[rustfmt::skip]
        let m = [
            1.0, 0.0,  0.0, 0.0,
            0.0, cos,  sin, 0.0,
            0.0, -sin, cos, 0.0,
            0.0, 0.0,  0.0, 1.0,
        ];
        m
    }

    fn rotation_y(deg: f64) -> [f32; 16] {
        let rad = deg.to_radians();
        let (sin, cos) = (-rad.sin() as f32, rad.cos() as f32);
        #[rustfmt::skip]
        let m = [
            cos, 0.0, -sin, 0.0,
            0.0, 1.0, 0.0,  0.0,
            sin, 0.0, cos,  0.0,
            0.0, 0.0, 0.0,  1.0,
        ];
        m
    }

    fn rotation_z(deg: f64) -> [f32; 16] {
        let rad = deg.to_radians();
        let (sin, cos) = (-rad.sin() as f32, rad.cos() as f32);
        #[rustfmt::skip]
        let m = [
            cos,  sin, 0.0, 0.0,
            -sin, cos, 0.0, 0.0,
            0.0,  0.0, 1.0, 0.0,
            0.0,  0.0, 0.0, 1.0,
        ];
        m
    }

    fn scaling(x: f64, y: f64, z: f64) -> [f32; 16] {
        #[rustfmt::skip]
        let m = [
            x as f32, 0.0,      0.0,      0.0,
            0.0,      y as f32, 0.0,      0.0,
            0.0,      0.0,      z as f32, 0.0,
            0.0,      0.0,      0.0,      1.0,
        ];
        m
    }

    fn translation(x: f64, y: f64, z: f64) -> [f32; 16] {
        #[rustfmt::skip]
        let m = [
            1.0,      0.0,      0.0,      0.0,
            0.0,      1.0,      0.0,      0.0,
            0.0,      0.0,      1.0,      0.0,
            x as f32, y as f32, z as f32, 1.0,
        ];
        m
    }

    /// The `d3d_transform_<rotations>[s]t` family: 30 functions that apply several transforms in
    /// one call, named for what they do -- `d3d_transform_xzyst(rx, rz, ry, sx, sy, sz, x, y, z)`
    /// rotates about x, then z, then y, then scales, then translates.
    ///
    /// Two things a reasonable guess gets wrong, both taken from `transform.cpp`: they **multiply
    /// into** the current world matrix rather than replacing it (so they compose with a
    /// `d3d_transform_set_*` before them), and the rotations are applied in the order the letters
    /// are written, which is not always x-y-z.
    fn transform_combo(game: &mut Game, order: &str, scale: bool, args: &[Value]) -> gml::Result<Value> {
        for (i, axis) in order.chars().enumerate() {
            let angle = a(args, i);
            game.renderer.mult_model_matrix(match axis {
                'x' => rotation_x(angle),
                'y' => rotation_y(angle),
                _ => rotation_z(angle),
            });
        }
        let mut next = order.len();
        if scale {
            game.renderer.mult_model_matrix(scaling(a(args, next), a(args, next + 1), a(args, next + 2)));
            next += 3;
        }
        game.renderer.mult_model_matrix(translation(a(args, next), a(args, next + 1), a(args, next + 2)));
        ok_num(0.0)
    }

    /// `Function::Runtime` is a bare fn pointer with nowhere to carry which combination a symbol
    /// meant, so each of the 30 needs its own function. Baking the order into a non-capturing
    /// closure (which coerces to a fn pointer) keeps them one line each.
    macro_rules! transform_thunk {
        ($order:literal, $scale:literal) => {
            |game: &mut Game, _: &mut Context, args: &[Value]| -> gml::Result<Value> {
                transform_combo(game, $order, $scale, args)
            }
        };
    }

    /// `d3d_transform_vertex(x, y, z)` -- the point under the current world matrix. Returns x; y
    /// and z are fetched afterwards, see `VERTEX`.
    fn transform_vertex(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let out = transform_coord((a(args, 0), a(args, 1), a(args, 2)), &game.renderer.get_model_matrix());
        VERTEX.set(out);
        ok_num(out.0)
    }

    /// `d3d_project_vertex(x, y, z)` -- the same point under view * projection, which the GML
    /// wrapper turns into a 0..1 screen position. Games use it to put a 2D marker on a 3D object.
    fn project_vertex(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let viewproj = crate::render::mat4mult(game.renderer.get_view_matrix(), game.renderer.get_proj_matrix());
        let out = transform_coord((a(args, 0), a(args, 1), a(args, 2)), &viewproj);
        VERTEX.set(out);
        ok_num(out.0)
    }

    /// Where the camera is: the translation of the inverted view matrix. Every view matrix this
    /// engine builds (`d3d_set_projection`, `set_projection_ortho`'s own) is a rotation plus a
    /// translation, so the inverse is the transpose applied to the negated translation rather than
    /// a general 4x4 inversion.
    fn eye_position(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        let view = game.renderer.get_view_matrix();
        let m = |i: usize| f64::from(view[i]);
        let (tx, ty, tz) = (m(12), m(13), m(14));
        let out = (
            -(tx * m(0) + ty * m(1) + tz * m(2)),
            -(tx * m(4) + ty * m(5) + tz * m(6)),
            -(tx * m(8) + ty * m(9) + tz * m(10)),
        );
        VERTEX.set(out);
        ok_num(out.0)
    }

    fn vertex_y(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(VERTEX.get().1)
    }

    fn vertex_z(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(VERTEX.get().2)
    }

    /// Sets the world matrix from sixteen floats in a buffer, for a game that keeps matrices as
    /// buffer data rather than as sixteen GML variables.
    fn set_matrix_from_buffer(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        if let Some(bytes) = super::super::gm82buf::read_at(a(args, 0), 64) {
            if bytes.len() == 64 {
                let mut m = [0.0f32; 16];
                for (i, slot) in m.iter_mut().enumerate() {
                    *slot = f32::from_le_bytes([bytes[i * 4], bytes[i * 4 + 1], bytes[i * 4 + 2], bytes[i * 4 + 3]]);
                }
                game.renderer.set_model_matrix(m);
            }
        }
        ok_num(0.0)
    }

    // -- render state ----------------------------------------------------------------------------

    fn set_color_mask(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        game.renderer.set_colour_mask(ab(args, 0), ab(args, 1), ab(args, 2), ab(args, 3));
        ok_num(0.0)
    }

    /// `cull_none` (0), `cull_clockwise` (1), `cull_counterclockwise` (2). The thresholds are the
    /// extension's own `>= 0.5` / `>= 1.5` ladder rather than an equality test.
    fn set_cull_mode(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let mode = a(args, 0);
        game.renderer.set_culling(mode >= 0.5);
        game.renderer.set_cull_clockwise(mode >= 0.5 && mode < 1.5);
        ok_num(0.0)
    }

    fn set_zcomp_mode(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        game.renderer.set_depth_func(DepthFunc::from(ai(args, 0)));
        ok_num(0.0)
    }

    /// `texture_set_mode(mode)` -- `SetTextureStageState(0, D3DTSS_TEXCOORDINDEX, mode)`, verbatim.
    ///
    /// This is how a GM8.2 game environment-maps a model, and it is the *only* way to texture one
    /// that `d3d_model_save` wrote as `vertex_normal` commands: those store no texture coordinates
    /// at all, so every vertex of such a model arrives here holding (0, 0) and samples one texel
    /// until a generator supplies the rest.
    fn texture_set_mode(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        game.renderer.set_texcoord_source(TexcoordSource::from_d3d_tci(ai(args, 0)));
        ok_num(0.0)
    }

    fn set_zbias(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        game.renderer.set_depth_bias(a(args, 0));
        ok_num(0.0)
    }

    /// `d3d_set_depth_scale(znear, zfar)` -- the depth range drawn fragments are squeezed into, so
    /// a game can reserve part of the buffer for a HUD or a weapon model.
    fn set_zscale(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        game.renderer.set_depth_range(a(args, 0), a(args, 1));
        ok_num(0.0)
    }

    /// GM's `bm_*` blend constants are D3D's `D3DBLEND_*` values, which is what arrives here.
    fn blend_type(v: f64) -> BlendType {
        match v as i32 {
            1 => BlendType::Zero,
            2 => BlendType::One,
            3 => BlendType::SrcColour,
            4 => BlendType::InvSrcColour,
            5 => BlendType::SrcAlpha,
            6 => BlendType::InvSrcAlpha,
            7 => BlendType::DestAlpha,
            8 => BlendType::InvDestAlpha,
            9 => BlendType::DestColour,
            10 => BlendType::InvDestColour,
            11 => BlendType::SrcAlphaSaturate,
            _ => BlendType::One,
        }
    }

    /// `draw_set_blend_alphamode(enable, src, dest)` -- blends the alpha channel with its own
    /// factors. The idiom it exists for is drawing onto a transparent surface without the usual
    /// `bm_inv_src_alpha` eating the alpha already there.
    fn set_sep_alpha_blend(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let modes = if ab(args, 0) { Some((blend_type(a(args, 1)), blend_type(a(args, 2)))) } else { None };
        game.renderer.set_blend_mode_alpha(modes);
        ok_num(0.0)
    }

    /// `d3d_set_fog_ext(type, colour, start, end)`. `fog_none` (0) disables; `fog_table` (1) and
    /// `fog_vertex` (2) both become this renderer's linear fog, which is per-fragment -- the
    /// vertex-fog distinction is a D3D fixed-function detail with no equivalent in a shader that
    /// always has the fragment's depth to hand, and the two differ only in how coarse the result is.
    fn set_range_fog(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let fog = if a(args, 0) > 0.0 {
            Some(crate::render::Fog { colour: ai(args, 1), begin: a(args, 2) as f32, end: a(args, 3) as f32 })
        } else {
            None
        };
        game.renderer.set_fog(fog);
        ok_num(0.0)
    }

    // Fog readback. The renderer forgets a disabled fog's parameters (`set_fog(None)`), where the
    // device kept them, so a game that turns fog off and then reads its colour back gets 0 instead
    // of what it last set. Games read these to save and restore fog around an effect, and that
    // pattern reads them while fog is on.

    fn get_fog_enabled(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(u8::from(game.renderer.get_fog().is_some())))
    }

    fn get_fog_color(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(game.renderer.get_fog().map_or(0.0, |f| f64::from(f.colour)))
    }

    fn get_fog_start(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(game.renderer.get_fog().map_or(0.0, |f| f64::from(f.begin)))
    }

    fn get_fog_end(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(game.renderer.get_fog().map_or(0.0, |f| f64::from(f.end)))
    }

    fn get_lighting(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(u8::from(game.renderer.get_lighting_enabled())))
    }

    fn light_get_ambient(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(game.renderer.get_ambient_colour()))
    }

    fn light_enabled(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let index = ai(args, 0);
        let lights = game.renderer.get_lights();
        ok_num(match usize::try_from(index).ok().and_then(|i| lights.get(i)) {
            Some((enabled, _)) => f64::from(u8::from(*enabled)),
            None => 0.0,
        })
    }

    /// `d3d_light_define_ext(index, type, x,y,z, dx,dy,dz, range, inner, outer, colour, factor,
    /// specular, specular_factor)` -- GM8's two light functions merged, plus spotlights, a diffuse
    /// scaling factor and specular highlights.
    ///
    /// Point (`lt_point`) and directional (`lt_sun`) lights map exactly onto what the renderer
    /// already does for `d3d_light_define_point`/`_direction`, with the colour pre-scaled by the
    /// factor since there is nowhere else to carry it. A spotlight becomes a point light: the cone
    /// is a per-fragment term this renderer's lighting has no uniform for, and dropping the light
    /// entirely would leave the scene unlit rather than merely uncooler.
    fn set_light(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let index = ai(args, 0);
        let kind = ai(args, 1);
        let range = a(args, 8);
        // The original refuses a non-directional light with no range, rather than dividing by it.
        if range <= 0.0 && kind != 3 {
            return ok_num(0.0)
        }
        let colour = gm_colour(a(args, 11));
        let factor = a(args, 12);
        let scale = |shift: u32| {
            let channel = f64::from((colour >> shift) & 0xff) * factor;
            (channel.clamp(0.0, 255.0) as u32) << shift
        };
        let colour = (scale(0) | scale(8) | scale(16)) as i32;
        let light = if kind == 3 {
            Light::Directional { direction: [a(args, 5) as f32, a(args, 6) as f32, a(args, 7) as f32], colour }
        } else {
            Light::Point {
                position: [a(args, 2) as f32, a(args, 3) as f32, a(args, 4) as f32],
                range: range as f32,
                colour,
            }
        };
        if let Ok(index) = usize::try_from(index) {
            if index < 8 {
                game.renderer.set_light(index, light);
                game.renderer.set_light_enabled(index, true);
            }
        }
        ok_num(0.0)
    }

    fn clear_depth(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        game.renderer.clear_zbuf();
        ok_num(1.0)
    }

    fn set_viewport(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        game.renderer.set_viewport(ai(args, 0), ai(args, 1), ai(args, 2), ai(args, 3));
        ok_num(0.0)
    }

    macro_rules! viewport_thunk {
        ($field:tt) => {
            |game: &mut Game, _: &mut Context, _: &[Value]| -> gml::Result<Value> {
                ok_num(f64::from(game.renderer.get_viewport().$field))
            }
        };
    }

    fn texture_max_size(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(game.renderer.max_texture_size()))
    }

    fn texture_get_interpolation(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(u8::from(game.renderer.get_pixel_interpolation())))
    }

    // -- surfaces --------------------------------------------------------------------------------

    /// How many surface slots exist, freed ones included -- games walk surfaces with
    /// `repeat (surface_get_count()) if (surface_exists(i))`, so this has to count holes.
    fn surface_get_count(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(game.surfaces.slots()))
    }

    /// Surface pixels are `D3DFMT_A8R8G8B8`, which in memory is B, G, R, A -- the order
    /// `argb_get_color` unpacks and `argb_make_color` builds. The renderer works in RGBA, so the
    /// two conversions are one byte swap.
    fn rgba_to_argb(mut data: Vec<u8>) -> Vec<u8> {
        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        data
    }

    fn surface_to_buffer(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (address, id, w, h) = (a(args, 0), ai(args, 1), ai(args, 2), ai(args, 3));
        let surf = match game.surfaces.get(id) {
            Some(surf) => surf,
            None => return ok_num(-1.0),
        };
        let atlas_ref = surf.atlas_ref;
        // Anything drawn into this surface but still queued has to land before it is read back.
        if game.surface_target == Some(id) {
            game.renderer.flush_queue();
        }
        let pixels = game.renderer.dump_sprite_part(atlas_ref, 0, 0, w, h);
        let ok = super::super::gm82buf::write_at(address, &rgba_to_argb(pixels.into_vec()));
        ok_num(if ok { 0.0 } else { -1.0 })
    }

    fn buffer_to_surface(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (address, id, w, h) = (a(args, 0), ai(args, 1), ai(args, 2), ai(args, 3));
        if w <= 0 || h <= 0 {
            return ok_num(-1.0)
        }
        let dest = match game.surfaces.get(id) {
            Some(surf) => surf.atlas_ref,
            None => return ok_num(-1.0),
        };
        let data = match super::super::gm82buf::read_at(address, (w * h * 4) as usize) {
            Some(data) if data.len() == (w * h * 4) as usize => rgba_to_argb(data),
            _ => return ok_num(-1.0),
        };
        // There is no "upload into an existing texture" in the renderer, so the pixels go into a
        // throwaway sprite that is copied over the surface and dropped again.
        let scratch = match game.renderer.upload_sprite(data.into_boxed_slice(), w, h, 0, 0) {
            Ok(atlas_ref) => atlas_ref,
            Err(_) => return ok_num(-1.0),
        };
        game.renderer.copy_surface(dest, 0, 0, scratch, 0, 0, w, h);
        game.renderer.delete_sprite(scratch);
        ok_num(0.0)
    }

    /// `ds_grid_get_surface` -- a rectangle of pixels as the hex string `ds_grid_read` takes, since
    /// GM8 has no other way to hand a data structure a block of values at once. Layout, from
    /// `kernel.rs`'s own `ds_grid_write`: the id 601, width and height as little-endian u32s, then
    /// one 16-byte cell per value with the double at offset 4, column by column.
    fn surface_to_grid(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (id, x, y) = (ai(args, 0), ai(args, 1).max(0), ai(args, 2).max(0));
        // Unlike `buffer_get_surface`, the rectangle here is the game's own, so it has to be held
        // inside the surface: the renderer packs surfaces into shared atlases, and reading past one
        // would quietly return a neighbour's pixels.
        let (atlas_ref, w, h) = match game.surfaces.get(id) {
            Some(surf) => {
                (surf.atlas_ref, ai(args, 3).min(surf.width as i32 - x), ai(args, 4).min(surf.height as i32 - y))
            },
            None => return Ok(Value::from("")),
        };
        if w <= 0 || h <= 0 {
            return Ok(Value::from(""))
        }
        if game.surface_target == Some(id) {
            game.renderer.flush_queue();
        }
        let pixels = game.renderer.dump_sprite_part(atlas_ref, x, y, w, h);
        let mut out = Vec::with_capacity(12 + (w * h * 16) as usize);
        out.extend_from_slice(&601u32.to_le_bytes());
        out.extend_from_slice(&(w as u32).to_le_bytes());
        out.extend_from_slice(&(h as u32).to_le_bytes());
        for col in 0..w {
            for row in 0..h {
                let i = ((row * w + col) * 4) as usize;
                let argb = match pixels.get(i..i + 4) {
                    Some(px) => u32::from_le_bytes([px[2], px[1], px[0], px[3]]),
                    None => 0,
                };
                out.extend_from_slice(&[0; 4]);
                out.extend_from_slice(&f64::from(argb).to_le_bytes());
                out.extend_from_slice(&[0; 4]);
            }
        }
        Ok(Value::from(hex::encode_upper(out)))
    }

    // -- vertex and index buffers ----------------------------------------------------------------

    #[derive(Default, Clone)]
    struct VertexFormat {
        /// (type, usage, stream, offset within that stream) per element, in declaration order.
        elements: Vec<(i32, i32, usize, usize)>,
        /// Bytes per vertex in each of the 16 streams -- `vertex_format_get_size(format, slot)`,
        /// which is also the stride a buffer built for that slot is read with.
        sizes: [usize; 16],
    }

    struct VertexBuffer {
        data: Vec<u8>,
        stride: usize,
    }

    struct IndexBuffer {
        data: Vec<u8>,
        /// `ib_format_16` (101) or `ib_format_32` (102), which are D3D's `D3DFMT_INDEX16/32`.
        format: i32,
    }

    /// Bytes a vertex element occupies, by `vf_type_*`. Transcribed whole even though only the
    /// common four are decoded below, because every one of them shifts the offsets of the elements
    /// after it -- a wrong size here silently misreads the whole buffer.
    fn element_size(kind: i32) -> usize {
        match kind {
            0 | 4 | 5 | 6 | 8 | 9 | 11 | 13 | 14 | 15 => 4, // float1, d3dcolor, ubyte4, short2, ...
            1 | 7 | 10 | 12 | 16 => 8,                      // float2, short4, short4n, ushort4n, float16_4
            2 => 12,                                        // float3
            3 => 16,                                        // float4
            _ => 0,                                         // vf_type_unused, and anything unknown
        }
    }

    fn format_begin(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        FORMAT_BUILDING.with(|f| *f.borrow_mut() = VertexFormat::default());
        ok_num(0.0)
    }

    fn format_add_custom(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (kind, usage, slot) = (ai(args, 0), ai(args, 1), ai(args, 2));
        if !(0..16).contains(&slot) {
            return ok_num(0.0)
        }
        let slot = slot as usize;
        FORMAT_BUILDING.with(|f| {
            let mut f = f.borrow_mut();
            let offset = f.sizes[slot];
            f.elements.push((kind, usage, slot, offset));
            f.sizes[slot] += element_size(kind);
        });
        ok_num(0.0)
    }

    fn format_end(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        let format = FORMAT_BUILDING.with(|f| f.borrow().clone());
        ok_num(f64::from(VERTEX_FORMATS.with(|r| r.borrow_mut().insert(format))))
    }

    fn format_get_size(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (id, slot) = (ai(args, 0), ai(args, 1));
        if !(0..16).contains(&slot) {
            return ok_num(-1.0)
        }
        ok_num(VERTEX_FORMATS.with(|r| match r.borrow().items.get(&id) {
            Some(format) => format.sizes[slot as usize] as f64,
            None => -1.0,
        }))
    }

    fn format_delete(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = ai(args, 0);
        ok_num(f64::from(u8::from(VERTEX_FORMATS.with(|r| r.borrow_mut().items.remove(&id).is_none()))))
    }

    /// `vertex_buffer_create(buffer, format, slot)` -- the GML wrapper resolves the format's stride
    /// and passes it, along with the raw bytes to copy in.
    fn vertex_buffer_create(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (address, length, stride) = (a(args, 0), a(args, 1).max(0.0) as usize, a(args, 2).max(0.0) as usize);
        let data = match super::super::gm82buf::read_at(address, length) {
            Some(data) => data,
            None => return ok_num(-1.0),
        };
        ok_num(f64::from(VERTEX_BUFFERS.with(|r| r.borrow_mut().insert(VertexBuffer { data, stride }))))
    }

    fn vertex_buffer_get_size(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = ai(args, 0);
        ok_num(VERTEX_BUFFERS.with(|r| match r.borrow().items.get(&id) {
            Some(buffer) => buffer.data.len() as f64,
            None => -1.0,
        }))
    }

    fn vertex_buffer_delete(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = ai(args, 0);
        ok_num(f64::from(u8::from(VERTEX_BUFFERS.with(|r| r.borrow_mut().items.remove(&id).is_none()))))
    }

    fn index_buffer_create(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (address, length, format) = (a(args, 0), a(args, 1).max(0.0) as usize, ai(args, 2));
        let data = match super::super::gm82buf::read_at(address, length) {
            Some(data) => data,
            None => return ok_num(-1.0),
        };
        ok_num(f64::from(INDEX_BUFFERS.with(|r| r.borrow_mut().insert(IndexBuffer { data, format }))))
    }

    fn index_buffer_get_size(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = ai(args, 0);
        ok_num(INDEX_BUFFERS.with(|r| match r.borrow().items.get(&id) {
            Some(buffer) => buffer.data.len() as f64,
            None => -1.0,
        }))
    }

    fn index_buffer_get_format(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = ai(args, 0);
        ok_num(INDEX_BUFFERS.with(|r| match r.borrow().items.get(&id) {
            Some(buffer) => f64::from(buffer.format),
            None => -1.0,
        }))
    }

    fn index_buffer_set(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        INDEX_BOUND.set(ai(args, 0));
        ok_num(0.0)
    }

    fn index_buffer_delete(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = ai(args, 0);
        if INDEX_BOUND.get() == id {
            INDEX_BOUND.set(-1);
        }
        ok_num(f64::from(u8::from(INDEX_BUFFERS.with(|r| r.borrow_mut().items.remove(&id).is_none()))))
    }

    fn read_f32(data: &[u8], at: usize) -> f64 {
        match data.get(at..at + 4) {
            Some(b) => f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            None => 0.0,
        }
    }

    /// One vertex, decoded through a format: position, normal, texture coordinate and colour, which
    /// are the four the fixed-function pipeline draws with. Anything else a custom format declares
    /// (blend weights, tangents, a second texture coordinate) only means something to a shader, so
    /// it is skipped -- its size still counts towards the stride, so the rest stays aligned.
    fn decode_vertex(
        format: &VertexFormat,
        data: &[u8],
        base: usize,
    ) -> (f64, f64, f64, f64, f64, f64, f64, f64, i32, f64) {
        let (mut x, mut y, mut z) = (0.0, 0.0, 0.0);
        let (mut nx, mut ny, mut nz) = (0.0, 0.0, 0.0);
        let (mut u, mut v) = (0.0, 0.0);
        let (mut colour, mut alpha) = (0xffffff, 1.0);
        for &(kind, usage, stream, offset) in &format.elements {
            if stream != 0 {
                continue // one stream is what this renderer draws; see `vertex_bind_buffer`
            }
            let at = base + offset;
            match usage {
                // vf_usage_position, and the pre-transformed variant games use for 2D overlays
                0 | 9 => {
                    x = read_f32(data, at);
                    y = read_f32(data, at + 4);
                    z = read_f32(data, at + 8);
                },
                3 => {
                    nx = read_f32(data, at);
                    ny = read_f32(data, at + 4);
                    nz = read_f32(data, at + 8);
                },
                5 => {
                    u = read_f32(data, at);
                    v = read_f32(data, at + 4);
                },
                10 if kind == 4 => {
                    // vf_type_d3dcolor: 0xAARRGGBB, so B, G, R, A in memory. GM wants 0xBBGGRR.
                    if let Some(px) = data.get(at..at + 4) {
                        colour = i32::from(px[0]) << 16 | i32::from(px[1]) << 8 | i32::from(px[2]);
                        alpha = f64::from(px[3]) / 255.0;
                    }
                },
                _ => (),
            }
        }
        (x, y, z, nx, ny, nz, u, v, colour, alpha)
    }

    /// `vertex_buffer_bind(buffer, slot)` -- `SetStreamSource(slot, buffer)`, with a negative id
    /// unbinding. Stream 0 is set by the draw itself, so in practice this is how the *extra* streams
    /// of an instanced draw arrive.
    fn vertex_bind_buffer(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (buffer_id, slot) = (ai(args, 0), ai(args, 1).clamp(0, 15) as usize);
        let mut sources = STREAM_SOURCES.get();
        sources[slot] = buffer_id;
        STREAM_SOURCES.set(sources);
        ok_num(0.0)
    }

    /// `__gm82dx9_vertex_set_instance_count(count)` -- how many instances the next draw makes, i.e.
    /// `SetStreamSourceFreq(0, D3DSTREAMSOURCE_INDEXEDDATA | count)`.
    ///
    /// This is how the shadow-volume pass works: a thirteen-vertex unit quad in stream 0, one
    /// block's transform per entry in stream 1. Ignoring it drew the template once, untransformed
    /// and in raw model space, and the lights it feeds cast no shadows at all.
    fn vertex_set_instance_count(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        INSTANCE_COUNT.set(ai(args, 0).max(0) as usize);
        ok_num(0.0)
    }

    /// `__gm82dx9_vertex_set_instances_per_vertex(slot, rate)` --
    /// `SetStreamSourceFreq(slot, D3DSTREAMSOURCE_INSTANCEDATA | rate)`. The rate is how many
    /// instances share one element of that stream, which is GL's attribute divisor exactly.
    fn vertex_set_instances_per_vertex(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (slot, rate) = (ai(args, 0).clamp(0, 15) as usize, ai(args, 1).max(0) as u32);
        let mut divisors = STREAM_DIVISOR.get();
        divisors[slot] = rate;
        STREAM_DIVISOR.set(divisors);
        ok_num(0.0)
    }

    /// `vertex_instance_reset()` -- `SetStreamSourceFreq(i, 1)` for all sixteen streams.
    ///
    /// That resets the *frequencies* only: a buffer stays bound to its stream, which is state a game
    /// leaves behind, as a probe on the real runner confirmed.
    fn vertex_instance_reset(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        INSTANCE_COUNT.set(1);
        STREAM_DIVISOR.set([0; 16]);
        ok_num(0.0)
    }

    /// Draws the buffer through the game's own shader, without decoding it -- the path a vertex
    /// buffer exists for. Returns whether it happened; `false` means no shader is set, and the
    /// caller falls back to unpacking.
    fn vertex_draw_buffer_shaded(
        game: &mut Game,
        format: &VertexFormat,
        vbuf_id: i32,
        primitive: i32,
        texture: Option<crate::render::atlas::AtlasRef>,
        indexed: bool,
    ) -> bool {
        use crate::render::{IndexData, VertexStream};

        let (sources, divisors, instance_count) = (STREAM_SOURCES.get(), STREAM_DIVISOR.get(), INSTANCE_COUNT.get());
        // Elements grouped by the stream they read from, since that is how GL wants them, and
        // numbered per usage in declaration order -- which is how D3D assigns the usage index the
        // shader knows them by. The format itself does not store one: `vertex_format_add_position`
        // takes the *stream*, and a second position is simply the second one added.
        let mut usage_counts = std::collections::HashMap::new();
        let numbered: Vec<(i32, i32, u8, usize, usize)> = format
            .elements
            .iter()
            .map(|&(kind, usage, slot, offset)| {
                let counter = usage_counts.entry(usage).or_insert(0u8);
                let index = *counter;
                *counter += 1;
                (kind, usage, index, slot, offset)
            })
            .collect();
        let elements_for = |slot: usize| -> Vec<(i32, i32, u8, usize)> {
            numbered.iter().filter(|(_, _, _, s, _)| *s == slot).map(|(k, u, i, _, o)| (*k, *u, *i, *o)).collect()
        };

        VERTEX_BUFFERS.with(|vb| {
            let buffers = vb.borrow();
            let Some(vertex_buffer) = buffers.items.get(&vbuf_id).filter(|b| b.stride != 0) else { return false };
            // Slot 0 is whatever this draw was handed -- `__gm82dx9_vertex_draw_buffer` sets that
            // stream source itself -- and every other slot comes from `vertex_bind_buffer`. A slot
            // the format declares nothing for has nothing to feed, so it is left out.
            let mut extra: Vec<(usize, Vec<(i32, i32, u8, usize)>)> = Vec::new();
            for (slot, &buffer_id) in sources.iter().enumerate().skip(1) {
                let elements = elements_for(slot);
                if buffer_id >= 0 && !elements.is_empty() {
                    extra.push((slot, elements));
                }
            }

            let vertex_elements = elements_for(0);
            let mut streams = vec![VertexStream {
                data: &vertex_buffer.data,
                stride: vertex_buffer.stride,
                elements: &vertex_elements,
                divisor: 0,
            }];
            for (slot, elements) in &extra {
                let Some(buffer) = buffers.items.get(&sources[*slot]).filter(|b| b.stride != 0) else { continue };
                streams.push(VertexStream {
                    data: &buffer.data,
                    stride: buffer.stride,
                    elements,
                    divisor: divisors[*slot],
                });
            }
            let instance_count = instance_count.max(1);

            INDEX_BUFFERS.with(|ib| {
                let index_buffers = ib.borrow();
                let indices = if indexed {
                    match index_buffers.items.get(&INDEX_BOUND.get()) {
                        Some(buffer) => Some(IndexData { data: &buffer.data, wide: buffer.format == 102 }),
                        // Told to draw indexed with nothing bound: nothing sensible to draw, and
                        // certainly not the whole buffer in order.
                        None => return true,
                    }
                } else {
                    None
                };
                game.renderer.draw_vertex_streams(
                    &streams,
                    indices,
                    PrimitiveType::from(primitive),
                    vertex_buffer.data.len() / vertex_buffer.stride,
                    instance_count,
                    texture,
                )
            })
        })
    }

    /// `vertex_buffer_draw(vbuffer, vformat, primitive, texture, [ibuffer])` -- the point of the
    /// whole vertex-buffer half of the extension.
    ///
    /// With a shader set, the buffer goes to the GPU as it is and the shader reads it, which is
    /// both what D3D does and the only way a game's own vertex declaration means anything.
    ///
    /// Without one, it is decoded into the same per-vertex calls `d3d_vertex_normal_texture_color`
    /// makes, so buffered geometry goes through exactly the path immediate-mode geometry does. That
    /// gives up the speed a vertex buffer exists for and keeps the appearance, which is the right
    /// way round for a game that is drawing its level rather than benchmarking.
    fn vertex_draw_buffer(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (vbuf_id, format_id, primitive, texture_id) = (ai(args, 0), ai(args, 1), ai(args, 2), ai(args, 3));
        let (count, indexed) = (a(args, 4).max(0.0) as usize, ab(args, 5));
        let format = match VERTEX_FORMATS.with(|r| r.borrow().items.get(&format_id).cloned()) {
            Some(format) => format,
            None => return ok_num(1.0),
        };
        // An indexed draw walks the bound index buffer instead of the vertices in order.
        let indices: Option<Vec<usize>> = if indexed {
            let bound = INDEX_BOUND.get();
            match INDEX_BUFFERS.with(|r| {
                r.borrow().items.get(&bound).map(|buffer| {
                    if buffer.format == 102 {
                        buffer
                            .data
                            .chunks_exact(4)
                            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
                            .collect()
                    } else {
                        buffer.data.chunks_exact(2).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]]))).collect()
                    }
                })
            }) {
                Some(indices) => Some(indices),
                // Told to draw indexed with nothing bound: nothing sensible to draw.
                None => return ok_num(1.0),
            }
        } else {
            None
        };

        let texture = game.renderer.get_texture_from_id(texture_id);
        if vertex_draw_buffer_shaded(game, &format, vbuf_id, primitive, texture, indexed) {
            return ok_num(1.0)
        }
        VERTEX_BUFFERS.with(|r| {
            let buffers = r.borrow();
            let buffer = match buffers.items.get(&vbuf_id) {
                Some(buffer) if buffer.stride != 0 => buffer,
                _ => return ok_num(1.0),
            };
            let vertices = buffer.data.len() / buffer.stride;
            game.renderer.reset_primitive_3d(PrimitiveType::from(primitive), texture);
            // `count` comes from the GML wrapper, which derives it from the buffer's own size, but
            // it is still the game's number: clamp rather than read off the end.
            let order: Box<dyn Iterator<Item = usize>> = match indices {
                Some(indices) => Box::new(indices.into_iter().take(count)),
                None => Box::new(0..count.min(vertices)),
            };
            for index in order {
                if index >= vertices {
                    continue
                }
                let (x, y, z, nx, ny, nz, u, v, colour, alpha) =
                    decode_vertex(&format, &buffer.data, index * buffer.stride);
                game.renderer.vertex_3d(x, y, z, nx, ny, nz, u, v, colour, alpha);
            }
            game.renderer.draw_primitive_3d();
            ok_num(0.0)
        })
    }

    // -- shaders ---------------------------------------------------------------------------------
    //
    // `shader_pixel_create_file` takes *compiled Direct3D 9 bytecode* -- `fxc` output, shader model
    // 3 -- so these run through `sm3`, which translates it to GLSL ES 3.00 at load time. The crate
    // has its own tests and its own account of the translation.
    //
    // The constant table that comes back with the GLSL is what makes the uniform API answerable.
    // GM8.2 sets uniforms *by register*, and a game gets the register from
    // `shader_*_uniform_get_address(shader, "lightpos")` -- so without the names the shader ships
    // with, there is nothing to resolve that call against.

    struct LoadedShader {
        stage: ShaderStage,
        handle: ShaderRef,
        constants: sm3::ConstantTable,
    }

    thread_local! {
        static SHADERS: RefCell<Registry<LoadedShader>> = RefCell::new(Registry::new());
        /// The shader each stage currently has set, as a GML id. Kept here as well as in the
        /// renderer because the uniform calls resolve names against the *set* shader, and the
        /// renderer has no idea what a name is.
        static SET_SHADERS: RefCell<[i32; 2]> = const { RefCell::new([-1, -1]) };
        /// Whether *any* shader has failed to load in this run. Only ever read by
        /// `uniform_address`, to decide which of two right answers to give -- see there.
        static SHADER_LOAD_FAILED: Cell<bool> = const { Cell::new(false) };
    }

    fn stage_index(stage: ShaderStage) -> usize {
        match stage {
            ShaderStage::Vertex => 0,
            ShaderStage::Pixel => 1,
        }
    }

    /// Compiles bytecode and registers it, or answers `-1` -- the extension's own not-a-shader
    /// value. Everything downstream depends on that being truthful: a game checks it, and
    /// `shader_set` on `-1` leaves the fixed-function pipeline alone.
    fn create_shader(game: &mut Game, stage: ShaderStage, bytecode: &[u8]) -> gml::Result<Value> {
        let shader = match sm3::translate(bytecode) {
            Ok(shader) => shader,
            Err(e) => {
                // Say what arrived, not just that it was wrong. The bytes reach here through a
                // base64 decode and a zlib inflate (that is what `shader_*_create_base64` does), so
                // "not bytecode" is as likely to mean one of those left the buffer alone as it is
                // to mean the shader itself is unusual -- and the first eight bytes tell the two
                // apart at a glance.
                let head: Vec<String> = bytecode.iter().take(8).map(|b| format!("{:02x}", b)).collect();
                web_sys::console::warn_1(
                    &format!(
                        "gm82dx9: shader could not be translated: {} ({} bytes, starting {})",
                        e,
                        bytecode.len(),
                        head.join(" ")
                    )
                    .into(),
                );
                SHADER_LOAD_FAILED.with(|f| f.set(true));
                return ok_num(-1.0)
            },
        };
        let handle = match game.renderer.create_shader(stage, &shader.glsl) {
            Ok(handle) => handle,
            Err(log) => {
                // The compile log is the only thing worth having when a translation turns out not
                // to be valid GLSL after all -- there is no GLSL compiler on the machine `sm3` was
                // written on, so this is where that finally gets checked.
                web_sys::console::warn_1(&format!("gm82dx9: translated shader did not compile: {}", log).into());
                SHADER_LOAD_FAILED.with(|f| f.set(true));
                return ok_num(-1.0)
            },
        };
        let id = SHADERS.with(|r| r.borrow_mut().insert(LoadedShader { stage, handle, constants: shader.constants }));
        ok_num(f64::from(id))
    }

    fn shader_create_file(game: &mut Game, stage: ShaderStage, args: &[Value]) -> gml::Result<Value> {
        let path = args.first().map(arg_str).unwrap_or_default().into_owned();
        match crate::gml::file::read_file_bytes(&path) {
            Some(bytes) => create_shader(game, stage, &bytes),
            None => {
                web_sys::console::warn_1(&format!("gm82dx9: no shader file at {}", path).into());
                ok_num(-1.0)
            },
        }
    }

    /// `__gm82dx9_shader_*_create_buffer(address, size)` -- the GML wrapper has already handed over
    /// the buffer's address, which is how `shader_*_create_base64` arrives here too (it decodes and
    /// inflates into a buffer first).
    fn shader_create_buffer(game: &mut Game, stage: ShaderStage, args: &[Value]) -> gml::Result<Value> {
        let (address, length) = (a(args, 0), a(args, 1).max(0.0) as usize);
        match super::super::gm82buf::read_at(address, length) {
            Some(bytes) => create_shader(game, stage, &bytes),
            None => ok_num(-1.0),
        }
    }

    fn shader_destroy(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = ai(args, 0);
        if let Some(shader) = SHADERS.with(|r| r.borrow_mut().items.remove(&id)) {
            game.renderer.delete_shader(shader.handle);
            SET_SHADERS.with(|s| {
                let mut set = s.borrow_mut();
                if set[stage_index(shader.stage)] == id {
                    set[stage_index(shader.stage)] = -1;
                }
            });
        }
        ok_num(0.0)
    }

    fn shader_set_stage(game: &mut Game, stage: ShaderStage, id: i32) {
        let handle = SHADERS
            .with(|r| r.borrow().items.get(&id).filter(|shader| shader.stage == stage).map(|shader| shader.handle));
        SET_SHADERS.with(|s| s.borrow_mut()[stage_index(stage)] = if handle.is_some() { id } else { -1 });
        game.renderer.set_shader_stage(stage, handle);
    }

    /// The register a named uniform starts at in the *set* shader.
    ///
    /// The answer for "not found" depends on *why*, and getting that wrong stops the game dead.
    /// `shader_*_uniform_get_address` pops a `show_error` on `noone`, so:
    ///
    /// - **A shader is set and genuinely has no such uniform** -> `noone`. That is a real mistake
    ///   in the game and the dialog is the right outcome; it is also what the real DLL does.
    /// - **No shader is set at all** -> `noone` as well. Measured, rather than assumed: the probe
    ///   suite asks the real runner with nothing set and `shader_pixel_uniform_exists` answers
    ///   *false*, so the DLL returns `noone` here too. Answering `0` would make
    ///   `_exists` answer **true** for a uniform that cannot exist -- and a game that guards a
    ///   write with it (`shader_draw_shadertoy` does, five times) would then write to register 0
    ///   and scribble over whatever `c0` means.
    /// - **...unless a shader has already failed to load**, in which case `0` again. That case
    ///   cannot arise on the real runner, only here, and answering `noone` for it turns a
    ///   rendering gap into a game that stops at a message box during room load. That is not
    ///   hypothetical -- it is what happened the first time these functions became real. The
    ///   console already carries the reason the shader was refused.
    fn uniform_address(stage: ShaderStage, args: &[Value], want_size: bool) -> gml::Result<Value> {
        let name = args.first().map(arg_str).unwrap_or_default().into_owned();
        let id = SET_SHADERS.with(|s| s.borrow()[stage_index(stage)]);
        let (shader_set, found) = SHADERS.with(|r| {
            let registry = r.borrow();
            match registry.items.get(&id) {
                Some(shader) => (
                    true,
                    shader.constants.find(&name).map(|c| if want_size { c.register_count } else { c.register_index }),
                ),
                None => (false, None),
            }
        });
        match found {
            Some(value) => ok_num(f64::from(value)),
            None if want_size => ok_num(0.0),
            None if !shader_set && SHADER_LOAD_FAILED.with(|f| f.get()) => ok_num(0.0),
            None => ok_num(f64::from(gml::NOONE)),
        }
    }

    /// `__gm82dx9_shader_*_uniform_4f(register, x, y, z, w)`. The GML wrapper splits however many
    /// values a game passed into groups of four and advances the register itself, so by the time it
    /// reaches here it is always exactly one register.
    fn uniform_4f(game: &mut Game, stage: ShaderStage, args: &[Value]) -> gml::Result<Value> {
        let register = ai(args, 0).max(0) as u32;
        let value = [a(args, 1) as f32, a(args, 2) as f32, a(args, 3) as f32, a(args, 4) as f32];
        game.renderer.set_shader_constant(stage, register, value);
        ok_num(0.0)
    }

    /// `__gm82dx9_shader_*_uniform_matrix(register, which, size)`, where `which` is a bitmask of
    /// GM8.2's `mtx_world` (1), `mtx_view` (2) and `mtx_projection` (4) -- so `mtx_world_view` is 3
    /// and `mtx_world_view_projection` is 7.
    ///
    /// Each register takes four consecutive floats of the matrix, which is the layout the
    /// translated GLSL reads: `fxc` expands a transform into `c0*x + c1*y + c2*z + c3*w`, so
    /// register `i` is column `i` -- exactly how GL stores a `mat4`, and therefore a straight copy.
    fn uniform_matrix(game: &mut Game, stage: ShaderStage, args: &[Value]) -> gml::Result<Value> {
        let register = ai(args, 0).max(0) as u32;
        let which = ai(args, 1);
        let size = ai(args, 2).clamp(0, 4) as usize;

        let mut matrix = if which & 1 != 0 { game.renderer.get_model_matrix() } else { IDENTITY_MATRIX };
        // The projection half is taken from the renderer's own combined matrix rather than rebuilt,
        // so a custom shader lands in the same space as everything drawn without one -- it carries
        // the vertical flip this renderer folds in for GL, which a raw view/projection pair does
        // not. See `RendererTrait::get_viewproj_matrix`.
        match (which & 2 != 0, which & 4 != 0) {
            (true, true) => matrix = crate::render::mat4mult(matrix, game.renderer.get_viewproj_matrix()),
            (true, false) => matrix = crate::render::mat4mult(matrix, game.renderer.get_view_matrix()),
            (false, true) => matrix = crate::render::mat4mult(matrix, game.renderer.get_proj_matrix()),
            (false, false) => (),
        }

        for row in 0..size.min(4) {
            let base = row * 4;
            game.renderer.set_shader_constant(stage, register + row as u32, [
                matrix[base],
                matrix[base + 1],
                matrix[base + 2],
                matrix[base + 3],
            ]);
        }
        ok_num(0.0)
    }

    const IDENTITY_MATRIX: [f32; 16] = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];

    /// `__gm82dx9_texture_stage_set(sampler, texture)` and its vertex twin. The GML wrapper
    /// resolves the sampler's *name* through the constant table first, so what arrives is the `s#`
    /// register number -- of whichever stage's register file, which is why `kind` is not guessable
    /// from the number.
    fn texture_stage_set(game: &mut Game, kind: ShaderStage, args: &[Value]) -> gml::Result<Value> {
        let (stage, texture) = (ai(args, 0), ai(args, 1));
        // `-1` is GM's "no texture", and any negative register is a name that did not resolve.
        if stage < 0 {
            return ok_num(0.0)
        }
        let atlas_ref = if texture < 0 { None } else { game.renderer.get_texture_from_id(texture) };
        game.renderer.set_shader_sampler(kind, stage as u32, atlas_ref);
        ok_num(0.0)
    }

    /// `__gm82dx9_texture_set_stage_interpolation(sampler, on)`.
    fn texture_stage_interpolation(game: &mut Game, kind: ShaderStage, args: &[Value]) -> gml::Result<Value> {
        let stage = ai(args, 0);
        if stage >= 0 {
            game.renderer.set_shader_sampler_state(kind, stage as u32, Some(ab(args, 1)), None);
        }
        ok_num(0.0)
    }

    /// `__gm82dx9_texture_set_stage_repeat(sampler, hrepeat, vrepeat, bordercolor)`.
    ///
    /// The border colour is dropped: it only means anything with D3D's `BORDER` addressing mode,
    /// which GLES has no equivalent for at all. Clamping instead is what every GL translation of
    /// this does, and is what a game turning repeat *off* is asking for anyway.
    fn texture_stage_repeat(game: &mut Game, kind: ShaderStage, args: &[Value]) -> gml::Result<Value> {
        let stage = ai(args, 0);
        if stage >= 0 {
            let repeat = (ab(args, 1), ab(args, 2));
            game.renderer.set_shader_sampler_state(kind, stage as u32, None, Some(repeat));
        }
        ok_num(0.0)
    }

    // -- deliberately inert ----------------------------------------------------------------------

    fn shader_noop(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(0.0)
    }

    /// Resizing the D3D backbuffer, and switching to exclusive fullscreen. Both reach into the GM8
    /// runner; here the canvas is sized by the page, and the browser's own fullscreen (which the
    /// page already wires up) is the only kind there is.
    fn device_noop(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(0.0)
    }

    // -- resolve ---------------------------------------------------------------------------------

    /// Extension functions carry *two* names: the one GML calls them by, and the `extname` that is
    /// the DLL's actual exported symbol. `game.rs` defines the external under the `extname`
    /// whenever one is set, so that -- not the GML name -- is what arrives here. They differ for 31
    /// of gm82dx9's functions (`d3d_set_cull_mode` exports as `__gm82dx9_set_cull_mode`,
    /// `vertex_buffer_delete` as `vertex_delete_buffer`), and matching only the GML name falls
    /// silently through to `external.rs`'s generic no-op.
    ///
    /// Both spellings are accepted per function: the extname is what actually resolves today, and
    /// the GML name costs nothing.
    pub fn resolve(symbol: &str) -> Option<Function> {
        let f: super::ContextFn = match symbol {
            // handshake and device
            "__gm82dx9_dllcheck" => dllcheck,
            "__gm82dx9_checkstart" => checkstart,
            "__gm82dx9_abort" => abort,
            "__gm82dx9_testfunc" => shader_noop,
            "d3d_get_free_video_memory" | "__gm82dx9_getvideomem" => free_video_memory,
            "__gm82dx9_surface_ptr" => surface_ptr,
            "__gm82dx9_resize_backbuffer" | "__gm82dx9_setfullscreen" | "__gm82dx9_setalphabuffer" => device_noop,

            // colour packing
            "argb_get_color" => argb_get_color,
            "argb_get_alpha" => argb_get_alpha,
            "argb_make_color" => argb_make_color,
            "color_to_d3dcolor" => color_to_d3dcolor,

            // matrices
            "d3d_transform_set_matrix" => transform_set_matrix,
            "__gm82dx9_set_matrix_from_buffer" => set_matrix_from_buffer,
            "d3d_transform_add_stack_top" => transform_add_stack_top,
            "d3d_transform_get_determinant" => transform_get_determinant,
            "d3d_projection_get_determinant" => projection_get_determinant,
            "__gm82dx9_transformvertex" => transform_vertex,
            "__gm82dx9_projectvertex" => project_vertex,
            "__gm82dx9_geteyepos" => eye_position,
            "__gm82dx9_getvertexy" => vertex_y,
            "__gm82dx9_getvertexz" => vertex_z,

            // one rotation
            "d3d_transform_xt" => transform_thunk!("x", false),
            "d3d_transform_yt" => transform_thunk!("y", false),
            "d3d_transform_zt" => transform_thunk!("z", false),
            "d3d_transform_xst" => transform_thunk!("x", true),
            "d3d_transform_yst" => transform_thunk!("y", true),
            "d3d_transform_zst" => transform_thunk!("z", true),
            // two rotations
            "d3d_transform_xyt" => transform_thunk!("xy", false),
            "d3d_transform_xzt" => transform_thunk!("xz", false),
            "d3d_transform_yxt" => transform_thunk!("yx", false),
            "d3d_transform_yzt" => transform_thunk!("yz", false),
            "d3d_transform_zxt" => transform_thunk!("zx", false),
            "d3d_transform_zyt" => transform_thunk!("zy", false),
            "d3d_transform_xyst" => transform_thunk!("xy", true),
            "d3d_transform_xzst" => transform_thunk!("xz", true),
            "d3d_transform_yxst" => transform_thunk!("yx", true),
            "d3d_transform_yzst" => transform_thunk!("yz", true),
            "d3d_transform_zxst" => transform_thunk!("zx", true),
            "d3d_transform_zyst" => transform_thunk!("zy", true),
            // three rotations
            "d3d_transform_xyzt" => transform_thunk!("xyz", false),
            "d3d_transform_xzyt" => transform_thunk!("xzy", false),
            "d3d_transform_yxzt" => transform_thunk!("yxz", false),
            "d3d_transform_yzxt" => transform_thunk!("yzx", false),
            "d3d_transform_zxyt" => transform_thunk!("zxy", false),
            "d3d_transform_zyxt" => transform_thunk!("zyx", false),
            "d3d_transform_xyzst" => transform_thunk!("xyz", true),
            "d3d_transform_xzyst" => transform_thunk!("xzy", true),
            "d3d_transform_yxzst" => transform_thunk!("yxz", true),
            "d3d_transform_yzxst" => transform_thunk!("yzx", true),
            "d3d_transform_zxyst" => transform_thunk!("zxy", true),
            "d3d_transform_zyxst" => transform_thunk!("zyx", true),

            // render state
            "d3d_set_color_mask" | "draw_set_color_write_enable" | "__gm82dx9_set_color_mask" => set_color_mask,
            "d3d_set_cull_mode" | "__gm82dx9_set_cull_mode" => set_cull_mode,
            "d3d_set_zcomp_mode" | "__gm82dx9_set_zcomp_mode" => set_zcomp_mode,
            "d3d_set_zbias" | "__gm82dx9_set_zbias" => set_zbias,
            "texture_set_mode" | "__gm82dx9_texture_set_mode" => texture_set_mode,
            "d3d_set_depth_scale" | "__gm82dx9_setzscale" => set_zscale,
            "__gm82dx9_setsepalphablend" => set_sep_alpha_blend,
            "d3d_set_fog_ext" | "__gm82dx9_setrangefog" => set_range_fog,
            "d3d_get_fog_enabled" => get_fog_enabled,
            "d3d_get_fog_color" => get_fog_color,
            "d3d_get_fog_start" => get_fog_start,
            "d3d_get_fog_end" => get_fog_end,
            "d3d_get_lighting" => get_lighting,
            "d3d_light_get_ambient" => light_get_ambient,
            "d3d_light_enabled" => light_enabled,
            "d3d_light_define_ext" | "__gm82dx9_set_light" => set_light,
            "d3d_clear_depth_buffer" | "__gm82dx9_cleardepth" => clear_depth,
            "__gm82dx9_setviewport" => set_viewport,
            "d3d_get_viewport_x" | "__gm82dx9_getviewportx" => viewport_thunk!(0),
            "d3d_get_viewport_y" | "__gm82dx9_getviewporty" => viewport_thunk!(1),
            "d3d_get_viewport_w" | "__gm82dx9_getviewportw" => viewport_thunk!(2),
            "d3d_get_viewport_h" | "__gm82dx9_getviewporth" => viewport_thunk!(3),
            "texture_get_max_width" | "__gm82dx9_getmaxwidth" => texture_max_size,
            "texture_get_max_height" | "__gm82dx9_getmaxheight" => texture_max_size,
            "texture_get_interpolation" => texture_get_interpolation,

            // surfaces
            "surface_get_count" => surface_get_count,
            "__gm82dx9_surface_to_buffer" => surface_to_buffer,
            "__gm82dx9_buffer_to_surface" => buffer_to_surface,
            "__gm82dx9_surface_to_grid" => surface_to_grid,

            // vertex and index buffers
            "vertex_format_begin" => format_begin,
            "vertex_format_add_custom" => format_add_custom,
            "vertex_format_end" => format_end,
            "vertex_format_get_size" => format_get_size,
            "vertex_format_delete" => format_delete,
            "__gm82dx9_vertex_create_buffer_from_buffer" => vertex_buffer_create,
            "vertex_buffer_get_size" => vertex_buffer_get_size,
            "vertex_buffer_delete" | "vertex_delete_buffer" => vertex_buffer_delete,
            "__gm82dx9_vertex_draw_buffer" => vertex_draw_buffer,
            "vertex_bind_buffer" | "__gm82dx9_vertex_bind_buffer" => vertex_bind_buffer,
            "vertex_set_instance_count" | "__gm82dx9_vertex_set_instance_count" => vertex_set_instance_count,
            "vertex_set_instances_per_vertex" | "__gm82dx9_vertex_set_instances_per_vertex" => {
                vertex_set_instances_per_vertex
            },
            "vertex_instance_reset" => vertex_instance_reset,
            "__gm82dx9_index_create_buffer_from_buffer" => index_buffer_create,
            "index_buffer_get_size" | "__gm82dx9_index_buffer_get_size" => index_buffer_get_size,
            "index_buffer_get_format" | "__gm82dx9_index_buffer_get_format" => index_buffer_get_format,
            "index_set_buffer" | "__gm82dx9_index_buffer_set" => index_buffer_set,
            "index_buffer_delete" | "index_delete_buffer" => index_buffer_delete,

            // Shaders. See the shader block above; these go through `sm3`.
            "shader_vertex_create_file" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| shader_create_file(g, ShaderStage::Vertex, a)
            },
            "shader_pixel_create_file" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| shader_create_file(g, ShaderStage::Pixel, a)
            },
            "__gm82dx9_shader_vertex_create_buffer" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| shader_create_buffer(g, ShaderStage::Vertex, a)
            },
            "__gm82dx9_shader_pixel_create_buffer" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| shader_create_buffer(g, ShaderStage::Pixel, a)
            },
            "shader_vertex_destroy" | "shader_pixel_destroy" => shader_destroy,
            "shader_vertex_set" => |g: &mut Game, _: &mut Context, a: &[Value]| {
                shader_set_stage(g, ShaderStage::Vertex, ai(a, 0));
                ok_num(0.0)
            },
            "shader_pixel_set" => |g: &mut Game, _: &mut Context, a: &[Value]| {
                shader_set_stage(g, ShaderStage::Pixel, ai(a, 0));
                ok_num(0.0)
            },
            "shader_vertex_reset" => |g: &mut Game, _: &mut Context, _: &[Value]| {
                shader_set_stage(g, ShaderStage::Vertex, -1);
                ok_num(0.0)
            },
            "shader_pixel_reset" => |g: &mut Game, _: &mut Context, _: &[Value]| {
                shader_set_stage(g, ShaderStage::Pixel, -1);
                ok_num(0.0)
            },
            "shader_set" => |g: &mut Game, _: &mut Context, a: &[Value]| {
                shader_set_stage(g, ShaderStage::Vertex, ai(a, 0));
                shader_set_stage(g, ShaderStage::Pixel, ai(a, 1));
                ok_num(0.0)
            },
            "shader_reset" => |g: &mut Game, _: &mut Context, _: &[Value]| {
                shader_set_stage(g, ShaderStage::Vertex, -1);
                shader_set_stage(g, ShaderStage::Pixel, -1);
                ok_num(0.0)
            },
            "__gm82dx9_get_vconst" => {
                |_: &mut Game, _: &mut Context, a: &[Value]| uniform_address(ShaderStage::Vertex, a, false)
            },
            "__gm82dx9_get_pconst" => {
                |_: &mut Game, _: &mut Context, a: &[Value]| uniform_address(ShaderStage::Pixel, a, false)
            },
            "__gm82dx9_get_vconst_size" => {
                |_: &mut Game, _: &mut Context, a: &[Value]| uniform_address(ShaderStage::Vertex, a, true)
            },
            "__gm82dx9_get_pconst_size" => {
                |_: &mut Game, _: &mut Context, a: &[Value]| uniform_address(ShaderStage::Pixel, a, true)
            },
            "__gm82dx9_shader_vertex_uniform_4f" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| uniform_4f(g, ShaderStage::Vertex, a)
            },
            "__gm82dx9_shader_pixel_uniform_4f" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| uniform_4f(g, ShaderStage::Pixel, a)
            },
            "__gm82dx9_shader_vertex_uniform_matrix" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| uniform_matrix(g, ShaderStage::Vertex, a)
            },
            "__gm82dx9_shader_pixel_uniform_matrix" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| uniform_matrix(g, ShaderStage::Pixel, a)
            },
            "__gm82dx9_texture_stage_set" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| texture_stage_set(g, ShaderStage::Pixel, a)
            },
            "__gm82dx9_texture_stage_vertex_set" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| texture_stage_set(g, ShaderStage::Vertex, a)
            },
            "__gm82dx9_texture_set_stage_interpolation" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| texture_stage_interpolation(g, ShaderStage::Pixel, a)
            },
            "__gm82dx9_texture_set_stage_vertex_interpolation" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| texture_stage_interpolation(g, ShaderStage::Vertex, a)
            },
            "__gm82dx9_texture_set_stage_repeat" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| texture_stage_repeat(g, ShaderStage::Pixel, a)
            },
            "__gm82dx9_texture_set_stage_vertex_repeat" => {
                |g: &mut Game, _: &mut Context, a: &[Value]| texture_stage_repeat(g, ShaderStage::Vertex, a)
            },

            // Left to `external.rs`'s generic no-op *on purpose*, so the first call says so in the
            // log rather than being silently ignored: the alpha test, wireframe and point-sprite
            // fill modes (no WebGL equivalent without a shader of our own), the shader uniform
            // setters and texture stages (nothing to set them on), multiple render targets and
            // per-instance streams (one target and one stream, see `vertex_draw_buffer`), and
            // `__gm82dx9_buffer_get_lights`, which serialises D3D's own light structs for a shader
            // to read.
            _ => return None,
        };
        Some(Function::Runtime(f))
    }

    /// Dropped when a new game loads, so one game's buffers and formats cannot leak into another's
    /// ids within the same page session.
    pub fn reset() {
        DX9_STARTED.set(false);
        VERTEX.set((0.0, 0.0, 0.0));
        VERTEX_FORMATS.with(|r| *r.borrow_mut() = Registry::new());
        VERTEX_BUFFERS.with(|r| *r.borrow_mut() = Registry::new());
        INDEX_BUFFERS.with(|r| *r.borrow_mut() = Registry::new());
        FORMAT_BUILDING.with(|f| *f.borrow_mut() = VertexFormat::default());
        INDEX_BOUND.set(-1);
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;

    pub fn resolve(_symbol: &str) -> Option<Function> {
        None
    }
    pub fn reset() {}
}

// The bare fn-pointer type `Function::Runtime` wraps (its own `ContextFunctionPtr` type alias in
// `gml.rs` is private, so this names the same signature structurally instead of by that name).
type ContextFn = fn(&mut Game, &mut Context, &[Value]) -> gml::Result<Value>;

pub use imp::{reset, resolve};
