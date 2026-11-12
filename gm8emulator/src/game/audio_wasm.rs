//! Web Audio API-backed `AudioManager` for the browser build. Matches the exact public shape of
//! the native (`udon`/`libsoundio`-backed) implementation in `game/audio.rs` so `game.rs`/
//! `gml/kernel.rs` compile unchanged against either platform.
//!
//! Both formats decode fully synchronously (no `decodeAudioData`, no Promise anywhere in this
//! module) since native's `add_mp3`/`add_wav` block until decoded and callers (`Game::launch`'s
//! embedded-sound loop, GML's `sound_add`/`sound_replace`) assume that. MP3 decode uses
//! `symphonia` (see `audio_wasm/mp3.rs` for why, not `rmp3`); WAV decode is a small hand-ported
//! RIFF/PCM parser (`audio_wasm/wav.rs`).
//!
//! `capture_audio`/`stop_audio_capture` are TAS-tooling-only (ffmpeg capture, tied to the `tas`
//! feature) and never called on wasm; kept as no-ops purely so this type's shape matches native's.

#[path = "audio_wasm/cache.rs"]
pub(crate) mod cache;
#[path = "audio_wasm/midi.rs"]
// `pub(crate)`: like `tracker`, this renders rather than decodes -- see the module docs.
pub(crate) mod midi;
#[path = "audio_wasm/mp3.rs"]
// `pub(crate)` (not private): reused directly by `game/external/fmod.rs`, since `FMODSoundAdd`
// commonly loads MP3 files (unlike `ssound.dll`, which this project has only seen used with OGG).
pub(crate) mod mp3;
#[path = "audio_wasm/ogg.rs"]
pub(crate) mod ogg;
#[path = "audio_wasm/tracker.rs"]
// `pub(crate)`: `game/external/fmod.rs` parses modules directly, the same way it reaches `mp3`,
// `wav` and `ogg`. Unlike those three this one renders rather than decodes -- see the module docs.
pub(crate) mod tracker;
#[path = "audio_wasm/wav.rs"]
// `pub(crate)` (not private): reused directly by `game/external/ssound.rs`, since ssound.dll's
// `SS_LoadSound` accepts WAV files too, not just OGG.
pub(crate) mod wav;

pub use cache::{
    AudioBufferCache, AudioCacheKey, AudioCachePolicy, AudioCacheStats, CacheEventType, DEFAULT_MIDI_SAMPLE_RATE,
    DEFAULT_MUSIC_BUDGET_BYTES, DEFAULT_SHORT_COMPRESSED_BYTES, DEFAULT_SHORT_DURATION_SECS, SoundFormat,
};

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, rc::Rc};
// Not `web_sys`: there is no `AudioContext` in a worker, so these are the stand-ins in
// `game/audio_proxy.rs`, which send what they are told to do to whoever has a device. Every
// signature matches the `web_sys` type it replaces, which is why nothing below this line changed
// when the engine moved off the main thread.
use crate::game::audio_proxy::{AudioBuffer, AudioBufferSourceNode, AudioContext, GainNode, StereoPannerNode};

/// GM8/DirectMusic's logarithmic volume scale, ported byte-for-byte from the native
/// implementation (`game/audio.rs`) -- many games rely on this exact curve for fades/ambience.
/// Minimum output is 0.001, matching GM8.
fn make_volume(vol: f64) -> f32 {
    1000.0f64.powf(vol.clamp(0.0, 1.0) - 1.0) as f32
}

fn decoded_to_buffer(
    ctx: &AudioContext,
    channels: u16,
    sample_rate: u32,
    planar: &[f32],
    frames: usize,
) -> Option<AudioBuffer> {
    if frames == 0 {
        return None
    }
    let buffer = ctx.create_buffer(u32::from(channels), frames as u32, sample_rate as f32).ok()?;
    for ch in 0..usize::from(channels) {
        buffer.copy_to_channel(&planar[ch * frames..(ch + 1) * frames], ch as i32).ok()?;
    }
    Some(buffer)
}

/// Native's `length_to_ns` divides by `sample_rate * channels`, but the two callers pass a
/// different `sample_count` convention each (mp3's is already a per-channel frame count, wav's is
/// total interleaved samples) such that both land on the same real duration either way.
/// `AudioBuffer::length` is always a per-channel frame count (the Web Audio convention), so every
/// caller here uses it uniformly with an implicit `channels = 1` divisor.
fn length_to_ns(frames: usize, sample_rate: u32) -> u128 {
    (frames as u128 * 1_000_000_000) / u128::from(sample_rate.max(1))
}

/// `AudioBufferSourceNode::stop` is deprecated in favor of the identical method on the
/// `AudioScheduledSourceNode` interface it inherits from -- both call the same underlying
/// `stop()`; this just keeps `#[allow(deprecated)]` in one place instead of at every call site.
#[allow(deprecated)]
fn stop_source(source: &AudioBufferSourceNode) {
    let _ = source.stop();
}

#[derive(Clone)]
pub struct Mp3Handle {
    /// Resolved through `AudioManager`'s cache at play time so lazy mode can drop and re-decode.
    key: AudioCacheKey,
    id: i32,
}

// Savestates/replays are never used on wasm (the `tas` feature is off), but `asset::sound::
// FileType` derives Serialize/Deserialize unconditionally since it wraps this type on every
// platform -- these are never actually invoked on wasm, just needed to satisfy that derive.
impl Serialize for Mp3Handle {
    fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
        unreachable!("savestates/replays are never used on wasm")
    }
}
impl<'de> Deserialize<'de> for Mp3Handle {
    fn deserialize<D: serde::Deserializer<'de>>(_d: D) -> Result<Self, D::Error> {
        unreachable!("savestates/replays are never used on wasm")
    }
}

#[derive(Clone)]
pub struct WavHandle {
    key: AudioCacheKey,
    // One persistent GainNode per handle (not per playback instance): `sound_volume` on native is
    // a live, shared value across every concurrently-playing instance of that sound id
    // (`WavHandle.params: Arc<SoundParams>` is the same `Arc` handed to every mixer `Add` for that
    // handle) -- every source spawned from this handle connects into the same GainNode, so
    // `set_value` retroactively affects all of them, matching native exactly.
    gain_node: GainNode,
    id: i32,
    exclusive: bool,
}

impl Serialize for WavHandle {
    fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
        unreachable!("savestates/replays are never used on wasm")
    }
}
impl<'de> Deserialize<'de> for WavHandle {
    fn deserialize<D: serde::Deserializer<'de>>(_d: D) -> Result<Self, D::Error> {
        unreachable!("savestates/replays are never used on wasm")
    }
}

impl WavHandle {
    pub fn set_volume(&self, vol: f64) {
        self.gain_node.gain().set_value(make_volume(vol));
    }
}

/// State for a single `ssound.dll`-loaded sound (see `game/external/ssound.rs`) -- richer than the
/// built-in sound system's handles above, since that DLL's contract needs live position/pause/
/// frequency/pan control that the regular embedded-sound API never exposes.
///
/// Web Audio has no native pause, seek, or position-query for `AudioBufferSourceNode` -- once
/// started it plays to the end or until `.stop()`, full stop. Pause, resume, and seeking
/// (`SetSoundPosition`) are all implemented the same way here: stop the current source (if any),
/// compute a target offset in seconds, and start a *brand new* source from that offset. "Current
/// position" is always computed from the bookkeeping below (elapsed context time since the
/// current source last started, scaled by the current playback rate), never queried from a node.
struct SsSoundState {
    key: AudioCacheKey,
    buffer: Option<Rc<AudioBuffer>>,
    original_sample_rate: f32,
    // Persistent per handle (like `WavHandle::gain_node` above) -- connects straight to
    // `ctx.destination()`, bypassing `AudioManager::global_gain` entirely: `ssound.dll`'s mixing
    // is independent of the built-in sound system on real GM8 too (a separate DirectSound
    // session), so `sound_global_volume()` has no effect on it here either, matching that.
    gain_node: GainNode,
    pan_node: StereoPannerNode,
    current_source: Option<AudioBufferSourceNode>,
    // Re-applied to every reconstructed source (SS_SetSoundFreq changes this).
    playback_rate_ratio: f64,
    looping: bool,
    started_at_ctx_time: f64,
    started_at_offset_secs: f64,
    paused_at_offset_secs: Option<f64>,
}

impl SsSoundState {
    fn duration_secs(&self) -> f64 {
        let Some(buffer) = &self.buffer else { return 0.0 };
        f64::from(buffer.length()) / f64::from(buffer.sample_rate())
    }

    fn current_offset_secs(&self, ctx: &AudioContext) -> f64 {
        if let Some(paused) = self.paused_at_offset_secs {
            paused
        } else if self.current_source.is_some() {
            let elapsed = (ctx.current_time() - self.started_at_ctx_time) * self.playback_rate_ratio;
            let raw = self.started_at_offset_secs + elapsed;
            let duration = self.duration_secs();
            // A looping source keeps playing past its own duration (wrapping around inside Web
            // Audio natively), so `raw` legitimately grows past `duration` the longer it plays --
            // clamping it (as the non-looping branch below does) would make every pause captured
            // after the first loop report the exact same position (`duration`, the clamp
            // ceiling), and resuming from "exactly the end" is indistinguishable from resuming at
            // the very start of the next loop. Wrap modulo duration instead, matching where
            // playback has actually looped around to.
            if self.looping && duration > 0.0 { raw % duration } else { raw.min(duration) }
        } else {
            self.started_at_offset_secs
        }
    }

    fn is_playing(&self, ctx: &AudioContext) -> bool {
        if self.paused_at_offset_secs.is_some() || self.current_source.is_none() {
            return false
        }
        self.looping || self.current_offset_secs(ctx) < self.duration_secs()
    }
}

/// State for a single playing `FMODSoundPlay`/`FMODSoundLoop` instance (see
/// `game/external/fmod.rs`). Position/pause/resume bookkeeping is a direct port of
/// `SsSoundState`'s, generalized to wrap around a custom loop window (`loop_start_secs`/
/// `loop_end_secs`, from `FMODSoundSetLoopPoints`) instead of always the whole buffer -- Web
/// Audio's lack of a native pause/seek primitive for `AudioBufferSourceNode` applies here
/// identically.
struct FmodInstanceState {
    buffer: Rc<AudioBuffer>,
    // The loaded sound this instance is playing (FMODInstanceGetSound).
    sound_id: u64,
    // Per-instance volume (FMODInstanceSetVolume, linear 0..1) -- chains into the instance's
    // group's `GainNode` (see `AudioManager::fmod_group_gains`), so instance and group volume
    // multiply for free, matching the DLL's own documented
    // `overall = maxvalue*instancevalue*group*global*distancefactor`.
    gain_node: GainNode,
    // FMODInstanceSetPan (-1 left .. 0 center .. 1 right -- a direct, unscaled match for
    // `StereoPannerNode.pan`, unlike ssound.dll's -10000..10000 DirectSound convention).
    pan_node: StereoPannerNode,
    current_source: Option<AudioBufferSourceNode>,
    looping: bool,
    // Re-applied to every reconstructed/live source. FMODInstanceSetFrequency (absolute Hz) and
    // FMODInstanceSetPitch (a 0..10 multiplier, 1 = normal) are just two different units for this
    // one underlying value -- Pitch sets it directly, Frequency divides by the buffer's own
    // sample rate first, mirroring ssound.dll's SetSoundFreq/original_sample_rate pattern.
    playback_rate_ratio: f64,
    // The instance's group's own pitch multiplier (`FMODGroupSetPitch`, default 1.0), denormalized
    // onto every instance in that group so `current_offset_secs` doesn't need a reference back to
    // `AudioManager::fmod_group_pitches`. The *actual* rate a source plays at is always
    // `playback_rate_ratio * group_pitch_ratio` -- matches the DLL's own documented
    // `overall = maxvalue*instancevalue*group*global*distancefactor` composition model, just
    // applied to pitch instead of volume (real games use a group-wide pitch fade to 0 as a
    // "grind the current music to a halt" death effect, distinct from actually stopping it).
    group_pitch_ratio: f64,
    started_at_ctx_time: f64,
    started_at_offset_secs: f64,
    paused_at_offset_secs: Option<f64>,
    // Copied from the sound's group (`FmodSoundState::group`) at the moment this instance was
    // created via `FMODSoundPlay`/`FMODSoundLoop` -- real games call `FMODSoundSetGroup` once
    // right after loading a sound, before ever playing it, so this doesn't need to track later
    // changes to the sound's group. Used to scope `FMODGroupStop`/`FMODGroupSetVolume` to only
    // the instances the game actually intends (e.g. group 2 = sound effects, group 3 = music in
    // one real game -- stopping group 2 on death/restart must NOT touch a paused group-3 music
    // instance the same code stops-then-resumes right after).
    group: u64,
    // Last value passed to FMODInstanceSetVolume (default 1.0) -- GetVolume returns this raw
    // value, not the max-volume-multiplied effective gain actually written to `gain_node`.
    raw_volume: f32,
    // Copied from the sound's FMODSoundSetMaxVolume at play time; multiplies with `raw_volume`
    // (never muted) to produce `gain_node`'s actual value.
    max_volume: f32,
    // FMODInstanceSetMuted -- forces effective gain to 0 without touching `raw_volume`, so
    // unmuting restores exactly what was set before (matches the DLL's own doc: muting "does not
    // stop" playback and volume settings survive it).
    muted: bool,
    // Copied from the sound's FMODSoundSetLoopPoints at play time (0.0/None = whole buffer,
    // Web Audio's own default loop behavior).
    loop_start_secs: f64,
    loop_end_secs: Option<f64>,
    // FMODInstanceSetLoopCount, in FMOD's own units: -1 = forever, 0 = play once, n = loop n more
    // times. `looping` above is the part Web Audio can actually express
    // (`AudioBufferSourceNode.loop` is a bool, with no count and no "stop after N laps" event), so
    // this drives `looping` (`count != 0`) and is otherwise remembered only so GetLoopCount can
    // return it. A *positive* count therefore loops forever instead of stopping after N laps --
    // the two counts games actually use, -1 and 0, are exact. Scheduling a `stop(when)` for the
    // finite case was the alternative and was deliberately not done: the stop time depends on the
    // playback rate, and FMODInstanceSetPitch/SetFrequency/FMODGroupSetPitch (and every
    // pause/seek, which rebuilds the source outright) would each have to recompute it or the
    // sound would cut off at visibly the wrong moment.
    loop_count: i32,
}

impl FmodInstanceState {
    fn duration_secs(&self) -> f64 {
        f64::from(self.buffer.length()) / f64::from(self.buffer.sample_rate())
    }

    /// The effective end of the loop window (`FMODSoundSetLoopPoints`'s `end`, defaulting to the
    /// whole buffer if never set).
    fn loop_end_secs(&self) -> f64 {
        self.loop_end_secs.unwrap_or_else(|| self.duration_secs())
    }

    /// Clamps the *effective* gain actually written to the node to 1.0 max, mirroring a real
    /// clamp FMOD evidently applies internally: at least one real game passes clearly
    /// out-of-spec values to `FMODGroupSetVolume`/`FMODInstanceSetVolume` (e.g. `200` where the
    /// documented range is 0..1) and sounds completely normal on real Windows -- meaning FMOD
    /// silently absorbs it rather than actually amplifying 200x. A `GainNode` has no such
    /// built-in ceiling, so without this a game like that produces severe digital clipping here
    /// that never happens on real hardware. `raw_volume`/`max_volume` are intentionally stored
    /// unclamped (so `FMODInstanceGetVolume`/`FMODSoundGetMaxVolume` still echo back exactly
    /// what the game set), and only their applied *product* is capped.
    fn recompute_gain(&self) {
        let volume = (self.raw_volume * self.max_volume).clamp(0.0, 1.0);
        self.gain_node.gain().set_value(if self.muted { 0.0 } else { volume });
    }

    /// The actual rate a live source plays at -- instance and group pitch multiply, per the
    /// DLL's documented composition model (see `group_pitch_ratio`'s own doc comment).
    fn effective_rate(&self) -> f64 {
        self.playback_rate_ratio * self.group_pitch_ratio
    }

    fn current_offset_secs(&self, ctx: &AudioContext) -> f64 {
        if let Some(paused) = self.paused_at_offset_secs {
            paused
        } else if self.current_source.is_some() {
            let elapsed = (ctx.current_time() - self.started_at_ctx_time) * self.effective_rate();
            let raw = self.started_at_offset_secs + elapsed;
            let duration = self.duration_secs();
            let loop_end = self.loop_end_secs();
            if !self.looping {
                raw.min(duration)
            } else if raw <= loop_end {
                raw
            } else {
                // Past the loop window's own end -- Web Audio has natively looped back to
                // `loop_start_secs` (or 0, the whole-buffer default) at least once. Wrap the
                // excess into the loop window itself rather than the *whole* buffer: a plain
                // `raw % duration` (correct only when the loop window is the entire buffer, i.e.
                // `loop_start_secs == 0.0 && loop_end == duration`) would report a position
                // *before* the loop point on any track with a real intro section that the loop
                // window skips over, breaking pause/resume position for exactly those tracks.
                let window = loop_end - self.loop_start_secs;
                if window > 0.0 { self.loop_start_secs + (raw - loop_end) % window } else { loop_end }
            }
        } else {
            self.started_at_offset_secs
        }
    }

