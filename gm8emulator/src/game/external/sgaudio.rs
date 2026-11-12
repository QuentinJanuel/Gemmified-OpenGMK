//! Real reimplementation of `SGAudio.dll`, an OpenAL-backed GM8-era community DLL used by fangames
//! for external OGG background music that GM8's own sound system can't stream. Games ship it
//! alongside `OpenAL32.dll` + `wrap_oal.dll`. Not specific to any one game.
//!
//! Unlike `ssound.dll` (string handles) and `GMFMODSimple.dll` (separate sound and instance handle
//! spaces), SGAudio has a single concept: an **emitter**, a small integer index into a table, which
//! owns one file and one playback state at a time. `sga_TrackPlay` is the whole music API in
//! practice -- create-or-reuse an emitter, load a file into it, play it, hand the id back.
//!
//! The contract below was taken from two sources rather than guessed. The declared signatures come
//! from a real game's own `sga_Init.gml`, which `external_define`s all 27 exports (all `dll_cdecl`,
//! all `ty_real` returns, only `sga_CreateEmitter` and `sga_TrackPlay` taking a string). The
//! *behaviour* -- which is not in the GML, and where guessing would be invisible until a game
//! behaved oddly -- was read off the DLL's own x86:
//!
//!   * every emitter-addressed function validates the id against the table and returns `0` for an
//!     unknown one, `1` on success. Getters return `0` too, so a bad id is indistinguishable from a
//!     zero reading -- that is what the DLL does.
//!   * `sga_Stop` and `sga_Rewind` are exported at the *same address*: one function, stop + rewind.
//!   * `sga_IsPlaying` is `alGetSourcei(AL_SOURCE_STATE) == AL_PLAYING`, so a paused emitter is
//!     **not** playing (the opposite of FMOD's `Channel::isPlaying`; see `AudioManager::
//!     sga_is_playing`).
//!   * an emitter's gain is its own volume multiplied by the volume of *every* group set in its
//!     `sga_SetType` bitmask -- the DLL recomputes it with a 32-step unrolled loop over the mask.
//!   * `sga_GetVolume` reports what `sga_SetVolume` last set, not the live value a `sga_FadeVolume`
//!     ramp is moving; the DLL keeps the two in separate fields.
//!   * `sga_Init` returns `0`, not `1`, when it has already been initialized.
//!
//! Playback lives in `game/audio_wasm.rs`'s `sga_*` methods on `AudioManager` (layered on the FMOD
//! sound+instance pair it already models); this module is only the GML marshalling layer, the same
//! division as `ssound.rs` and `fmod.rs`. File bytes come from `gml::file`'s registry, shared with
//! the ordinary `file_exists`/`file_bin_open` GML API -- games pass Windows-style relative paths
//! (`"Music\bgm_lobby.ogg"`), which the VFS already resolves case-insensitively against the game's
//! own folder.
//!
//! Deliberately not implemented: the positional half (`sga_Set3DMode`, `sga_SetDistFactor`,
//! `sga_SetPosition`, `sga_SetListenerPosition`, `sga_SetListenerDirection`). Those are accepted and
//! recorded so a game's calls succeed and read back, but nothing pans by distance -- see
//! `SgaEmitter::three_d` for why that is a deliberate gap rather than an approximation.
//!
//! Wired in from `game/external.rs`'s `make_call`, via the same `external::Call::Emulated(Function)`
//! extension point `ssound.rs` and `fmod.rs` use.

