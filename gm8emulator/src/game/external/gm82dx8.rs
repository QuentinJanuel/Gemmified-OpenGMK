//! Reimplementation of `gm82dx8.dll`, the **DirectX 8** graphics extension GameMaker 8.2 shipped
//! before `gm82dx9` replaced it (<https://github.com/GM82Project>). A game built against the older
//! IDE links this one instead, and like its successor it refuses to start without a version
//! handshake:
//!
//! ```text
//! if (__gm82dx8_dllcheck()!=820) {
//!     show_error('GM8.2 DirectX8 Extension failed to link DLL.',1)
//!     exit
//! }
//! ```
//!
//! That `show_error` is fatal (`,1`), so a game shipping this package died on its first frame with
//! nothing else tried -- which is exactly how it presented: "Failed to load: failed to initialize
//! game: show_error: GM8.2 DirectX8 Extension failed to link DLL."
//!
//! The two extensions are mutually exclusive -- `gm82dx9.rs`'s `__gm82dx9_abort` exists precisely
//! for a project that loaded both -- so nothing here ever runs alongside that module. What they
//! share is the *device*: dx8's 62 exports are a subset of dx9's surface under older names, sitting
//! on the same D3D fixed-function state that `render/webgl.rs` stands in for. So this module is a
//! **name-translation table onto `gm82dx9`'s implementations** rather than a second copy of them:
//! `dx8_transform_xzyst` is `d3d_transform_xzyst`, `__gm82dx8_setzscale` is `__gm82dx9_setzscale`,
//! and so on for 47 of the 62. Duplicating the 30 transform combinators and the render-state block
//! would have been two bodies of code to keep in step for no behavioural gain.
//!
//! What dx8 has and dx9 does not is a **vblank-synchronisation block**, and its shape matters here
//! because getting it wrong hangs the page rather than glitching it:
//!
//! ```text
//! while (!__gm82dx8_waitvblank()) {/*òwó*/}
//! ```
//!
//! That busy-wait has no escape. `waitvblank` therefore answers 1 -- see its comment.
//!
//! What is *absent* from dx8 is as informative as what is present: no shaders, no vertex buffers,
//! no index buffers, no surface/buffer interop, no lights. D3D8 had none of the interfaces those
//! need. So the entire `sm3` shader path, and the traps around it, simply do not arise for a dx8
//! game.