    /// `Channel::isPlaying`, and it is **true for a paused channel** -- pause is an attribute of a
    /// still-allocated channel, not a stop, which is why FMOD has a separate `getPaused` at all
    /// (and why `SsSoundState::is_playing`, whose DLL is DirectSound-backed and where a paused
    /// buffer really is stopped, deliberately answers the other way).
    ///
    /// This is load-bearing rather than a nicety. gm82snd installs `__gm82snd_update2d` in the
    /// gm82core object's End Step, and it reaps every instance that reports not-playing --
    /// `FMODInstanceStop` plus removal from its lists -- once per frame. Answering `false` while
    /// paused therefore destroyed any paused instance one frame later, so the whole
    /// `sound_pause`/`sound_resume` half of gm82snd's API resumed silence: renex engine pauses the
    /// background music on death and resumes it on restart, and the instance handle it resumed no
    /// longer existed. That the extension ships both a reaper and a pause API is itself the proof
    /// of which answer is right.
    fn is_playing(&self, ctx: &AudioContext) -> bool {
        if self.paused_at_offset_secs.is_some() {
            return true
        }
        if self.current_source.is_none() {
            return false
        }
        self.looping || self.current_offset_secs(ctx) < self.duration_secs()
    }
}

pub struct AudioManager {
    ctx: AudioContext,
    global_gain: GainNode,
    /// Shared decoded-buffer LRU for built-in, FMOD, and ssound paths.
    cache: AudioBufferCache,
    /// Built-in sound ids currently playing (or held as exclusive) -- used to pin/unpin cache entries.
    builtin_pins: HashMap<i32, u32>,
    /// A single slot, replaced (not stacked) on every play -- mirrors native's `Mixer::
    /// exclusive_source`. MP3s are always exclusive; WAVs are exclusive only when `kind >= 3`
    /// (`SoundKind::Multimedia`).
    exclusive_slot: Option<(AudioBufferSourceNode, i32)>,
    /// Concurrent, independently-playing instances per sound id -- mirrors native's `Mixer::
    /// sources: Vec<...>` (non-exclusive `Add` always pushes, never replaces, so e.g. rapid-fire
    /// SFX can overlap).
    non_exclusive: HashMap<i32, Vec<AudioBufferSourceNode>>,
    end_times: HashMap<i32, Option<u128>>,
    multimedia_end: Option<(i32, Option<u128>)>,

    /// The instrument bank MIDI sounds are rendered through, once the host has fetched it.
    ///
    /// Not loaded with the game: a SoundFont is tens of megabytes and almost no game needs one, so
    /// the host asks `needs_soundfont()` after loading and only then fetches and installs it. Until
    /// it does, a `.mid` sound is registered and silent rather than absent, so nothing downstream
    /// has to care whether the bank arrived.
    soundfont: Option<std::sync::Arc<rustysynth::SoundFont>>,
    /// Whether this game registered any MIDI at all -- what `needs_soundfont()` reports.
    has_midi: bool,

    /// `ssound.dll`-loaded sounds (see `game/external/ssound.rs`), keyed by an opaque handle
    /// string. Monotonically increasing, never reused even after a handle is freed -- avoids a
    /// freed handle a game might still be holding accidentally referencing an unrelated new sound.
    ssound: HashMap<String, SsSoundState>,
    next_ssound_handle: u64,
    /// Pin counts for ssound handles that use the shared cache.
    ssound_pins: HashMap<String, u32>,

    /// `GMFMODSimple.dll`-loaded sounds (see `game/external/fmod.rs`), keyed by a numeric handle
    /// -- unlike `ssound.dll`, FMOD separates a loaded "Sound" (this map) from a playing
    /// "Instance" (`fmod_instances` below); multiple concurrent instances can play one sound.
    fmod_sounds: HashMap<u64, FmodSoundState>,
    next_fmod_sound: u64,
    /// Pin counts for FMOD sounds that use the shared cache (Cached / Tracker).
    fmod_pins: HashMap<u64, u32>,
    fmod_instances: HashMap<u64, FmodInstanceState>,
    next_fmod_instance: u64,
    /// One persistent `GainNode` per FMOD group id (`FMODSoundSetGroup`'s group argument),
    /// created lazily the first time a group is either played into or has its volume set.
    /// Real games route distinct categories of sound through different groups (e.g. group 2 =
    /// sound effects, group 3 = music) specifically so `FMODGroupStop`/`FMODGroupSetVolume` can
    /// target one category without touching the other -- so these must stay genuinely separate
    /// per-group nodes, not one shared node. Each connects to `fmod_master_gain` (not straight to
    /// `ctx.destination()`, so `FMODMasterSetVolume` can duck every group at once).
    fmod_group_gains: HashMap<u64, GainNode>,
    /// `FMODGroupSetPitch`'s value per group (default 1.0 if never set) -- unlike volume, pitch
    /// has no chainable Web Audio node (`AudioBufferSourceNode.playbackRate` is per-source, not
    /// something a shared node can multiply for every instance at once), so setting a group's
    /// pitch must walk every instance currently in that group and update each live source
    /// directly (see `fmod_group_set_pitch`). Real games use this for a "grind the current music
    /// to a halt" effect (fading a group's pitch to 0), distinct from actually stopping it.
    fmod_group_pitches: HashMap<u64, f64>,
    /// Persisted alongside `fmod_group_gains`' actual node value so `FMODGroupSetMuted` can force
    /// a group silent without losing whatever volume `FMODGroupSetVolume` last set (mute/unmute
    /// must restore exactly that, same reasoning as `FmodInstanceState::muted`/`raw_volume`).
    fmod_group_volumes: HashMap<u64, f32>,
    fmod_group_muted: HashMap<u64, bool>,
    /// `FMODGroupSetPaused`/`FMODGroupGetPaused`. Pause, unlike volume and mute, has no node to
    /// hang off (see `fmod_group_set_paused`), so this is the whole of the group's own pause
    /// state; the instances were paused individually when it was set.
    fmod_group_paused: HashMap<u64, bool>,
    /// `FMODMasterSetVolume` -- every group routes through this one node, which connects straight
    /// to `ctx.destination()`, bypassing `global_gain`: like `ssound.dll`, FMOD's mixing is
    /// architecturally independent of GM8's own sound system on real Windows, so
    /// `sound_global_volume()` must not affect it.
    fmod_master_gain: GainNode,
    /// `FMODMasterSetVolume` and `FMODAllSetMuted` both drive `fmod_master_gain`, so neither can
    /// own the node's value outright -- same split as `fmod_group_volumes`/`fmod_group_muted` one
    /// level down, so that unmuting restores the volume last set rather than 1.0.
    fmod_master_volume: f32,
    fmod_master_muted: bool,

    /// `SGAudio.dll` emitters (see `game/external/sgaudio.rs`), keyed by the small integer id the
    /// DLL hands back to GML. Each owns exactly one FMOD sound + instance pair rather than a second
    /// copy of the node graph: an SGAudio emitter *is* "one file plus one playback state", which is
    /// what an FMOD instance already models, so everything below delegates.
    sga_emitters: HashMap<i32, SgaEmitter>,
    /// The 32 group volumes `sga_SetGroupVolume`/`sga_GetGroupVolume` address, all 1.0 until set.
    /// Not `fmod_group_gains`: SGAudio's groups are a *bitmask* per emitter (`sga_SetType`), so one
    /// emitter can belong to several at once and its gain is the product of all of them -- which no
    /// arrangement of one-group-per-instance gain nodes can express. See `sga_effective_gain`.
    sga_group_volumes: [f64; SGA_GROUP_COUNT],
    /// `sga_Init` succeeds once and then reports failure until `sga_Free`, which is the whole of
    /// what the DLL's init flag is observable as.
    sga_initialized: bool,

    /// `MaizeMusic.dll` streams (see `game/external/maizemusic.rs`), keyed by the handle GML
    /// holds. One stream owns one file and one playback state, like an SGAudio emitter -- but its
    /// FMOD instance is created on first play rather than at load, because the load must not
    /// decode anything (see `MmStream::instance`).
    mm_streams: HashMap<u32, MmStream>,
    next_mm_handle: u32,
    /// `mmInit`/`mmFree`, the same once-only flag `sga_initialized` is.
    mm_initialized: bool,
    /// `mmMusicSetGlobalVolume`, which is `BASS_SetVolume`: BASS's whole output, so it multiplies
    /// into every stream's own volume rather than living on any one of them. Not a shared gain
    /// node, because mm streams route through the per-instance nodes `fmod_play` builds and
    /// nothing else; see `mm_apply_gain`.
    mm_global_volume: f64,
}

/// How many groups `sga_SetType`'s bitmask addresses -- the DLL's own recompute is a 32-step
/// unrolled loop over a 32-entry volume table, one `rol`/`test` per bit.
const SGA_GROUP_COUNT: usize = 32;

/// One `SGAudio.dll` emitter. Created not-playing (`sga_CreateEmitter`) or created-and-playing
/// (`sga_TrackPlay`); either way the underlying FMOD instance exists from the start, held paused,
/// so every later call is a plain delegation rather than a "does it exist yet" branch.
struct SgaEmitter {
    sound_id: u64,
    instance: u64,
    /// What `sga_SetVolume` last set, and exactly what `sga_GetVolume` returns. The DLL keeps this
    /// (`+0x34`) separate from the volume actually applied to the source (`+0x38`), which is what a
    /// `sga_FadeVolume` ramp moves -- so a `GetVolume` mid-fade reports the *target*, not the live
    /// value. Preserved here for the same reason: the game that surfaced this drives a volume fade
    /// by re-reading and re-setting, and a live value would make it chase its own tail.
    volume: f64,
    /// `sga_SetType`'s group bitmask (`+0x3c`). Bit *n* means "in group *n*".
    type_mask: u32,
    /// `sga_SetLooping`. Remembered as well as pushed onto the instance because `sga_Stop` +
    /// `sga_Play` has to restart with the looping mode last set, not the one it was created with.
    looping: bool,
    /// `sga_Set3DMode`, `sga_SetDistFactor`, `sga_SetPosition` -- recorded and echoed back, not
    /// acted on. Positional audio would need the whole OpenAL listener/distance-attenuation model
    /// behind a `PannerNode`, and nothing is known to use it: SGAudio's own GML wrapper ships
    /// `sga_Create3DFX`/`sga_Play3DFX` helper scripts, and the game they were dumped from defines
    /// both and calls neither. Deliberately inert rather than approximated, so a game that *does*
    /// use it is a missing feature rather than a wrong-sounding one.
    three_d: bool,
    dist_factor: f64,
    position: (f64, f64, f64),
}

/// One `MaizeMusic.dll` stream: the handle `mmLoadMusic` hands back, which in the real DLL is a
/// BASS channel (a `BASS_StreamCreateFile` decode stream wrapped in a BASS_FX tempo stream).
///
/// Unlike `SgaEmitter`, the FMOD instance is `None` until the stream first plays. A BASS stream
/// holds no decoded audio -- it reads the file as it goes -- so `mmLoadMusic` must cost nothing
/// but the bytes, and creating the instance is what forces a decode here (`fmod_play` needs a
/// buffer). Every setting a game can apply before the first play therefore lives in this struct
/// and is pushed onto the instance when it is finally created.
struct MmStream {
    sound_id: u64,
    /// The live FMOD instance, or `None` when the stream has never played or has been stopped.
    /// Stopping really does drop it: a stopped BASS channel at position 0 is indistinguishable
    /// from "nothing is playing and the next play starts at the beginning", and dropping it
    /// unpins the decoded buffer so the music budget can reclaim it -- which is the whole point
    /// of `register_streamed` (a game like this one holds 46 handles at once).
    instance: Option<u64>,
    /// The file it was loaded from. Only diagnostics use it -- the bytes themselves are the
    /// cache's, keyed by this same path.
    path: String,
    /// `BASS_SAMPLE_LOOP`, i.e. `mmLoadMusic`'s second argument.
    looping: bool,
    /// `BASS_ATTRIB_VOL` (0..1), which `mmMusicGetVolume` reads straight back out of the channel.
    /// Kept here rather than read off the instance because a game sets it *before* the first
    /// play: this game's `mms_Play` is `sound_set_volume(h, ...)` then `mm_play(h)`.
    volume: f64,
    /// `BASS_ATTRIB_PAN` (-1..1).
    pan: f64,
    /// `mmMusicSetSpeed`: a multiplier on the stream's own sample rate (the DLL multiplies
    /// `BASS_CHANNELINFO::freq` by it and sets `BASS_ATTRIB_FREQ`), so 1.0 is normal speed.
    speed: f64,
    /// `mmMusicSetLoopSection`'s (start, end) in seconds. In the DLL this is a `BASS_SYNC_POS` at
    /// `end` whose callback seeks back to `start`, so it loops the section forever regardless of
    /// whether the stream itself was created looping -- which is why applying it forces
    /// `looping`. `None` until set, or after `mmMusicRemoveLoopSection`.
    loop_section: Option<(f64, f64)>,
    /// Where a `mmMusicResume` would pick up on a stream with no live instance: 0 after a stop,
    /// or whatever `mmMusicSetPosition` last asked for. `mmMusicPlay` ignores it -- the DLL calls
    /// `BASS_ChannelPlay(h, TRUE)`, which restarts from the beginning.
    pending_offset_secs: f64,
}

/// A loaded (but not yet playing) FMOD sound resource (`FMODSoundAdd`). `group` defaults to 0
/// (ungrouped) until `FMODSoundSetGroup` reassigns it -- games that use groups at all always call
/// it once, immediately after loading, before ever playing the sound.
struct FmodSoundState {
    source: FmodSource,
    group: u64,
    /// FMODSoundSetMaxVolume -- default 1.0 (no cap), matching the DLL's own documented default.
    max_volume: f32,
    /// FMODSoundSetLoopPoints, in seconds (converted from the 0..1 fractions the GML wrapper
    /// passes, using this sound's own duration) -- `None` until set, meaning "loop the whole
    /// buffer" (Web Audio's own default `AudioBufferSourceNode` loop behavior).
    loop_points_secs: Option<(f64, f64)>,
    /// `FMODSoundAdd`'s 2nd and 3rd arguments, i.e. `FMODSoundAdd(file, 3d, streamed)`. Both are
    /// inert here -- 3D positioning isn't emulated (see `external/fmod.rs`'s
    /// `warn_3d_ignored_once`) and every
    /// sound is decoded into memory up front rather than streamed -- but `FMODSoundIs3d` /
    /// `FMODSoundIsStreamed` exist to read them back, so they're remembered rather than dropped.
    is_3d: bool,
    streamed: bool,
    /// FMODSoundSetLoopCount, in FMOD's own units: `-1` (the default) = loop forever, `0` = play
    /// once, `n` = loop `n` more times. Copied onto each instance at play time, exactly like
    /// `max_volume` and `loop_points_secs`. See `FmodInstanceState::loop_count` for what the
    /// positive case actually does.
    loop_count: i32,
    /// FMODSoundSet3dMinMaxDistance, in the game's own world units. Purely so
    /// `FMODSoundGetMaxDist`/`FMODInstanceGetMaxDist` can read back what the game set -- nothing
    /// applies distance attenuation, since there are no 3D positions to attenuate against.
    min_max_distance: (f64, f64),
}

/// Where a loaded sound's PCM comes from.
///
/// Sampled formats decode once at load and are simply a buffer. A tracker module is not decoded at
/// all -- it is *performed* (see `audio_wasm/tracker.rs`), and the result is enormous relative to
/// the file: renex engine's `radiorevolution.mod` is 244 KB on disk and 142 MB of stereo `f32`
/// once played out, and it loads nine such files at startup. Rendering them all eagerly would cost
/// most of a gigabyte for music the game plays one track of at a time, so a module is parsed at
/// load (cheap, and it is what makes the load succeed or fail honestly) and performed on first use.
enum FmodSource {
    Pcm(Rc<AudioBuffer>),
    /// Compressed bytes live in `AudioBufferCache` under this key until first play.
    Cached {
        key: AudioCacheKey,
    },
    Tracker {
        module: Rc<tracker::Module>,
        /// The file it came from, for the log line each performance writes. Nothing else uses it.
        name: Rc<str>,
        /// Cache key for the rendered PCM (music LRU + pin while playing/paused).
        key: AudioCacheKey,
    },
}

