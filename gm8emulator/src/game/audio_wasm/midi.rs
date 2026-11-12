//! MIDI "decoding", which is really synthesis.
//!
//! A `.mid` sound is a score, not encoded audio: there is nothing in it to decode, and what it
//! sounds like is decided entirely by the instrument bank it is played through. GM8 games embed
//! MIDI for background music more often than the engine's two supported formats suggest -- Not
//! Another Magic Tower Game's battle themes are MIDI and nothing else in it is, which is exactly
//! why "the battle music doesn't play" was the whole of the symptom.
//!
//! `rustysynth` renders a score offline into a stereo pair, which is the shape the rest of the
//! audio path already wants, so a synthesised track joins the normal cache as if it had been
//! decoded from a file. Two things make this different from the other three formats, and both are
//! about memory rather than sound:
//!
//! - It is the only format whose output size is unbounded by its input: a 60KB file can describe
//!   twenty minutes of music. Hence [`Score::frames`], settled before a byte is rendered, so the
//!   caller can decide against a render it cannot afford.
//! - A full render is far too big to hold twice. Rendering a two-minute track whole would put
//!   ~75MB of `f32` into wasm's linear memory on the way to an equally large `AudioBuffer`, and
//!   because linear memory never shrinks, that peak is paid permanently by the tab. So rendering
//!   is a chunked stream ([`render`]) rather than one buffer: ~128KB of scratch, whatever the
//!   track's length.

use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};
use std::{io::Cursor, sync::Arc};

/// Frames handed to the sink at a time: 128KB of scratch across the two channels.
///
/// `MidiFileSequencer::render` keeps its own position across calls and tops up partial blocks, so
/// any chunk length renders the same continuous stream as one whole-track call would. The size is
/// a trade between scratch memory and the number of JS boundary crossings per track (~600 for a
/// four-minute render, which is nothing beside the synthesis itself).
pub const CHUNK_FRAMES: usize = 16_384;

/// Renders are always stereo: a SoundFont bank pans instruments, so mixing down to mono would
/// throw away positioning the score asked for, and the budget below is in frames either way.
pub const CHANNELS: u16 = 2;

/// The rate to render at when the host says nothing. A host may lower it (the
/// `audio.midiSampleRate` setting) on memory-constrained devices -- halving the rate halves
/// both the resident buffer and the time spent rendering it.
pub const DEFAULT_SAMPLE_RATE: u32 = 44_100;

/// Rates outside this are either unsupported by Web Audio's `createBuffer` or so low as to be
/// pointless; a host asking for something silly gets clamped rather than a failed render.
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 48_000;

/// A hard ceiling on one track's rendered length, independent of what the caller can afford.
///
/// Every GM8 game using MIDI is using it for a loop of a couple of minutes, so a score claiming to
/// run longer than this is far likelier to be malformed -- a stuck tempo event, a bad delta -- than
/// real music, and a cap keeps one from spending minutes of synthesis on garbage.
const MAX_SECONDS: f64 = 600.0;

/// A parsed score, and what rendering it would cost. Separate from [`render`] so the caller can
/// size (or refuse) the destination before any synthesis happens.
pub struct Score {
    file: Arc<MidiFile>,
    /// Rate the render will run at: the requested one, clamped.
    pub sample_rate: u32,
    /// Frames the render will produce, already limited by the caller's cap and [`MAX_SECONDS`].
    pub frames: usize,
    /// Whether one of those limits cut the track short. Worth telling the user about: it is the
    /// one case here where the music is wrong rather than merely expensive.
    pub truncated: bool,
}

impl Score {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate.max(1))
    }

    /// Bytes the rendered `AudioBuffer` will occupy, as the cache measures it.
    pub fn rendered_bytes(&self) -> usize {
        self.frames.saturating_mul(usize::from(CHANNELS)).saturating_mul(4)
    }
}

/// Parses a SoundFont bank. The bytes are the `.sf2` the host fetched -- see
/// `AudioManager::set_soundfont` for why this happens once, separately from the game's own load.
pub fn parse_soundfont(bytes: &[u8]) -> Option<Arc<SoundFont>> {
    let mut cursor = Cursor::new(bytes);
    SoundFont::new(&mut cursor).ok().map(Arc::new)
}

/// Reads `midi` and works out what rendering it would produce, without rendering any of it.
///
/// `max_frames` is the caller's own ceiling -- for the audio cache, what one track is allowed to
/// claim of the music budget. `None` if the bytes aren't valid MIDI, which for an embedded game
/// asset means it was never going to play on a real runner either.
pub fn parse_score(midi: &[u8], sample_rate: u32, max_frames: usize) -> Option<Score> {
    let mut cursor = Cursor::new(midi);
    let file = Arc::new(MidiFile::new(&mut cursor).ok()?);

    let sample_rate = sample_rate.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE);
    let rate = f64::from(sample_rate);
    let full = (file.get_length() * rate) as usize;
    let cap = max_frames.min((MAX_SECONDS * rate) as usize);
    let frames = full.min(cap);
    if frames == 0 {
        return None
    }
    Some(Score { file, sample_rate, frames, truncated: frames < full })
}

/// Renders `score` through `soundfont`, handing the output to `sink` a chunk at a time as
/// `(start_frame, left, right)`.
///
/// `sink` returning false aborts the render -- the destination went away, so there is no point
/// synthesising the rest -- and `render` returns false in turn. True means the whole of
/// [`Score::frames`] reached the sink.
///
/// Rendered without looping even when the game will loop the result: a GML `sound_loop` repeats
/// the buffer itself, so baking repeats in here would only multiply the memory by the number of
/// repeats and still not loop forever.
pub fn render<F>(score: &Score, soundfont: &Arc<SoundFont>, mut sink: F) -> bool
where
    F: FnMut(usize, &[f32], &[f32]) -> bool,
{
    let settings = SynthesizerSettings::new(score.sample_rate as i32);
    let Ok(synthesizer) = Synthesizer::new(soundfont, &settings) else { return false };
    let mut sequencer = MidiFileSequencer::new(synthesizer);
    sequencer.play(&score.file, false);

    let mut left = vec![0f32; CHUNK_FRAMES];
    let mut right = vec![0f32; CHUNK_FRAMES];
    let mut start = 0usize;
    while start < score.frames {
        let n = CHUNK_FRAMES.min(score.frames - start);
        sequencer.render(&mut left[..n], &mut right[..n]);
        if !sink(start, &left[..n], &right[..n]) {
            return false
        }
        start += n;
    }
    true
}
