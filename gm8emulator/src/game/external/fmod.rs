//! Real reimplementation of `GMFMODSimple.dll`, a GM8-era wrapper around FMOD Ex used by many
//! fangames for external background music. Not specific to any one game.
//!
//! Full function contract below was confirmed by decompiling a real game's GML wrapper scripts
//! (`FMODXxx.gml`, thin `external_call` passthroughs to `global.dll_FMODXxx`) -- FMOD's declared
//! surface covers ~146 functions (3D audio, DSP effects, spectrum analysis, mic recording, tags,
//! password/encryption, etc.), all intentionally out of scope here: load/play/loop/stop/volume/
//! pause/mute/pitch/pan/frequency/loop-points/group management (`FMODSoundSetGroup`,
//! `FMODGroupSetVolume/Muted`, `FMODGroupStop`, `FMODAllStop`, `FMODMasterSetVolume` -- games
//! route distinct sound categories, e.g. sound effects vs. music, through different numeric group
//! ids specifically so one can be stopped/ducked without touching the other) cover everything a
//! typical 2D fangame actually calls. Anything outside that surface is intentionally
//! unimplemented -- unrecognized symbols fall through to `external.rs`'s generic dummy fallback
//! rather than panicking.
//!
//! Unlike `ssound.dll`'s single string-handle model, FMOD conceptually separates a loaded "Sound"
//! (`FMODSoundAdd`'s numeric return) from a playing "Instance" (`FMODSoundPlay`/`FMODSoundLoop`'s
//! numeric return, a *different* handle space) -- multiple concurrent instances can play from one
//! loaded sound. See `game/audio_wasm.rs`'s `fmod_*` methods for the actual Web Audio playback
//! logic; this module is just the GML marshalling layer, same division as `ssound.rs`.
//!
//! Wired in from `game/external.rs`'s `make_call`, via the same `external::Call::Emulated
//! (Function)` extension point `ssound.rs` already uses.

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

    fn arg_u64(v: &Value) -> u64 {
        arg_f64(v).max(0.0) as u64
    }

    // FMOD's declared return type is `ty_real` for every function here except
    // `fmod_get_error_string` (the one place `ok_str` is used) -- unlike ssound.dll's entirely
    // string-based contract, `Call::Emulated` never coerces a Runtime function's return value to
    // match the DLL's declared type, so this must construct a real `Value::Real` directly rather
    // than a numeric-looking string (which happens to work fine for direct assignment/comparison
    // due to GML's loose typing, but breaks the moment a game does real arithmetic on the result,
    // e.g. `FMODSoundGetLength(sound) / 2` -- exactly what surfaced this).
    fn ok_num(n: f64) -> gml::Result<Value> {
        Ok(Value::Real(n.into()))
    }

    fn ok_str(s: impl Into<String>) -> gml::Result<Value> {
        Ok(Value::from(s.into()))
    }

    fn ok_fail() -> gml::Result<Value> {
        ok_num(0.0)
    }

    /// Every one of FMOD's `Set`-shaped functions returns nonzero on success, so a function that
    /// deliberately does nothing still has to say it worked -- a game that checks the result (or,
    /// worse, retries until it succeeds) must not be told its call failed.
    fn ok_accepted() -> gml::Result<Value> {
        ok_num(1.0)
    }

    /// The second 40-byte table GMFMODSimple's file cipher XORs with, recovered from the DLL.
    ///
    /// `FMODEncryptFile`'s cipher is `plain[i] = cipher[i] ^ A[k] ^ B[k]`, where `k` cycles `0..40`
    /// -- that loop is at `.text:0x640436ba` in the shipped DLL, two `xorb`s and a `cmp $0x27`
    /// wrap. `A` is the uppercase SHA-1 hex digest of the password (40 characters, which is why the
    /// period is 40); `B` is this, the digest of whatever default string the DLL hashes when no
    /// password is set. The literal isn't in the binary -- it's computed at startup -- so it was
    /// recovered from known plaintext instead: every WAV begins `RIFF????WAVEfmt `, which pins most
    /// of the keystream directly, and the rest fell out of the header's own arithmetic
    /// (`byteRate == rate * channels * bits/8`) being solvable across 138 files at once.
    ///
    /// Treated as a constant because it is one: it depends only on the DLL, and every game ships
    /// the same GMFMODSimple. Verified against I Wanna Kill the Kamilia 3, whose password is
    /// `sexking6974` -- 134 of its 138 encrypted effects decrypt to structurally valid PCM WAV,
    /// and the other four are files whose own headers disagree with their contents in the original.
    const PASSWORD_TABLE: &[u8; 40] = b"12C7EC3DF247345FA6566E7AA7DD62029B3AFF56";

    thread_local! {
        /// The keystream `FMODSetPassword` left for the loader, or `None` while no password is set
        /// (the overwhelmingly common case -- almost no game encrypts its audio).
        static PASSWORD_KEY: std::cell::RefCell<Option<[u8; 40]>> = const { std::cell::RefCell::new(None) };
    }

    /// Drops any password from a previous game in this page session, so one game's key can never
    /// decrypt (i.e. corrupt) the next one's audio.
    pub fn reset_password() {
        PASSWORD_KEY.with(|k| *k.borrow_mut() = None);
    }

    /// The 40-byte repeating keystream a password decrypts with. See `PASSWORD_TABLE`.
    pub(super) fn keystream_for(password: &str) -> [u8; 40] {
        use sha1::{Digest, Sha1};
        // Uppercase hex, which is what the DLL hashes into -- lowercase gives a different key.
        let digest = format!("{:X}", Sha1::digest(password.as_bytes()));
        let mut key = [0u8; 40];
        for (slot, (a, b)) in key.iter_mut().zip(digest.bytes().zip(PASSWORD_TABLE.iter())) {
            *slot = a ^ b;
        }
        key
    }

    fn fmod_set_password(_game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(password) = args.first() else { return ok_fail() };
        let password = arg_str(password);
        // An empty password clears it, rather than installing the key for "".
        let key = if password.is_empty() { None } else { Some(keystream_for(password.as_ref())) };
        PASSWORD_KEY.with(|k| *k.borrow_mut() = key);
        ok_num(1.0)
    }

    /// Whether `bytes` already opens with a container this engine can decode. Used to tell an
    /// encrypted file from a plain one, because nothing in the file says which it is: a game sets
    /// one password for the whole run and then loads both kinds through the same call. Kamilia 3
    /// is exactly that -- its effects are encrypted WAV, its music is plain Ogg, and both are
    /// loaded by `FMODSoundAdd` after the password is set.
    fn looks_like_audio(bytes: &[u8]) -> bool {
        matches!(bytes.get(..4), Some(b"RIFF" | b"OggS" | b"fLaC" | b"FORM"))
            || matches!(bytes.get(..3), Some(b"ID3"))
            // MPEG frame sync: eleven set bits.
            || matches!(bytes.get(..2), Some([0xff, b]) if b & 0xe0 == 0xe0)
    }

    /// Decrypts a sound file if it needs it, leaving it alone otherwise.
    ///
    /// Both guards matter. A file that is already recognisable is never touched, so a password set
    /// for the effects cannot corrupt the music; and a decryption that fails to produce anything
    /// recognisable is discarded rather than handed on, so a wrong password degrades to the same
    /// failed load it was before instead of feeding the decoder garbage.
    fn decrypt_if_needed(bytes: Vec<u8>) -> Vec<u8> {
        if looks_like_audio(&bytes) {
            return bytes
        }
        let Some(key) = PASSWORD_KEY.with(|k| *k.borrow()) else { return bytes };
        let decrypted: Vec<u8> = bytes.iter().enumerate().map(|(i, b)| b ^ key[i % key.len()]).collect();
        if looks_like_audio(&decrypted) { decrypted } else { bytes }
    }

    /// Warns once per page load that a 3D-positional call was accepted and ignored.
    ///
    /// Nothing here models a 3D sound field: there is no listener, no per-source position, no
    /// distance attenuation, no doppler, and no occlusion geometry. Sounds positioned in 3D still
    /// *play*, at full volume and centred, which is the behaviour that matters -- these games
    /// invariably leave the listener at the origin and put their music at the origin too, so the
    /// 3D path is decoration around what is really ordinary stereo playback. The alternative
    /// (rejecting the call, the way the real DLL rejects `Channel::set3DAttributes` on a 2D sound)
    /// is exactly what produced the bug this all came from: `FMODSoundPlay3d` returning 0 silently
    /// cost a game its entire soundtrack.
    ///
    /// Web Audio does have `PannerNode`/`AudioListener` and could do this properly; it is simply
    /// far more work than registering the functions, and no game found so far needs it.
    fn warn_3d_ignored_once() {
        use std::cell::Cell;
        thread_local! {
            static WARNED: Cell<bool> = const { Cell::new(false) };
        }
        if !WARNED.with(|w| w.replace(true)) {
            web_sys::console::warn_1(
                &"GMFMODSimple: 3D positional audio is not emulated -- the listener, per-source \
                  positions, cones, doppler, spread, speaker mix and occlusion geometry are all \
                  accepted and ignored. Sounds still play, but centred and at full volume."
                    .into(),
            );
        }
    }

    /// Shared body for every 3D-positional call that has nothing to store and nothing to do: warn
    /// once, report success, move on. One function rather than forty identical ones, since a bare
    /// `fn` pointer can't carry which symbol it was registered under and the warning is deliberately
    /// about the whole family anyway.
    fn fmod_3d_ignored(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        warn_3d_ignored_once();
        ok_accepted()
    }

    /// `FMODInstanceGet3DPanLevel` / `FMODInstanceGet3DSpread` -- the two 3D reads whose FMOD
    /// defaults aren't 0, so returning 0 would be a worse answer than returning the value a game
    /// that never set them would really have got.
    fn fmod_instance_get_3d_pan_level(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        warn_3d_ignored_once();
        ok_num(1.0)
    }

    fn fmod_instance_get_3d_spread(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        warn_3d_ignored_once();
        ok_num(0.0)
    }

    /// `FMODBlockerGetStrength` -- 0, i.e. "nothing is being occluded", which is the one answer in
    /// this family that is literally true of the audio the player will hear. The rest of the
    /// blocker calls go through `fmod_3d_ignored` and report success so a game's blocker handles
    /// and enable flags stay coherent, but reporting a nonzero *occlusion strength* would be a
    /// number a game could act on (ducking a voice line, say) that nothing here is doing.
    fn fmod_blocker_get_strength(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        warn_3d_ignored_once();
        ok_num(0.0)
    }

    /// A tracker module this engine can't perform -- `.xm` and `.it`, which `tracker.rs` doesn't
    /// implement, or a corrupt file.
    ///
    /// Worth naming in the log rather than failing quietly, because the game's own reaction is a
    /// `show_error("Sound does not exist: ...")` dialog from gm82snd's wrapper, several steps
    /// removed from the actual cause, and the file it names is the *sound* name rather than the
    /// path.
    fn warn_tracker_unsupported(filename: &str) {
        web_sys::console::warn_1(
            &format!(
                "tracker module could not be loaded: {filename} -- only ProTracker (.mod) and \
                 Scream Tracker 3 (.s3m) modules are supported, not .xm or .it. This track will \
                 be silent and the game may report the sound as missing."
            )
            .into(),
        );
    }

    fn fmod_init(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    fn fmod_free(game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        game.audio.fmod_unload();
        ok_num(1.0)
    }

    fn fmod_update(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    fn fmod_update_take_over_while_locked(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    fn fmod_update_take_over_done(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_num(1.0)
    }

    /// `FMODSoundAdd(file, 3d, streamed)` and `FMODSoundAddAsyncStream(file, 3d)` both land here.
    /// Argument order was read out of the real DLL rather than guessed: `FMODSoundAdd`'s 2nd
    /// argument picks mode `FMOD_3D|3D_LINEARROLLOFF|SOFTWARE|LOOP_NORMAL|ACCURATETIME` over
    /// `FMOD_2D|...`, and its 3rd picks `System::createStream` over `System::createSound`.
    ///
    /// Neither flag changes anything here -- there is no 3D field to place the sound in, and
    /// everything is decoded into memory up front, so "streamed" has no separate code path -- but
    /// both are recorded for `FMODSoundIs3d`/`FMODSoundIsStreamed` to read back, and the 3D one is
    /// deliberately *not* warned about: loading a sound as 3D is harmless on its own, and warning
    /// here would fire for games that then only ever play it through the plain 2D calls.
    fn fmod_load_sound(game: &mut Game, filename: &str, is_3d: bool, streamed: bool) -> gml::Result<Value> {
        let Some(bytes) = crate::gml::file::read_file_bytes(filename) else { return ok_fail() };
        // Before anything looks at the bytes: an encrypted file is not a `.wav` in any sense a
        // decoder would recognise until this runs. See `decrypt_if_needed`.
        let bytes = decrypt_if_needed(bytes);
        let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();

        // Tracker modules first, and by extension rather than by sniffing every file: a `.mod` is
        // a sequencer program, not encoded audio, so it takes a different path entirely (see
        // `audio_wasm/tracker.rs`). gm82snd advertises `.mod`/`.s3m`/`.xm`/`.it` and fangames use
        // them for their whole soundtrack -- renex engine has nine `.mod`s and one `.s3m`.
        // `.xm`/`.it` are listed here so `tracker::parse` gets the chance to reject them by
        // content; it currently does, and the caller then reports a failed load, which is honest.
        if matches!(ext.as_str(), "mod" | "s3m" | "xm" | "it") {
            let Some(module) = crate::game::audio::tracker::parse(&bytes) else {
                warn_tracker_unsupported(filename);
                return ok_fail()
            };
            let name = filename.rsplit(['/', '\\']).next().unwrap_or(filename).to_owned();
            return match game.audio.fmod_register_tracker(module, &name, is_3d, streamed) {
                Some(handle) => ok_num(handle as f64),
                None => ok_fail(),
            }
        }

        // Unlike `ssound.dll` (only ever seen used with OGG in this project), FMOD games commonly
        // load MP3 -- dispatch explicitly by extension rather than assuming everything
        // non-`.wav` is OGG, so MP3 files go through the decoder that replicates the documented
        // native GM8/rmp3 channel-mismatch-skip quirk (`mp3.rs`) instead of the OGG-oriented one.
        //
        // Compressed bytes are registered into the shared audio cache; PCM decode is deferred
        // until first play when lazy mode is on.
        let format = match ext.as_str() {
            "wav" => crate::game::audio::SoundFormat::Wav,
            "mp3" => crate::game::audio::SoundFormat::Mp3,
            _ => crate::game::audio::SoundFormat::Ogg,
        };
        match game.audio.fmod_register_file(filename, bytes.into_boxed_slice(), format, is_3d, streamed) {
            Some(handle) => ok_num(handle as f64),
            None => ok_fail(),
        }
    }

    fn fmod_sound_add(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(filename) = args.first() else { return ok_fail() };
        let filename = arg_str(filename).into_owned();
        let is_3d = args.get(1).map(arg_f64).unwrap_or(0.0) != 0.0;
        let streamed = args.get(2).map(arg_f64).unwrap_or(0.0) != 0.0;
        fmod_load_sound(game, &filename, is_3d, streamed)
    }

    /// `FMODSoundAddAsyncStream(file, 3d)` -- the real DLL hands back a handle immediately and
    /// loads on a background thread, with `FMODSoundAsyncReady` polled until the data has arrived.
    /// Decoding here is synchronous and already done by the time this returns, so the sound is
    /// simply ready on the first poll; a game's "still loading" branch just never runs.
    fn fmod_sound_add_async_stream(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(filename) = args.first() else { return ok_fail() };
        let filename = arg_str(filename).into_owned();
        let is_3d = args.get(1).map(arg_f64).unwrap_or(0.0) != 0.0;
        fmod_load_sound(game, &filename, is_3d, true)
    }

    /// `FMODSoundAsyncReady` / `FMODInstanceAsyncOK` -- see `fmod_sound_add_async_stream`. True
    /// for any handle that exists at all, false for one that doesn't, so a game polling on a load
    /// that actually failed still doesn't spin forever.
    fn fmod_sound_async_ready(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_sound_exists(arg_u64(sound)) as u8 as f64)
    }

    fn fmod_instance_async_ok(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_instance_get_sound(arg_u64(instance)).is_some() as u8 as f64)
    }

    fn fmod_sound_play(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        let paused = args.get(1).map(arg_f64).unwrap_or(0.0) != 0.0;
        match game.audio.fmod_play(arg_u64(sound), false, paused) {
            Some(instance) => ok_num(instance as f64),
            None => ok_fail(),
        }
    }

    fn fmod_sound_loop(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        let paused = args.get(1).map(arg_f64).unwrap_or(0.0) != 0.0;
        match game.audio.fmod_play(arg_u64(sound), true, paused) {
            Some(instance) => ok_num(instance as f64),
            None => ok_fail(),
        }
    }

    /// `FMODSoundPlay3d(sound, x, y, z, paused)` / `FMODSoundLoop3d(sound, x, y, z, paused)`.
    ///
    /// Identical to `FMODSoundPlay`/`FMODSoundLoop` apart from the position, which is dropped (see
    /// `warn_3d_ignored_once`). Confirmed against the real DLL rather than assumed: both start a
    /// channel exactly the way `FMODSoundInstanciate` does, then `Channel::setMode` with
    /// `LOOP_OFF|3D_WORLDRELATIVE` (Play3d) or `LOOP_NORMAL|3D_WORLDRELATIVE` (Loop3d), then
    /// `Channel::set3DAttributes(&(x,y,z), &(0,0,0))`, then unpause iff the 5th argument is 0.
    ///
    /// These being unregistered is what started all this: they fell through to
    /// `external.rs`'s generic dummy, the game stored the resulting 0 as its music channel, and
    /// every later `FMODInstanceSetPitch`/`SetPosition`/`Stop` on that non-existent instance
    /// no-opped too -- a game with working sound effects and no music at all.
    fn fmod_sound_play_3d(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        fmod_play_3d(game, args, false)
    }

    fn fmod_sound_loop_3d(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        fmod_play_3d(game, args, true)
    }

    fn fmod_play_3d(game: &mut Game, args: &[Value], looping: bool) -> gml::Result<Value> {
        warn_3d_ignored_once();
        let Some(sound) = args.first() else { return ok_fail() };
        let paused = args.get(4).map(arg_f64).unwrap_or(0.0) != 0.0;
        match game.audio.fmod_play(arg_u64(sound), looping, paused) {
            Some(instance) => ok_num(instance as f64),
            None => ok_fail(),
        }
    }

    /// `FMODSoundInstanciate` -- creates a channel for a sound and leaves it paused, for a game
    /// that wants to set volume/pan/position before anything is audible. The real DLL is literally
    /// this: `System::playSound(FMOD_CHANNEL_FREE, sound, paused: true, &channel)`, which is also
    /// the helper `FMODSoundPlay3d` itself calls before unpausing.
    fn fmod_sound_instanciate(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        match game.audio.fmod_play(arg_u64(sound), false, true) {
            Some(instance) => ok_num(instance as f64),
            None => ok_fail(),
        }
    }

    fn fmod_instance_stop(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_instance_stop(arg_u64(instance)) as u8 as f64)
    }

    fn fmod_instance_set_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [instance, volume] = args else { return ok_fail() };
        ok_num(game.audio.fmod_instance_set_volume(arg_u64(instance), arg_f64(volume)) as u8 as f64)
    }

    fn fmod_instance_get_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_instance_get_volume(arg_u64(instance)).unwrap_or(0.0))
    }

    fn fmod_instance_set_paused(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [instance, pause] = args else { return ok_fail() };
        ok_num(game.audio.fmod_instance_set_paused(arg_u64(instance), arg_f64(pause) != 0.0) as u8 as f64)
    }

    fn fmod_instance_get_paused(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_instance_get_paused(arg_u64(instance)).unwrap_or(false) as u8 as f64)
    }

    fn fmod_instance_get_position(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_instance_get_position(arg_u64(instance)).unwrap_or(0.0))
    }

    fn fmod_instance_set_position(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [instance, pos] = args else { return ok_fail() };
        ok_num(game.audio.fmod_instance_set_position(arg_u64(instance), arg_f64(pos)) as u8 as f64)
    }

    fn fmod_instance_sound_get_length(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_instance_sound_get_length_ms(arg_u64(instance)).unwrap_or(0.0))
    }

    fn fmod_instance_is_playing(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_instance_is_playing(arg_u64(instance)).unwrap_or(false) as u8 as f64)
    }

    fn fmod_sound_set_group(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [sound, group] = args else { return ok_fail() };
        ok_num(game.audio.fmod_set_sound_group(arg_u64(sound), arg_u64(group)) as u8 as f64)
    }

    fn fmod_sound_get_length(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_sound_get_length_ms(arg_u64(sound)).unwrap_or(0.0))
    }

    fn fmod_sound_set_max_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [sound, volume] = args else { return ok_fail() };
        ok_num(game.audio.fmod_set_sound_max_volume(arg_u64(sound), arg_f64(volume)) as u8 as f64)
    }

    fn fmod_sound_get_max_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_get_sound_max_volume(arg_u64(sound)).unwrap_or(0.0))
    }

    fn fmod_sound_set_loop_points(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [sound, start, end] = args else { return ok_fail() };
        ok_num(game.audio.fmod_set_sound_loop_points(arg_u64(sound), arg_f64(start), arg_f64(end)) as u8 as f64)
    }

    fn fmod_sound_is_3d(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_sound_is_3d(arg_u64(sound)).unwrap_or(false) as u8 as f64)
    }

    fn fmod_sound_is_streamed(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_sound_is_streamed(arg_u64(sound)).unwrap_or(false) as u8 as f64)
    }

    fn fmod_sound_get_num_channels(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_sound_get_num_channels(arg_u64(sound)).unwrap_or(0) as f64)
    }

    fn fmod_sound_set_loop_count(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [sound, count] = args else { return ok_fail() };
        ok_num(game.audio.fmod_set_sound_loop_count(arg_u64(sound), arg_f64(count) as i32) as u8 as f64)
    }

    fn fmod_sound_get_loop_count(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        ok_num(f64::from(game.audio.fmod_get_sound_loop_count(arg_u64(sound)).unwrap_or(-1)))
    }

    /// `FMODSoundSet3dMinMaxDistance` / `FMODSoundGetMaxDist` -- stored and echoed back but never
    /// applied, since nothing here has a distance to attenuate over. Kept as real storage rather
    /// than folded into `fmod_3d_ignored` so a game that sets a max distance and reads it back
    /// (to fade a sound out by hand at the edge of its own range, say) gets its own number.
    fn fmod_sound_set_3d_min_max_distance(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        warn_3d_ignored_once();
        let [sound, min, max] = args else { return ok_fail() };
        ok_num(game.audio.fmod_set_sound_min_max_distance(arg_u64(sound), arg_f64(min), arg_f64(max)) as u8 as f64)
    }

    fn fmod_sound_get_max_dist(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_get_sound_max_distance(arg_u64(sound)).unwrap_or(0.0))
    }

    /// `FMODInstanceSet3dMinMaxDistance` / `FMODInstanceGetMaxDist` -- the per-instance twins,
    /// which the real DLL also resolves onto the underlying sound's record.
    fn fmod_instance_set_3d_min_max_distance(
        game: &mut Game,
        _ctx: &mut Context,
        args: &[Value],
    ) -> gml::Result<Value> {
        warn_3d_ignored_once();
        let [instance, min, max] = args else { return ok_fail() };
        let Some(sound) = game.audio.fmod_instance_get_sound(arg_u64(instance)) else { return ok_fail() };
        ok_num(game.audio.fmod_set_sound_min_max_distance(sound, arg_f64(min), arg_f64(max)) as u8 as f64)
    }

    fn fmod_instance_get_max_dist(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_instance_get_max_distance(arg_u64(instance)).unwrap_or(0.0))
    }

    fn fmod_sound_free(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(sound) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_sound_free(arg_u64(sound)) as u8 as f64)
    }

    fn fmod_instance_set_frequency(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [instance, freq] = args else { return ok_fail() };
        ok_num(game.audio.fmod_instance_set_frequency(arg_u64(instance), arg_f64(freq)) as u8 as f64)
    }

    fn fmod_instance_get_frequency(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_instance_get_frequency(arg_u64(instance)).unwrap_or(0.0))
    }

    fn fmod_instance_set_pitch(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [instance, pitch] = args else { return ok_fail() };
        ok_num(game.audio.fmod_instance_set_pitch(arg_u64(instance), arg_f64(pitch)) as u8 as f64)
    }

    fn fmod_instance_get_pitch(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_instance_get_pitch(arg_u64(instance)).unwrap_or(0.0))
    }

    fn fmod_instance_set_pan(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [instance, pan] = args else { return ok_fail() };
        ok_num(game.audio.fmod_instance_set_pan(arg_u64(instance), arg_f64(pan)) as u8 as f64)
    }

    fn fmod_instance_get_pan(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_instance_get_pan(arg_u64(instance)).unwrap_or(0.0))
    }

    fn fmod_instance_set_muted(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [instance, mute] = args else { return ok_fail() };
        ok_num(game.audio.fmod_instance_set_muted(arg_u64(instance), arg_f64(mute) != 0.0) as u8 as f64)
    }

    fn fmod_instance_get_muted(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_instance_get_muted(arg_u64(instance)).unwrap_or(false) as u8 as f64)
    }

    fn fmod_instance_get_sound(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_fail() };
        ok_num(game.audio.fmod_instance_get_sound(arg_u64(instance)).unwrap_or(0) as f64)
    }

    fn fmod_instance_set_loop_points(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [instance, start, end] = args else { return ok_fail() };
        ok_num(game.audio.fmod_set_instance_loop_points(arg_u64(instance), arg_f64(start), arg_f64(end)) as u8 as f64)
    }

    fn fmod_instance_set_loop_count(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [instance, count] = args else { return ok_fail() };
        ok_num(game.audio.fmod_instance_set_loop_count(arg_u64(instance), arg_f64(count) as i32) as u8 as f64)
    }

    fn fmod_instance_get_loop_count(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_fail() };
        ok_num(f64::from(game.audio.fmod_instance_get_loop_count(arg_u64(instance)).unwrap_or(-1)))
    }

    fn fmod_instance_get_audibility(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(instance) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_instance_get_audibility(arg_u64(instance)).unwrap_or(0.0))
    }

    fn fmod_group_set_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [group, volume] = args else { return ok_fail() };
        game.audio.fmod_group_set_volume(arg_u64(group), arg_f64(volume));
        ok_num(1.0)
    }

    fn fmod_group_get_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(group) = args.first() else { return ok_num(1.0) };
        ok_num(game.audio.fmod_group_get_volume(arg_u64(group)))
    }

    fn fmod_group_set_muted(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [group, mute] = args else { return ok_fail() };
        game.audio.fmod_group_set_muted(arg_u64(group), arg_f64(mute) != 0.0);
        ok_num(1.0)
    }

    fn fmod_group_get_muted(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(group) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_group_get_muted(arg_u64(group)) as u8 as f64)
    }

    fn fmod_group_set_pitch(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [group, pitch] = args else { return ok_fail() };
        game.audio.fmod_group_set_pitch(arg_u64(group), arg_f64(pitch));
        ok_num(1.0)
    }

    fn fmod_group_get_pitch(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(group) = args.first() else { return ok_num(1.0) };
        ok_num(game.audio.fmod_group_get_pitch(arg_u64(group)))
    }

    fn fmod_group_stop(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(group) = args.first() else { return ok_fail() };
        game.audio.fmod_group_stop(arg_u64(group));
        ok_num(1.0)
    }

    fn fmod_group_set_paused(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [group, paused] = args else { return ok_fail() };
        game.audio.fmod_group_set_paused(arg_u64(group), arg_f64(paused) != 0.0);
        ok_accepted()
    }

    fn fmod_group_get_paused(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(group) = args.first() else { return ok_num(0.0) };
        ok_num(game.audio.fmod_group_get_paused(arg_u64(group)) as u8 as f64)
    }

    fn fmod_group_set_pan(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [group, pan] = args else { return ok_fail() };
        game.audio.fmod_group_set_pan(arg_u64(group), arg_f64(pan));
        ok_accepted()
    }

    fn fmod_group_set_frequency(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let [group, hz] = args else { return ok_fail() };
        game.audio.fmod_group_set_frequency(arg_u64(group), arg_f64(hz));
        ok_accepted()
    }

    fn fmod_all_stop(game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        game.audio.fmod_all_stop();
        ok_num(1.0)
    }

    fn fmod_all_set_paused(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(paused) = args.first() else { return ok_fail() };
        game.audio.fmod_all_set_paused(arg_f64(paused) != 0.0);
        ok_accepted()
    }

    fn fmod_all_set_muted(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(muted) = args.first() else { return ok_fail() };
        game.audio.fmod_all_set_muted(arg_f64(muted) != 0.0);
        ok_accepted()
    }

    fn fmod_master_set_volume(game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let Some(volume) = args.first() else { return ok_fail() };
        game.audio.fmod_master_set_volume(arg_f64(volume));
        ok_num(1.0)
    }

    fn fmod_get_num_instances(game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        ok_num(game.audio.fmod_get_num_instances() as f64)
    }

    fn fmod_get_last_error(_game: &mut Game, _ctx: &mut Context, _args: &[Value]) -> gml::Result<Value> {
        // This reimplementation never generates FMOD's own numeric error codes -- failures are
        // signaled through a 0 handle/return instead (same convention as ssound.dll), so there's
        // never a nonzero "last error" to report.
        ok_num(0.0)
    }

    fn fmod_get_error_string(_game: &mut Game, _ctx: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let code = args.first().map(arg_f64).unwrap_or(0.0);
        if code == 0.0 { ok_str("No errors.") } else { ok_str("Unknown error.") }
    }

    pub fn resolve(symbol: &str) -> Option<Function> {
        let f: super::ContextFn = match symbol {
            "FMODinit" => fmod_init,
            "FMODfree" => fmod_free,
            "FMODUpdate" => fmod_update,
            "FMODUpdateTakeOverWhileLocked" => fmod_update_take_over_while_locked,
            "FMODUpdateTakeOverDone" => fmod_update_take_over_done,
            "FMODSetPassword" => fmod_set_password,
            "FMODSoundAdd" => fmod_sound_add,
            "FMODSoundAddAsyncStream" => fmod_sound_add_async_stream,
            "FMODSoundAsyncReady" => fmod_sound_async_ready,
            "FMODSoundSetGroup" => fmod_sound_set_group,
            "FMODSoundGetLength" => fmod_sound_get_length,
            "FMODSoundSetMaxVolume" => fmod_sound_set_max_volume,
            "FMODSoundGetMaxVolume" => fmod_sound_get_max_volume,
            "FMODSoundSetLoopPoints" => fmod_sound_set_loop_points,
            "FMODSoundSetLoopCount" => fmod_sound_set_loop_count,
            "FMODSoundGetLoopCount" => fmod_sound_get_loop_count,
            "FMODSoundIs3d" => fmod_sound_is_3d,
            "FMODSoundIsStreamed" => fmod_sound_is_streamed,
            "FMODSoundGetNumChannels" => fmod_sound_get_num_channels,
            "FMODSoundFree" => fmod_sound_free,
            "FMODSoundPlay" => fmod_sound_play,
            "FMODSoundLoop" => fmod_sound_loop,
            "FMODSoundPlay3d" => fmod_sound_play_3d,
            "FMODSoundLoop3d" => fmod_sound_loop_3d,
            "FMODSoundInstanciate" => fmod_sound_instanciate,
            "FMODInstanceStop" => fmod_instance_stop,
            "FMODInstanceAsyncOK" => fmod_instance_async_ok,
            "FMODInstanceSetVolume" => fmod_instance_set_volume,
            "FMODInstanceGetVolume" => fmod_instance_get_volume,
            "FMODInstanceSetFrequency" => fmod_instance_set_frequency,
            "FMODInstanceGetFrequency" => fmod_instance_get_frequency,
            "FMODInstanceSetPitch" => fmod_instance_set_pitch,
            "FMODInstanceGetPitch" => fmod_instance_get_pitch,
            "FMODInstanceSetPan" => fmod_instance_set_pan,
            "FMODInstanceGetPan" => fmod_instance_get_pan,
            "FMODInstanceSetMuted" => fmod_instance_set_muted,
            "FMODInstanceGetMuted" => fmod_instance_get_muted,
            "FMODInstanceGetSound" => fmod_instance_get_sound,
            "FMODInstanceGetPosition" => fmod_instance_get_position,
            "FMODInstanceSetPosition" => fmod_instance_set_position,
            "FMODInstanceSoundGetLength" => fmod_instance_sound_get_length,
            "FMODInstanceSetPaused" => fmod_instance_set_paused,
            "FMODInstanceGetPaused" => fmod_instance_get_paused,
            "FMODInstanceIsPlaying" => fmod_instance_is_playing,
            "FMODInstanceSetLoopPoints" => fmod_instance_set_loop_points,
            "FMODInstanceSetLoopCount" => fmod_instance_set_loop_count,
            "FMODInstanceGetLoopCount" => fmod_instance_get_loop_count,
            "FMODInstanceGetAudibility" => fmod_instance_get_audibility,
            "FMODGroupSetVolume" => fmod_group_set_volume,
            "FMODGroupGetVolume" => fmod_group_get_volume,
            "FMODGroupSetMuted" => fmod_group_set_muted,
            "FMODGroupGetMuted" => fmod_group_get_muted,
            "FMODGroupSetPitch" => fmod_group_set_pitch,
            "FMODGroupGetPitch" => fmod_group_get_pitch,
            "FMODGroupSetPaused" => fmod_group_set_paused,
            "FMODGroupGetPaused" => fmod_group_get_paused,
            "FMODGroupSetPan" => fmod_group_set_pan,
            "FMODGroupSetFrequency" => fmod_group_set_frequency,
            "FMODGroupStop" => fmod_group_stop,
            "FMODAllStop" => fmod_all_stop,
            "FMODAllSetPaused" => fmod_all_set_paused,
            "FMODAllSetMuted" => fmod_all_set_muted,
            "FMODMasterSetVolume" => fmod_master_set_volume,
            "FMODGetNumInstances" => fmod_get_num_instances,
            "FMODGetLastError" => fmod_get_last_error,
            "FMODGetErrorString" => fmod_get_error_string,

            // -- 3D positioning: accepted, ignored, warned about once (see
            // `warn_3d_ignored_once`). Registered rather than left to `external.rs`'s generic
            // dummy for two reasons: the dummy's 0 reads as *failure* to a game that checks, and
            // these are precisely the calls that surround the 3D playback the section above does
            // support -- letting them fall through would make every such game log the
            // "unimplemented external" warning for calls that are working as intended.
            "FMODSoundSet3dMinMaxDistance" => fmod_sound_set_3d_min_max_distance,
            "FMODSoundGetMaxDist" => fmod_sound_get_max_dist,
            "FMODInstanceSet3dMinMaxDistance" => fmod_instance_set_3d_min_max_distance,
            "FMODInstanceGetMaxDist" => fmod_instance_get_max_dist,
            "FMODInstanceGet3DPanLevel" => fmod_instance_get_3d_pan_level,
            "FMODInstanceGet3DSpread" => fmod_instance_get_3d_spread,
            "FMODBlockerGetStrength" => fmod_blocker_get_strength,
            "FMODListenerSet3dPosition"
            | "FMODListenerSet3dPositionEx"
            | "FMODListenerSetNumber"
            | "FMODListenerHearsDistanceOnly"
            | "FMODInstanceSet3dPosition"
            | "FMODInstanceSet3dCone"
            | "FMODInstanceSet3dConeOrientation"
            | "FMODInstanceSet3dDopplerMax"
            | "FMODInstanceSet3DPanLevel"
            | "FMODInstanceSet3DSpread"
            | "FMODInstanceSetSpeakerMix"
            | "FMODSoundSet3dCone"
            | "FMODSoundSet3dDopplerMax"
            | "FMODSetWorldScale"
            | "FMODSetDopplerFPS"
            | "FMODBlockersInit"
            | "FMODBlockersFree"
            | "FMODBlockerAdd"
            | "FMODBlockerSetEnabled"
            | "FMODBlockerGetEnabled"
            | "FMODBlockerSetStrength"
            | "FMODBlockerSet3dPosition"
            | "FMODBlockerSet3dScale"
            | "FMODBlockerSet3dOrientation" => fmod_3d_ignored,

            // Everything else on this DLL -- DSP effects, spectrum/waveform analysis, microphone
            // recording, ID3 tags, file encryption, tracker-module channel volumes -- is left to
            // `external.rs`'s generic dummy on purpose. None of it can silently cost a game its
            // audio the way an unregistered play call can, and the dummy path now says out loud
            // which symbol went unanswered (`warn_unsupported_once`), which is the right outcome
            // for a game that genuinely depends on one of them.
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

    pub fn reset_password() {}
}

// The bare fn-pointer type `Function::Runtime` wraps (its own `ContextFunctionPtr` type alias in
// `gml.rs` is private, so this names the same signature structurally instead of by that name).
type ContextFn = fn(&mut Game, &mut Context, &[Value]) -> gml::Result<Value>;

pub use imp::{reset_password, resolve};

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    /// The cipher, against real bytes: the first 16 bytes of `SE/sndJump.wav` as shipped by
    /// I Wanna Kill the Kamilia 3 v1.48, whose GML calls `FMODSetPassword("sexking6974")`
    /// immediately before loading every one of its sound effects.
    ///
    /// This is the whole fix in one assertion. An encrypted effect is indistinguishable from
    /// garbage until the keystream is right -- and the keystream is only right if the digest is
    /// SHA-1, hex, *uppercase*, and XORed with the table recovered from the DLL.
    #[test]
    fn kamilia_3_sound_effect_decrypts_to_a_wav_header() {
        let cipher = [0x51u8, 0x4e, 0x47, 0x37, 0x85, 0x54, 0x75, 0x06, 0x50, 0x30, 0x20, 0x31, 0x61, 0x6c, 0x74, 0x53];
        let key = super::imp::keystream_for("sexking6974");
        let plain: Vec<u8> = cipher.iter().enumerate().map(|(i, b)| b ^ key[i % key.len()]).collect();
        assert_eq!(&plain[0..4], b"RIFF");
        assert_eq!(&plain[8..16], b"WAVEfmt ");
    }

    /// A different password must not produce the same keystream -- i.e. the password is actually
    /// an input, not decoration around a hardcoded table.
    #[test]
    fn a_different_password_gives_a_different_keystream() {
        assert_ne!(super::imp::keystream_for("sexking6974"), super::imp::keystream_for("something else"));
    }
}