/// Sample rate tracker modules are performed at.
///
/// Fixed rather than following the `AudioContext` (which is 48 kHz on most machines) because the
/// buffer size scales with it and nothing in a module justifies the extra 9%: the samples inside
/// one are typically 8-16 kHz to begin with. Web Audio resamples on playback either way.
/// Rendered tracker PCM is kept in the shared audio cache (music LRU), not a fixed-count list.
const TRACKER_SAMPLE_RATE: u32 = 44100;

impl AudioManager {
    pub fn new(_do_output: bool, _capture_audio: bool) -> Self {
        // `do_output`/`capture_audio` are native-only concerns (headless TAS playback, ffmpeg
        // capture) with no wasm equivalent -- always produce real output here.
        let ctx = AudioContext::new().expect("failed to create AudioContext");
        let global_gain = ctx.create_gain().expect("failed to create global GainNode");
        let _ = global_gain.connect_with_audio_node(ctx.destination().as_ref());
        let _ = ctx.resume();

        // Browsers block audio output until a user gesture occurs on the page. Waiting for one is
        // the host's job, not the engine's. It owns
        // the context -- so only it can resume one -- and it is the side with a document to hear a
        // gesture on at all, which a worker does not have. `resume` above is still sent, and the
        // host's own unlock answers it when the player first touches anything.
        //
        // Nothing is lost by waiting: the Web Audio spec freezes a suspended context's clock, so a
        // sound started while blocked begins from its first sample the instant the context really
        // resumes.

        let fmod_master_gain = ctx.create_gain().expect("failed to create FMOD master GainNode");
        let _ = fmod_master_gain.connect_with_audio_node(ctx.destination().as_ref());

        let (lazy, policy) = crate::audio_cache_bridge::initial_mode_and_policy();
        Self {
            ctx,
            global_gain,
            cache: AudioBufferCache::new(lazy, policy),
            builtin_pins: HashMap::new(),
            exclusive_slot: None,
            non_exclusive: HashMap::new(),
            end_times: HashMap::new(),
            multimedia_end: None,
            soundfont: None,
            has_midi: false,
            ssound: HashMap::new(),
            next_ssound_handle: 0,
            ssound_pins: HashMap::new(),
            fmod_sounds: HashMap::new(),
            next_fmod_sound: 0,
            fmod_pins: HashMap::new(),
            fmod_instances: HashMap::new(),
            next_fmod_instance: 0,
            fmod_group_gains: HashMap::new(),
            fmod_group_pitches: HashMap::new(),
            fmod_group_volumes: HashMap::new(),
            fmod_group_muted: HashMap::new(),
            fmod_group_paused: HashMap::new(),
            fmod_master_gain,
            fmod_master_volume: 1.0,
            fmod_master_muted: false,
            sga_emitters: HashMap::new(),
            sga_group_volumes: [1.0; SGA_GROUP_COUNT],
            sga_initialized: false,
            mm_streams: HashMap::new(),
            next_mm_handle: MM_FIRST_HANDLE,
            mm_initialized: false,
            mm_global_volume: 1.0,
        }
    }

    pub fn capture_audio(&mut self) {}

    pub fn stop_audio_capture(&mut self) {}

    pub fn set_audio_cache_mode(&mut self, lazy: bool, policy: AudioCachePolicy) {
        self.cache.set_mode(lazy, policy);
    }

    pub fn audio_cache_stats(&self) -> AudioCacheStats {
        self.cache.stats()
    }

    fn decode_key_into_buffer(&mut self, key: &AudioCacheKey) -> Option<Rc<AudioBuffer>> {
        if let Some(buf) = self.cache.get_buffer(key) {
            self.cache.touch(key);
            return Some(buf);
        }
        let format = self.cache.format(key)?;
        let bytes = self.cache.compressed(key)?.to_vec().into_boxed_slice();
        // MIDI leaves before the planar step the three codecs share: it renders straight into its
        // destination buffer instead. See `render_midi`.
        if format == SoundFormat::Midi {
            let (buffer, frames, sample_rate) = self.render_midi(&bytes)?;
            let duration = frames as f64 / f64::from(sample_rate.max(1));
            return Some(self.cache.put_decoded(key.clone(), Rc::new(buffer), duration))
        }
        let (channels, sample_rate, planar, frames) = match format {
            SoundFormat::Mp3 => {
                let d = mp3::decode(&bytes)?;
                (d.channels, d.sample_rate, d.planar_samples, d.frames)
            },
            SoundFormat::Wav => {
                let d = wav::decode(&bytes)?;
                (d.channels, d.sample_rate, d.planar_samples, d.frames)
            },
            SoundFormat::Ogg => {
                let d = ogg::decode(&bytes)?;
                (d.channels, d.sample_rate, d.planar_samples, d.frames)
            },
            // Handled above, before this match.
            SoundFormat::Midi => return None,
        };
        let buffer = decoded_to_buffer(&self.ctx, channels, sample_rate, &planar, frames)?;
        let duration = frames as f64 / sample_rate.max(1) as f64;
        Some(self.cache.put_decoded(key.clone(), Rc::new(buffer), duration))
    }

    /// Renders a MIDI score into its destination `AudioBuffer` a chunk at a time.
    ///
    /// The three real codecs decode into a planar `Vec` and copy that whole into a new buffer,
    /// which for MIDI would mean holding the track twice: a four-minute render is ~84MB, so the
    /// pair would put ~168MB in flight and permanently raise wasm's linear-memory high-water mark
    /// (it never shrinks) for a track the cache may well evict a minute later. Instead the buffer
    /// is sized from the score up front and each rendered chunk is copied in at its own offset,
    /// which keeps wasm's share down to `midi::CHUNK_FRAMES` -- 128KB -- however long the music is.
    ///
    /// Returns the buffer with its frame count and rate, for the caller to hand to the cache.
    fn render_midi(&self, bytes: &[u8]) -> Option<(AudioBuffer, usize, u32)> {
        // No soundfont means the host never installed one, so there is nothing to render this
        // score with. Leaving the sound silent is exactly what happened before MIDI was supported
        // at all, rather than failing the play.
        let Some(soundfont) = self.soundfont.clone() else {
            web_sys::console::warn_1(&"MIDI sound played with no SoundFont installed -- it will be silent.".into());
            return None
        };
        let (max_frames, sample_rate) = {
            let policy = self.cache.policy();
            let share = policy.music_budget_bytes / cache::MIDI_BUDGET_SHARE_DIVISOR;
            (share / (usize::from(midi::CHANNELS) * 4), policy.midi_sample_rate)
        };
        let score = midi::parse_score(bytes, sample_rate, max_frames)?;
        if score.truncated {
            // The only case here where the result is wrong rather than merely expensive, so it
            // says so: a GM8 MIDI loop this long is a malformed score far more often than real
            // music, and either way silence after N minutes is worth being able to explain.
            web_sys::console::warn_1(
                &format!("MIDI score is longer than one track may claim; truncated to {:.0}s.", score.seconds()).into(),
            );
        }

        let buffer =
            self.ctx.create_buffer(u32::from(midi::CHANNELS), score.frames as u32, score.sample_rate as f32).ok()?;
        let started = js_sys::Date::now();
        let complete = midi::render(&score, &soundfont, |start, left, right| {
            let start = start as u32;
            buffer.copy_to_channel_with_start_in_channel(left, 0, start).is_ok()
                && buffer.copy_to_channel_with_start_in_channel(right, 1, start).is_ok()
        });
        if !complete {
            return None
        }

        // Worth a line: rendering is by far the most expensive thing this cache does, it happens
        // on the frame the music starts, and this is the number to look at if a game hitches when
        // its battle theme kicks in. Once per track -- a decoded entry is reused until evicted.
        web_sys::console::log_1(
            &format!(
                "rendered MIDI from {}KB to {:.1}s of audio ({}kHz, {:.0}ms, {:.1}MB)",
                bytes.len() / 1024,
                score.seconds(),
                score.sample_rate / 1000,
                js_sys::Date::now() - started,
                score.rendered_bytes() as f64 / (1024.0 * 1024.0),
            )
            .into(),
        );
        Some((buffer, score.frames, score.sample_rate))
    }

    fn pin_builtin(&mut self, id: i32, key: &AudioCacheKey) {
        let n = self.builtin_pins.entry(id).or_insert(0);
        *n += 1;
        if *n == 1 {
            self.cache.pin(key);
        }
    }

    fn unpin_builtin(&mut self, id: i32, key: &AudioCacheKey) {
        let clear = if let Some(n) = self.builtin_pins.get_mut(&id) {
            *n = n.saturating_sub(1);
            *n == 0
        } else {
            false
        };
        if clear {
            self.builtin_pins.remove(&id);
            self.cache.unpin(key);
        }
    }

    pub fn add_mp3(&mut self, file: Box<[u8]>, sound_id: i32) -> Option<Mp3Handle> {
        let key = AudioCacheKey::EmbeddedSound { sound_id };
        let kind = self.cache.register(key.clone(), file, SoundFormat::Mp3);
        let handle = Mp3Handle { key: key.clone(), id: sound_id };
        if !self.cache.lazy() || kind == cache::AudioKind::Sfx {
            self.decode_key_into_buffer(&key)?;
        }
        Some(handle)
    }

    pub fn add_wav(
        &mut self,
        file: Box<[u8]>,
        sound_id: i32,
        volume: f64,
        _use_3d: bool,
        exclusive: bool,
    ) -> Option<WavHandle> {
        let key = AudioCacheKey::EmbeddedSound { sound_id };
        let kind = self.cache.register(key.clone(), file, SoundFormat::Wav);
        let gain_node = self.ctx.create_gain().ok()?;
        gain_node.gain().set_value(make_volume(volume));
        let _ = gain_node.connect_with_audio_node(self.global_gain.as_ref());
        let handle = WavHandle { key: key.clone(), gain_node, id: sound_id, exclusive };
        if !self.cache.lazy() || kind == cache::AudioKind::Sfx {
            self.decode_key_into_buffer(&key)?;
        }
        Some(handle)
    }

    /// Registers a `.mid` built-in sound. Mirrors `add_wav` -- the handle type is the same, so
    /// `sound_loop`/`sound_stop`/`sound_volume` need to know nothing about MIDI.
    ///
    /// `register_streamed`, not `register`, and deliberately never decoded here. A `.mid` is tiny
    /// (tens of KB) so `classify_compressed` would call it SFX on size and mark it permanent and
    /// eager -- but its *rendered* form is full-length music, tens of megabytes, and rendering it
    /// is far more expensive than decoding anything else here. Streamed registration is precisely
    /// the "small on disk, big and costly once realised, must obey the music budget" case.
    pub fn add_midi(&mut self, file: Box<[u8]>, sound_id: i32, volume: f64, exclusive: bool) -> Option<WavHandle> {
        let key = AudioCacheKey::EmbeddedSound { sound_id };
        self.has_midi = true;
        self.cache.register_streamed(key.clone(), file, SoundFormat::Midi);
        let gain_node = self.ctx.create_gain().ok()?;
        gain_node.gain().set_value(make_volume(volume));
        let _ = gain_node.connect_with_audio_node(self.global_gain.as_ref());
        Some(WavHandle { key, gain_node, id: sound_id, exclusive })
    }

    /// Whether this game has MIDI sounds and no bank to play them through yet.
    pub fn needs_soundfont(&self) -> bool {
        self.has_midi && self.soundfont.is_none()
    }

    /// Installs the SoundFont bank. `false` if the bytes aren't a usable `.sf2`, which leaves MIDI
    /// silent rather than stopping the game.
    pub fn set_soundfont(&mut self, bytes: &[u8]) -> bool {
        match midi::parse_soundfont(bytes) {
            Some(sf) => {
                self.soundfont = Some(sf);
                true
            },
            None => false,
        }
    }

    pub fn discard_sound(&mut self, sound_id: i32) {
        let key = AudioCacheKey::EmbeddedSound { sound_id };
        self.stop_sound(sound_id);
        self.cache.discard_decoded(&key);
    }

    pub fn restore_sound(&mut self, sound_id: i32) {
        let key = AudioCacheKey::EmbeddedSound { sound_id };
        let _ = self.decode_key_into_buffer(&key);
    }

    fn play_exclusive(&mut self, buffer: &AudioBuffer, id: i32, looping: bool) {
        if let Some((old, old_id)) = self.exclusive_slot.take() {
            stop_source(&old);
            let old_key = AudioCacheKey::EmbeddedSound { sound_id: old_id };
            self.unpin_builtin(old_id, &old_key);
        }
        let Ok(source) = self.ctx.create_buffer_source() else { return };
        source.set_buffer(Some(buffer));
        source.set_loop(looping);
        // Exclusive playback bypasses every GainNode, connecting straight to the destination --
        // matches native's Mixer::write_samples exactly: the exclusive branch writes to the
        // output with no volume multiply at all, only non-exclusive sources get gain applied.
        // sound_volume/sound_global_volume therefore have zero effect on MP3s or Multimedia-kind
        // WAVs, which is real native GM8 behavior (confirmed by reading mixer.rs), not a bug here.
        let _ = source.connect_with_audio_node(self.ctx.destination().as_ref());
        let _ = source.start();
        self.exclusive_slot = Some((source, id));
    }

    fn play_non_exclusive(&mut self, handle: &WavHandle, buffer: &AudioBuffer, looping: bool) {
        let Ok(source) = self.ctx.create_buffer_source() else { return };
        source.set_buffer(Some(buffer));
        source.set_loop(looping);
        let _ = source.connect_with_audio_node(handle.gain_node.as_ref());
        let _ = source.start();
        self.non_exclusive.entry(handle.id).or_default().push(source);
    }

    pub fn play_mp3(&mut self, handle: &Mp3Handle, start_time: u128) {
        let Some(buffer) = self.decode_key_into_buffer(&handle.key) else { return };
        self.pin_builtin(handle.id, &handle.key);
        let end_time = length_to_ns(buffer.length() as usize, buffer.sample_rate() as u32) + start_time;
        self.multimedia_end = Some((handle.id, Some(end_time)));
        self.play_exclusive(&buffer, handle.id, false);
    }

    pub fn play_wav(&mut self, handle: &WavHandle, start_time: u128) {
        let Some(buffer) = self.decode_key_into_buffer(&handle.key) else { return };
        self.pin_builtin(handle.id, &handle.key);
        let end_time = length_to_ns(buffer.length() as usize, buffer.sample_rate() as u32) + start_time;
        if handle.exclusive {
            self.multimedia_end = Some((handle.id, Some(end_time)));
        } else if self.end_times.get(&handle.id) != Some(&None) {
            self.end_times.insert(handle.id, Some(end_time));
        }
        if handle.exclusive {
            self.play_exclusive(&buffer, handle.id, false);
        } else {
            self.play_non_exclusive(handle, &buffer, false);
        }
    }

    pub fn loop_mp3(&mut self, handle: &Mp3Handle) {
        let Some(buffer) = self.decode_key_into_buffer(&handle.key) else { return };
        self.pin_builtin(handle.id, &handle.key);
        self.multimedia_end = Some((handle.id, None));
        self.play_exclusive(&buffer, handle.id, true);
    }

    pub fn loop_wav(&mut self, handle: &WavHandle) {
        let Some(buffer) = self.decode_key_into_buffer(&handle.key) else { return };
        self.pin_builtin(handle.id, &handle.key);
        if handle.exclusive {
            self.multimedia_end = Some((handle.id, None));
        } else {
            self.end_times.insert(handle.id, None);
        }
        if handle.exclusive {
            self.play_exclusive(&buffer, handle.id, true);
        } else {
            self.play_non_exclusive(handle, &buffer, true);
        }
    }

    pub fn stop_sound(&mut self, id: i32) {
        self.end_times.remove(&id);
        if self.multimedia_end.map(|(x, _)| x) == Some(id) {
            self.multimedia_end = None;
        }
        if let Some(sources) = self.non_exclusive.remove(&id) {
            for source in sources {
                stop_source(&source);
            }
        }
        if self.exclusive_slot.as_ref().map(|(_, x)| *x) == Some(id) {
            if let Some((source, _)) = self.exclusive_slot.take() {
                stop_source(&source);
            }
        }
        let key = AudioCacheKey::EmbeddedSound { sound_id: id };
        // Drop all pins for this id (stop ends every instance).
        if self.builtin_pins.remove(&id).is_some() {
            self.cache.unpin(&key);
        }
    }

    pub fn stop_all(&mut self) {
        self.end_times.clear();
        self.multimedia_end = None;
        for (_, sources) in self.non_exclusive.drain() {
            for source in sources {
                stop_source(&source);
            }
        }
        if let Some((source, _)) = self.exclusive_slot.take() {
            stop_source(&source);
        }
        for id in self.builtin_pins.drain().map(|(id, _)| id).collect::<Vec<_>>() {
            let key = AudioCacheKey::EmbeddedSound { sound_id: id };
            self.cache.unpin(&key);
        }
    }

    pub fn set_global_volume(&self, vol: f64) {
        self.global_gain.gain().set_value(make_volume(vol));
    }

