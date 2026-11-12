//! Reimplementation of the DLLs shipped with **GameMaker 8.2**, a community fork of GM8.1
//! (<https://github.com/GM82Project>). Not specific to any one game: every GM8.2 game embeds these
//! extension packages, because the IDE adds them to a blank project by default.
//!
//! GM8.2 is not a new engine -- it is stock GM8.1 plus five bundled extension packages, with no
//! GML or data-format changes, so the exe reader and interpreter need nothing. What it does add is
//! a hard startup dependency on native code: both `gm82core` and `gm82dx9` open their initializer
//! with a version handshake and `show_error` + `exit` if it fails, e.g.
//!
//! ```text
//! if (__gm82core_dllcheck()!=820) {
//!     show_error('GM8.2 Core Extension failed to link DLL.',1)
//!     exit
//! }
//! ```
//!
//! `external.rs`'s generic no-op fallback answers `0`, so *every* GM8.2 game died on that line
//! before this module existed. The handshake is the only thing standing between us and a boot:
//! most of what these extensions expose is plain GML (gm82core is 129 DLL functions but ~227 GML
//! ones), and the GML half runs on the interpreter for free.
//!
//! Two conventions here were read out of the extensions' own C sources rather than guessed, and
//! both are easy to get backwards:
//!
//! - **The two `checkstart`s are inverted.** `__gm82core_checkstart` returns **1** on the first
//!   call and 0 after (`gm82core/source/windows.c`); `__gm82dx9_checkstart` returns **0** on the
//!   first call and 1 after (`gm82dx9/source/gm82dx9.cpp`). They are used accordingly --
//!   `if (!__gm82core_checkstart(...)) __gm82core_restart()` vs `if (__gm82dx9_checkstart()) exit`
//!   -- so a single shared "first call" answer silently breaks one of them. Getting dx9's
//!   backwards makes `__gm82dx9_init` `exit` before it installs the app-surface compose hooks,
//!   which presents as a black screen with nothing in the log.
//! - **`820` is the literal expected value** of both `dllcheck`s (the fork's version number), not
//!   a success/failure boolean.
//!
//! Wired in from `external.rs`'s `make_call`, via the same `external::Call::Emulated(Function)`
//! extension point `ssound.rs` and `fmod.rs` already use.
//!
//! Scope: `gm82core.dll` (its handshake, host services and pure-math surface) plus the
//! `inst_meta.dll` initializer. Its siblings have their own modules: `gm82buf.rs` for the buffer
//! API, `gm82dx9.rs` for the graphics extension. Unrecognized symbols fall through to
//! `external.rs`'s generic dummy, which warns once on first call rather than panicking.
//!
//! `gm82snd.dll` is *not* handled here: it re-exports the exact GMFMODSimple symbol names
//! (`FMODinit`, `FMODSoundAdd`, `FMODSoundPlay`, ...), so `external.rs` routes it straight to
//! `fmod.rs`, which already implements them.

use crate::{
    game::Game,
    gml::{self, Context, Function, Value},
};