use crate::{
    game::Game,
    gml::{self, Context, Function, Value},
};

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use crate::game::audio::SoundFormat;

    fn arg_str(v: &Value) -> std::borrow::Cow<'_, str> {
        crate::game::decode_gml_string(<&Value as Into<&[u8]>>::into(v))
    }

    fn arg_f64(v: &Value) -> f64 {
        match v {
            Value::Real(r) => (*r).into(),
            Value::Str(_) => arg_str(v).parse().unwrap_or(0.0),
        }
    }

    /// Emitter ids cross the DLL boundary as `double`s, and the DLL truncates toward zero before
    /// indexing its table. A game's "no emitter yet" sentinel is `-1`, which must stay negative
    /// (and so fail the lookup) rather than saturating to 0 and hitting a live emitter.
    fn arg_id(v: &Value) -> i32 {
        let n = arg_f64(v);
        if n.is_nan() { -1 } else { n.trunc().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32 }
    }

    /// Every function here declares `ty_real`, so the value must be a real `Value::Real` -- a
    /// numeric-looking string would survive assignment and comparison but break the moment a game
    /// did arithmetic on it (the same trap documented in `fmod.rs`).
    fn ok_num(n: f64) -> gml::Result<Value> {
        Ok(Value::Real(n.into()))
    }

    /// The DLL's universal "bad emitter id" answer, and also its failure answer for `sga_Init` /
    /// `sga_Free`.
    fn ok_fail() -> gml::Result<Value> {
        ok_num(0.0)
    }

    fn ok_bool(b: bool) -> gml::Result<Value> {
        ok_num(b as u8 as f64)
    }

    /// `.ogg` unless the name says `.wav`. SGAudio decodes by content, not extension, but the only
    /// two formats it is ever handed in practice are distinguishable this way, and the cache wants
    /// to be told which decoder to use.
    fn format_for(filename: &str) -> SoundFormat {
        match filename.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
            "wav" => SoundFormat::Wav,
            "mp3" => SoundFormat::Mp3,
            _ => SoundFormat::Ogg,
        }
    }

    /// Reads the file a `sga_CreateEmitter`/`sga_TrackPlay` argument names, warning once per missing
    /// path. Worth a warning rather than a silent `0`: a game whose music folder didn't make it into
    /// the upload is otherwise indistinguishable from one whose audio isn't emulated, which is
    /// exactly the confusion this whole module exists to clear up.
    fn load(filename: &str) -> Option<(Vec<u8>, SoundFormat)> {
        match crate::gml::file::read_file_bytes(filename) {
            Some(bytes) => Some((bytes, format_for(filename))),
            None => {
                warn_missing_once(filename);
                None
            },
        }
    }

    fn warn_missing_once(filename: &str) {
        use std::{cell::RefCell, collections::HashSet};
        thread_local! {
            static WARNED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
        }
        let key = filename.to_lowercase();
        if WARNED.with(|w| w.borrow_mut().insert(key)) {
            web_sys::console::warn_1(
                &format!("SGAudio: {filename} is not in this game's files, so it will be silent").into(),
            );
        }
    }

    fn sga_init(game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_bool(game.audio.sga_init())
    }

    fn sga_free(game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_bool(game.audio.sga_free())
    }

    fn sga_create_emitter(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(filename) = args.first() else { return ok_num(-1.0) };
        let filename = arg_str(filename).into_owned();
        let Some((bytes, format)) = load(&filename) else { return ok_num(-1.0) };
        match game.audio.sga_create_emitter(&filename, bytes.into_boxed_slice(), format) {
            Some(id) => ok_num(f64::from(id)),
            None => ok_num(-1.0),
        }
    }

    fn sga_destroy_emitter(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(id) = args.first() else { return ok_fail() };
        ok_bool(game.audio.sga_destroy_emitter(arg_id(id)))
    }

    /// `sga_TrackPlay(emitter_or_-1, filename, looping, type_flags)`, returning the emitter id.
    ///
    /// Reuses the emitter when the first argument names a live one and creates a new one otherwise
    /// (which is what `-1` means, and also what any stale id degrades to) -- matching the DLL, which
    /// validates the id against its table first and allocates only on a miss. Either way the track
    /// starts playing, 2D: the DLL pins every track to `AL_SOURCE_RELATIVE` at position (0,0,0)
    /// regardless of what `sga_Set3DMode` was told.
    fn sga_track_play(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, filename, looping, flags] = args else { return ok_num(-1.0) };
        let filename = arg_str(filename).into_owned();
        let looping = arg_f64(looping) != 0.0;
        let type_mask = arg_f64(flags).max(0.0) as u32;
        let Some((bytes, format)) = load(&filename) else { return ok_num(-1.0) };
        let id = arg_id(id);
        if game.audio.sga_exists(id) {
            if game.audio.sga_retrack(id, &filename, bytes.into_boxed_slice(), format, looping) {
                game.audio.sga_set_type(id, type_mask);
                return ok_num(f64::from(id))
            }
            return ok_num(-1.0)
        }
        match game.audio.sga_track_play(&filename, bytes.into_boxed_slice(), format, looping, type_mask) {
            Some(new_id) => ok_num(f64::from(new_id)),
            None => ok_num(-1.0),
        }
    }

    fn sga_play(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(id) = args.first() else { return ok_fail() };
        ok_bool(game.audio.sga_play(arg_id(id)))
    }

    fn sga_pause(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(id) = args.first() else { return ok_fail() };
        ok_bool(game.audio.sga_pause(arg_id(id)))
    }

    /// Backs both `sga_Stop` and `sga_Rewind` -- the same address in the real DLL.
    fn sga_stop(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(id) = args.first() else { return ok_fail() };
        ok_bool(game.audio.sga_stop(arg_id(id)))
    }

    fn sga_is_playing(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(id) = args.first() else { return ok_fail() };
        ok_bool(game.audio.sga_is_playing(arg_id(id)).unwrap_or(false))
    }

    fn sga_set_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, volume] = args else { return ok_fail() };
        ok_bool(game.audio.sga_set_volume(arg_id(id), arg_f64(volume)))
    }

    fn sga_get_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(id) = args.first() else { return ok_fail() };
        ok_num(game.audio.sga_get_volume(arg_id(id)).unwrap_or(0.0))
    }

    fn sga_fade_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, target, seconds] = args else { return ok_fail() };
        ok_bool(game.audio.sga_fade_volume(arg_id(id), arg_f64(target), arg_f64(seconds)))
    }

    fn sga_set_looping(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, looping] = args else { return ok_fail() };
        ok_bool(game.audio.sga_set_looping(arg_id(id), arg_f64(looping) != 0.0))
    }

    fn sga_set_panning(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, pan] = args else { return ok_fail() };
        ok_bool(game.audio.sga_set_panning(arg_id(id), arg_f64(pan)))
    }

    fn sga_set_pitch(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, pitch] = args else { return ok_fail() };
        ok_bool(game.audio.sga_set_pitch(arg_id(id), arg_f64(pitch)))
    }

    fn sga_set_play_offset(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, secs] = args else { return ok_fail() };
        ok_bool(game.audio.sga_set_play_offset(arg_id(id), arg_f64(secs)))
    }

    fn sga_get_play_offset(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(id) = args.first() else { return ok_fail() };
        ok_num(game.audio.sga_get_play_offset(arg_id(id)).unwrap_or(0.0))
    }

    /// Group ids are documented as 0-31; a real one always fits, and a negative or oversized one
    /// has no table slot to land in, so it fails rather than wrapping onto an unrelated group.
    fn arg_group(v: &Value) -> Option<usize> {
        let n = arg_f64(v);
        if n < 0.0 || !n.is_finite() { None } else { Some(n.trunc() as usize) }
    }

    fn sga_set_group_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [group, volume] = args else { return ok_fail() };
        let Some(group) = arg_group(group) else { return ok_fail() };
        ok_bool(game.audio.sga_set_group_volume(group, arg_f64(volume)))
    }

    fn sga_get_group_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(group) = args.first() else { return ok_fail() };
        let Some(group) = arg_group(group) else { return ok_fail() };
        ok_num(game.audio.sga_get_group_volume(group).unwrap_or(0.0))
    }

    fn sga_set_type(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, mask] = args else { return ok_fail() };
        ok_bool(game.audio.sga_set_type(arg_id(id), arg_f64(mask).max(0.0) as u32))
    }

    fn sga_get_type(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(id) = args.first() else { return ok_fail() };
        ok_num(f64::from(game.audio.sga_get_type(arg_id(id)).unwrap_or(0)))
    }

    fn sga_set_3d_mode(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, mode] = args else { return ok_fail() };
        ok_bool(game.audio.sga_set_3d_mode(arg_id(id), arg_f64(mode) != 0.0))
    }

    fn sga_set_dist_factor(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, factor] = args else { return ok_fail() };
        ok_bool(game.audio.sga_set_dist_factor(arg_id(id), arg_f64(factor)))
    }

    fn sga_set_position(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [id, x, y, z] = args else { return ok_fail() };
        ok_bool(game.audio.sga_set_position(arg_id(id), arg_f64(x), arg_f64(y), arg_f64(z)))
    }

    /// The listener half of the 3D model. Accepted and discarded rather than left to the generic
    /// no-op: a game that moves the listener every step would otherwise print a warning claiming
    /// something is missing, when what is missing is the positional model as a whole (already said
    /// once, by the emitter side) rather than these calls in particular.
    fn sga_listener_noop(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    pub fn resolve(symbol: &str) -> Option<Function> {
        let f: super::ContextFn = match symbol {
            "sga_Init" => sga_init,
            "sga_Free" => sga_free,
            "sga_CreateEmitter" => sga_create_emitter,
            "sga_DestroyEmitter" => sga_destroy_emitter,
            "sga_TrackPlay" => sga_track_play,
            "sga_Play" => sga_play,
            "sga_Pause" => sga_pause,
            // One function in the DLL, exported under two names.
            "sga_Stop" | "sga_Rewind" => sga_stop,
            "sga_IsPlaying" => sga_is_playing,
            "sga_SetVolume" => sga_set_volume,
            "sga_GetVolume" => sga_get_volume,
            "sga_FadeVolume" => sga_fade_volume,
            "sga_SetLooping" => sga_set_looping,
            "sga_SetPanning" => sga_set_panning,
            "sga_SetPitch" => sga_set_pitch,
            "sga_SetPlayOffset" => sga_set_play_offset,
            "sga_GetPlayOffset" => sga_get_play_offset,
            "sga_SetGroupVolume" => sga_set_group_volume,
            "sga_GetGroupVolume" => sga_get_group_volume,
            "sga_SetType" => sga_set_type,
            "sga_GetType" => sga_get_type,
            "sga_Set3DMode" => sga_set_3d_mode,
            "sga_SetDistFactor" => sga_set_dist_factor,
            "sga_SetPosition" => sga_set_position,
            "sga_SetListenerPosition" | "sga_SetListenerDirection" => sga_listener_noop,
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