    pub fn sound_playing(&self, sound_id: i32, current_time: u128) -> bool {
        self.mp3_playing(sound_id, current_time) || self.wav_playing(sound_id, current_time)
    }

    fn mp3_playing(&self, sound_id: i32, current_time: u128) -> bool {
        self.multimedia_end
            .map(|(id, end_time)| id == sound_id && end_time.map(|x| x > current_time).unwrap_or(true))
            .unwrap_or(false)
    }

    fn wav_playing(&self, sound_id: i32, current_time: u128) -> bool {
        match self.end_times.get(&sound_id) {
            Some(&Some(x)) => x > current_time,
            Some(None) => true,
            None => false,
        }
    }

    // -- ssound.dll support (game/external/ssound.rs) -----------------------------------------
    //
    // Unlike the embedded-sound API above, every value here is in the units `ssound.dll`'s own
    // GML wrapper scripts use (confirmed by decompiling a real game's `ss_*.gml` scripts):
    // volume in hundredths of a decibel (-10000 silent .. 0 full, DirectSound's own convention),
    // frequency as an absolute Hz value (not a ratio), pan -10000 (left) .. 10000 (right). Byte-
    // domain position/length conversions happen in `ssound.rs`, not here, so this stays in
    // natural Web Audio units (seconds, Hz, linear gain, -1.0..1.0 pan).

    fn ss_next_handle(&mut self) -> String {
        self.next_ssound_handle += 1;
        self.next_ssound_handle.to_string()
    }

    /// Registers compressed bytes for ssound. Decode on first play when lazy + music.
    pub fn ss_register_file(&mut self, path: &str, bytes: Box<[u8]>, format: SoundFormat) -> Option<String> {
        let key = AudioCacheKey::ExternalFile { path: format!("ss:{path}") };
        let kind = self.cache.register(key.clone(), bytes, format);
        let sample_rate = if !self.cache.lazy() || kind == cache::AudioKind::Sfx {
            let buf = self.decode_key_into_buffer(&key)?;
            buf.sample_rate()
        } else {
            44100.0
        };
        let gain_node = self.ctx.create_gain().ok()?;
        let pan_node = self.ctx.create_stereo_panner().ok()?;
        let _ = gain_node.connect_with_audio_node(pan_node.as_ref());
        let _ = pan_node.connect_with_audio_node(self.ctx.destination().as_ref());
        let handle = self.ss_next_handle();
        let buffer = self.cache.get_buffer(&key);
        self.ssound.insert(handle.clone(), SsSoundState {
            key,
            buffer,
            original_sample_rate: sample_rate,
            gain_node,
            pan_node,
            current_source: None,
            playback_rate_ratio: 1.0,
            looping: false,
            started_at_ctx_time: 0.0,
            started_at_offset_secs: 0.0,
            paused_at_offset_secs: None,
        });
        Some(handle)
    }

    /// Registers a freshly-decoded sound and returns its handle. Not playing yet -- matches
    /// `SS_LoadSound`, which only loads into memory (`SS_PlaySound`/`SS_LoopSound` start it).
    pub fn ss_register_sound(
        &mut self,
        channels: u16,
        sample_rate: u32,
        planar: &[f32],
        frames: usize,
    ) -> Option<String> {
        let buffer = decoded_to_buffer(&self.ctx, channels, sample_rate, planar, frames)?;
        let gain_node = self.ctx.create_gain().ok()?;
        let pan_node = self.ctx.create_stereo_panner().ok()?;
        let _ = gain_node.connect_with_audio_node(pan_node.as_ref());
        let _ = pan_node.connect_with_audio_node(self.ctx.destination().as_ref());
        let handle = self.ss_next_handle();
        let key = AudioCacheKey::ExternalFile { path: format!("ss-pcm:{handle}") };
        let duration = frames as f64 / sample_rate.max(1) as f64;
        let buffer = self.cache.store_decoded(key.clone(), Rc::new(buffer), duration, false);
        self.ssound.insert(handle.clone(), SsSoundState {
            key,
            buffer: Some(buffer),
            original_sample_rate: sample_rate as f32,
            gain_node,
            pan_node,
            current_source: None,
            playback_rate_ratio: 1.0,
            looping: false,
            started_at_ctx_time: 0.0,
            started_at_offset_secs: 0.0,
            paused_at_offset_secs: None,
        });
        Some(handle)
    }

    fn ss_ensure_buffer(&mut self, handle: &str) -> Option<Rc<AudioBuffer>> {
        let key = self.ssound.get(handle)?.key.clone();
        let buf = self.decode_key_into_buffer(&key)?;
        if let Some(state) = self.ssound.get_mut(handle) {
            state.buffer = Some(buf.clone());
            if state.original_sample_rate <= 0.0 {
                state.original_sample_rate = buf.sample_rate();
            }
        }
        Some(buf)
    }

    fn ss_pin(&mut self, handle: &str) {
        let Some(key) = self.ssound.get(handle).map(|s| s.key.clone()) else { return };
        let n = self.ssound_pins.entry(handle.to_string()).or_insert(0);
        *n += 1;
        if *n == 1 {
            self.cache.pin(&key);
        }
    }

    fn ss_unpin(&mut self, handle: &str) {
        let Some(key) = self.ssound.get(handle).map(|s| s.key.clone()) else { return };
        let clear = if let Some(n) = self.ssound_pins.get_mut(handle) {
            *n = n.saturating_sub(1);
            *n == 0
        } else {
            false
        };
        if clear {
            self.ssound_pins.remove(handle);
            self.cache.unpin(&key);
        }
    }

    /// (Re)starts playback of `handle` from `offset_secs`, stopping whatever was previously
    /// playing for it first. Shared by play/loop/resume/seek, which only differ in what offset
    /// and `looping` flag they pass in.
    fn ss_start_source(&mut self, handle: &str, offset_secs: f64) {
        let Some(buffer) = self.ss_ensure_buffer(handle) else { return };
        let ctx = self.ctx.clone();
        let Some(state) = self.ssound.get_mut(handle) else { return };
        if let Some(old) = state.current_source.take() {
            stop_source(&old);
        }
        let Ok(source) = ctx.create_buffer_source() else { return };
        source.set_buffer(Some(&buffer));
        source.set_loop(state.looping);
        source.playback_rate().set_value(state.playback_rate_ratio as f32);
        let _ = source.connect_with_audio_node(state.gain_node.as_ref());
        let offset_secs = offset_secs.max(0.0);
        let _ = source.start_with_when_and_grain_offset(0.0, offset_secs);
        state.started_at_ctx_time = ctx.current_time();
        state.started_at_offset_secs = offset_secs;
        state.paused_at_offset_secs = None;
        state.current_source = Some(source);
    }

    pub fn ss_play_sound(&mut self, handle: &str) -> bool {
        if !self.ssound.contains_key(handle) {
            return false
        }
        if let Some(state) = self.ssound.get_mut(handle) {
            state.looping = false;
        }
        if !self.ssound_pins.contains_key(handle) {
            self.ss_pin(handle);
        }
        self.ss_start_source(handle, 0.0);
        true
    }

    pub fn ss_loop_sound(&mut self, handle: &str) -> bool {
        if !self.ssound.contains_key(handle) {
            return false
        }
        if let Some(state) = self.ssound.get_mut(handle) {
            state.looping = true;
        }
        if !self.ssound_pins.contains_key(handle) {
            self.ss_pin(handle);
        }
        self.ss_start_source(handle, 0.0);
        true
    }

    pub fn ss_stop_sound(&mut self, handle: &str) -> bool {
        let Some(state) = self.ssound.get_mut(handle) else { return false };
        if let Some(source) = state.current_source.take() {
            stop_source(&source);
        }
        state.started_at_offset_secs = 0.0;
        state.paused_at_offset_secs = None;
        self.ss_unpin(handle);
        true
    }

    pub fn ss_pause_sound(&mut self, handle: &str) -> bool {
        let ctx = self.ctx.clone();
        let Some(state) = self.ssound.get_mut(handle) else { return false };
        if state.paused_at_offset_secs.is_some() {
            return true // already paused
        }
        if state.current_source.is_none() {
            return false
        }
        // Must read the current elapsed position *before* taking `current_source` out --
        // `current_offset_secs` falls back to `started_at_offset_secs` (i.e. wherever playback
        // last started or seeked from, typically the very beginning of a freshly looped track)
        // whenever there's no live source, so computing it after `.take()` always reported the
        // wrong position -- this was the actual cause of "resume restarts from the beginning"
        // instead of the real pause point.
        let pos = state.current_offset_secs(&ctx);
        if let Some(source) = state.current_source.take() {
            stop_source(&source);
        }
        state.paused_at_offset_secs = Some(pos);
        true
    }

    /// If paused, continues from where it was. If stopped, restarts from the beginning. If
    /// already playing, does nothing (matches `SS_ResumeSound`'s own documented behavior --
    /// avoids an audible restart-glitch on a sound that never actually stopped).
    pub fn ss_resume_sound(&mut self, handle: &str) -> bool {
        let Some(state) = self.ssound.get(handle) else { return false };
        let resume_at = match state.paused_at_offset_secs {
            Some(pos) => pos,
            None if state.current_source.is_none() => 0.0,
            None => return true,
        };
        self.ss_start_source(handle, resume_at);
        true
    }

    pub fn ss_set_sound_vol(&self, handle: &str, centibels: f64) -> bool {
        let Some(state) = self.ssound.get(handle) else { return false };
        // Standard DirectSound centibels-to-linear-gain conversion: 0cB -> gain 1.0 (full),
        // -10000cB (-100dB) -> gain 1e-5 (effectively silent). Clamped to 1.0 max in case a game
        // passes a positive centibels value (out of DirectSound's documented -10000..0 range) --
        // same defensive reasoning as the FMOD volume functions: a real DLL almost certainly
        // clamps rather than actually amplifying, and a `GainNode` has no such ceiling on its own.
        state.gain_node.gain().set_value((10f64.powf(centibels / 2000.0) as f32).min(1.0));
        true
    }

    pub fn ss_get_sound_vol(&self, handle: &str) -> Option<f64> {
        let state = self.ssound.get(handle)?;
        let gain = f64::from(state.gain_node.gain().value()).max(1e-9);
        Some(2000.0 * gain.log10())
    }

    /// Changing frequency mid-playback re-anchors the position bookkeeping first, since it
    /// assumes a constant rate between `started_at_ctx_time` and now -- without this, a later
    /// `GetSoundPosition`/`GetSoundLength` after a frequency change would be computed as if the
    /// old rate had applied for the whole elapsed duration.
    pub fn ss_set_sound_freq(&mut self, handle: &str, hz: f64) -> bool {
        let ctx = self.ctx.clone();
        let Some(state) = self.ssound.get_mut(handle) else { return false };
        if state.current_source.is_some() && state.paused_at_offset_secs.is_none() {
            state.started_at_offset_secs = state.current_offset_secs(&ctx);
            state.started_at_ctx_time = ctx.current_time();
        }
        state.playback_rate_ratio = f64::from(hz as f32) / f64::from(state.original_sample_rate);
        if let Some(source) = &state.current_source {
            source.playback_rate().set_value(state.playback_rate_ratio as f32);
        }
        true
    }

    pub fn ss_get_sound_freq(&self, handle: &str) -> Option<f64> {
        let state = self.ssound.get(handle)?;
        Some(state.playback_rate_ratio * f64::from(state.original_sample_rate))
    }

    pub fn ss_set_sound_pan(&self, handle: &str, pan: f64) -> bool {
        let Some(state) = self.ssound.get(handle) else { return false };
        // Linear approximation of DirectSound's own attenuation-based pan curve -- close enough
        // that the difference shouldn't be perceptible; not an exact match.
        state.pan_node.pan().set_value((pan / 10000.0).clamp(-1.0, 1.0) as f32);
        true
    }

    pub fn ss_get_sound_pan(&self, handle: &str) -> Option<f64> {
        let state = self.ssound.get(handle)?;
        Some(f64::from(state.pan_node.pan().value()) * 10000.0)
    }

    pub fn ss_get_sound_position_secs(&self, handle: &str) -> Option<f64> {
        Some(self.ssound.get(handle)?.current_offset_secs(&self.ctx))
    }

    pub fn ss_set_sound_position_secs(&mut self, handle: &str, offset_secs: f64) -> bool {
        let offset_secs = offset_secs.max(0.0);
        let Some(state) = self.ssound.get(handle) else { return false };
        if state.paused_at_offset_secs.is_some() {
            self.ssound.get_mut(handle).unwrap().paused_at_offset_secs = Some(offset_secs);
        } else if state.current_source.is_some() {
            self.ss_start_source(handle, offset_secs);
        } else {
            self.ssound.get_mut(handle).unwrap().started_at_offset_secs = offset_secs;
        }
        true
    }

    pub fn ss_sound_duration_secs(&mut self, handle: &str) -> Option<f64> {
        self.ss_ensure_buffer(handle)?;
        Some(self.ssound.get(handle)?.duration_secs())
    }

    /// `(sample_rate, channel_count)` -- lets `ssound.rs` convert its byte-domain position/
    /// length/bytes-per-second values to/from the seconds this module deals in natively.
    pub fn ss_sound_format(&mut self, handle: &str) -> Option<(u32, u16)> {
        let buffer = self.ss_ensure_buffer(handle)?;
        Some((buffer.sample_rate() as u32, buffer.number_of_channels() as u16))
    }

    pub fn ss_is_sound_playing(&self, handle: &str) -> Option<bool> {
        Some(self.ssound.get(handle)?.is_playing(&self.ctx))
    }

    pub fn ss_is_sound_looping(&self, handle: &str) -> Option<bool> {
        Some(self.ssound.get(handle)?.looping)
    }

    pub fn ss_is_sound_paused(&self, handle: &str) -> Option<bool> {
        Some(self.ssound.get(handle)?.paused_at_offset_secs.is_some())
    }

    pub fn ss_free_sound(&mut self, handle: &str) {
        self.ss_stop_sound(handle);
        if let Some(state) = self.ssound.remove(handle) {
            self.cache.remove(&state.key);
        }
        self.ssound_pins.remove(handle);
    }

    pub fn ss_unload(&mut self) {
        for handle in self.ssound.keys().cloned().collect::<Vec<_>>() {
            self.ss_free_sound(&handle);
        }
    }

    // -- GMFMODSimple.dll support (game/external/fmod.rs) --------------------------------------
    //
    // Unlike ssound.dll, FMOD separates a loaded "Sound" (`fmod_sounds`, from `FMODSoundAdd`) from
    // a playing "Instance" (`fmod_instances`, from `FMODSoundPlay`/`FMODSoundLoop`) -- multiple
    // instances can play concurrently from one loaded sound. All units here are already in the
    // form FMOD's own GML wrappers use directly (linear 0..1 volume, no byte/centibel
    // conversions needed), confirmed by decompiling a real game's `FMODXxx.gml` scripts.

    fn fmod_next_sound_handle(&mut self) -> u64 {
        self.next_fmod_sound += 1;
        self.next_fmod_sound
    }

    fn fmod_next_instance_handle(&mut self) -> u64 {
        self.next_fmod_instance += 1;
        self.next_fmod_instance
    }

    /// Registers compressed bytes (lazy-friendly). Decode happens on first play / length query
    /// when lazy mode is on and the file classifies as music.
    pub fn fmod_register_file(
        &mut self,
        path: &str,
        bytes: Box<[u8]>,
        format: SoundFormat,
        is_3d: bool,
        streamed: bool,
    ) -> Option<u64> {
        let key = AudioCacheKey::ExternalFile { path: path.to_string() };
        let kind = self.cache.register(key.clone(), bytes, format);
        if !self.cache.lazy() || kind == cache::AudioKind::Sfx {
            self.decode_key_into_buffer(&key)?;
        }
        Some(self.fmod_insert_sound(FmodSource::Cached { key }, is_3d, streamed))
    }

    /// Registers a freshly-decoded sound and returns its handle. Not playing yet -- matches
    /// `FMODSoundAdd`, which only loads into memory (`FMODSoundPlay`/`FMODSoundLoop` start it).
    ///
    /// Prefer `fmod_register_file` for new loads so lazy mode can keep compressed bytes. This
    /// path remains for callers that already hold PCM.
    ///
    /// `is_3d`/`streamed` are `FMODSoundAdd`'s own 2nd/3rd arguments, recorded rather than acted
    /// on -- see `FmodSoundState`. `loop_count` starts at FMOD's documented default of -1 (loop
    /// forever *if* the sound is played via a looping call at all; a `FMODSoundPlay` instance is
    /// one-shot regardless).
    pub fn fmod_register_sound(
        &mut self,
        channels: u16,
        sample_rate: u32,
        planar: &[f32],
        frames: usize,
        is_3d: bool,
        streamed: bool,
    ) -> Option<u64> {
        let buffer = decoded_to_buffer(&self.ctx, channels, sample_rate, planar, frames)?;
        Some(self.fmod_insert_sound(FmodSource::Pcm(Rc::new(buffer)), is_3d, streamed))
    }