use crate::{
    game::Game,
    gml::{self, Context, Function, Value},
};

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use std::cell::Cell;

    fn ok_num(n: f64) -> gml::Result<Value> {
        Ok(Value::Real(n.into()))
    }

    thread_local! {
        static DX8_STARTED: Cell<bool> = Cell::new(false);
    }

    // -- handshake -------------------------------------------------------------------------------

    fn dllcheck(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(super::super::gm82::GM82_VERSION)
    }

    /// Read as `if (__gm82dx8_checkstart()) exit`, the same way round as dx9's and the opposite way
    /// round from `__gm82core_checkstart`: **0** the first time, 1 after. Getting it backwards makes
    /// `__gm82dx8_init` bail before it installs the end-step and compose hooks, so a game using the
    /// application surface renders nothing and logs nothing.
    fn checkstart(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(u8::from(DX8_STARTED.replace(true))))
    }

    /// The DLL file's own initializer (`init="__gm82dx8_dll_init"`), which runs once as the
    /// extension loads and before any GML wrapper. The original uses it to find the runner's
    /// `IDirect3DDevice8` by walking the runner's memory; there is no device to find here and
    /// nothing reads its result.
    fn dll_init(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(0.0)
    }

    // -- vblank sync -----------------------------------------------------------------------------
    //
    // `__gm82dx8_vsync()` runs after every frame's draw events and, when enabled, spins until the
    // display's vertical blank so the game's own frame pacing lands on it. In a browser
    // `requestAnimationFrame` already delivers us at one and there is no way to observe or wait for
    // another, so the three functions below make that spin terminate immediately.

    /// Windows XP could not do the DWM-based sync, so the extension gates vsync on this and
    /// `dx8_set_vsync` ANDs every later request with it. We are not Windows XP; answering 0 here
    /// would work too (it disables the block wholesale) but by lying about the OS rather than by
    /// making the wait trivially satisfied, which is what actually happens.
    fn not_xp(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    /// **Load-bearing: this must not return 0.** The second of the extension's two waits is
    /// `while (!__gm82dx8_waitvblank()) {}` with no timeout and no break -- a 0 here is an infinite
    /// loop inside one GML call, which on the browser's single thread is a permanently frozen page,
    /// not a slow one.
    ///
    /// 1 is also the honest answer rather than merely the safe one: the caller asks "are we at the
    /// vertical blank yet", and code running from a `requestAnimationFrame` callback is.
    fn wait_vblank(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    /// Microseconds off a monotonic clock -- the same source `get_timer()` uses, since the game's
    /// `__gm82dx8_time` is differenced against `1000000/room_speed`.
    fn time_now(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(gml::datetime::monotonic_micros())
    }

    /// `Sleep(ms)`, called only inside the timed half of the vsync wait to avoid burning a core.
    /// Nothing on a browser main thread can block for a millisecond without freezing everything
    /// else, and with `wait_vblank` answering 1 this is unreachable anyway.
    fn sleep(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(0.0)
    }

    /// `DwmFlush()` -- lines the game's frame up with the Desktop Window Manager's own composition.
    /// The browser's compositor is not ours to synchronise with.
    fn sync_dwm(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(0.0)
    }

    // -- video memory ----------------------------------------------------------------------------

    /// **Megabytes**, unlike dx9's byte-valued `d3d_get_free_video_memory`, and unlike dx9's it is
    /// consulted *before* the allocation rather than after one has already failed:
    /// `dx8_surface_ensure` computes `w * h * 8 / 1048576` and pops a fatal `show_error` if this
    /// number is smaller. Sharing dx9's constant would have been safe by accident (it is enormous
    /// read as MB) but wrong as a figure; this is a plausible amount of free VRAM that clears any
    /// surface the maximum texture size permits -- 4096x4096 needs 128 MB by that formula.
    fn free_video_memory(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(4096.0)
    }

    // -- resolve ---------------------------------------------------------------------------------

    /// dx8 symbols with an identical dx9 counterpart, as `(dx8, dx9)` pairs. Every entry is a pure
    /// rename: same arguments in the same order, same meaning, same device state underneath.
    ///
    /// As everywhere in this family, the name that arrives here is the DLL `extname` when the
    /// extension declares one and the GML name when it does not, which is why the two columns mix
    /// spellings -- `dx8_set_cull_mode` (no extname) maps to `__gm82dx9_set_cull_mode` (extname).
    #[rustfmt::skip]
    const ALIASES: &[(&str, &str)] = &[
        // device and viewport
        ("__gm82dx8_cleardepth",      "__gm82dx9_cleardepth"),
        ("__gm82dx8_setviewport",     "__gm82dx9_setviewport"),
        ("__gm82dx8_getviewportx",    "__gm82dx9_getviewportx"),
        ("__gm82dx8_getviewporty",    "__gm82dx9_getviewporty"),
        ("__gm82dx8_getviewportw",    "__gm82dx9_getviewportw"),
        ("__gm82dx8_getviewporth",    "__gm82dx9_getviewporth"),
        ("__gm82dx8_getmaxwidth",     "__gm82dx9_getmaxwidth"),
        ("__gm82dx8_getmaxheight",    "__gm82dx9_getmaxheight"),
        // Device-level settings the page owns, not the game: dx9 routes all three to the same
        // inert body, and so do we.
        ("__gm82dx8_resize_backbuffer", "__gm82dx9_resize_backbuffer"),
        ("__gm82dx8_setfullscreen",     "__gm82dx9_setfullscreen"),
        ("__gm82dx8_setalphabuffer",    "__gm82dx9_setalphabuffer"),

        // render state
        ("dx8_set_color_mask",        "__gm82dx9_set_color_mask"),
        ("dx8_set_cull_mode",         "__gm82dx9_set_cull_mode"),
        ("dx8_set_zbias",             "__gm82dx9_set_zbias"),
        ("__gm82dx8_setzscale",       "__gm82dx9_setzscale"),
        ("__gm82dx8_setrangefog",     "__gm82dx9_setrangefog"),

        // vertex transform readback: one call returns x, two getters fetch y and z
        ("__gm82dx8_transformvertex", "__gm82dx9_transformvertex"),
        ("__gm82dx8_getvertexy",      "__gm82dx9_getvertexy"),
        ("__gm82dx8_getvertexz",      "__gm82dx9_getvertexz"),

        // The 30 `<rotations>[s]t` combinators, renamed from `d3d_` to `dx8_` and otherwise
        // identical -- they multiply into the world matrix rather than replacing it, and apply
        // their rotations in the order the letters are written. See `gm82dx9::transform_combo`.
        ("dx8_transform_xt",    "d3d_transform_xt"),
        ("dx8_transform_yt",    "d3d_transform_yt"),
        ("dx8_transform_zt",    "d3d_transform_zt"),
        ("dx8_transform_xst",   "d3d_transform_xst"),
        ("dx8_transform_yst",   "d3d_transform_yst"),
        ("dx8_transform_zst",   "d3d_transform_zst"),
        ("dx8_transform_xyt",   "d3d_transform_xyt"),
        ("dx8_transform_xzt",   "d3d_transform_xzt"),
        ("dx8_transform_yxt",   "d3d_transform_yxt"),
        ("dx8_transform_yzt",   "d3d_transform_yzt"),
        ("dx8_transform_zxt",   "d3d_transform_zxt"),
        ("dx8_transform_zyt",   "d3d_transform_zyt"),
        ("dx8_transform_xyst",  "d3d_transform_xyst"),
        ("dx8_transform_xzst",  "d3d_transform_xzst"),
        ("dx8_transform_yxst",  "d3d_transform_yxst"),
        ("dx8_transform_yzst",  "d3d_transform_yzst"),
        ("dx8_transform_zxst",  "d3d_transform_zxst"),
        ("dx8_transform_zyst",  "d3d_transform_zyst"),
        ("dx8_transform_xyzt",  "d3d_transform_xyzt"),
        ("dx8_transform_xzyt",  "d3d_transform_xzyt"),
        ("dx8_transform_yxzt",  "d3d_transform_yxzt"),
        ("dx8_transform_yzxt",  "d3d_transform_yzxt"),
        ("dx8_transform_zxyt",  "d3d_transform_zxyt"),
        ("dx8_transform_zyxt",  "d3d_transform_zyxt"),
        ("dx8_transform_xyzst", "d3d_transform_xyzst"),
        ("dx8_transform_xzyst", "d3d_transform_xzyst"),
        ("dx8_transform_yxzst", "d3d_transform_yxzst"),
        ("dx8_transform_yzxst", "d3d_transform_yzxst"),
        ("dx8_transform_zxyst", "d3d_transform_zxyst"),
        ("dx8_transform_zyxst", "d3d_transform_zyxst"),
    ];

    pub fn resolve(symbol: &str) -> Option<Function> {
        let f: super::ContextFn = match symbol {
            "__gm82dx8_dllcheck" => dllcheck,
            "__gm82dx8_checkstart" => checkstart,
            "__gm82dx8_dll_init" => dll_init,
            "__gm82dx8_not_xp" => not_xp,
            "__gm82dx8_waitvblank" => wait_vblank,
            "__gm82dx8_time_now" => time_now,
            "__gm82dx8_sleep" => sleep,
            "__gm82dx8_sync_dwm" => sync_dwm,
            "__gm82dx8_getvideomem" => free_video_memory,

            // Left to `external.rs`'s warn-once dummy on purpose, matching dx9: the wireframe and
            // point fill modes (`dx8_set_fill_mode`) and point-sprite scaling
            // (`__gm82dx8_setpointscale`), neither of which WebGL can do without a shader of our
            // own.
            _ => return alias(symbol),
        };
        Some(Function::Runtime(f))
    }

    /// Hands a dx8 symbol to `gm82dx9`'s resolver under its dx9 name.
    ///
    /// A miss on the right-hand column is a typo in `ALIASES`, not a game doing something unusual,
    /// and it would otherwise present as a silently inert function -- the failure mode this whole
    /// family is most prone to. Say so instead: `external.rs`'s dummy will still keep the game
    /// running, and the log will name which mapping is wrong.
    fn alias(symbol: &str) -> Option<Function> {
        let dx9 = ALIASES.iter().find(|(dx8, _)| *dx8 == symbol)?.1;
        let resolved = super::super::gm82dx9::resolve(dx9);
        if resolved.is_none() {
            web_sys::console::warn_1(
                &format!("gm82dx8: '{symbol}' is mapped to '{dx9}', which gm82dx9 does not define").into(),
            );
        }
        resolved
    }

    /// Dropped when a new game loads, so a second load's handshake behaves like a first.
    pub fn reset() {
        DX8_STARTED.set(false);
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
