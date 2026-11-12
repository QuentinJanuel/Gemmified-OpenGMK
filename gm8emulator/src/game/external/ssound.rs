//! Real reimplementation of `ssound.dll` ("SS Sound"), a well-known GM8-era community DLL used by
//! many fangames for external OGG/WAV background music that GM8's own built-in sound system can't
//! handle. Not specific to any one game.
//!
//! Full function contract below was confirmed by decompiling a real game's GML wrapper scripts
//! (`ss_*.gml`, all `stdcall`, all string-in/string-out) rather than guessing at the DLL's binary
//! interface -- there is no way to run the actual DLL's x86 code in a browser at all, so this
//! reimplements its observable behavior against Web Audio instead (see `game/audio_wasm.rs`'s
//! `ss_*` methods on `AudioManager` for the actual playback logic; this module is just the GML
//! marshalling layer). `SS_LoadSound`'s file bytes come from `gml::file`'s external-files registry
//! (loose files from a folder upload, e.g. a game's BGM/ folder), shared with the general
//! `file_exists`/`file_bin_open` GML API rather than kept as a separate copy.
//!
//! Wired in from `game/external.rs`'s `make_call`, via the `external::Call::Emulated(Function)`
//! extension point that already existed (dispatched at `game.rs`'s `call_external`) but was never
//! constructed anywhere before this.