    /// Registers a parsed tracker module. Same contract as `fmod_register_sound`, except that no
    /// audio exists yet -- the module is performed on first use. See `FmodSource`.
    pub fn fmod_register_tracker(
        &mut self,
        module: tracker::Module,
        name: &str,
        is_3d: bool,
        streamed: bool,
    ) -> Option<u64> {
        let key = AudioCacheKey::Tracker { path: name.to_string() };
        // No compressed bytes; rendered PCM is stored under this key for the music LRU.
        let source = FmodSource::Tracker { module: Rc::new(module), name: name.into(), key };
        Some(self.fmod_insert_sound(source, is_3d, streamed))
    }

    fn fmod_insert_sound(&mut self, source: FmodSource, is_3d: bool, streamed: bool) -> u64 {
        let handle = self.fmod_next_sound_handle();
        self.fmod_sounds.insert(handle, FmodSoundState {
            source,
            group: 0,
            max_volume: 1.0,
            loop_points_secs: None,
            is_3d,
            streamed,
            loop_count: -1,
            min_max_distance: (1.0, 10000.0),
        });
        handle
    }

    fn fmod_cache_key(&self, sound_id: u64) -> Option<AudioCacheKey> {
        match &self.fmod_sounds.get(&sound_id)?.source {
            FmodSource::Cached { key } => Some(key.clone()),
            FmodSource::Tracker { key, .. } => Some(key.clone()),
            FmodSource::Pcm(_) => None,
        }
    }

    fn fmod_pin_sound(&mut self, sound_id: u64) {
        let Some(key) = self.fmod_cache_key(sound_id) else { return };
        let n = self.fmod_pins.entry(sound_id).or_insert(0);
        *n += 1;
        if *n == 1 {
            self.cache.pin(&key);
        }
    }

    fn fmod_unpin_sound(&mut self, sound_id: u64) {
        let Some(key) = self.fmod_cache_key(sound_id) else { return };
        let clear = if let Some(n) = self.fmod_pins.get_mut(&sound_id) {
            *n = n.saturating_sub(1);
            *n == 0
        } else {
            false
        };
        if clear {
            self.fmod_pins.remove(&sound_id);
            self.cache.unpin(&key);
        }
    }

    /// The sound's PCM, performing its tracker module first if that hasn't happened yet.
    ///
    /// Every read of a sound's audio goes through here rather than touching a field, because for a
    /// module "the buffer" is a thing that may not exist yet and may have been evicted since -- and
    /// a caller that forgot would get silence for a sound that loaded perfectly well.
    fn fmod_buffer(&mut self, sound_id: u64) -> Option<Rc<AudioBuffer>> {
        enum Need {
            Pcm(Rc<AudioBuffer>),
            Cached(AudioCacheKey),
            Tracker { module: Rc<tracker::Module>, name: Rc<str>, key: AudioCacheKey },
        }
        let need = match &self.fmod_sounds.get(&sound_id)?.source {
            FmodSource::Pcm(buffer) => Need::Pcm(buffer.clone()),
            FmodSource::Cached { key } => Need::Cached(key.clone()),
            FmodSource::Tracker { module, name, key } => {
                Need::Tracker { module: module.clone(), name: name.clone(), key: key.clone() }
            },
        };
        match need {
            Need::Pcm(buffer) => Some(buffer),
            Need::Cached(key) => self.decode_key_into_buffer(&key),
            Need::Tracker { module, name, key } => {
                if let Some(buffer) = self.cache.get_buffer(&key) {
                    self.cache.touch(&key);
                    return Some(buffer);
                }
                let decoded = module.render(TRACKER_SAMPLE_RATE);
                web_sys::console::log_1(
                    &format!(
                        "performed tracker module {name}: {:.1}s, {} MB",
                        decoded.frames as f64 / decoded.sample_rate.max(1) as f64,
                        decoded.planar_samples.len() * 4 / 1_048_576,
                    )
                    .into(),
                );
                let buffer = Rc::new(decoded_to_buffer(
                    &self.ctx,
                    decoded.channels,
                    decoded.sample_rate,
                    &decoded.planar_samples,
                    decoded.frames,
                )?);
                let duration = decoded.frames as f64 / decoded.sample_rate.max(1) as f64;
                Some(self.cache.store_decoded(key, buffer, duration, false))
            },
        }
    }

    /// Whether `sound_id` is a live handle at all. Backs `FMODSoundAsyncReady`, whose answer here
    /// is simply "did the load succeed", since loading isn't asynchronous.
    pub fn fmod_sound_exists(&self, sound_id: u64) -> bool {
        self.fmod_sounds.contains_key(&sound_id)
    }

    /// `FMODSoundIs3d`/`FMODSoundIsStreamed` -- echo back what `FMODSoundAdd` was told.
    pub fn fmod_sound_is_3d(&self, sound_id: u64) -> Option<bool> {
        Some(self.fmod_sounds.get(&sound_id)?.is_3d)
    }

    pub fn fmod_sound_is_streamed(&self, sound_id: u64) -> Option<bool> {
        Some(self.fmod_sounds.get(&sound_id)?.streamed)
    }

    /// `FMODSoundGetNumChannels` -- the *audio* channel count (1 mono / 2 stereo) of the decoded
    /// sound, not anything to do with FMOD's playback "channels" (which this module calls
    /// instances).
    pub fn fmod_sound_get_num_channels(&mut self, sound_id: u64) -> Option<u32> {
        Some(self.fmod_buffer(sound_id)?.number_of_channels())
    }

    /// `FMODSoundSetLoopCount`/`FMODSoundGetLoopCount` -- the default for instances played from
    /// this sound afterwards; already-playing instances keep the count they were created with,
    /// same as `max_volume` and the loop points.
    pub fn fmod_set_sound_loop_count(&mut self, sound_id: u64, count: i32) -> bool {
        let Some(state) = self.fmod_sounds.get_mut(&sound_id) else { return false };
        state.loop_count = count;
        true
    }

    pub fn fmod_get_sound_loop_count(&self, sound_id: u64) -> Option<i32> {
        Some(self.fmod_sounds.get(&sound_id)?.loop_count)
    }

    /// `FMODSoundSet3dMinMaxDistance`/`FMODSoundGetMaxDist` -- stored, never applied. See
    /// `FmodSoundState::min_max_distance`.
    pub fn fmod_set_sound_min_max_distance(&mut self, sound_id: u64, min: f64, max: f64) -> bool {
        let Some(state) = self.fmod_sounds.get_mut(&sound_id) else { return false };
        state.min_max_distance = (min, max);
        true
    }

    pub fn fmod_get_sound_max_distance(&self, sound_id: u64) -> Option<f64> {
        Some(self.fmod_sounds.get(&sound_id)?.min_max_distance.1)
    }

    /// `FMODSoundSetGroup` -- only affects instances created *after* this call (real games always
    /// call it immediately after loading, before ever playing the sound, so this ordering is
    /// never actually observable as a bug).
    pub fn fmod_set_sound_group(&mut self, sound_id: u64, group: u64) -> bool {
        let Some(state) = self.fmod_sounds.get_mut(&sound_id) else { return false };
        state.group = group;
        true
    }

    /// `FMODSoundGetLength` -- the sound's own duration in milliseconds (matches the DLL's
    /// documented unit; everything else in this module works in seconds internally).
    pub fn fmod_sound_get_length_ms(&mut self, sound_id: u64) -> Option<f64> {
        let buffer = self.fmod_buffer(sound_id)?;
        Some(f64::from(buffer.length()) / f64::from(buffer.sample_rate()) * 1000.0)
    }

    pub fn fmod_set_sound_max_volume(&mut self, sound_id: u64, volume: f64) -> bool {
        let Some(state) = self.fmod_sounds.get_mut(&sound_id) else { return false };
        state.max_volume = volume as f32;
        true
    }

    pub fn fmod_get_sound_max_volume(&self, sound_id: u64) -> Option<f64> {
        Some(f64::from(self.fmod_sounds.get(&sound_id)?.max_volume))
    }

    /// `FMODSoundSetLoopPoints` -- `start`/`end` are 0..1 fractions of the sound's own duration
    /// (matching the DLL's documented unit), converted to seconds here since that's what
    /// `FmodInstanceState`/`AudioBufferSourceNode.loopStart`/`loopEnd` deal in.
    pub fn fmod_set_sound_loop_points(&mut self, sound_id: u64, start: f64, end: f64) -> bool {
        let Some(buffer) = self.fmod_buffer(sound_id) else { return false };
        let duration = f64::from(buffer.length()) / f64::from(buffer.sample_rate());
        let Some(state) = self.fmod_sounds.get_mut(&sound_id) else { return false };
        state.loop_points_secs = Some((start.clamp(0.0, 1.0) * duration, end.clamp(0.0, 1.0) * duration));
        true
    }

    /// `FMODSoundFree` -- only frees the loaded resource, matching the DLL's own doc ("returns 0
    /// on error (sound is being played by an instance)... don't forget to stop the instance...
    /// before calling this"): if any instance is still playing from it, refuse, rather than
    /// silently pulling the buffer out from under a live `Rc<AudioBuffer>` some instance still
    /// holds (which would actually be harmless here since it's a reference-counted clone, but
    /// matching the DLL's own documented error behavior is more useful to a game checking the
    /// return value than always succeeding).
    pub fn fmod_sound_free(&mut self, sound_id: u64) -> bool {
        if self.fmod_instances.values().any(|i| i.sound_id == sound_id) {
            return false
        }
        if let Some(key) = self.fmod_cache_key(sound_id) {
            self.cache.remove(&key);
        }
        self.fmod_pins.remove(&sound_id);
        self.fmod_sounds.remove(&sound_id).is_some()
    }

    fn fmod_group_gain_node(&mut self, group: u64) -> Option<GainNode> {
        if let Some(node) = self.fmod_group_gains.get(&group) {
            return Some(node.clone())
        }
        let node = self.ctx.create_gain().ok()?;
        let _ = node.connect_with_audio_node(self.fmod_master_gain.as_ref());
        self.fmod_group_gains.insert(group, node.clone());
        self.fmod_group_volumes.insert(group, 1.0);
        self.fmod_group_muted.insert(group, false);
        Some(node)
    }

    /// Clamps the applied gain to 1.0 max -- same reasoning as `FmodInstanceState::
    /// recompute_gain`: at least one real game passes `FMODGroupSetVolume` values far outside
    /// the documented 0..1 range (e.g. `200`) and sounds normal on real Windows, so FMOD is
    /// evidently clamping internally. `fmod_group_volumes` itself stays unclamped so
    /// `FMODGroupGetVolume` still echoes back exactly what was set.
    fn fmod_recompute_group_gain(&mut self, group: u64) {
        let volume = self.fmod_group_volumes.get(&group).copied().unwrap_or(1.0).clamp(0.0, 1.0);
        let muted = self.fmod_group_muted.get(&group).copied().unwrap_or(false);
        if let Some(node) = self.fmod_group_gain_node(group) {
            node.gain().set_value(if muted { 0.0 } else { volume });
        }
    }

    /// (Re)starts playback of `instance` from `offset_secs`, stopping whatever was previously
    /// playing for it first. Shared by play/loop/resume/pause-then-immediately-resume-at-same-spot,
    /// mirroring `ss_start_source`.
    fn fmod_start_source(&mut self, instance: u64, offset_secs: f64) {
        let ctx = self.ctx.clone();
        let Some(state) = self.fmod_instances.get_mut(&instance) else { return };
        if let Some(old) = state.current_source.take() {
            stop_source(&old);
        }
        let Ok(source) = ctx.create_buffer_source() else { return };
        source.set_buffer(Some(&state.buffer));
        source.set_loop(state.looping);
        if state.looping {
            if state.loop_start_secs > 0.0 {
                source.set_loop_start(state.loop_start_secs);
            }
            if let Some(end) = state.loop_end_secs {
                source.set_loop_end(end);
            }
        }
        source.playback_rate().set_value(state.effective_rate() as f32);
        let _ = source.connect_with_audio_node(state.gain_node.as_ref());
        let offset_secs = offset_secs.max(0.0);
        let _ = source.start_with_when_and_grain_offset(0.0, offset_secs);
        state.started_at_ctx_time = ctx.current_time();
        state.started_at_offset_secs = offset_secs;
        state.paused_at_offset_secs = None;
        state.current_source = Some(source);
    }

    /// Creates a new instance of `sound_id` and starts it (unless `start_paused`, matching
    /// `FMODSoundPlay`/`FMODSoundLoop`'s optional `paused` argument -- lets the caller tune the
    /// instance before it's actually audible). Returns the new instance handle, or `None` if
    /// `sound_id` isn't a registered sound.
    pub fn fmod_play(&mut self, sound_id: u64, looping: bool, start_paused: bool) -> Option<u64> {
        // Performs the module if this is its first play, so the rest of this reads a real buffer.
        let buffer = self.fmod_buffer(sound_id)?;
        self.fmod_pin_sound(sound_id);
        let sound = self.fmod_sounds.get(&sound_id)?;
        let group = sound.group;
        let max_volume = sound.max_volume;
        // A non-looping play is one-shot whatever the sound's loop count says: the DLL's
        // `FMODSoundPlay`/`FMODSoundPlay3d` set FMOD_LOOP_OFF on the channel, which overrides the
        // FMOD_LOOP_NORMAL the sound was created with.
        let loop_count = if looping { sound.loop_count } else { 0 };
        let looping = looping && loop_count != 0;
        let (loop_start_secs, loop_end_secs) = match sound.loop_points_secs {
            Some((start, end)) => (start, Some(end)),
            None => (0.0, None),
        };
        let group_gain = self.fmod_group_gain_node(group)?;
        let group_pitch_ratio = self.fmod_group_pitches.get(&group).copied().unwrap_or(1.0);
        let gain_node = self.ctx.create_gain().ok()?;
        let pan_node = self.ctx.create_stereo_panner().ok()?;
        let _ = gain_node.connect_with_audio_node(pan_node.as_ref());
        let _ = pan_node.connect_with_audio_node(group_gain.as_ref());
        let instance = self.fmod_next_instance_handle();
        self.fmod_instances.insert(instance, FmodInstanceState {
            buffer,
            sound_id,
            gain_node,
            pan_node,
            current_source: None,
            looping,
            playback_rate_ratio: 1.0,
            group_pitch_ratio,
            started_at_ctx_time: 0.0,
            started_at_offset_secs: 0.0,
            paused_at_offset_secs: if start_paused { Some(0.0) } else { None },
            group,
            raw_volume: 1.0,
            max_volume,
            muted: false,
            loop_start_secs,
            loop_end_secs,
            loop_count,
        });
        if let Some(state) = self.fmod_instances.get(&instance) {
            state.recompute_gain();
        }
        if !start_paused {
            self.fmod_start_source(instance, 0.0);
        }
        Some(instance)
    }

    pub fn fmod_instance_stop(&mut self, instance: u64) -> bool {
        match self.fmod_instances.remove(&instance) {
            Some(state) => {
                if let Some(source) = state.current_source {
                    stop_source(&source);
                }
                self.fmod_unpin_sound(state.sound_id);
                true
            },
            None => false,
        }
    }

    pub fn fmod_instance_set_volume(&mut self, instance: u64, volume: f64) -> bool {
        let Some(state) = self.fmod_instances.get_mut(&instance) else { return false };
        state.raw_volume = volume as f32;
        state.recompute_gain();
        true
    }

    pub fn fmod_instance_get_volume(&self, instance: u64) -> Option<f64> {
        Some(f64::from(self.fmod_instances.get(&instance)?.raw_volume))
    }

    pub fn fmod_instance_set_muted(&mut self, instance: u64, muted: bool) -> bool {
        let Some(state) = self.fmod_instances.get_mut(&instance) else { return false };
        state.muted = muted;
        state.recompute_gain();
        true
    }

    pub fn fmod_instance_get_muted(&self, instance: u64) -> Option<bool> {
        Some(self.fmod_instances.get(&instance)?.muted)
    }

    pub fn fmod_instance_get_sound(&self, instance: u64) -> Option<u64> {
        Some(self.fmod_instances.get(&instance)?.sound_id)
    }

    /// Re-anchors position bookkeeping first (same reasoning as `ss_set_sound_freq`): the offset
    /// math assumes a constant rate since `started_at_ctx_time`, so it must be re-based before
    /// the rate itself changes, not after.
    fn fmod_set_playback_rate(&mut self, instance: u64, ratio: f64) -> bool {
        let ctx = self.ctx.clone();
        let Some(state) = self.fmod_instances.get_mut(&instance) else { return false };
        if state.current_source.is_some() && state.paused_at_offset_secs.is_none() {
            state.started_at_offset_secs = state.current_offset_secs(&ctx);
            state.started_at_ctx_time = ctx.current_time();
        }
        state.playback_rate_ratio = ratio;
        if let Some(source) = &state.current_source {
            source.playback_rate().set_value(state.effective_rate() as f32);
        }
        true
    }

