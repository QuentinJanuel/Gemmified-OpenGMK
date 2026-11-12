//! Real reimplementation of `MaizeMusic.dll` (MageCorn's "MaizeMusic Gamemaker BGM Plugin"), a
//! BASS-backed GM8-era extension used by fangames for external OGG/MP3 background music. Games
//! ship it as an extension package that unpacks `MaizeMusic.dll` plus `bass.dll` and `bass_fx.dll`,
//! and call it through the package's own `mm_*` GML wrappers. Not specific to any one game.
//!
//! The contract below came from two sources rather than guesswork. The declared signatures are the
//! extension's own `mm_init` script, which `external_define`s all 56 exports (every one
//! `dll_stdcall`, every one `ty_real` apart from `mmGetDeviceName`, and only the four loaders
//! taking a string). The *behaviour* -- which the GML cannot show, and where a wrong guess is
//! invisible until a game behaves oddly -- was read off the DLL's own x86, where every function
//! turns out to be a thin shim over one or two BASS calls:
//!
//!   * `mmLoadMusic(file, loop)` is `mmLoadMusicExt(file, loop, 0, 0)`: `BASS_StreamCreateFile`
//!     with `BASS_SAMPLE_FLOAT`, plus `BASS_SAMPLE_LOOP` when `loop` is non-zero, wrapped in a
//!     BASS_FX tempo stream. The handle comes back as an *unsigned* 32-bit value.
//!   * `mmMusicPlay` is `BASS_ChannelPlay(h, TRUE)` -- a **restart**, not a resume. `mmMusicResume`
//!     is the same call with `FALSE`, and `mmMusicStop` is `BASS_ChannelStop` plus a seek to 0.
//!   * `mmMusicIsActive` is `BASS_ChannelIsActive` passed through: `0` stopped, `1` playing,
//!     `3` paused. A *paused* stream therefore reads as active, which is load-bearing -- see
//!     `AudioManager::mm_pause`.
//!   * positions and lengths are in **seconds** (the DLL converts with
//!     `BASS_ChannelSeconds2Bytes`/`Bytes2Seconds`), volume and pan are linear `0..1` / `-1..1`
//!     (`BASS_ATTRIB_VOL` / `BASS_ATTRIB_PAN`), and `mmMusicSetSpeed` multiplies the stream's own
//!     `BASS_CHANNELINFO::freq`.
//!   * the return values are blunter than the documentation suggests: most setters answer `1`
//!     unconditionally, whatever BASS said, and `mmMusicSetVolume` answers `0` unconditionally.
//!     Each is matched below rather than tidied up, because a wrapper script is entitled to test
//!     them.
//!
//! Playback lives in `game/audio_wasm.rs`'s `mm_*` methods on `AudioManager`; this module is only
//! the GML marshalling layer, the same division as `ssound.rs`, `fmod.rs` and `sgaudio.rs`. File
//! bytes come from `gml::file`'s registry, shared with the ordinary `file_exists`/`file_bin_open`
//! GML API -- games pass Windows-style relative paths (`"Data\Bgm\0.OGG"`), which the VFS resolves
//! case-insensitively against the game's own folder.
//!
//! Deliberately not implemented, and inert rather than approximated so that a game using one is a
//! missing feature rather than a wrong-sounding one:
//!
//!   * **tempo and pitch** (`mmMusicSetTempo`, `mmMusicSetPitch`). These are BASS_FX's
//!     time-stretching, i.e. tempo *without* pitch change and pitch *without* tempo change;
//!     Web Audio's `playbackRate` can only do both at once, which is `mmMusicSetSpeed` and is
//!     implemented. Faking either with a rate change would be audibly wrong in a way silence is
//!     not.
//!   * **the FFT/level readouts** (`mmMusicGetData`, `mmMusicGetWaveData`, `mmMusicReadDataPos`,
//!     `mmMusicGetLeftLevel`, `mmMusciGetRightLevel` -- the DLL's own spelling). These exist for
//!     drawing spectrum analysers and VU meters; they would need an `AnalyserNode` per stream and
//!     affect nothing audible.
//!   * **the DirectX effects** (`mmMusicSetFX` and the eight `mmMusicEffect*` setters), which
//!     answer `0`, exactly as the real DLL does when `BASS_ChannelSetFX` fails.
//!   * **microphone recording** (`mmRecordStart` and friends) and **device selection**
//!     (`mmGetDeviceNumber`, `mmSetDevice`): there is one output device in a browser and no
//!     reason for a fangame to record from the microphone.
//!
//! Wired in from `game/external.rs`'s `make_call`, via the same `external::Call::Emulated(Function)`
//! extension point the other three reimplemented audio DLLs use.

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

    /// Handles cross the DLL boundary as `double`s holding an unsigned 32-bit BASS handle (the
    /// DLL's own conversion adds 2^32 to a negative one on the way out, so what GML holds is
    /// always positive). Anything that cannot be one -- a negative sentinel, a fraction, a
    /// resource id a game passed by mistake -- becomes a handle no stream has, which is how the
    /// real DLL treats it too: BASS validates the handle and does nothing.
    fn arg_handle(v: &Value) -> u32 {
        let n = arg_f64(v);
        if n.is_finite() && n >= 0.0 && n <= f64::from(u32::MAX) { n.trunc() as u32 } else { 0 }
    }

    /// Every function here declares `ty_real`, except `mmGetDeviceName`. The value must therefore
    /// be a real `Value::Real` -- a numeric-looking string would survive assignment and comparison
    /// but break the moment a game did arithmetic on it (the same trap documented in `fmod.rs`),
    /// and these values are arithmetic by nature: handles get stored in arrays and compared.
    fn ok_num(n: f64) -> gml::Result<Value> {
        Ok(Value::Real(n.into()))
    }

    /// What most of the DLL's setters answer, unconditionally.
    fn ok_one() -> gml::Result<Value> {
        ok_num(1.0)
    }

    fn ok_zero() -> gml::Result<Value> {
        ok_num(0.0)
    }

    /// `.ogg` unless the name says otherwise. BASS decodes by content, but the cache wants to be
    /// told which decoder to use, and these are the three formats it has.
    fn format_for(filename: &str) -> SoundFormat {
        match filename.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
            "wav" => SoundFormat::Wav,
            "mp3" => SoundFormat::Mp3,
            _ => SoundFormat::Ogg,
        }
    }

    /// Reads the file a loader names, warning once per missing path. Worth a warning rather than a
    /// silent `0`: a game whose music folder didn't make it into the upload is otherwise
    /// indistinguishable from one whose audio isn't emulated, which is exactly the confusion this
    /// module exists to clear up.
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
                &format!("MaizeMusic: {filename} is not in this game's files, so it will be silent").into(),
            );
        }
    }

    /// Said once for the whole of the time-stretching half of the API, rather than per function:
    /// what is missing is the time stretcher, not these calls in particular.
    fn warn_time_stretch_once() {
        use std::cell::Cell;
        thread_local! {
            static WARNED: Cell<bool> = const { Cell::new(false) };
        }
        if !WARNED.with(|w| w.replace(true)) {
            web_sys::console::warn_1(
                &"MaizeMusic: tempo/pitch shifting is not emulated, so music plays at its normal \
                  tempo and pitch (mmMusicSetSpeed, which changes both together, does work)"
                    .into(),
            );
        }
    }

    fn mm_init(game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_num(game.audio.mm_init() as u8 as f64)
    }

    /// `mmFree` returns 1 whatever happened -- `BASS_Free`'s own result is discarded.
    fn mm_free(game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        game.audio.mm_free();
        ok_one()
    }

    /// `mmLoadMusic(file, loop)`. `0` on failure, which is BASS's own.
    fn mm_load_music(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [filename, looping] = args else { return ok_zero() };
        mm_load(game, &arg_str(filename), arg_f64(looping) != 0.0)
    }

    /// `mmLoadMusicExt(file, loop, offset, length)`. The two extra arguments are
    /// `BASS_StreamCreateFile`'s own offset/length *into the file*, for a track packed inside a
    /// bigger archive. Nothing is known to use them, and the files this build can decode are
    /// whole files, so a non-zero one is honestly a failure rather than silently the whole file.
    fn mm_load_music_ext(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [filename, looping, offset, length] = args else { return ok_zero() };
        if arg_f64(offset) != 0.0 || arg_f64(length) != 0.0 {
            return ok_zero()
        }
        mm_load(game, &arg_str(filename), arg_f64(looping) != 0.0)
    }

    fn mm_load(game: &mut Game, filename: &str, looping: bool) -> gml::Result<Value> {
        let Some((bytes, format)) = load(filename) else { return ok_zero() };
        ok_num(f64::from(game.audio.mm_load(filename, bytes.into_boxed_slice(), format, looping)))
    }

    /// `mmLoadMusicMem`/`mmLoadMusicMemExt` take a pointer to bytes already in the game's memory,
    /// and `mmLoadMusicUrl` streams over HTTP. A GM8 pointer means nothing here, and a browser
    /// cannot fetch an arbitrary URL from inside the game loop synchronously, so both report
    /// failure (`0`) rather than pretending to have loaded something.
    fn mm_load_unsupported(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_zero()
    }

    /// `mmLoadMusicRes(sound, loop)` loads one of the game's *own* GM8 sound resources through
    /// BASS. Reported as failure: the sound's bytes are the embedded-sound system's, not this
    /// one's, and a game that wants a GM8 sound played has `sound_play` for it.
    fn mm_load_music_res(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_zero()
    }

    fn mm_free_music(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_zero() };
        ok_num(game.audio.mm_free_music(arg_handle(handle)) as u8 as f64)
    }

    fn mm_music_play(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_one() };
        game.audio.mm_play(arg_handle(handle));
        ok_one()
    }

    fn mm_music_pause(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_one() };
        game.audio.mm_pause(arg_handle(handle));
        ok_one()
    }

    fn mm_music_resume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_one() };
        game.audio.mm_resume(arg_handle(handle));
        ok_one()
    }

    fn mm_music_stop(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_one() };
        game.audio.mm_stop(arg_handle(handle));
        ok_one()
    }

    fn mm_music_is_active(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_zero() };
        ok_num(f64::from(game.audio.mm_is_active(arg_handle(handle))))
    }

    fn mm_music_get_length(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_zero() };
        ok_num(game.audio.mm_get_length_secs(arg_handle(handle)).unwrap_or(0.0))
    }

    fn mm_music_get_position(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_zero() };
        ok_num(game.audio.mm_get_position_secs(arg_handle(handle)).unwrap_or(0.0))
    }

    fn mm_music_set_position(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, secs] = args else { return ok_one() };
        game.audio.mm_set_position_secs(arg_handle(handle), arg_f64(secs));
        ok_one()
    }

    fn mm_music_get_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_zero() };
        ok_num(game.audio.mm_get_volume(arg_handle(handle)).unwrap_or(0.0))
    }

    /// `mmMusicSetVolume` really does answer `0` on success -- the DLL ends in `fldz`, discarding
    /// `BASS_ChannelSetAttribute`'s own result.
    fn mm_music_set_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, volume] = args else { return ok_zero() };
        game.audio.mm_set_volume(arg_handle(handle), arg_f64(volume));
        ok_zero()
    }

    fn mm_music_slide_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, volume, millis] = args else { return ok_one() };
        game.audio.mm_slide_volume(arg_handle(handle), arg_f64(volume), arg_f64(millis));
        ok_one()
    }

    fn mm_music_get_global_volume(game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_num(game.audio.mm_get_global_volume())
    }

    fn mm_music_set_global_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(volume) = args.first() else { return ok_one() };
        game.audio.mm_set_global_volume(arg_f64(volume));
        ok_one()
    }

    fn mm_music_get_pan(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_zero() };
        ok_num(game.audio.mm_get_pan(arg_handle(handle)).unwrap_or(0.0))
    }

    fn mm_music_set_pan(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, pan] = args else { return ok_one() };
        game.audio.mm_set_pan(arg_handle(handle), arg_f64(pan));
        ok_one()
    }

    /// `mmMusicGetSpeed` is `BASS_ATTRIB_FREQ / BASS_CHANNELINFO::freq`, i.e. the multiplier
    /// `mmMusicSetSpeed` was given -- `1.0` on a stream nobody has changed, and `0` for a handle
    /// that does not exist (the DLL divides its zeroed local by the info of a channel it never
    /// found).
    fn mm_music_get_speed(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_zero() };
        ok_num(game.audio.mm_get_speed(arg_handle(handle)).unwrap_or(0.0))
    }

    fn mm_music_set_speed(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, speed] = args else { return ok_one() };
        game.audio.mm_set_speed(arg_handle(handle), arg_f64(speed));
        ok_one()
    }

    fn mm_music_set_loop_section(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [handle, start, end] = args else { return ok_zero() };
        let (start, end) = (arg_f64(start), arg_f64(end));
        if end <= start {
            return ok_zero()
        }
        ok_num(game.audio.mm_set_loop_section(arg_handle(handle), start, end) as u8 as f64)
    }

    fn mm_music_remove_loop_section(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(handle) = args.first() else { return ok_one() };
        game.audio.mm_remove_loop_section(arg_handle(handle));
        ok_one()
    }

    /// `mmMusicSetTempo` answers `0` when `BASS_ChannelSetAttribute` fails, which is exactly what
    /// a stream with no tempo stream behind it does -- so reporting failure here is the real DLL's
    /// own answer for the case this build is in. `mmMusicSetPitch` has no such branch and answers
    /// `1` regardless, so it does too.
    fn mm_music_set_tempo(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        warn_time_stretch_once();
        ok_zero()
    }

    fn mm_music_set_pitch(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        warn_time_stretch_once();
        ok_one()
    }

    /// `mmMusicGetTempo` is `BASS_ATTRIB_TEMPO / 100 + 1` and `mmMusicGetPitch` the raw
    /// semitone attribute, so an untouched stream reads 1 and 0 respectively. Nothing shifts
    /// either, so that is what they always read.
    fn mm_music_get_tempo(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_one()
    }

    fn mm_music_get_pitch(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_zero()
    }

    /// The FFT/level/effect/record surface: accepted and inert. `0` is the real DLL's own failure
    /// answer for each of them (`BASS_ChannelGetData` returning -1, `BASS_ChannelSetFX` returning
    /// a null handle, `BASS_RecordStart` on a machine with no recording device), and a level or a
    /// data sample reads 0 on a channel BASS knows nothing about. See the module docs.
    fn mm_inert_zero(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_zero()
    }

    /// `mmMusicRemoveFX` and `mmMusicEffectReset` answer 1 regardless in the DLL.
    fn mm_inert_one(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_one()
    }

    /// `mmGetDeviceNumber`: one output device, which is the browser's. `mmSetDevice` then succeeds
    /// only for that one (`mmGetDeviceName` is the single `ty_string` export).
    fn mm_get_device_number(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    fn mm_get_device_name(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        Ok(Value::from("Default"))
    }

    pub fn resolve(symbol: &str) -> Option<Function> {
        let f: super::ContextFn = match symbol {
            "mmInit" => mm_init,
            "mmFree" => mm_free,
            "mmLoadMusic" => mm_load_music,
            "mmLoadMusicExt" => mm_load_music_ext,
            "mmLoadMusicMem" | "mmLoadMusicMemExt" | "mmLoadMusicUrl" => mm_load_unsupported,
            "mmLoadMusicRes" => mm_load_music_res,
            "mmFreeMusic" => mm_free_music,
            "mmMusicPlay" => mm_music_play,
            "mmMusicPause" => mm_music_pause,
            "mmMusicResume" => mm_music_resume,
            "mmMusicStop" => mm_music_stop,
            "mmMusicIsActive" => mm_music_is_active,
            "mmMusicGetLength" => mm_music_get_length,
            "mmMusicGetPosition" => mm_music_get_position,
            "mmMusicSetPosition" => mm_music_set_position,
            "mmMusicGetVolume" => mm_music_get_volume,
            "mmMusicSetVolume" => mm_music_set_volume,
            "mmMusicSlideVolume" => mm_music_slide_volume,
            "mmMusicGetGlobalVolume" => mm_music_get_global_volume,
            "mmMusicSetGlobalVolume" => mm_music_set_global_volume,
            "mmMusicGetPan" => mm_music_get_pan,
            "mmMusicSetPan" => mm_music_set_pan,
            "mmMusicGetSpeed" => mm_music_get_speed,
            "mmMusicSetSpeed" => mm_music_set_speed,
            "mmMusicSetLoopSection" => mm_music_set_loop_section,
            "mmMusicRemoveLoopSection" => mm_music_remove_loop_section,
            "mmMusicSetTempo" => mm_music_set_tempo,
            "mmMusicGetTempo" => mm_music_get_tempo,
            "mmMusicSetPitch" => mm_music_set_pitch,
            "mmMusicGetPitch" => mm_music_get_pitch,
            // The deliberate gaps. Grouped by what they answer, not by what they are; see the
            // module docs for why each is inert.
            "mmMusicGetData"
            | "mmMusicGetWaveData"
            | "mmMusicReadDataPos"
            | "mmMusicGetLeftLevel"
            // The DLL's own spelling of "Music".
            | "mmMusciGetRightLevel"
            | "mmMusicSetFX"
            | "mmMusicEffectChorus"
            | "mmMusicEffectEcho"
            | "mmMusicEffectFlanger"
            | "mmMusicEffectGargle"
            | "mmMusicEffectReverb"
            | "mmMusicEffectCompressor"
            | "mmMusicEffectEqualizer"
            | "mmRecordStart"
            | "mmRecordStop"
            | "mmRecordGetVolume"
            | "mmRecordSetInput"
            // BASS's CPU usage figures, which measure a mixer this build does not have.
            | "mmGetCPU"
            | "mmGetMusicCPU" => mm_inert_zero,
            "mmMusicRemoveFX" | "mmMusicEffectReset" | "mmSetDevice" => mm_inert_one,
            "mmGetDeviceNumber" => mm_get_device_number,
            "mmGetDeviceName" => mm_get_device_name,
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