/// The version handshake value both `dllcheck` functions must return: GM8.2, as an integer.
pub const GM82_VERSION: f64 = 820.0;

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use std::cell::Cell;

    fn ok_num(n: f64) -> gml::Result<Value> {
        Ok(Value::Real(n.into()))
    }

    /// Coerces a GML argument to a number the way GM does: a string argument is parsed, and
    /// anything unparseable is 0 rather than an error.
    /// Raw GML string bytes, for functions that must not round-trip through the text encoding.
    fn arg_raw(v: &Value) -> Vec<u8> {
        <&Value as Into<&[u8]>>::into(v).to_vec()
    }

    fn arg_str(v: &Value) -> std::borrow::Cow<'_, str> {
        crate::game::decode_gml_string(<&Value as Into<&[u8]>>::into(v))
    }

    fn arg_f64(v: &Value) -> f64 {
        match v {
            Value::Real(r) => (*r).into(),
            Value::Str(_) => crate::game::decode_gml_string(<&Value as Into<&[u8]>>::into(v)).parse().unwrap_or(0.0),
        }
    }

    // Both extensions gate one-time setup on a "have I started yet" flag that lives in the DLL's
    // own data segment for the lifetime of the process. A thread-local is the faithful equivalent
    // here: each game load gets a fresh iframe (and so a fresh wasm instance), which is exactly the
    // process lifetime the originals assume.
    thread_local! {
        // (text, separator, cursor, exhausted) for string_token_start/next.
        static TOKENS: std::cell::RefCell<(Vec<u8>, Vec<u8>, usize, bool)> =
            std::cell::RefCell::new((Vec::new(), Vec::new(), 0, true));
        static CORE_STARTED: Cell<bool> = Cell::new(false);
    }

    // -- gm82core.dll ------------------------------------------------------------------------

    fn core_dllcheck(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(GM82_VERSION)
    }

    /// Real signature takes the GM window handle and does Win32 window setup we have no analogue
    /// for. The return value is the part that matters to GML: 1 the first time, 0 on every later
    /// call, which `__gm82core_create` reads as "this is a `game_restart`, re-run
    /// `__gm82core_restart()`".
    fn core_checkstart(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(f64::from(u8::from(!CORE_STARTED.replace(true))))
    }

    /// Real one calls `QueryPerformanceFrequency`/`QueryPerformanceCounter` and returns whether
    /// both succeeded. `performance.now()` is always available, so: yes.
    fn core_hrt_init(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    /// `get_timer()` -- microseconds off a monotonic clock. Load-bearing rather than cosmetic:
    /// `__gm82core_update` runs every begin-step and computes `delta_time` by differencing this,
    /// so a constant would pin `delta_time` at 0 and stop anything written against it.
    fn core_hrt_now(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(gml::datetime::monotonic_micros())
    }

    /// `get_delta()` -- microseconds since the previous call. Keeps its own "last" the same way
    /// the original's file-static `lastTime` does, so it stays independent of `get_timer`.
    fn core_hrt_delta(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        thread_local! {
            static LAST: Cell<f64> = Cell::new(f64::NAN);
        }
        let now = gml::datetime::monotonic_micros();
        let last = LAST.replace(now);
        ok_num(if last.is_nan() { 0.0 } else { now - last })
    }

    /// `window_has_focus()`. The page-level "click to resume" overlay already handles real focus
    /// loss for every engine, and a browser tab that isn't focused isn't running rAF anyway, so
    /// reporting "focused" is both simpler and closer to what the player sees.
    fn core_getfore(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    /// The original patches two bytes of the GM8 runner's machine code in place
    /// (`WriteProcessMemory` at `0x606865`) to stop it clearing keyboard state on room end. There
    /// is no compiled runner to patch here; honouring the *intent* means a flag on our own input
    /// handling, which Tier 1 wires up. Inert for now: the game keeps GM8's stock behaviour of
    /// clearing on room end, so a key held across a room transition may be missed.
    fn core_io_set_roomend_clear(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(0.0)
    }

    // -- inst_meta.dll (gm82test package) ----------------------------------------------------

    /// Set-up/tear-down for a reflection helper that walks a live instance's variable names by
    /// reading GM8 runner structures directly. Nothing to set up when there is no such runner;
    /// the functions it enables are opt-in (`variable_instance_get_all` and friends) and stay on
    /// the generic no-op until something asks for them.
    fn inst_meta_init(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(0.0)
    }

    // -- gm82core.dll: pure math -----------------------------------------------------------------
    //
    // The bulk of what gm82core exposes, and the part games call constantly: angle helpers, degree
    // trig, interpolation, and point/rectangle tests. All pure functions of their arguments, so
    // they are reproduced exactly rather than approximated -- several have deliberately asymmetric
    // definitions a reasonable guess would get wrong, and a silently-wrong `modwrap` or
    // `point_in_rectangle` is a gameplay bug with no error attached to it.
    //
    // Behaviours worth not "tidying up" later:
    //   * `point_in_rectangle` is half-open: x1 <= p < x2. Not a typo.
    //   * `rectangle_in_rectangle` returns 0, 1 (fully inside) or 2 (partial overlap), not a bool.
    //   * `unpack_bool` returns the masked bit's *value* (bit 3 -> 8), not 0/1.
    //   * `secant` takes radians while `dsecant` takes degrees.
    //   * `lengthdir_zy`/`lengthdir_zz` are negated, matching GM's y-down screen convention.
    //   * `angle_difference(a, b)` is based on b - a.
    //
    // gm82core is zlib-licensed, which explicitly permits use and alteration.

    fn a(args: &[Value], i: usize) -> f64 {
        args.get(i).map(arg_f64).unwrap_or(0.0)
    }

    fn signnum(x: f64) -> f64 {
        if x > 0.0 {
            1.0
        } else if x < 0.0 {
            -1.0
        } else {
            x
        }
    }

    fn modwrap_f(val: f64, minv: f64, maxv: f64) -> f64 {
        let f = val - minv;
        let w = maxv - minv;
        f - (f / w).floor() * w + minv
    }

    fn dsin_f(deg: f64) -> f64 {
        (deg / 180.0 * std::f64::consts::PI).sin()
    }
    fn dcos_f(deg: f64) -> f64 {
        (deg / 180.0 * std::f64::consts::PI).cos()
    }
    fn darccos_f(x: f64) -> f64 {
        x.acos() * 180.0 / std::f64::consts::PI
    }
    fn point_dis(x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
        (x2 - x1).hypot(y2 - y1)
    }
    fn point_dis3d(x1: f64, y1: f64, z1: f64, x2: f64, y2: f64, z2: f64) -> f64 {
        ((x2 - x1).powi(2) + (y2 - y1).powi(2) + (z2 - z1).powi(2)).sqrt()
    }
    fn point_dir(x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
        modwrap_f((-(y2 - y1)).atan2(x2 - x1) * 180.0 / std::f64::consts::PI, 0.0, 360.0)
    }
    fn lendirx(len: f64, dir: f64) -> f64 {
        len * dcos_f(dir)
    }
    fn lendiry(len: f64, dir: f64) -> f64 {
        -len * dsin_f(dir)
    }
    fn saturate_f(v: f64) -> f64 {
        v.clamp(0.0, 1.0)
    }
    fn angle_difference_f(ang1: f64, ang2: f64) -> f64 {
        modwrap_f(ang2 - ang1 + 540.0, 0.0, 360.0) - 180.0
    }
    fn dot_norm(x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
        let a1 = point_dis(0.0, 0.0, x1, y1);
        let b1 = point_dis(0.0, 0.0, x2, y2);
        if a1 * b1 == 0.0 {
            return 1.0
        }
        (x1 / a1) * (x2 / b1) + (y1 / a1) * (y2 / b1)
    }
    fn dot_norm3d(x1: f64, y1: f64, z1: f64, x2: f64, y2: f64, z2: f64) -> f64 {
        let a1 = point_dis3d(0.0, 0.0, 0.0, x1, y1, z1);
        let b1 = point_dis3d(0.0, 0.0, 0.0, x2, y2, z2);
        if a1 * b1 == 0.0 {
            return 1.0
        }
        (x1 / a1) * (x2 / b1) + (y1 / a1) * (y2 / b1) + (z1 / a1) * (z2 / b1)
    }
    fn dsecant_f(deg: f64) -> f64 {
        1.0 / dcos_f(deg)
    }
    /// Where a point projects onto a line, as a fraction of the way from (x1,y1) to (x2,y2).
    /// `segment` clamps it into 0..1; a zero-length line projects to 0.
    fn point_line_lerp_f(px: f64, py: f64, x1: f64, y1: f64, x2: f64, y2: f64, segment: f64) -> f64 {
        let (dx, dy) = (x2 - x1, y2 - y1);
        if dx == 0.0 && dy == 0.0 {
            return 0.0
        }
        let t = (dx * (px - x1) + dy * (py - y1)) / (dx * dx + dy * dy);
        if segment > 0.5 { t.clamp(0.0, 1.0) } else { t }
    }

    /// `Function::Runtime` is a bare fn pointer with nowhere to carry the symbol, so each math
    /// export needs its own function. This bakes the name into a non-capturing closure (which
    /// coerces to a fn pointer) so the fifty implementations can still live in one readable
    /// `math_value` match instead of fifty near-identical `fn`s.
    macro_rules! math_thunk {
        ($sym:literal) => {
            |_: &mut Game, _: &mut Context, args: &[Value]| -> gml::Result<Value> {
                ok_num(math_value($sym, args).unwrap_or(0.0))
            }
        };
    }

    fn math_value(symbol: &str, args: &[Value]) -> Option<f64> {
        let v = match symbol {
            "modwrap" => modwrap_f(a(args, 0), a(args, 1), a(args, 2)),
            "unlerp" => (a(args, 2) - a(args, 0)) / (a(args, 1) - a(args, 0)),
            "unclerp" => saturate_f((a(args, 2) - a(args, 0)) / (a(args, 1) - a(args, 0))),
            "lerp2" => ((a(args, 4) - a(args, 0)) / (a(args, 1) - a(args, 0))) * (a(args, 3) - a(args, 2)) + a(args, 2),
            "clerp2" => {
                saturate_f((a(args, 4) - a(args, 0)) / (a(args, 1) - a(args, 0))) * (a(args, 3) - a(args, 2))
                    + a(args, 2)
            },
            "esign" => {
                let x = a(args, 0);
                if x == 0.0 {
                    a(args, 1)
                } else if x > 0.0 {
                    1.0
                } else {
                    -1.0
                }
            },
            "saturate" => saturate_f(a(args, 0)),
            "smoothstep" => {
                let t = saturate_f((a(args, 2) - a(args, 0)) / (a(args, 1) - a(args, 0)));
                t * t * (3.0 - 2.0 * t)
            },
            "approach" | "inch" => {
                let (val, go, step) = (a(args, 0), a(args, 1), a(args, 2));
                if val < go { go.min(val + step) } else { go.max(val - step) }
            },
            "lerproach" => {
                let (val, go, lerpamt, appamt) = (a(args, 0), a(args, 1), a(args, 2), a(args, 3));
                let val = val * (1.0 - lerpamt) + go * lerpamt;
                if val < go { go.min(val + appamt) } else { go.max(val - appamt) }
            },
            "round_unbiased" => (a(args, 0) + 0.5).floor(),
            "roundto" => (a(args, 0) / a(args, 1)).round() * a(args, 1),
            "roundto_unbiased" => (a(args, 0) / a(args, 1) + 0.5).floor() * a(args, 1),
            "floorto" => (a(args, 0) / a(args, 1)).floor() * a(args, 1),
            "ceilto" => (a(args, 0) / a(args, 1)).ceil() * a(args, 1),
            "cosine" => {
                let mu = (1.0 - (a(args, 2) * std::f64::consts::PI).cos()) / 2.0;
                a(args, 0) * (1.0 - mu) + a(args, 1) * mu
            },
            "angle_difference" => angle_difference_f(a(args, 0), a(args, 1)),
            "angle_mean" => a(args, 0) + 0.5 * angle_difference_f(a(args, 0), a(args, 1)),
            "angle_abs" => (modwrap_f(a(args, 0) + 540.0, 0.0, 360.0) - 180.0).abs(),
            "lerp_angle" => a(args, 0) + angle_difference_f(a(args, 0), a(args, 1)) * a(args, 2),
            "approach_angle" | "inch_angle" => {
                let (ang1, ang2, step) = (a(args, 0), a(args, 1), a(args, 2));
                let d = (ang2 - ang1 + 540.0) % 360.0 - 180.0;
                if d > 0.0 {
                    (ang1 + d.min(step)) % 360.0
                } else {
                    (360.0 - (360.0 - (ang1 + (-step).max(d))) % 360.0) % 360.0
                }
            },
            "approach_angle_biased" => {
                let (ang1, ang2, step, bias) = (a(args, 0), a(args, 1), a(args, 2), a(args, 3));
                let d = (ang2 - ang1 + 540.0) % 360.0 - 180.0;
                if (d.abs() - 180.0).abs() < bias.abs() {
                    modwrap_f(ang1 + d.abs().min(step) * signnum(bias), 0.0, 360.0)
                } else if d > 0.0 {
                    (ang1 + d.min(step)) % 360.0
                } else {
                    (360.0 - (360.0 - (ang1 + (-step).max(d))) % 360.0) % 360.0
                }
            },
            "dsin" => dsin_f(a(args, 0)),
            "dcos" => dcos_f(a(args, 0)),
            "dtan" => (a(args, 0) / 180.0 * std::f64::consts::PI).tan(),
            "darcsin" => a(args, 0).asin() * 180.0 / std::f64::consts::PI,
            "darccos" => darccos_f(a(args, 0)),
            "darctan" => a(args, 0).atan() * 180.0 / std::f64::consts::PI,
            "darctan2" => a(args, 0).atan2(a(args, 1)) * 180.0 / std::f64::consts::PI,
            "secant" => 1.0 / a(args, 0).cos(),
            "dsecant" => dsecant_f(a(args, 0)),
            "in_range" => f64::from(u8::from(a(args, 0) >= a(args, 1) && a(args, 0) <= a(args, 2))),
            "range_collision" => {
                let (a1, a2, b1, b2) = (a(args, 0), a(args, 1), a(args, 2), a(args, 3));
                f64::from(u8::from(a1.max(a2) > b1.min(b2) && b1.max(b2) > a1.min(a2)))
            },
            "triangle_is_clockwise" => {
                let (x0, y0, x1, y1, x2, y2) = (a(args, 0), a(args, 1), a(args, 2), a(args, 3), a(args, 4), a(args, 5));
                let clockwise = if x0 == x1 && y0 == y1 {
                    false
                } else if x0 == x1 {
                    (x2 < x1) ^ (y0 > y1)
                } else {
                    let m = (y0 - y1) / (x0 - x1);
                    (y2 > (m * x2 + y0 - m * x0)) ^ (x0 > x1)
                };
                f64::from(u8::from(clockwise))
            },
            "lengthdir_zx" => a(args, 0) * dcos_f(a(args, 2)) * dcos_f(a(args, 1)),
            "lengthdir_zy" => -a(args, 0) * dcos_f(a(args, 2)) * dsin_f(a(args, 1)),
            "lengthdir_zz" => -a(args, 0) * dsin_f(a(args, 2)),
            "point_direction_pitch" => {
                point_dir(0.0, a(args, 2), point_dis(a(args, 0), a(args, 1), a(args, 3), a(args, 4)), a(args, 5))
            },
            "pivot_pos_x" => lendirx(a(args, 0), a(args, 2)) + lendirx(a(args, 1), a(args, 2) - 90.0),
            "pivot_pos_y" => lendiry(a(args, 0), a(args, 2)) + lendiry(a(args, 1), a(args, 2) - 90.0),
            "point_in_circle" => {
                f64::from(u8::from(point_dis(a(args, 0), a(args, 1), a(args, 2), a(args, 3)) < a(args, 4)))
            },
            "circle_in_circle" => {
                f64::from(u8::from(point_dis(a(args, 0), a(args, 1), a(args, 3), a(args, 4)) < a(args, 2) + a(args, 5)))
            },
            "rectangle_in_circle" => {
                let (x1, y1, x2, y2, cx, cy, cr) =
                    (a(args, 0), a(args, 1), a(args, 2), a(args, 3), a(args, 4), a(args, 5), a(args, 6));
                let tx = cx.clamp(x1, x2.max(x1));
                let ty = cy.clamp(y1, y2.max(y1));
                f64::from(u8::from(point_dis(tx, ty, cx, cy) < cr))
            },
            // Half-open on the far edge, deliberately: x1 <= p < x2.
            "point_in_rectangle" => {
                let (px, py) = (a(args, 0), a(args, 1));
                f64::from(u8::from(px >= a(args, 2) && px < a(args, 4) && py >= a(args, 3) && py < a(args, 5)))
            },
            "rectangle_in_rectangle" => {
                let (mut ax1, mut ay1, mut ax2, mut ay2) = (a(args, 0), a(args, 1), a(args, 2), a(args, 3));
                let (mut bx1, mut by1, mut bx2, mut by2) = (a(args, 4), a(args, 5), a(args, 6), a(args, 7));
                if ax1 > ax2 {
                    std::mem::swap(&mut ax1, &mut ax2);
                }
                if ay1 > ay2 {
                    std::mem::swap(&mut ay1, &mut ay2);
                }
                if bx1 > bx2 {
                    std::mem::swap(&mut bx1, &mut bx2);
                }
                if by1 > by2 {
                    std::mem::swap(&mut by1, &mut by2);
                }
                if ax1 >= bx1 && ax2 <= bx2 && ay1 >= by1 && ay2 <= by2 {
                    1.0
                } else if ax1 > bx2 || ax2 < bx1 || ay1 > by2 || ay2 < by1 {
                    0.0
                } else {
                    2.0
                }
            },
            "point_in_triangle" => {
                let (x0, y0) = (a(args, 0), a(args, 1));
                let (x1, y1, x2, y2, x3, y3) = (a(args, 2), a(args, 3), a(args, 4), a(args, 5), a(args, 6), a(args, 7));
                let p = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
                let q = (x2 - x0) * (y3 - y0) - (x3 - x0) * (y2 - y0);
                let r = (x3 - x0) * (y1 - y0) - (x1 - x0) * (y3 - y0);
                f64::from(u8::from(signnum(p) == signnum(q) && signnum(q) == signnum(r)))
            },
            "dot_product_normalized" => dot_norm(a(args, 0), a(args, 1), a(args, 2), a(args, 3)),
            "dot_product_3d_normalized" => {
                dot_norm3d(a(args, 0), a(args, 1), a(args, 2), a(args, 3), a(args, 4), a(args, 5))
            },
            "angle_difference_3d" => {
                darccos_f(dot_norm3d(a(args, 0), a(args, 1), a(args, 2), a(args, 3), a(args, 4), a(args, 5)))
            },
            "box_distance" => dsecant_f(45.0 - (45.0 - a(args, 1) % 90.0).abs()) * a(args, 0),
            "pack_bools" => {
                let mut out = 0u32;
                // Argument order is b7 first down to b0, so index i carries bit (7 - i).
                for i in 0..8 {
                    if a(args, i) >= 0.5 {
                        out |= 1 << (7 - i);
                    }
                }
                f64::from(out)
            },
            // Returns the masked bit's value (bit 3 -> 8), not a 0/1 flag.
            "unpack_bool" => f64::from((a(args, 0) as u8) & (1u8 << (a(args, 1) as u8 & 7))),
            // How far along a line segment a point projects, 0..1 when clamped to the segment.
            "point_line_lerp" => {
                point_line_lerp_f(a(args, 0), a(args, 1), a(args, 2), a(args, 3), a(args, 4), a(args, 5), a(args, 6))
            },
            "point_line_distance" => {
                let (px, py) = (a(args, 0), a(args, 1));
                let (x1, y1, x2, y2) = (a(args, 2), a(args, 3), a(args, 4), a(args, 5));
                let t = point_line_lerp_f(px, py, x1, y1, x2, y2, a(args, 6));
                point_dis(x1 + t * (x2 - x1), y1 + t * (y2 - y1), px, py)
            },
            // How far along the *first* line the two cross, or 0 if they are parallel -- and, with
            // `segment` set, 0 as well when the crossing is off the end of either. So 0 means both
            // "they meet right at the start" and "they don't meet", which is the original's own
            // ambiguity, from GMLscripts.com.
            "lines_intersect" => {
                let (x1, y1, x2, y2) = (a(args, 0), a(args, 1), a(args, 2), a(args, 3));
                let (x3, y3, x4, y4) = (a(args, 4), a(args, 5), a(args, 6), a(args, 7));
                let (ux, uy) = (x2 - x1, y2 - y1);
                let (vx, vy) = (x4 - x3, y4 - y3);
                let (wx, wy) = (x1 - x3, y1 - y3);
                let ud = vy * ux - vx * uy;
                let mut ua = 0.0;
                if ud != 0.0 {
                    ua = (vx * wy - vy * wx) / ud;
                    if a(args, 8) >= 0.5 {
                        let ub = (ux * wy - uy * wx) / ud;
                        if !(0.0..=1.0).contains(&ua) || !(0.0..=1.0).contains(&ub) {
                            ua = 0.0;
                        }
                    }
                }
                ua
            },
            "color_reverse" => {
                let col = a(args, 0).round() as i64;
                (((col & 0xff) << 16) + (col & 0xff00) + ((col & 0xff0000) >> 16)) as f64
            },
            "color_inverse" => (0xffffff - a(args, 0).round() as i64) as f64,
            _ => return None,
        };
        Some(v)
    }

    // -- gm82core.dll: portable host services ------------------------------------------------
    //
    // The half of gm82core that isn't pure math. Only the parts with an honest browser equivalent
    // are here; see the resolve table's tail for what is deliberately left inert and why.

    /// `string_token_start(str, sep)` / `string_token_next()` -- a stateful splitter. The original
    /// keeps one token cursor in the DLL's data segment, so a game calls `start` once and then
    /// `next` in a loop; a thread-local is the faithful equivalent for a per-page wasm instance.
    ///
    /// `start` returns the token *count*, specifically so it can drive a `repeat` loop.
    fn token_start(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let text = args.first().map(arg_raw).unwrap_or_default();
        let sep = args.get(1).map(arg_raw).unwrap_or_default();
        let count = if sep.is_empty() {
            1
        } else {
            // Split count = separators found + 1, matching the original's scan.
            let mut n = 1usize;
            let mut i = 0usize;
            while i + sep.len() <= text.len() {
                if &text[i..i + sep.len()] == sep.as_slice() {
                    n += 1;
                    i += sep.len();
                } else {
                    i += 1;
                }
            }
            n
        };
        TOKENS.with(|t| {
            let mut t = t.borrow_mut();
            t.0 = text;
            t.1 = sep;
            t.2 = 0;
            t.3 = false;
        });
        ok_num(count as f64)
    }

    fn token_next(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        let out = TOKENS.with(|t| {
            let mut t = t.borrow_mut();
            if t.3 {
                return Vec::new()
            }
            let (text, sep, pos) = (t.0.clone(), t.1.clone(), t.2);
            if sep.is_empty() {
                t.3 = true;
                return text[pos.min(text.len())..].to_vec()
            }
            let mut i = pos;
            while i + sep.len() <= text.len() {
                if &text[i..i + sep.len()] == sep.as_slice() {
                    let out = text[pos..i].to_vec();
                    t.2 = i + sep.len();
                    return out
                }
                i += 1;
            }
            t.3 = true; // last token; further calls return empty
            text[pos.min(text.len())..].to_vec()
        });
        Ok(Value::from(out.as_slice()))
    }

    /// Uppercase hex, no leading zeros -- matches the original's lookup table and its walk back
    /// from the end of a fixed buffer.
    fn string_hex(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        Ok(Value::from(format!("{:X}", a(args, 0) as i64 as u64)))
    }

    fn real_hex(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let text = args.first().map(arg_str).unwrap_or_default().into_owned();
        let mut out: u64 = 0;
        for c in text.chars() {
            // Non-hex characters contribute nothing, matching the original's zero-filled lookup.
            if let Some(d) = c.to_digit(16) {
                out = out.wrapping_mul(16).wrapping_add(u64::from(d));
            }
        }
        ok_num(out as f64)
    }

    /// The next power of two at or above `x`. The original does it by rounding the double's
    /// exponent field up, which also maps anything <= 0 to 0; reproduced here without the pun.
    fn power_next(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let x = a(args, 0);
        if !(x > 0.0) {
            return ok_num(0.0)
        }
        ok_num(x.log2().ceil().exp2())
    }

    fn file_size(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let path = args.first().map(arg_str).unwrap_or_default().into_owned();
        // -1 for a file that isn't there, which is what the original reports.
        match crate::gml::file::read_file_bytes(&path) {
            Some(data) => ok_num(data.len() as f64),
            None => ok_num(-1.0),
        }
    }

    /// Creates the file if absent and leaves an existing one alone -- the original opens in append
    /// mode purely for that effect.
    fn file_create(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let path = args.first().map(arg_str).unwrap_or_default().into_owned();
        if crate::gml::file::read_file_bytes(&path).is_none() {
            crate::gml::vfs::write(&path, &[]);
        }
        ok_num(1.0)
    }

    // -- gm82core.dll: Include Files -------------------------------------------------------------
    //
    // The original walks the GM8 runner's own include-file table through a pointer at a hardcoded
    // address (`terrible_gm8_hacking.c`, and the file is named that for a reason). `Game` keeps the
    // same table as `included_files`, so these read it directly and the address games play no part.
    //
    // Semantics transcribed from that source: lookup is a case-*sensitive* `strcmp` on the name,
    // `include_file_name` is 0-based and answers "" out of range, and a file the runner has no data
    // for reads as empty rather than failing.
    //
    // Not a nicety for this family: a game that ships data as Include Files reads them at startup,
    // and `include_file_get_buffer`'s own wrapper calls `show_error` and stops the game when
    // `include_file_exists` says no -- which, before this, it always did.

    fn find_included(game: &Game, name: &str) -> Option<usize> {
        game.included_files.iter().position(|f| f.name == name)
    }

    fn included_data<'a>(game: &'a Game, name: &str) -> Option<&'a [u8]> {
        let index = find_included(game, name)?;
        game.included_files[index].data.as_deref()
    }

    fn include_file_count(game: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(game.included_files.len() as f64)
    }

    fn include_file_exists(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let name = args.first().map(arg_str).unwrap_or_default().into_owned();
        ok_num(f64::from(u8::from(find_included(game, &name).is_some())))
    }

    fn include_file_name(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let index = args.first().map(arg_f64).unwrap_or(-1.0);
        let name = usize::try_from(index as i64).ok().and_then(|i| game.included_files.get(i));
        Ok(Value::from(name.map(|f| f.name.clone()).unwrap_or_default()))
    }

    /// The size the *engine* can actually serve, which is the embedded data's length.
    ///
    /// The original reports the runner's stored size even for a file it holds no data for, but here
    /// that would be a trap: `include_file_get_buffer` sizes a buffer to this and then copies the
    /// data in, so a non-zero size with nothing behind it hands the game a buffer full of zeroes
    /// that reads like real content. 0 is the honest answer -- there is nothing to read.
    fn include_file_size(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let name = args.first().map(arg_str).unwrap_or_default().into_owned();
        ok_num(included_data(game, &name).map_or(0.0, |data| data.len() as f64))
    }

    fn include_file_get_string(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let name = args.first().map(arg_str).unwrap_or_default().into_owned();
        Ok(Value::from(included_data(game, &name).unwrap_or_default()))
    }

    /// `include_file_get_buffer(name, buffer)`'s DLL half: the wrapper has already sized the buffer
    /// to `include_file_size` and handed over its address.
    fn include_file_get_buffer(game: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let name = args.first().map(arg_str).unwrap_or_default().into_owned();
        let address = args.get(1).map(arg_f64).unwrap_or(0.0);
        let data = match included_data(game, &name) {
            Some(data) => data.to_vec(),
            None => return ok_num(0.0),
        };
        ok_num(f64::from(u8::from(super::super::gm82buf::write_at(address, &data))))
    }

    use wasm_bindgen::prelude::wasm_bindgen;

    #[wasm_bindgen(inline_js = "