    /// `FMODGroupSetPitch` -- unlike volume, pitch has no chainable node, so this walks every
    /// instance currently in `group` and re-anchors + re-applies its live source's rate directly
    /// (mirroring `fmod_set_playback_rate`'s own re-anchor-before-rate-change logic, since the
    /// elapsed-position math assumes a constant rate since the last anchor point).
    pub fn fmod_group_set_pitch(&mut self, group: u64, pitch: f64) {
        let ctx = self.ctx.clone();
        self.fmod_group_pitches.insert(group, pitch);
        let ids: Vec<u64> = self.fmod_instances.iter().filter(|(_, s)| s.group == group).map(|(id, _)| *id).collect();
        for id in ids {
            let Some(state) = self.fmod_instances.get_mut(&id) else { continue };
            if state.current_source.is_some() && state.paused_at_offset_secs.is_none() {
                state.started_at_offset_secs = state.current_offset_secs(&ctx);
                state.started_at_ctx_time = ctx.current_time();
            }
            state.group_pitch_ratio = pitch;
            if let Some(source) = &state.current_source {
                source.playback_rate().set_value(state.effective_rate() as f32);
            }
        }
    }

    pub fn fmod_group_get_pitch(&self, group: u64) -> f64 {
        self.fmod_group_pitches.get(&group).copied().unwrap_or(1.0)
    }

    /// `FMODInstanceSetFrequency` -- absolute Hz (44100 = normal), converted to a playback-rate
    /// ratio against the instance's own buffer sample rate (same convention as ssound.dll's
    /// `SS_SetSoundFreq`).
    pub fn fmod_instance_set_frequency(&mut self, instance: u64, hz: f64) -> bool {
        let Some(sample_rate) = self.fmod_instances.get(&instance).map(|s| f64::from(s.buffer.sample_rate())) else {
            return false
        };
        self.fmod_set_playback_rate(instance, hz / sample_rate.max(1.0))
    }

    pub fn fmod_instance_get_frequency(&self, instance: u64) -> Option<f64> {
        let state = self.fmod_instances.get(&instance)?;
        Some(state.playback_rate_ratio * f64::from(state.buffer.sample_rate()))
    }

    /// `FMODInstanceSetPitch` -- a direct 0..10 multiplier (1 = normal), i.e. the same underlying
    /// value `SetFrequency` computes indirectly from an absolute Hz.
    pub fn fmod_instance_set_pitch(&mut self, instance: u64, pitch: f64) -> bool {
        self.fmod_set_playback_rate(instance, pitch)
    }

    pub fn fmod_instance_get_pitch(&self, instance: u64) -> Option<f64> {
        Some(self.fmod_instances.get(&instance)?.playback_rate_ratio)
    }

    /// `FMODInstanceSetPan` -- already -1..1, a direct match for `StereoPannerNode.pan` (no
    /// scaling needed, unlike ssound.dll's -10000..10000 DirectSound convention).
    pub fn fmod_instance_set_pan(&self, instance: u64, pan: f64) -> bool {
        let Some(state) = self.fmod_instances.get(&instance) else { return false };
        state.pan_node.pan().set_value(pan.clamp(-1.0, 1.0) as f32);
        true
    }

    pub fn fmod_instance_get_pan(&self, instance: u64) -> Option<f64> {
        Some(f64::from(self.fmod_instances.get(&instance)?.pan_node.pan().value()))
    }

    /// Port of `ss_pause_sound`'s logic: read the elapsed position *before* taking the source out
    /// (the actual root cause of the ssound.dll "resume restarts from the beginning" bug fixed
    /// earlier), and let `FmodInstanceState::current_offset_secs`'s loop-window-aware wrap handle
    /// sounds that have looped at least once (including ones with custom loop points).
    pub fn fmod_instance_set_paused(&mut self, instance: u64, pause: bool) -> bool {
        if pause {
            let ctx = self.ctx.clone();
            let Some(state) = self.fmod_instances.get_mut(&instance) else { return false };
            if state.paused_at_offset_secs.is_some() {
                return true // already paused
            }
            if state.current_source.is_none() {
                return false
            }
            let pos = state.current_offset_secs(&ctx);
            if let Some(source) = state.current_source.take() {
                stop_source(&source);
            }
            state.paused_at_offset_secs = Some(pos);
            true
        } else {
            let Some(state) = self.fmod_instances.get(&instance) else { return false };
            let resume_at = match state.paused_at_offset_secs {
                Some(pos) => pos,
                None if state.current_source.is_none() => 0.0,
                None => return true, // already playing
            };
            self.fmod_start_source(instance, resume_at);
            true
        }
    }

    pub fn fmod_instance_get_paused(&self, instance: u64) -> Option<bool> {
        Some(self.fmod_instances.get(&instance)?.paused_at_offset_secs.is_some())
    }

    pub fn fmod_instance_is_playing(&self, instance: u64) -> Option<bool> {
        Some(self.fmod_instances.get(&instance)?.is_playing(&self.ctx))
    }

    /// `FMODInstanceGetPosition` -- a 0 (start) .. 1 (end) fraction of the instance's own sound
    /// duration (matching the DLL's documented unit; internally this module works in seconds).
    pub fn fmod_instance_get_position(&self, instance: u64) -> Option<f64> {
        let state = self.fmod_instances.get(&instance)?;
        let duration = state.duration_secs();
        if duration <= 0.0 {
            return Some(0.0)
        }
        Some((state.current_offset_secs(&self.ctx) / duration).clamp(0.0, 1.0))
    }

    /// `FMODInstanceSetPosition` -- same 0..1 fraction, converted to seconds. Seeking is
    /// implemented identically to `ss_set_sound_position_secs`: while paused, just move the
    /// remembered offset; while playing, restart the source at the new offset (mirroring
    /// `fmod_start_source`'s pause/resume machinery, since Web Audio has no native seek); while
    /// fully stopped, just move where the next `Play`/resume will start from.
    pub fn fmod_instance_set_position(&mut self, instance: u64, pos: f64) -> bool {
        let Some(state) = self.fmod_instances.get(&instance) else { return false };
        let offset_secs = (pos.clamp(0.0, 1.0)) * state.duration_secs();
        if state.paused_at_offset_secs.is_some() {
            self.fmod_instances.get_mut(&instance).unwrap().paused_at_offset_secs = Some(offset_secs);
        } else if state.current_source.is_some() {
            self.fmod_start_source(instance, offset_secs);
        } else {
            self.fmod_instances.get_mut(&instance).unwrap().started_at_offset_secs = offset_secs;
        }
        true
    }

    /// `FMODInstanceSoundGetLength` -- like `fmod_sound_get_length_ms`, but keyed by the instance
    /// itself rather than needing to look its sound back up separately.
    pub fn fmod_instance_sound_get_length_ms(&self, instance: u64) -> Option<f64> {
        Some(self.fmod_instances.get(&instance)?.duration_secs() * 1000.0)
    }

    /// `FMODInstanceSetLoopPoints` -- the per-instance twin of `FMODSoundSetLoopPoints`, same
    /// 0..1 fractions. Applies to the live source immediately if there is one, so a game can
    /// narrow the loop window of music that is already playing.
    pub fn fmod_set_instance_loop_points(&mut self, instance: u64, start: f64, end: f64) -> bool {
        let Some(state) = self.fmod_instances.get_mut(&instance) else { return false };
        let duration = state.duration_secs();
        state.loop_start_secs = start.clamp(0.0, 1.0) * duration;
        state.loop_end_secs = Some(end.clamp(0.0, 1.0) * duration);
        if let Some(source) = &state.current_source {
            if state.looping {
                source.set_loop_start(state.loop_start_secs);
                if let Some(end) = state.loop_end_secs {
                    source.set_loop_end(end);
                }
            }
        }
        true
    }

    /// `FMODInstanceSetLoopCount`/`FMODInstanceGetLoopCount` -- see `FmodInstanceState::
    /// loop_count` for why only `0` and `-1` are exact. `AudioBufferSourceNode.loop` is settable
    /// on a source that is already running, so switching an instance between one-shot and looping
    /// mid-playback doesn't need the source rebuilt (which would audibly restart it).
    pub fn fmod_instance_set_loop_count(&mut self, instance: u64, count: i32) -> bool {
        let Some(state) = self.fmod_instances.get_mut(&instance) else { return false };
        state.loop_count = count;
        state.looping = count != 0;
        if let Some(source) = &state.current_source {
            source.set_loop(state.looping);
            if state.looping {
                if state.loop_start_secs > 0.0 {
                    source.set_loop_start(state.loop_start_secs);
                }
                if let Some(end) = state.loop_end_secs {
                    source.set_loop_end(end);
                }
            }
        }
        true
    }

    pub fn fmod_instance_get_loop_count(&self, instance: u64) -> Option<i32> {
        Some(self.fmod_instances.get(&instance)?.loop_count)
    }

    /// `FMODInstanceGetAudibility` -- how loud this instance actually ends up, i.e. its own
    /// (max-volume- and mute-adjusted) gain multiplied by every gain stage it routes through.
    /// Reads the nodes rather than recomputing from the bookkeeping fields so it can't drift out
    /// of step with what is really being mixed.
    pub fn fmod_instance_get_audibility(&self, instance: u64) -> Option<f64> {
        let state = self.fmod_instances.get(&instance)?;
        let group_gain = self.fmod_group_gains.get(&state.group).map_or(1.0, |g| g.gain().value());
        Some(f64::from(state.gain_node.gain().value() * group_gain * self.fmod_master_gain.gain().value()))
    }

    /// `FMODInstanceGetMaxDist` -- reads through to the sound the instance is playing, which is
    /// where `FMODSoundSet3dMinMaxDistance` stored it.
    pub fn fmod_instance_get_max_distance(&self, instance: u64) -> Option<f64> {
        let sound_id = self.fmod_instances.get(&instance)?.sound_id;
        self.fmod_get_sound_max_distance(sound_id)
    }

    /// Sets `group`'s persistent gain node (unless muted, in which case the value is remembered
    /// but not applied until unmuted) -- only instances actually assigned to `group` (via
    /// `FMODSoundSetGroup` on their sound) are affected; other groups (e.g. a separate music
    /// group) are untouched.
    pub fn fmod_group_set_volume(&mut self, group: u64, volume: f64) {
        self.fmod_group_gain_node(group); // ensure it exists before we touch the bookkeeping maps
        self.fmod_group_volumes.insert(group, volume as f32);
        self.fmod_recompute_group_gain(group);
    }

    pub fn fmod_group_get_volume(&mut self, group: u64) -> f64 {
        self.fmod_group_gain_node(group);
        f64::from(self.fmod_group_volumes.get(&group).copied().unwrap_or(1.0))
    }

    /// `FMODGroupSetMuted` -- "does not affect individual settings of instances", so this forces
    /// the group's own gain node to 0 without touching any instance's own volume/mute state,
    /// exactly like `FmodInstanceState::muted` does at the instance level.
    pub fn fmod_group_set_muted(&mut self, group: u64, muted: bool) {
        self.fmod_group_gain_node(group);
        self.fmod_group_muted.insert(group, muted);
        self.fmod_recompute_group_gain(group);
    }

    pub fn fmod_group_get_muted(&mut self, group: u64) -> bool {
        self.fmod_group_gain_node(group);
        self.fmod_group_muted.get(&group).copied().unwrap_or(false)
    }

    /// `FMODGroupSetPaused` -- pauses/resumes every instance currently in `group`, via the same
    /// per-instance path `FMODInstanceSetPaused` uses (Web Audio has no group-level pause: pausing
    /// means remembering an offset and tearing the source down, which is inherently per-source).
    ///
    /// "Currently in": an instance created *later*, while the group is still flagged paused, does
    /// start playing, where real FMOD would hold it paused with the group. Modelling that
    /// faithfully would mean giving every instance a separate self-paused and group-paused state
    /// and resolving the pair on every play/pause/seek -- not worth it for a case games don't hit,
    /// since group pause is a "pause the whole game" control and a paused game isn't starting new
    /// music.
    pub fn fmod_group_set_paused(&mut self, group: u64, paused: bool) {
        self.fmod_group_paused.insert(group, paused);
        let ids: Vec<u64> = self.fmod_instances.iter().filter(|(_, s)| s.group == group).map(|(id, _)| *id).collect();
        for id in ids {
            self.fmod_instance_set_paused(id, paused);
        }
    }

    pub fn fmod_group_get_paused(&self, group: u64) -> bool {
        self.fmod_group_paused.get(&group).copied().unwrap_or(false)
    }

    /// `FMODGroupSetPan`/`FMODGroupSetFrequency` -- FMOD calls these "override" for a reason:
    /// they write straight through to every channel currently in the group, replacing whatever
    /// `FMODInstanceSetPan`/`SetFrequency` had set on each, rather than layering a group-wide
    /// offset on top. Reproduced literally, which is also why (unlike `FMODGroupSetPitch`) there's
    /// nothing to denormalize onto instances created later -- an override applies to the channels
    /// that existed when it was called.
    pub fn fmod_group_set_pan(&mut self, group: u64, pan: f64) {
        let ids: Vec<u64> = self.fmod_instances.iter().filter(|(_, s)| s.group == group).map(|(id, _)| *id).collect();
        for id in ids {
            self.fmod_instance_set_pan(id, pan);
        }
    }

    pub fn fmod_group_set_frequency(&mut self, group: u64, hz: f64) {
        let ids: Vec<u64> = self.fmod_instances.iter().filter(|(_, s)| s.group == group).map(|(id, _)| *id).collect();
        for id in ids {
            self.fmod_instance_set_frequency(id, hz);
        }
    }

    /// `FMODAllSetPaused` -- the master channel group's pause, i.e. every instance regardless of
    /// group. Same per-instance mechanism (and same documented limitation) as
    /// `fmod_group_set_paused`.
    pub fn fmod_all_set_paused(&mut self, paused: bool) {
        let ids: Vec<u64> = self.fmod_instances.keys().copied().collect();
        for id in ids {
            self.fmod_instance_set_paused(id, paused);
        }
    }

    /// `FMODAllSetMuted` -- unlike pause, mute *does* have a single node to hang off (the master
    /// gain everything routes through), so this forces it silent without touching any instance's
    /// or group's own volume/mute, and unmuting restores exactly what `FMODMasterSetVolume` last
    /// set. Same split as `FMODGroupSetMuted` one level down.
    pub fn fmod_all_set_muted(&mut self, muted: bool) {
        self.fmod_master_muted = muted;
        self.fmod_recompute_master_gain();
    }

    fn fmod_recompute_master_gain(&self) {
        self.fmod_master_gain.gain().set_value(if self.fmod_master_muted { 0.0 } else { self.fmod_master_volume });
    }

    /// Stops and frees only instances whose sound was assigned to `group` via
    /// `FMODSoundSetGroup` -- must NOT touch instances in other groups (e.g. stopping a
    /// sound-effects group on death/restart must leave a separately-grouped, currently-paused
    /// music instance alone, since the same restart code immediately resumes it afterward).
    pub fn fmod_group_stop(&mut self, group: u64) {
        let to_remove: Vec<(u64, u64)> = self
            .fmod_instances
            .iter()
            .filter(|(_, state)| state.group == group)
            .map(|(id, state)| (*id, state.sound_id))
            .collect();
        for (id, sound_id) in to_remove {
            if let Some(state) = self.fmod_instances.remove(&id) {
                if let Some(source) = state.current_source {
                    stop_source(&source);
                }
            }
            self.fmod_unpin_sound(sound_id);
        }
    }

    /// `FMODAllStop` -- a distinct DLL symbol from `FMODGroupStop`, not merely `FMODGroupStop(0)`:
    /// stops and frees *every* instance regardless of group. Leaves loaded sounds (`fmod_sounds`)
    /// alone, unlike `fmod_unload`/`FMODfree`, which is a full shutdown.
    pub fn fmod_all_stop(&mut self) {
        let sound_ids: Vec<u64> = self.fmod_instances.values().map(|s| s.sound_id).collect();
        for (_, state) in self.fmod_instances.drain() {
            if let Some(source) = state.current_source {
                stop_source(&source);
            }
        }
        for sound_id in sound_ids {
            self.fmod_unpin_sound(sound_id);
        }
    }

    /// `FMODMasterSetVolume` -- every group routes through this one node, so it multiplies with
    /// every group's and every instance's own volume for free. Clamped to 1.0 max for the same
    /// reason as `recompute_gain`/`fmod_recompute_group_gain` -- an out-of-spec value here would
    /// otherwise amplify literally everything at once.
    pub fn fmod_master_set_volume(&mut self, volume: f64) {
        self.fmod_master_volume = volume.clamp(0.0, 1.0) as f32;
        self.fmod_recompute_master_gain();
    }

    pub fn fmod_get_num_instances(&self) -> usize {
        self.fmod_instances.len()
    }