use crate::{
    game::Game,
    gml::{self, Context, Function, Value},
};

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;

    fn arg_str(v: &Value) -> std::borrow::Cow<'_, str> {
        crate::game::decode_gml_string(<&Value as Into<&[u8]>>::into(v))
    }

    fn arg_f64(v: &Value) -> f64 {
        match v {
            Value::Real(r) => (*r).into(),
            Value::Str(_) => arg_str(v).parse().unwrap_or(0.0),
        }
    }

    fn ok_str(s: impl Into<String>) -> gml::Result<Value> {
        Ok(Value::from(s.into()))
    }

    fn ok_num(n: f64) -> gml::Result<Value> {
        Ok(Value::from(format!("{n}")))
    }

    fn ok_fail() -> gml::Result<Value> {
        Ok(Value::from("0"))
    }

    /// `SS_GetSoundBytesPerSecond`'s implied unit: real ssound.dll almost certainly reports
    /// bytes for a decoded 16-bit PCM stream internally (Web Audio buffers are f32 regardless of
    /// the source format, so there's no "real" byte size to report otherwise). Documented
    /// approximation -- unlikely to matter to any game's actual logic, which typically uses this
    /// for coarse seeking, not exact-byte comparisons.
    fn bytes_per_second(sample_rate: u32, channels: u16) -> f64 {
        f64::from(sample_rate) * f64::from(channels) * 2.0
    }

    fn ss_init(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_str("Yes")
    }

    fn ss_unload(game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        game.audio.ss_unload();
        ok_str("Yes")
    }

    fn ss_load_sound(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(filename) = args.first() else { return ok_str("0") };
        let filename = arg_str(filename).into_owned();
        let Some(bytes) = crate::gml::file::read_file_bytes(&filename) else { return ok_str("0") };
        let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
        let format = match ext.as_str() {
            "wav" => crate::game::audio::SoundFormat::Wav,
            _ => crate::game::audio::SoundFormat::Ogg,
        };
        match game.audio.ss_register_file(&filename, bytes.into_boxed_slice(), format) {
            Some(handle) => ok_str(handle),
            None => ok_str("0"),
        }
    }

    fn ss_play_sound(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_fail() };
        ok_num(game.audio.ss_play_sound(&arg_str(handle)) as u8 as f64)
    }

    fn ss_loop_sound(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_fail() };
        ok_num(game.audio.ss_loop_sound(&arg_str(handle)) as u8 as f64)
    }

    fn ss_stop_sound(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_fail() };
        ok_num(game.audio.ss_stop_sound(&arg_str(handle)) as u8 as f64)
    }

    fn ss_pause_sound(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_fail() };
        ok_num(game.audio.ss_pause_sound(&arg_str(handle)) as u8 as f64)
    }

    fn ss_resume_sound(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_fail() };
        ok_num(game.audio.ss_resume_sound(&arg_str(handle)) as u8 as f64)
    }

    fn ss_free_sound(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_str("0") };
        game.audio.ss_free_sound(&arg_str(handle));
        ok_str("1")
    }

    fn ss_set_sound_vol(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, vol] = args else { return ok_fail() };
        ok_num(game.audio.ss_set_sound_vol(&arg_str(handle), arg_f64(vol)) as u8 as f64)
    }

    fn ss_get_sound_vol(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_num(-1.0) };
        match game.audio.ss_get_sound_vol(&arg_str(handle)) {
            Some(v) => ok_num(v),
            None => ok_num(-1.0),
        }
    }

    fn ss_set_sound_freq(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, hz] = args else { return ok_fail() };
        ok_num(game.audio.ss_set_sound_freq(&arg_str(handle), arg_f64(hz)) as u8 as f64)
    }

    fn ss_get_sound_freq(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_num(-1.0) };
        match game.audio.ss_get_sound_freq(&arg_str(handle)) {
            Some(v) => ok_num(v),
            None => ok_num(-1.0),
        }
    }

    fn ss_set_sound_pan(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, pan] = args else { return ok_fail() };
        ok_num(game.audio.ss_set_sound_pan(&arg_str(handle), arg_f64(pan)) as u8 as f64)
    }

    fn ss_get_sound_pan(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_num(0.0) };
        match game.audio.ss_get_sound_pan(&arg_str(handle)) {
            Some(v) => ok_num(v),
            None => ok_num(0.0),
        }
    }

    fn ss_get_sound_position(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_num(-1.0) };
        let handle = arg_str(handle);
        let Some((rate, ch)) = game.audio.ss_sound_format(&handle) else { return ok_num(-1.0) };
        let Some(secs) = game.audio.ss_get_sound_position_secs(&handle) else { return ok_num(-1.0) };
        ok_num(secs * bytes_per_second(rate, ch))
    }

    fn ss_set_sound_position(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, bytes] = args else { return ok_num(0.0) };
        let handle = arg_str(handle);
        let Some((rate, ch)) = game.audio.ss_sound_format(&handle) else { return ok_num(0.0) };
        let secs = arg_f64(bytes) / bytes_per_second(rate, ch);
        ok_num(game.audio.ss_set_sound_position_secs(&handle, secs) as u8 as f64)
    }

    fn ss_get_sound_length(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_num(0.0) };
        let handle = arg_str(handle);
        let Some(secs) = game.audio.ss_sound_duration_secs(&handle) else { return ok_num(0.0) };
        let Some((rate, ch)) = game.audio.ss_sound_format(&handle) else { return ok_num(0.0) };
        ok_num(secs * bytes_per_second(rate, ch))
    }

    fn ss_get_sound_bytes_per_second(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_num(0.0) };
        match game.audio.ss_sound_format(&arg_str(handle)) {
            Some((rate, ch)) => ok_num(bytes_per_second(rate, ch)),
            None => ok_num(0.0),
        }
    }

    fn ss_is_sound_playing(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_num(-1.0) };
        match game.audio.ss_is_sound_playing(&arg_str(handle)) {
            Some(b) => ok_num(b as u8 as f64),
            None => ok_num(-1.0),
        }
    }

    fn ss_is_sound_looping(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_num(-1.0) };
        match game.audio.ss_is_sound_looping(&arg_str(handle)) {
            Some(b) => ok_num(b as u8 as f64),
            None => ok_num(-1.0),
        }
    }

    fn ss_is_sound_paused(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_num(-1.0) };
        match game.audio.ss_is_sound_paused(&arg_str(handle)) {
            Some(b) => ok_num(b as u8 as f64),
            None => ok_num(-1.0),
        }
    }

    pub fn resolve(symbol: &str) -> Option<Function> {
        let f: super::ContextFn = match symbol {
            "SS_Init" => ss_init,
            "SS_Unload" => ss_unload,
            "SS_LoadSound" => ss_load_sound,
            "SS_PlaySound" => ss_play_sound,
            "SS_LoopSound" => ss_loop_sound,
            "SS_StopSound" => ss_stop_sound,
            "SS_PauseSound" => ss_pause_sound,
            "SS_ResumeSound" => ss_resume_sound,
            "SS_FreeSound" => ss_free_sound,
            "SS_SetSoundVol" => ss_set_sound_vol,
            "SS_GetSoundVol" => ss_get_sound_vol,
            "SS_SetSoundFreq" => ss_set_sound_freq,
            "SS_GetSoundFreq" => ss_get_sound_freq,
            "SS_SetSoundPan" => ss_set_sound_pan,
            "SS_GetSoundPan" => ss_get_sound_pan,
            "SS_GetSoundPosition" => ss_get_sound_position,
            "SS_SetSoundPosition" => ss_set_sound_position,
            "SS_GetSoundLength" => ss_get_sound_length,
            "SS_GetSoundBytesPerSecond" => ss_get_sound_bytes_per_second,
            "SS_IsSoundPlaying" => ss_is_sound_playing,
            "SS_IsSoundLooping" => ss_is_sound_looping,
            "SS_IsSoundPaused" => ss_is_sound_paused,
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