export function emu_timezone_hours() {
    return -new Date().getTimezoneOffset() / 60;
}
export function emu_locale() {
    return (globalThis.navigator && navigator.language) || 'en-US';
}
")]
    extern "C" {
        fn emu_timezone_hours() -> f64;
        fn emu_locale() -> String;
    }

    fn timezone(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(emu_timezone_hours())
    }

    /// Windows LCID for the browser's language. Games branch on this to pick a localization, so
    /// always answering "English" would silently give every non-English player the wrong text.
    ///
    /// Approximate by construction: a browser reports a BCP-47 tag (`ja`, `pt-BR`) and Windows uses
    /// a numeric LCID, and the mapping is neither total nor stable. This covers the languages
    /// fangames actually ship and falls back to en-US, which is what a game would have got anyway.
    fn locale_lcid() -> f64 {
        let tag = emu_locale().to_lowercase();
        let primary = tag.split('-').next().unwrap_or("en").to_string();
        let region = tag.split('-').nth(1).unwrap_or("").to_string();
        let lcid: u32 = match primary.as_str() {
            "ja" => 0x0411,
            "ko" => 0x0412,
            "zh" => {
                if region == "tw" || region == "hk" {
                    0x0404
                } else {
                    0x0804
                }
            },
            "es" => 0x0c0a,
            "fr" => 0x040c,
            "de" => 0x0407,
            "it" => 0x0410,
            "ru" => 0x0419,
            "pt" => {
                if region == "br" {
                    0x0416
                } else {
                    0x0816
                }
            },
            "pl" => 0x0415,
            "nl" => 0x0413,
            _ => 0x0409, // en-US
        };
        f64::from(lcid)
    }

    fn system_language(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok_num(locale_lcid())
    }

    /// Extension functions carry *two* names: the one GML calls them by, and the `extname` that is
    /// the DLL's actual exported symbol. `game.rs` defines the external under the `extname`
    /// whenever one is set, so that -- not the GML name -- is what arrives here. The two differ
    /// for 22 of gm82core's functions (`get_timer` exports as `hrt_now`, `__gm82core_getfore` as
    /// `get_foreground_window`, ...), and matching only the GML name silently falls through to the
    /// generic no-op -- which is exactly how `get_timer` first came back as a flat 0 and pinned
    /// `delta_time` at zero.
    ///
    /// Both spellings are accepted per function: the extname is what actually resolves today, and
    /// the GML name costs nothing and covers any GM8.2 build that declares these without a remap.
    pub fn resolve(symbol: &str) -> Option<Function> {
        let f: super::ContextFn = match symbol {
            // gm82core.dll -- pure math (see `math_value`)
            "modwrap" => math_thunk!("modwrap"),
            "unlerp" => math_thunk!("unlerp"),
            "unclerp" => math_thunk!("unclerp"),
            "lerp2" => math_thunk!("lerp2"),
            "clerp2" => math_thunk!("clerp2"),
            "esign" => math_thunk!("esign"),
            "saturate" => math_thunk!("saturate"),
            "smoothstep" => math_thunk!("smoothstep"),
            "approach" => math_thunk!("approach"),
            "inch" => math_thunk!("inch"),
            "lerproach" => math_thunk!("lerproach"),
            "round_unbiased" => math_thunk!("round_unbiased"),
            "roundto" => math_thunk!("roundto"),
            "roundto_unbiased" => math_thunk!("roundto_unbiased"),
            "floorto" => math_thunk!("floorto"),
            "ceilto" => math_thunk!("ceilto"),
            "cosine" => math_thunk!("cosine"),
            "angle_difference" => math_thunk!("angle_difference"),
            "angle_mean" => math_thunk!("angle_mean"),
            "angle_abs" => math_thunk!("angle_abs"),
            "lerp_angle" => math_thunk!("lerp_angle"),
            "approach_angle" => math_thunk!("approach_angle"),
            "inch_angle" => math_thunk!("inch_angle"),
            "approach_angle_biased" => math_thunk!("approach_angle_biased"),
            "dsin" => math_thunk!("dsin"),
            "dcos" => math_thunk!("dcos"),
            "dtan" => math_thunk!("dtan"),
            "darcsin" => math_thunk!("darcsin"),
            "darccos" => math_thunk!("darccos"),
            "darctan" => math_thunk!("darctan"),
            "darctan2" => math_thunk!("darctan2"),
            "secant" => math_thunk!("secant"),
            "dsecant" => math_thunk!("dsecant"),
            "in_range" => math_thunk!("in_range"),
            "range_collision" => math_thunk!("range_collision"),
            "triangle_is_clockwise" => math_thunk!("triangle_is_clockwise"),
            "lengthdir_zx" => math_thunk!("lengthdir_zx"),
            "lengthdir_zy" => math_thunk!("lengthdir_zy"),
            "lengthdir_zz" => math_thunk!("lengthdir_zz"),
            "point_direction_pitch" => math_thunk!("point_direction_pitch"),
            "pivot_pos_x" => math_thunk!("pivot_pos_x"),
            "pivot_pos_y" => math_thunk!("pivot_pos_y"),
            "point_in_circle" => math_thunk!("point_in_circle"),
            "circle_in_circle" => math_thunk!("circle_in_circle"),
            "rectangle_in_circle" => math_thunk!("rectangle_in_circle"),
            "point_in_rectangle" => math_thunk!("point_in_rectangle"),
            "rectangle_in_rectangle" => math_thunk!("rectangle_in_rectangle"),
            "point_in_triangle" => math_thunk!("point_in_triangle"),
            "dot_product_normalized" => math_thunk!("dot_product_normalized"),
            "dot_product_3d_normalized" => math_thunk!("dot_product_3d_normalized"),
            "angle_difference_3d" => math_thunk!("angle_difference_3d"),
            "box_distance" => math_thunk!("box_distance"),
            "point_line_lerp" => math_thunk!("point_line_lerp"),
            "point_line_distance" => math_thunk!("point_line_distance"),
            "lines_intersect" => math_thunk!("lines_intersect"),
            "pack_bools" => math_thunk!("pack_bools"),
            "unpack_bool" => math_thunk!("unpack_bool"),
            "color_reverse" => math_thunk!("color_reverse"),
            "color_inverse" => math_thunk!("color_inverse"),

            // gm82core.dll -- host services
            "__gm82core_dllcheck" => core_dllcheck,
            "__gm82core_checkstart" => core_checkstart,
            "__gm82core_hrt_init" | "__hrt_init" => core_hrt_init,
            "hrt_time_now" | "get_timer" | "hrt_now" => core_hrt_now,
            "hrt_time_delta" | "get_delta" | "hrt_delta" => core_hrt_delta,
            "__gm82core_getfore" | "get_foreground_window" => core_getfore,
            "io_set_roomend_clear" => core_io_set_roomend_clear,
            "string_token_start" => token_start,
            "string_token_next" => token_next,
            "string_hex" => string_hex,
            "real_hex" => real_hex,
            "power_next" => power_next,
            "file_size" => file_size,
            "file_create" => file_create,
            "include_file_count" => include_file_count,
            "include_file_exists" => include_file_exists,
            "include_file_name" => include_file_name,
            "include_file_size" => include_file_size,
            "include_file_get_string" => include_file_get_string,
            "__gm82core_include_file_get_buffer" => include_file_get_buffer,
            "date_get_current_timezone" => timezone,
            "get_system_language" | "get_system_region" | "io_get_language" => system_language,

            // inst_meta.dll (the gm82test package). Its initializer/finalizer are the only part
            // that runs unprompted; the reflection functions it wraps are opt-in.
            "__gm82_inst_meta_init" | "__gm82_inst_meta_final" => inst_meta_init,

            _ => return None,
        };
        Some(Function::Runtime(f))
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;

    pub fn resolve(_symbol: &str) -> Option<Function> {
        None
    }
}

// The bare fn-pointer type `Function::Runtime` wraps (its own `ContextFunctionPtr` type alias in
// `gml.rs` is private, so this names the same signature structurally instead of by that name).
type ContextFn = fn(&mut Game, &mut Context, &[Value]) -> gml::Result<Value>;

pub use imp::resolve;