    pub fn fmod_unload(&mut self) {
        self.fmod_all_stop();
        self.fmod_sounds.clear();
    }

    // ---- SGAudio.dll (see game/external/sgaudio.rs) ----

    /// `sga_Init`: true the first time, false while already initialized -- the DLL checks its init
    /// flag and returns 0 rather than re-initializing.
    pub fn sga_init(&mut self) -> bool {
        if self.sga_initialized {
            return false
        }
        self.sga_initialized = true;
        true
    }

    /// `sga_Free`: tears down every emitter. False if it was never initialized, which is the
    /// DLL's own answer (it checks the same flag before doing anything).
    pub fn sga_free(&mut self) -> bool {
        for id in self.sga_emitters.keys().copied().collect::<Vec<_>>() {
            self.sga_destroy_emitter(id);
        }
        let was_initialized = self.sga_initialized;
        self.sga_initialized = false;
        self.sga_group_volumes = [1.0; SGA_GROUP_COUNT];
        was_initialized
    }

    /// The lowest unused id, mirroring the DLL's emitter table: it scans for a free slot and only
    /// grows when there is none, so ids stay small and are reused after a `sga_DestroyEmitter`.
    /// Worth matching rather than handing out a monotonic counter, because these ids round-trip
    /// through GML as *reals* -- a game holding a stale one must be able to hit a live emitter with
    /// it exactly as often as it would on Windows, or a latent bug shows up only in one of the two.
    fn sga_alloc_id(&self) -> i32 {
        (0..).find(|id| !self.sga_emitters.contains_key(id)).unwrap_or(0)
    }

    /// `volume * every group in the mask` -- measured off the DLL's own recompute, which walks all
    /// 32 bits of the type mask and multiplies in each set group's volume.
    ///
    /// Takes the volume separately from the mask so `sga_fade_volume` can ask what the gain *would*
    /// be at its target without having to fabricate an emitter to ask about.
    fn sga_gain_for(&self, volume: f64, type_mask: u32) -> f64 {
        let mut gain = volume;
        for (group, group_volume) in self.sga_group_volumes.iter().enumerate() {
            if type_mask & (1 << group) != 0 {
                gain *= group_volume;
            }
        }
        gain
    }

    fn sga_apply_gain(&mut self, id: i32) {
        let Some(emitter) = self.sga_emitters.get(&id) else { return };
        let (instance, volume, type_mask) = (emitter.instance, emitter.volume, emitter.type_mask);
        let gain = self.sga_gain_for(volume, type_mask);
        self.fmod_instance_set_volume(instance, gain);
    }

    /// Loads `bytes` and attaches them to a fresh FMOD sound + paused instance. Shared by
    /// `sga_CreateEmitter` (which leaves it paused) and `sga_TrackPlay` (which then plays it).
    fn sga_attach_sound(
        &mut self,
        path: &str,
        bytes: Box<[u8]>,
        format: SoundFormat,
        looping: bool,
    ) -> Option<(u64, u64)> {
        // Not streamed and not 3D: `sga_TrackPlay` pins every track to `AL_SOURCE_RELATIVE` with
        // position (0,0,0), i.e. always 2D, whatever a later `sga_Set3DMode` records.
        let sound_id = self.fmod_register_file(path, bytes, format, false, false)?;
        match self.fmod_play(sound_id, looping, true) {
            Some(instance) => Some((sound_id, instance)),
            None => {
                // Undecodable file: drop the sound again rather than leaving a handle whose buffer
                // never materializes.
                self.fmod_sound_free(sound_id);
                None
            },
        }
    }

    /// `sga_CreateEmitter`: loads a file into a new, not-yet-playing emitter.
    pub fn sga_create_emitter(&mut self, path: &str, bytes: Box<[u8]>, format: SoundFormat) -> Option<i32> {
        let (sound_id, instance) = self.sga_attach_sound(path, bytes, format, false)?;
        let id = self.sga_alloc_id();
        self.sga_emitters.insert(id, SgaEmitter {
            sound_id,
            instance,
            // The DLL initializes both volume fields to 1.0 and the distance factor likewise.
            volume: 1.0,
            type_mask: 0,
            looping: false,
            three_d: false,
            dist_factor: 1.0,
            position: (0.0, 0.0, 0.0),
        });
        Some(id)
    }

    /// `sga_TrackPlay`'s reuse path: swaps the file behind an existing emitter, keeping its id and
    /// its volume/type settings, and starts it. The DLL loads the new buffer into the same emitter
    /// struct, so the id a game stored stays valid across a track change.
    pub fn sga_retrack(&mut self, id: i32, path: &str, bytes: Box<[u8]>, format: SoundFormat, looping: bool) -> bool {
        if !self.sga_emitters.contains_key(&id) {
            return false
        }
        let Some((sound_id, instance)) = self.sga_attach_sound(path, bytes, format, looping) else { return false };
        // Only tear the old pair down once the new one exists, so a failed load leaves the emitter
        // playing what it already had rather than silent.
        if let Some(old) = self.sga_emitters.get(&id) {
            let (old_instance, old_sound) = (old.instance, old.sound_id);
            self.fmod_instance_stop(old_instance);
            self.sga_release_sound(old_sound);
        }
        if let Some(emitter) = self.sga_emitters.get_mut(&id) {
            emitter.sound_id = sound_id;
            emitter.instance = instance;
            emitter.looping = looping;
        }
        self.sga_apply_gain(id);
        self.sga_play(id);
        true
    }

    /// Creates an emitter already playing `path` -- `sga_TrackPlay`'s fresh-emitter path.
    pub fn sga_track_play(
        &mut self,
        path: &str,
        bytes: Box<[u8]>,
        format: SoundFormat,
        looping: bool,
        type_mask: u32,
    ) -> Option<i32> {
        let (sound_id, instance) = self.sga_attach_sound(path, bytes, format, looping)?;
        let id = self.sga_alloc_id();
        self.sga_emitters.insert(id, SgaEmitter {
            sound_id,
            instance,
            volume: 1.0,
            type_mask,
            looping,
            three_d: false,
            dist_factor: 1.0,
            position: (0.0, 0.0, 0.0),
        });
        self.sga_apply_gain(id);
        self.sga_play(id);
        Some(id)
    }

    /// Drops a sound handle without purging its decoded bytes from the cache, unlike
    /// `fmod_sound_free`. Deliberate: this game's whole music system is
    /// `sga_DestroyEmitter` + `sga_TrackPlay` on every room change, and several rooms in a row
    /// share a track -- purging on destroy would re-decode a multi-megabyte Vorbis file on each
    /// transition. Leaving the entry for the music LRU to evict is exactly what the budget is for.
    fn sga_release_sound(&mut self, sound_id: u64) {
        // A no-op once the instance's own stop has unpinned it (`fmod_unpin_sound` drops the
        // per-sound counter entirely at zero, so it cannot double-unpin the cache), and the right
        // thing when there was no instance to stop.
        self.fmod_unpin_sound(sound_id);
        self.fmod_sounds.remove(&sound_id);
    }

    /// `sga_DestroyEmitter`. False for an id that was never live, matching "nothing happens with an
    /// invalid ID" -- which every call site relies on, since the game destroys emitter variables
    /// that are still at their `-1` initial value.
    pub fn sga_destroy_emitter(&mut self, id: i32) -> bool {
        let Some(emitter) = self.sga_emitters.remove(&id) else { return false };
        self.fmod_instance_stop(emitter.instance);
        self.sga_release_sound(emitter.sound_id);
        true
    }

    pub fn sga_exists(&self, id: i32) -> bool {
        self.sga_emitters.contains_key(&id)
    }

    /// `sga_Play`. Resumes a paused emitter, and restarts a playing one from the beginning -- which
    /// is what `alSourcePlay` does to an `AL_PLAYING` source, and what makes the DLL's own
    /// `sga_PlayFX` (`sga_Stop` then `sga_Play`) a re-trigger either way.
    pub fn sga_play(&mut self, id: i32) -> bool {
        let Some(emitter) = self.sga_emitters.get(&id) else { return false };
        let instance = emitter.instance;
        match self.fmod_instance_get_paused(instance) {
            // Paused (which `sga_Stop` leaves it as, at offset 0): resume from where it sits.
            Some(true) => self.fmod_instance_set_paused(instance, false),
            // Already running, or run off the end of a non-looping track: start over.
            Some(false) => {
                self.fmod_start_source(instance, 0.0);
                true
            },
            None => false,
        }
    }

    /// `sga_Pause`: stops output, keeping the play offset. Cancels a running `sga_FadeVolume` --
    /// the DLL clears the emitter's fade state here before calling `alSourcePause`.
    pub fn sga_pause(&mut self, id: i32) -> bool {
        let Some(emitter) = self.sga_emitters.get(&id) else { return false };
        let instance = emitter.instance;
        self.sga_cancel_fade(id);
        self.fmod_instance_set_paused(instance, true);
        true
    }

    /// `sga_Stop` *and* `sga_Rewind` -- one function in the DLL, exported twice under the same
    /// address: stop output and reset the offset to 0, clearing any fade.
    pub fn sga_stop(&mut self, id: i32) -> bool {
        let Some(emitter) = self.sga_emitters.get(&id) else { return false };
        let instance = emitter.instance;
        self.sga_cancel_fade(id);
        self.fmod_instance_set_paused(instance, true);
        if let Some(state) = self.fmod_instances.get_mut(&instance) {
            state.paused_at_offset_secs = Some(0.0);
            state.started_at_offset_secs = 0.0;
        }
        true
    }

    /// `sga_IsPlaying`, which is `alGetSourcei(AL_SOURCE_STATE) == AL_PLAYING` -- so a *paused*
    /// emitter answers false, and so does one that has run off the end of a non-looping track.
    /// Note this is the opposite of `fmod_instance_is_playing`, which answers true while paused on
    /// purpose (FMOD's `Channel::isPlaying` really does); composing the two is what makes each
    /// DLL's own semantics come out right.
    pub fn sga_is_playing(&self, id: i32) -> Option<bool> {
        let emitter = self.sga_emitters.get(&id)?;
        let paused = self.fmod_instance_get_paused(emitter.instance)?;
        Some(!paused && self.fmod_instance_is_playing(emitter.instance)?)
    }

    /// `sga_SetVolume` (linear 0..1). Applies immediately, cancelling any fade in flight.
    pub fn sga_set_volume(&mut self, id: i32, volume: f64) -> bool {
        if !self.sga_emitters.contains_key(&id) {
            return false
        }
        self.sga_cancel_fade(id);
        if let Some(emitter) = self.sga_emitters.get_mut(&id) {
            emitter.volume = volume;
        }
        self.sga_apply_gain(id);
        true
    }

    /// `sga_GetVolume` -- the last value set, not the live gain. See `SgaEmitter::volume`.
    pub fn sga_get_volume(&self, id: i32) -> Option<f64> {
        Some(self.sga_emitters.get(&id)?.volume)
    }

    /// `sga_FadeVolume(id, target, seconds)`: a linear ramp to `target`, left to Web Audio's own
    /// automation rather than stepped per frame (this manager has no frame hook, and a ramp
    /// survives the source rebuilds that pause/seek do).
    ///
    /// `SgaEmitter::volume` is deliberately *not* moved to the target: the DLL ramps the applied
    /// volume and leaves the value `sga_GetVolume` reads alone.
    pub fn sga_fade_volume(&mut self, id: i32, target: f64, seconds: f64) -> bool {
        let Some(emitter) = self.sga_emitters.get(&id) else { return false };
        let (instance, type_mask) = (emitter.instance, emitter.type_mask);
        let target_gain = self.sga_gain_for(target, type_mask);
        let Some(state) = self.fmod_instances.get(&instance) else { return false };
        let now = self.ctx.current_time();
        let gain = state.gain_node.gain();
        let current = gain.value();
        let _ = gain.cancel_scheduled_values(now);
        let _ = gain.set_value_at_time(current, now);
        // A zero or negative time is an immediate set, not a degenerate ramp.
        if seconds > 0.0 {
            let _ = gain.linear_ramp_to_value_at_time(target_gain.clamp(0.0, 1.0) as f32, now + seconds);
        } else {
            gain.set_value(target_gain.clamp(0.0, 1.0) as f32);
        }
        true
    }

    /// Drops any scheduled ramp and pins the gain back to what the emitter's own settings say.
    /// Needed because `FmodInstanceState::recompute_gain` writes the node with `set_value`, which an
    /// automation event already on the timeline would otherwise override.
    fn sga_cancel_fade(&mut self, id: i32) {
        let Some(emitter) = self.sga_emitters.get(&id) else { return };
        let instance = emitter.instance;
        let now = self.ctx.current_time();
        if let Some(state) = self.fmod_instances.get(&instance) {
            let _ = state.gain_node.gain().cancel_scheduled_values(now);
        }
        self.sga_apply_gain(id);
    }

    /// `sga_SetLooping` (0 = once, 1 = forever).
    pub fn sga_set_looping(&mut self, id: i32, looping: bool) -> bool {
        let Some(emitter) = self.sga_emitters.get_mut(&id) else { return false };
        emitter.looping = looping;
        let instance = emitter.instance;
        self.fmod_instance_set_loop_count(instance, if looping { -1 } else { 0 });
        true
    }

    /// `sga_SetPanning` (-1 left .. 1 right), a direct match for `StereoPannerNode.pan`.
    pub fn sga_set_panning(&mut self, id: i32, pan: f64) -> bool {
        let Some(emitter) = self.sga_emitters.get(&id) else { return false };
        self.fmod_instance_set_pan(emitter.instance, pan)
    }

    /// `sga_SetPitch`, a playback-rate multiplier around 1.0.
    pub fn sga_set_pitch(&mut self, id: i32, pitch: f64) -> bool {
        let Some(emitter) = self.sga_emitters.get(&id) else { return false };
        let instance = emitter.instance;
        self.fmod_instance_set_pitch(instance, pitch)
    }

    /// `sga_SetPlayOffset`, in seconds -- converted to the 0..1 fraction the FMOD instance layer
    /// works in.
    pub fn sga_set_play_offset(&mut self, id: i32, secs: f64) -> bool {
        let Some(emitter) = self.sga_emitters.get(&id) else { return false };
        let instance = emitter.instance;
        let Some(length_ms) = self.fmod_instance_sound_get_length_ms(instance) else { return false };
        if length_ms <= 0.0 {
            return false
        }
        self.fmod_instance_set_position(instance, secs / (length_ms / 1000.0))
    }

    /// `sga_GetPlayOffset`, in seconds.
    pub fn sga_get_play_offset(&self, id: i32) -> Option<f64> {
        let emitter = self.sga_emitters.get(&id)?;
        let fraction = self.fmod_instance_get_position(emitter.instance)?;
        let length_ms = self.fmod_instance_sound_get_length_ms(emitter.instance)?;
        Some(fraction * length_ms / 1000.0)
    }

    /// `sga_SetGroupVolume`. Out-of-range group ids are ignored, as the DLL's fixed 32-entry table
    /// leaves them nowhere to go.
    pub fn sga_set_group_volume(&mut self, group: usize, volume: f64) -> bool {
        if group >= SGA_GROUP_COUNT {
            return false
        }
        self.sga_group_volumes[group] = volume;
        // Every emitter whose mask includes this group changes gain, so re-push all of them.
        for id in self.sga_emitters.keys().copied().collect::<Vec<_>>() {
            self.sga_apply_gain(id);
        }
        true
    }

    pub fn sga_get_group_volume(&self, group: usize) -> Option<f64> {
        self.sga_group_volumes.get(group).copied()
    }

    /// `sga_SetType`: the emitter's group bitmask.
    pub fn sga_set_type(&mut self, id: i32, type_mask: u32) -> bool {
        let Some(emitter) = self.sga_emitters.get_mut(&id) else { return false };
        emitter.type_mask = type_mask;
        self.sga_apply_gain(id);
        true
    }

    pub fn sga_get_type(&self, id: i32) -> Option<u32> {
        Some(self.sga_emitters.get(&id)?.type_mask)
    }

    /// `sga_Set3DMode` / `sga_SetDistFactor` / `sga_SetPosition` -- recorded, not acted on. See
    /// `SgaEmitter::three_d`.
    pub fn sga_set_3d_mode(&mut self, id: i32, three_d: bool) -> bool {
        let Some(emitter) = self.sga_emitters.get_mut(&id) else { return false };
        emitter.three_d = three_d;
        true
    }

    pub fn sga_set_dist_factor(&mut self, id: i32, factor: f64) -> bool {
        let Some(emitter) = self.sga_emitters.get_mut(&id) else { return false };
        emitter.dist_factor = factor;
        true
    }

    pub fn sga_set_position(&mut self, id: i32, x: f64, y: f64, z: f64) -> bool {
        let Some(emitter) = self.sga_emitters.get_mut(&id) else { return false };
        emitter.position = (x, y, z);
        true
    }

    // ---- MaizeMusic.dll (see game/external/maizemusic.rs) ----
    //
    // Units are the DLL's own, which are BASS's: volume and pan linear (0..1 and -1..1), position
    // and length in seconds, speed a multiplier. Playback is layered on the FMOD sound/instance
    // pair, same as the SGAudio block above.

    /// `mmInit`, which is `BASS_Init`: true the first time, false once already initialized
    /// (BASS answers `BASS_ERROR_ALREADY`, and `mmInit` normalizes its result to a bool).
    pub fn mm_init(&mut self) -> bool {
        if self.mm_initialized {
            return false
        }
        self.mm_initialized = true;
        true
    }

    /// `mmFree`, which is `BASS_Free`: every stream goes with it.
    pub fn mm_free(&mut self) {
        for handle in self.mm_streams.keys().copied().collect::<Vec<_>>() {
            self.mm_free_music(handle);
        }
        self.mm_initialized = false;
        self.mm_global_volume = 1.0;
    }

    /// Handles are not sequential from zero on purpose. They are BASS channel handles in the real
    /// DLL -- opaque 32-bit values, nowhere near a small integer -- and games pass things that are
    /// *not* handles into these functions all the time: this one's `killPlayer` calls
    /// `mm_stop(global.mv[2])` where `mv[2]` is the sound resource `sndJump`, because its music
    /// array holds sound resources and filenames side by side and only the filenames ever became
    /// handles. On Windows those calls hit no channel and do nothing. A handle space starting at
    /// 0 or 1 would turn each of them into a hit on an unrelated live stream.
    fn mm_alloc_handle(&mut self) -> u32 {
        let handle = self.next_mm_handle;
        self.next_mm_handle = self.next_mm_handle.wrapping_add(1).max(MM_FIRST_HANDLE);
        handle
    }

    /// `mmLoadMusic`/`mmLoadMusicExt`: registers the file and hands back a handle, without
    /// decoding or playing anything -- which is what makes a game that loads its whole soundtrack
    /// up front affordable. Whether the bytes are decodable at all is therefore not known until
    /// the first play; see `mm_start_instance`.
    pub fn mm_load(&mut self, path: &str, bytes: Box<[u8]>, format: SoundFormat, looping: bool) -> u32 {
        let key = AudioCacheKey::ExternalFile { path: path.to_string() };
        self.cache.register_streamed(key.clone(), bytes, format);
        let sound_id = self.fmod_insert_sound(FmodSource::Cached { key }, false, true);
        let handle = self.mm_alloc_handle();
        self.mm_streams.insert(handle, MmStream {
            sound_id,
            instance: None,
            path: path.to_string(),
            looping,
            // BASS's own defaults for a fresh channel.
            volume: 1.0,
            pan: 0.0,
            speed: 1.0,
            loop_section: None,
            pending_offset_secs: 0.0,
        });
        handle
    }

    /// `mmFreeMusic` (`BASS_ChannelStop` + `BASS_StreamFree`). False for a handle that was never
    /// live, which is what the DLL reports when `BASS_StreamFree` fails.
    pub fn mm_free_music(&mut self, handle: u32) -> bool {
        let Some(stream) = self.mm_streams.remove(&handle) else { return false };
        if let Some(instance) = stream.instance {
            self.fmod_instance_stop(instance);
        }
        // Drops the sound handle but leaves the cache entry, so re-loading the same file (which a
        // game that frees and reloads per room does constantly) reuses its compressed bytes
        // instead of reading and re-registering them. Same reasoning as `sga_release_sound`.
        self.fmod_unpin_sound(stream.sound_id);
        self.fmod_sounds.remove(&stream.sound_id);
        true
    }

    /// The gain a stream's instance should carry: its own volume times BASS's output volume.
    fn mm_apply_gain(&mut self, handle: u32) {
        let Some(stream) = self.mm_streams.get(&handle) else { return };
        let Some(instance) = stream.instance else { return };
        let gain = stream.volume * self.mm_global_volume;
        let now = self.ctx.current_time();
        if let Some(state) = self.fmod_instances.get(&instance) {
            // A `mmMusicSlideVolume` ramp still on the timeline would otherwise win over the
            // `set_value` inside `fmod_instance_set_volume`.
            let _ = state.gain_node.gain().cancel_scheduled_values(now);
        }
        self.fmod_instance_set_volume(instance, gain);
    }

    /// Creates the stream's FMOD instance and starts it at `offset_secs`, decoding the file if
    /// this is its first play, then pushes every setting the game applied beforehand onto it.
    fn mm_start_instance(&mut self, handle: u32, offset_secs: f64) -> bool {
        let Some(stream) = self.mm_streams.get(&handle) else { return false };
        let (sound_id, looping, loop_section, pan, speed) =
            (stream.sound_id, stream.looping, stream.loop_section, stream.pan, stream.speed);
        // A loop section loops the section forever whatever the stream's own loop flag says.
        let Some(instance) = self.fmod_play(sound_id, looping || loop_section.is_some(), true) else {
            // The load could not report this -- it never looked at the bytes. Say it once here,
            // because the symptom otherwise is one track of a soundtrack silently missing.
            let path = self.mm_streams.get(&handle).map(|s| s.path.clone()).unwrap_or_default();
            warn_once(format!("MaizeMusic: {path} could not be decoded, so it will be silent"));
            return false
        };
        if let Some(stream) = self.mm_streams.get_mut(&handle) {
            stream.instance = Some(instance);
        }
        self.mm_apply_gain(handle);
        self.fmod_instance_set_pan(instance, pan);
        self.fmod_instance_set_pitch(instance, speed);
        if let Some((start, end)) = loop_section {
            self.mm_apply_loop_section(instance, start, end);
        }
        self.fmod_start_source(instance, offset_secs);
        true
    }

    /// `mmMusicSetLoopSection`'s seconds, as the fractions the FMOD instance layer works in.
    fn mm_apply_loop_section(&mut self, instance: u64, start: f64, end: f64) -> bool {
        let Some(length_ms) = self.fmod_instance_sound_get_length_ms(instance) else { return false };
        let duration = length_ms / 1000.0;
        if duration <= 0.0 {
            return false
        }
        self.fmod_instance_set_loop_count(instance, -1);
        self.fmod_set_instance_loop_points(instance, start / duration, end / duration)
    }

    /// `mmMusicPlay`, which is `BASS_ChannelPlay(h, TRUE)` -- restart, so it always starts from
    /// the beginning even on a stream that is already playing or paused.
    pub fn mm_play(&mut self, handle: u32) -> bool {
        let Some(stream) = self.mm_streams.get(&handle) else { return false };
        match stream.instance {
            Some(instance) => {
                self.fmod_start_source(instance, 0.0);
                if let Some(stream) = self.mm_streams.get_mut(&handle) {
                    stream.pending_offset_secs = 0.0;
                }
                true
            },
            None => self.mm_start_instance(handle, 0.0),
        }
    }

    /// `mmMusicPause` (`BASS_ChannelPause`). A paused channel keeps its position, and keeps
    /// answering `mmMusicIsActive` with "paused" rather than "stopped" -- which this game depends
    /// on: it fades the music out and pauses it on death, and the room's own `MusicSet` must then
    /// *not* treat the track as finished and restart it from the top.
    pub fn mm_pause(&mut self, handle: u32) -> bool {
        let Some(stream) = self.mm_streams.get(&handle) else { return false };
        let Some(instance) = stream.instance else { return true };
        self.fmod_instance_set_paused(instance, true);
        true
    }

    /// `mmMusicResume`, which is `BASS_ChannelPlay(h, FALSE)`: carry on from where the channel
    /// sits, whether that is a pause position or the start of a stopped channel.
    pub fn mm_resume(&mut self, handle: u32) -> bool {
        let Some(stream) = self.mm_streams.get(&handle) else { return false };
        match stream.instance {
            Some(instance) => {
                self.fmod_instance_set_paused(instance, false);
                true
            },
            None => {
                let offset = stream.pending_offset_secs;
                self.mm_start_instance(handle, offset)
            },
        }
    }

    /// `mmMusicStop`: `BASS_ChannelStop` plus a seek back to 0, so the next play or resume starts
    /// from the beginning. See `MmStream::instance` for why the instance goes away entirely.
    pub fn mm_stop(&mut self, handle: u32) -> bool {
        let Some(stream) = self.mm_streams.get(&handle) else { return false };
        let instance = stream.instance;
        if let Some(instance) = instance {
            self.fmod_instance_stop(instance);
        }
        if let Some(stream) = self.mm_streams.get_mut(&handle) {
            stream.instance = None;
            stream.pending_offset_secs = 0.0;
        }
        true
    }

    /// `mmMusicIsActive`, passed straight through from `BASS_ChannelIsActive`: `0` stopped,
    /// `1` playing, `3` paused (`2`, stalled, cannot arise here -- nothing is being downloaded).
    /// An unknown handle is `0`, which is also what BASS answers for one.
    pub fn mm_is_active(&self, handle: u32) -> u32 {
        let Some(stream) = self.mm_streams.get(&handle) else { return MM_ACTIVE_STOPPED };
        let Some(instance) = stream.instance else { return MM_ACTIVE_STOPPED };
        match self.fmod_instance_get_paused(instance) {
            Some(true) => MM_ACTIVE_PAUSED,
            // Not paused, but a non-looping track may have run off its end by now.
            Some(false) if self.fmod_instance_is_playing(instance) == Some(true) => MM_ACTIVE_PLAYING,
            _ => MM_ACTIVE_STOPPED,
        }
    }

    /// `mmMusicSetVolume` (`BASS_ATTRIB_VOL`, 0..1 linear).
    pub fn mm_set_volume(&mut self, handle: u32, volume: f64) -> bool {
        let Some(stream) = self.mm_streams.get_mut(&handle) else { return false };
        stream.volume = volume;
        self.mm_apply_gain(handle);
        true
    }

    /// `mmMusicGetVolume`. The channel's own volume, which does not include the global one.
    pub fn mm_get_volume(&self, handle: u32) -> Option<f64> {
        Some(self.mm_streams.get(&handle)?.volume)
    }

    /// `mmMusicSlideVolume(h, volume, milliseconds)` (`BASS_ChannelSlideAttribute`): a linear
    /// ramp, left to Web Audio's own automation. Like `sga_fade_volume` it deliberately does not
    /// move `MmStream::volume`; BASS slides the live attribute and a `mmMusicSetVolume` is what
    /// replaces the value.
    pub fn mm_slide_volume(&mut self, handle: u32, volume: f64, millis: f64) -> bool {
        let Some(stream) = self.mm_streams.get(&handle) else { return false };
        let Some(instance) = stream.instance else { return true };
        let target = (volume * self.mm_global_volume).clamp(0.0, 1.0) as f32;
        let Some(state) = self.fmod_instances.get(&instance) else { return true };
        let now = self.ctx.current_time();
        let gain = state.gain_node.gain();
        let current = gain.value();
        let _ = gain.cancel_scheduled_values(now);
        let _ = gain.set_value_at_time(current, now);
        if millis > 0.0 {
            let _ = gain.linear_ramp_to_value_at_time(target, now + millis / 1000.0);
        } else {
            gain.set_value(target);
        }
        true
    }

    /// `mmMusicSetGlobalVolume`, which is `BASS_SetVolume`: BASS's entire output, so every live
    /// stream's gain is recomputed. GM8's own sounds are untouched -- they are not BASS's.
    pub fn mm_set_global_volume(&mut self, volume: f64) {
        self.mm_global_volume = volume.clamp(0.0, 1.0);
        for handle in self.mm_streams.keys().copied().collect::<Vec<_>>() {
            self.mm_apply_gain(handle);
        }
    }

    /// `mmMusicGetGlobalVolume` (`BASS_GetVolume`).
    pub fn mm_get_global_volume(&self) -> f64 {
        self.mm_global_volume
    }

    /// `mmMusicGetLength`, in seconds. Decodes the file if it has not been decoded yet -- the
    /// length is a property of the audio, and BASS knows it from the stream it has open.
    pub fn mm_get_length_secs(&mut self, handle: u32) -> Option<f64> {
        let sound_id = self.mm_streams.get(&handle)?.sound_id;
        Some(self.fmod_sound_get_length_ms(sound_id)? / 1000.0)
    }

    /// `mmMusicGetPosition`, in seconds. A stream with no live instance reports where its next
    /// resume would start, which is where BASS's stopped channel sits.
    pub fn mm_get_position_secs(&self, handle: u32) -> Option<f64> {
        let stream = self.mm_streams.get(&handle)?;
        let Some(instance) = stream.instance else { return Some(stream.pending_offset_secs) };
        let state = self.fmod_instances.get(&instance)?;
        Some(state.current_offset_secs(&self.ctx))
    }

    /// `mmMusicSetPosition`, in seconds.
    pub fn mm_set_position_secs(&mut self, handle: u32, secs: f64) -> bool {
        let Some(stream) = self.mm_streams.get_mut(&handle) else { return false };
        stream.pending_offset_secs = secs.max(0.0);
        let Some(instance) = stream.instance else { return true };
        let Some(length_ms) = self.fmod_instance_sound_get_length_ms(instance) else { return false };
        if length_ms <= 0.0 {
            return false
        }
        self.fmod_instance_set_position(instance, secs / (length_ms / 1000.0))
    }

    /// `mmMusicSetPan` (`BASS_ATTRIB_PAN`, -1 left .. 1 right).
    pub fn mm_set_pan(&mut self, handle: u32, pan: f64) -> bool {
        let Some(stream) = self.mm_streams.get_mut(&handle) else { return false };
        stream.pan = pan;
        let Some(instance) = stream.instance else { return true };
        self.fmod_instance_set_pan(instance, pan)
    }

    pub fn mm_get_pan(&self, handle: u32) -> Option<f64> {
        Some(self.mm_streams.get(&handle)?.pan)
    }

    /// `mmMusicSetSpeed`: a multiplier on the stream's own sample rate, i.e. a playback rate.
    pub fn mm_set_speed(&mut self, handle: u32, speed: f64) -> bool {
        let Some(stream) = self.mm_streams.get_mut(&handle) else { return false };
        stream.speed = speed;
        let Some(instance) = stream.instance else { return true };
        self.fmod_instance_set_pitch(instance, speed)
    }

    pub fn mm_get_speed(&self, handle: u32) -> Option<f64> {
        Some(self.mm_streams.get(&handle)?.speed)
    }

    /// `mmMusicSetLoopSection(h, start, end)`, both in seconds.
    pub fn mm_set_loop_section(&mut self, handle: u32, start: f64, end: f64) -> bool {
        let Some(stream) = self.mm_streams.get_mut(&handle) else { return false };
        stream.loop_section = Some((start, end));
        let Some(instance) = stream.instance else { return true };
        self.mm_apply_loop_section(instance, start, end)
    }

    /// `mmMusicRemoveLoopSection`: back to the stream's own loop flag over the whole file.
    pub fn mm_remove_loop_section(&mut self, handle: u32) -> bool {
        let Some(stream) = self.mm_streams.get_mut(&handle) else { return false };
        stream.loop_section = None;
        let (looping, instance) = (stream.looping, stream.instance);
        let Some(instance) = instance else { return true };
        if let Some(state) = self.fmod_instances.get_mut(&instance) {
            state.loop_start_secs = 0.0;
            state.loop_end_secs = None;
        }
        self.fmod_instance_set_loop_count(instance, if looping { -1 } else { 0 });
        true
    }
}

/// Logs `message` the first time it is seen, and never again. For diagnostics raised from a play
/// path, which a game reaches once per frame.
fn warn_once(message: String) {
    use std::{cell::RefCell, collections::HashSet};
    thread_local! {
        static WARNED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    }
    if WARNED.with(|w| w.borrow_mut().insert(message.clone())) {
        web_sys::console::warn_1(&message.into());
    }
}

/// Where `MaizeMusic.dll` handles start. See `AudioManager::mm_alloc_handle` for why it is not 1.
const MM_FIRST_HANDLE: u32 = 0x0200_0001;

/// `BASS_ChannelIsActive`'s own return values, which `mmMusicIsActive` passes through unchanged.
const MM_ACTIVE_STOPPED: u32 = 0;
const MM_ACTIVE_PLAYING: u32 = 1;
const MM_ACTIVE_PAUSED: u32 = 3;

impl Drop for AudioManager {
    /// A Web Audio `AudioContext` is not tied to the lifetime of its Rust/JS wrapper -- dropping
    /// this struct's fields alone would leave every currently-playing source (and the context
    /// itself) running indefinitely in the browser's audio thread. Without this, loading a new
    /// game (which constructs a brand new `AudioManager`/`AudioContext`) would leave the previous
    /// game's music/sound effects playing forever alongside the new game's, since nothing ever
    /// told the browser the old context was done. `stop_all`'s node-stopping logic handles the
    /// currently-tracked sources; `ctx.close()` also releases the context itself.
    fn drop(&mut self) {
        self.stop_all();
        let _ = self.ctx.close();
    }
}
