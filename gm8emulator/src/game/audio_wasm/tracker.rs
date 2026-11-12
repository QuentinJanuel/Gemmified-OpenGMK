//! Tracker-module playback: Amiga ProTracker (`.mod`) and Scream Tracker 3 (`.s3m`), rendered to
//! PCM so the rest of the audio layer can treat a module like any other decoded sound.
//!
//! **Why this exists.** GM8.2's sound extension (gm82snd) advertises `.mod`/`.s3m`/`.xm`/`.it`
//! alongside the sampled formats, and fangames take it up -- renex engine ships nine `.mod` files
//! and one `.s3m` and no sampled music at all. A module that fails to load isn't merely silent:
//! `FMODSoundAdd` returns 0, `sound_exists` is false, and gm82snd raises `show_error("Sound does
//! not exist: ...")` on every attempt to play it.
//!
//! A module is not audio data, it is a little sequencer program -- a set of instrument samples plus
//! pattern rows saying which sample to trigger on which channel at which pitch, with an effect
//! column that bends pitch and volume between rows. So there is nothing to "decode"; the only way
//! to get PCM is to run the sequencer and mix. That is what this module does.
//!
//! **Not implemented:** `.xm` and `.it`. They are a strictly larger job (envelopes, instrument/
//! sample split, NNAs for IT) rather than more of the same, and neither appears in a game seen so
//! far. `parse` answers `None` for them, which is the same "cannot load" the caller already
//! handles.
//!
//! ## Shape of the thing
//!
//! Both formats collapse onto one playback model ([`Module`]) and one mixer, because underneath the
//! differing file layouts they are the same machine: channels, each playing one sample at some
//! *period*, stepped by a tick clock. What genuinely differs is kept behind [`Format`]:
//!
//! - **period → hertz.** MOD periods are Amiga hardware divisors (PAL clock / 2 / period); S3M's
//!   are scaled by the sample's own C-2 rate. One method, two arms.
//! - **the effect column.** MOD numbers its effects `0`..`F` and S3M letters them `A`..`X`, and the
//!   overlap is *not* positional -- MOD's `A` is a volume slide, S3M's `A` sets speed. They get one
//!   dispatch function each, over shared helpers (`do_vibrato`, `do_volume_slide`, ...), since the
//!   underlying operations really are the same and only their encoding and edge cases differ.
//! - **panning.** MOD is the Amiga's fixed hard-panned LRRL; S3M carries a pan table.
//!
//! ## Rendering is one pass through the song, and the caller loops it
//!
//! `render` stops when the sequencer reaches a (position, row) it has already played -- the point
//! where a module starts repeating itself. Callers loop the resulting buffer, which is what a
//! module does anyway, and it keeps the buffer to one song length instead of an arbitrary fade-out.
//! `MAX_RENDER_SECS` bounds a pathological song (or a bug here) rather than being a musical choice.
//!
//! Nothing in this file is wasm-specific: no `web_sys`, no `Game`. That is deliberate -- it is
//! testable on its own, the way `sm3` is.

/// Per-channel volume ceiling in both formats. Volumes are 0..=64, not 0..=255.
const MAX_VOLUME: i32 = 64;

/// Safety bound on one render, in seconds. Songs that legitimately run this long are not known to
/// exist; a song that reaches it is far likelier to be a sequencer bug here (an order jump
/// that never revisits a row) than real music, and either way an unbounded loop in a browser tab
/// is not an acceptable failure mode.
const MAX_RENDER_SECS: usize = 600;

/// Output of a render, in the exact shape `game/audio_wasm.rs`'s other decoders produce.
pub struct Decoded {
    pub channels: u16,
    pub sample_rate: u32,
    /// Planar (per-channel contiguous), NOT interleaved.
    pub planar_samples: Vec<f32>,
    pub frames: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Mod,
    S3m,
}

/// One instrument's waveform, normalised to `f32` at parse time so the mixer never has to care
/// whether the file stored 8-bit signed (MOD, most S3M), 8-bit unsigned (older S3M) or 16-bit.
struct Sample {
    data: Vec<f32>,
    /// In frames. `loop_len == 0` means "one-shot": play to the end and stop.
    loop_start: usize,
    loop_len: usize,
    /// Default volume applied when the sample is triggered without a volume column, 0..=64.
    volume: i32,
    /// MOD only, -8..=7. Selects one of sixteen period tables; see `PERIODS`.
    finetune: i8,
    /// S3M only: the playback rate, in Hz, at which this sample sounds a C-2.
    c2spd: u32,
}

impl Default for Sample {
    fn default() -> Self {
        Sample { data: Vec::new(), loop_start: 0, loop_len: 0, volume: MAX_VOLUME, finetune: 0, c2spd: 8363 }
    }
}

/// One cell of the pattern grid. `note` is a semitone index, 0 = C-0; `NO_NOTE` for an empty cell.
#[derive(Clone, Copy, Default)]
struct Cell {
    note: u8,
    instrument: u8,
    /// S3M's volume column. MOD has none, so it is always `NO_VOLUME` there.
    volume: u8,
    effect: u8,
    param: u8,
}

const NO_NOTE: u8 = 0xFF;
const NOTE_OFF: u8 = 0xFE;
const NO_VOLUME: u8 = 0xFF;

struct Pattern {
    rows: usize,
    /// Row-major, `rows * channels` cells.
    cells: Vec<Cell>,
}

pub struct Module {
    format: Format,
    channels: usize,
    samples: Vec<Sample>,
    patterns: Vec<Pattern>,
    /// Playback order, as indices into `patterns`.
    order: Vec<usize>,
    initial_speed: u32,
    initial_tempo: u32,
    /// S3M's global volume, 0..=64. MOD has none and uses the maximum.
    global_volume: i32,
    /// Per-channel stereo position, 0.0 = hard left, 1.0 = hard right.
    panning: Vec<f32>,
}

/// Parses a module, dispatching on the file's own magic rather than on `ext`.
///
/// The extension is only a hint from a filename a game chose, and these two formats are trivially
/// and reliably distinguishable by content (S3M carries `SCRM` at offset 44; MOD carries one of a
/// known set of four-character tags at 1080, or none at all for the 15-instrument ancestor).
/// Sniffing means a mislabelled file still plays, and a `.xm`/`.it` handed here still gets a clean
/// `None` rather than being force-fed to the wrong parser.
pub fn parse(bytes: &[u8]) -> Option<Module> {
    if bytes.len() > 48 && &bytes[44..48] == b"SCRM" { parse_s3m(bytes) } else { parse_mod(bytes) }
}

// ---------------------------------------------------------------------------------------------
// MOD
// ---------------------------------------------------------------------------------------------

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// How many channels a four-character MOD tag means, and whether it is a tag at all.
///
/// The 31-instrument extension to Soundtracker's format is identified by these tags at offset
/// 1080; a file with none of them is the original 15-instrument layout, whose header is 600 bytes
/// rather than 1084 and which has no tag anywhere. `None` here means "assume the old layout".
fn mod_tag_channels(tag: &[u8]) -> Option<usize> {
    match tag {
        // ProTracker and the trackers that cloned its tag. `M!K!` is `M.K.` with more than 64
        // patterns; the tag is the only difference.
        b"M.K." | b"M!K!" | b"M&K!" | b"N.T." | b"FLT4" | b"4CHN" => Some(4),
        b"FLT8" | b"8CHN" | b"OCTA" | b"CD81" => Some(8),
        b"6CHN" => Some(6),
        // TakeTracker/FastTracker's generalisation: `<n>CHN` up to 9, then `<nn>CH` for 10..=32.
        _ if tag.len() == 4 => {
            let s = std::str::from_utf8(tag).ok()?;
            let n = if let Some(d) = s.strip_suffix("CHN") {
                d.parse::<usize>().ok()?
            } else if let Some(d) = s.strip_suffix("CH") {
                d.parse::<usize>().ok()?
            } else {
                return None
            };
            (1..=32).contains(&n).then_some(n)
        },
        _ => None,
    }
}

fn parse_mod(bytes: &[u8]) -> Option<Module> {
    // Two header layouts, told apart by whether offset 1080 holds a known tag. The 31-sample one
    // is 1084 bytes (title, sample headers, order table, tag); the 15-sample ancestor is 600 and
    // has no tag at all, so every offset after the sample headers moves.
    let (num_samples, channels, tag_len) = if bytes.len() >= 1084 {
        match mod_tag_channels(&bytes[1080..1084]) {
            Some(ch) => (31, ch, 4),
            None => (15, 4, 0),
        }
    } else {
        (15, 4, 0)
    };
    let song_len_at = 20 + num_samples * 30;
    let order_at = song_len_at + 2;
    let header_len = order_at + 128 + tag_len;
    if bytes.len() < header_len {
        return None
    }

    let mut samples = Vec::with_capacity(num_samples);
    for i in 0..num_samples {
        let at = 20 + i * 30;
        // Lengths and loop points are in *words*, which is why every one of these is doubled.
        let length = be16(bytes, at + 22) as usize * 2;
        let finetune = ((bytes[at + 24] & 0x0F) as i8) << 4 >> 4; // low nibble, sign-extended
        let volume = (bytes[at + 25] as i32).min(MAX_VOLUME);
        let loop_start = be16(bytes, at + 26) as usize * 2;
        let loop_len = be16(bytes, at + 28) as usize * 2;
        samples.push(Sample {
            data: Vec::with_capacity(length),
            // A loop length of two bytes (one word) is the format's idiom for "no loop", not a
            // two-sample loop -- ProTracker writes it into every unlooped sample. Honouring it
            // literally makes a one-shot instrument buzz on its final word forever.
            loop_start: if loop_len > 2 { loop_start } else { 0 },
            loop_len: if loop_len > 2 { loop_len } else { 0 },
            volume,
            finetune,
            c2spd: 8363,
        });
    }

    let song_len = bytes[song_len_at] as usize;
    let order_bytes = &bytes[order_at..order_at + 128];
    // The pattern count isn't stored: it is one more than the highest pattern the order references.
    // Scanning all 128 entries rather than just the first `song_len` is deliberate -- trackers leave
    // real patterns parked past the end of the song, and undercounting here would misplace every
    // sample's data, since sample data begins immediately after the last pattern.
    let num_patterns = order_bytes.iter().map(|&p| p as usize + 1).max().unwrap_or(1);
    let order: Vec<usize> =
        order_bytes.iter().take(song_len.clamp(1, 128)).map(|&p| p as usize).filter(|&p| p < num_patterns).collect();
    if order.is_empty() {
        return None
    }

    // 64 rows, `channels` cells, 4 bytes each.
    let pattern_size = 64 * channels * 4;
    let mut patterns = Vec::with_capacity(num_patterns);
    for p in 0..num_patterns {
        let base = header_len + p * pattern_size;
        let mut cells = vec![Cell { note: NO_NOTE, volume: NO_VOLUME, ..Default::default() }; 64 * channels];
        if base + pattern_size <= bytes.len() {
            for (i, cell) in cells.iter_mut().enumerate() {
                let at = base + i * 4;
                // The four bytes pack as: sample hi nibble, 12-bit period, sample lo nibble,
                // effect nibble, parameter byte.
                let period = (((bytes[at] & 0x0F) as u16) << 8) | bytes[at + 1] as u16;
                let instrument = (bytes[at] & 0xF0) | (bytes[at + 2] >> 4);
                *cell = Cell {
                    note: if period == 0 { NO_NOTE } else { mod_period_to_note(period) },
                    instrument,
                    volume: NO_VOLUME,
                    effect: bytes[at + 2] & 0x0F,
                    param: bytes[at + 3],
                };
            }
        }
        patterns.push(Pattern { rows: 64, cells });
    }

    // Sample data follows the last pattern, in header order.
    let mut at = header_len + num_patterns * pattern_size;
    for (i, sample) in samples.iter_mut().enumerate() {
        let len = be16(bytes, 20 + i * 30 + 22) as usize * 2;
        let end = (at + len).min(bytes.len());
        sample.data = bytes[at.min(bytes.len())..end].iter().map(|&b| (b as i8) as f32 / 128.0).collect();
        // A truncated file (or a wrong pattern count) must not leave loop points past the data.
        let n = sample.data.len();
        if sample.loop_start >= n || sample.loop_start + sample.loop_len > n {
            sample.loop_start = sample.loop_start.min(n);
            sample.loop_len = n.saturating_sub(sample.loop_start);
        }
        at += len;
    }

    Some(Module {
        format: Format::Mod,
        channels,
        samples,
        patterns,
        order,
        initial_speed: 6,
        initial_tempo: 125,
        global_volume: MAX_VOLUME,
        // The Amiga's four hardware voices are wired to the two outputs as left, right, right,
        // left, and MOD has no pan column to say otherwise. It really is this hard-panned; the
        // format has no notion of a centre.
        panning: (0..channels).map(|c| if matches!(c & 3, 0 | 3) { 0.0 } else { 1.0 }).collect(),
    })
}

/// ProTracker's period tables: sixteen finetune settings (index `finetune & 15`, so 0..7 then
/// -8..-1) of 36 semitones each, C-1 to B-3.
///
/// These are the literal Amiga hardware divisors ProTracker ships, not a computed equal-tempered
/// scale. The difference is audible -- the table is what every other player matches, and a game's
/// music has been mixed against it.
#[rustfmt::skip]
const PERIODS: [[u16; 36]; 16] = [
    [856,808,762,720,678,640,604,570,538,508,480,453, 428,404,381,360,339,320,302,285,269,254,240,226, 214,202,190,180,170,160,151,143,135,127,120,113],
    [850,802,757,715,674,637,601,567,535,505,477,450, 425,401,379,357,337,318,300,284,268,253,239,225, 213,201,189,179,169,159,150,142,134,126,119,113],
    [844,796,752,709,670,632,597,563,532,502,474,447, 422,398,376,355,335,316,298,282,266,251,237,224, 211,199,188,177,167,158,149,141,133,125,118,112],
    [838,791,746,704,665,628,592,559,528,498,470,444, 419,395,373,352,332,314,296,280,264,249,235,222, 209,198,187,176,166,157,148,140,132,125,118,111],
    [832,785,741,699,660,623,588,555,524,495,467,441, 416,392,370,350,330,312,294,278,262,247,233,220, 208,196,185,175,165,156,147,139,131,124,117,110],
    [826,779,736,694,655,619,584,551,520,491,463,437, 413,390,368,347,328,309,292,276,260,245,232,219, 206,195,184,174,164,155,146,138,130,123,116,109],
    [820,774,730,689,651,614,580,547,516,487,460,434, 410,387,365,345,325,307,290,274,258,244,230,217, 205,193,183,172,163,154,145,137,129,122,115,109],
    [814,768,725,684,646,610,575,543,513,484,457,431, 407,384,363,342,323,305,288,272,256,242,228,216, 204,192,181,171,161,152,144,136,128,121,114,108],
    [907,856,808,762,720,678,640,604,570,538,508,480, 453,428,404,381,360,339,320,302,285,269,254,240, 226,214,202,190,180,170,160,151,143,135,127,120],
    [900,850,802,757,715,675,636,601,567,535,505,477, 450,425,401,379,357,337,318,300,284,268,253,238, 225,212,200,189,179,169,159,150,142,134,126,119],
    [894,844,796,752,709,670,632,597,563,532,502,474, 447,422,398,376,355,335,316,298,282,266,251,237, 223,211,199,188,177,167,158,149,141,133,125,118],
    [887,838,791,746,704,665,628,592,559,528,498,470, 444,419,395,373,352,332,314,296,280,264,249,235, 222,209,198,187,176,166,157,148,140,132,125,118],
    [881,832,785,741,699,660,623,588,555,524,494,467, 441,416,392,370,350,330,312,294,278,262,247,233, 220,208,196,185,175,165,156,147,139,131,124,117],
    [875,826,779,736,694,655,619,584,551,520,491,463, 437,413,390,368,347,328,309,292,276,260,245,232, 219,206,195,184,174,164,155,146,138,130,123,116],
    [868,820,774,730,689,651,614,580,547,516,487,460, 434,410,387,365,345,325,307,290,274,258,244,230, 217,205,193,183,172,163,154,145,137,129,122,115],
    [862,814,768,725,684,646,610,575,543,513,484,457, 431,407,384,363,342,323,305,288,272,256,242,228, 216,203,192,181,171,161,152,144,136,128,121,114],
];

/// MOD stores a *period*, not a note, but arpeggio and tone portamento both need a note index to
/// step through the table with. Recovering it by nearest match against the untuned table is what
/// ProTracker itself does, and it is exact for any period a tracker actually wrote.
///
/// The returned index is in the same C-0-based space as S3M's notes, offset so that the table's
/// first entry (C-1) lands on 12.
fn mod_period_to_note(period: u16) -> u8 {
    let mut best = 0usize;
    let mut best_err = u32::MAX;
    for (i, &p) in PERIODS[0].iter().enumerate() {
        let err = (p as i32 - period as i32).unsigned_abs();
        if err < best_err {
            best_err = err;
            best = i;
        }
    }
    (best + 12) as u8
}

/// Period for a note at a finetune, or `None` for a note outside the three octaves MOD covers.
fn mod_note_period(note: u8, finetune: i8) -> Option<f64> {
    let idx = (note as i32) - 12;
    if !(0..36).contains(&idx) {
        return None
    }
    Some(PERIODS[(finetune as usize) & 15][idx as usize] as f64)
}

// ---------------------------------------------------------------------------------------------
// S3M
// ---------------------------------------------------------------------------------------------

/// Scream Tracker 3's period for the twelve semitones of its base octave; higher octaves halve.
#[rustfmt::skip]
const S3M_PERIODS: [u32; 12] = [1712, 1616, 1524, 1440, 1356, 1280, 1208, 1140, 1076, 1016, 960, 907];

/// S3M stores its effect column as 1 = A, 2 = B and so on. Named rather than written as bare
/// numbers because the letters are how every S3M reference -- and every tracker's own UI -- refers
/// to these, so `eff::K` reads directly against the documentation and `11` does not.
mod eff {
    pub const A: u8 = 1;
    pub const B: u8 = 2;
    pub const C: u8 = 3;
    pub const D: u8 = 4;
    pub const E: u8 = 5;
    pub const F: u8 = 6;
    pub const G: u8 = 7;
    pub const H: u8 = 8;
    pub const I: u8 = 9;
    pub const J: u8 = 10;
    pub const K: u8 = 11;
    pub const L: u8 = 12;
    pub const O: u8 = 15;
    pub const Q: u8 = 17;
    pub const R: u8 = 18;
    pub const S: u8 = 19;
    pub const T: u8 = 20;
    pub const U: u8 = 21;
    pub const V: u8 = 22;
    pub const X: u8 = 24;
}

fn parse_s3m(bytes: &[u8]) -> Option<Module> {
    if bytes.len() < 96 {
        return None
    }
    let ord_num = le16(bytes, 32) as usize;
    let ins_num = le16(bytes, 34) as usize;
    let pat_num = le16(bytes, 36) as usize;
    // 2 = unsigned samples (Scream Tracker 3.00), 1 = signed (everything since).
    let unsigned_samples = le16(bytes, 42) == 2;
    let global_volume = (bytes[48] as i32).min(MAX_VOLUME);
    let initial_speed = (bytes[49] as u32).max(1);
    let initial_tempo = (bytes[50] as u32).max(32);
    let stereo = bytes[51] & 0x80 != 0;
    let default_pan = bytes[53] == 252;

    let order_at = 96;
    let ins_ptr_at = order_at + ord_num;
    let pat_ptr_at = ins_ptr_at + ins_num * 2;
    let pan_at = pat_ptr_at + pat_num * 2;
    if bytes.len() < pan_at {
        return None
    }

    // 254 is a marker channel and 255 an unused one; neither is a pattern position to play.
    let order: Vec<usize> = bytes[order_at..ins_ptr_at].iter().map(|&p| p as usize).filter(|&p| p < pat_num).collect();
    if order.is_empty() || pat_num == 0 {
        return None
    }

    // Channel settings live at 64..96: bit 7 set means the channel is disabled, and the low bits
    // give its default pan side (0..7 left, 8..15 right).
    let mut channels = 0usize;
    for c in 0..32 {
        if bytes[64 + c] & 0x80 == 0 {
            channels = c + 1;
        }
    }
    let channels = channels.max(1);

    let mut panning = Vec::with_capacity(channels);
    for c in 0..channels {
        let setting = bytes[64 + c];
        let mut pan = if !stereo {
            0.5
        } else if setting & 0x0F >= 8 {
            0.75
        } else {
            0.25
        };
        // The optional pan table overrides the channel setting, but only for entries that flag
        // themselves as set (bit 5).
        if default_pan && pan_at + c < bytes.len() {
            let entry = bytes[pan_at + c];
            if entry & 0x20 != 0 {
                pan = (entry & 0x0F) as f32 / 15.0;
            }
        }
        panning.push(pan);
    }

    let mut samples = Vec::with_capacity(ins_num);
    for i in 0..ins_num {
        let ptr = le16(bytes, ins_ptr_at + i * 2) as usize * 16;
        let mut sample = Sample::default();
        if ptr + 80 <= bytes.len() && bytes[ptr] == 1 {
            // Sample data lives at a paragraph pointer split oddly across three bytes: the high
            // byte first, then a little-endian low word.
            let data_at = (((bytes[ptr + 13] as usize) << 16) | le16(bytes, ptr + 14) as usize) * 16;
            let length = le32(bytes, ptr + 16) as usize;
            let loop_start = le32(bytes, ptr + 20) as usize;
            let loop_end = le32(bytes, ptr + 24) as usize;
            let flags = bytes[ptr + 31];
            let sixteen_bit = flags & 0x04 != 0;
            let bytes_per_frame = if sixteen_bit { 2 } else { 1 };
            let end = (data_at + length * bytes_per_frame).min(bytes.len());
            let raw = &bytes[data_at.min(bytes.len())..end];
            sample.data = if sixteen_bit {
                raw.chunks_exact(2)
                    .map(|c| {
                        let v = u16::from_le_bytes([c[0], c[1]]);
                        if unsigned_samples { (v as f32 - 32768.0) / 32768.0 } else { (v as i16) as f32 / 32768.0 }
                    })
                    .collect()
            } else {
                raw.iter()
                    .map(|&b| if unsigned_samples { (b as f32 - 128.0) / 128.0 } else { (b as i8) as f32 / 128.0 })
                    .collect()
            };
            sample.volume = (bytes[ptr + 28] as i32).min(MAX_VOLUME);
            sample.c2spd = le32(bytes, ptr + 32).max(1);
            let n = sample.data.len();
            if flags & 0x01 != 0 && loop_end > loop_start && loop_start < n {
                sample.loop_start = loop_start;
                sample.loop_len = loop_end.min(n) - loop_start;
            }
        }
        samples.push(sample);
    }

    let mut patterns = Vec::with_capacity(pat_num);
    for p in 0..pat_num {
        let ptr = le16(bytes, pat_ptr_at + p * 2) as usize * 16;
        patterns.push(parse_s3m_pattern(bytes, ptr, channels));
    }

    Some(Module {
        format: Format::S3m,
        channels,
        samples,
        patterns,
        order,
        initial_speed,
        initial_tempo,
        global_volume,
        panning,
    })
}

/// S3M packs a pattern as a byte stream: a zero byte ends a row, and otherwise the low five bits
/// pick the channel and the top three say which of note+instrument, volume and effect follow.
fn parse_s3m_pattern(bytes: &[u8], ptr: usize, channels: usize) -> Pattern {
    let mut cells = vec![Cell { note: NO_NOTE, volume: NO_VOLUME, ..Default::default() }; 64 * channels];
    // The first word is the packed length; an out-of-range pointer means an empty pattern, which
    // is a real thing in these files (ST3 writes unused patterns as a null pointer).
    if ptr == 0 || ptr + 2 > bytes.len() {
        return Pattern { rows: 64, cells }
    }
    let end = (ptr + 2 + le16(bytes, ptr) as usize).min(bytes.len());
    let mut at = ptr + 2;
    let mut row = 0usize;
    while at < end && row < 64 {
        let what = bytes[at];
        at += 1;
        if what == 0 {
            row += 1;
            continue
        }
        let channel = (what & 31) as usize;
        // Read every field the flags announce even for a channel this module doesn't use, or the
        // stream desynchronises for every channel after it.
        let mut cell = Cell { note: NO_NOTE, volume: NO_VOLUME, ..Default::default() };
        if what & 32 != 0 {
            if at + 1 >= end {
                break
            }
            let note = bytes[at];
            cell.note = match note {
                255 => NO_NOTE,
                254 => NOTE_OFF,
                // High nibble octave, low nibble semitone.
                n => (n >> 4) * 12 + (n & 0x0F),
            };
            cell.instrument = bytes[at + 1];
            at += 2;
        }
        if what & 64 != 0 {
            if at >= end {
                break
            }
            cell.volume = bytes[at];
            at += 1;
        }
        if what & 128 != 0 {
            if at + 1 >= end {
                break
            }
            cell.effect = bytes[at];
            cell.param = bytes[at + 1];
            at += 2;
        }
        if channel < channels {
            cells[row * channels + channel] = cell;
        }
    }
    Pattern { rows: 64, cells }
}

// ---------------------------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------------------------

/// ProTracker's vibrato/tremolo sine table -- a quarter-wave in 32 steps, mirrored and negated by
/// the lookup. Sampled, not computed, for the same reason as `PERIODS`.
#[rustfmt::skip]
const SINE: [i32; 32] = [
    0, 24, 49, 74, 97, 120, 141, 161, 180, 197, 212, 224, 235, 244, 250, 253,
    255, 253, 250, 244, 235, 224, 212, 197, 180, 161, 141, 120, 97, 74, 49, 24,
];

#[derive(Default, Clone)]
struct Channel {
    /// Index into `Module::samples`, or `None` when nothing has been triggered yet.
    sample: Option<usize>,
    /// Playback position within the sample, in frames. Fractional: the mixer interpolates.
    pos: f64,
    /// Current period. 0 means silent.
    ///
    /// This is the *base*: the note, plus anything that permanently moves it (portamento, the fine
    /// slides). Vibrato and arpeggio must not touch it -- see `period_mod`.
    period: f64,
    /// Vibrato/arpeggio's offset from `period`, recomputed from scratch every tick and added only
    /// at mix time.
    ///
    /// Keeping it separate is not tidiness. Both effects are oscillations *around* the note, and
    /// folding them into `period` would accumulate: a vibrato would walk the note off pitch over
    /// its own cycle instead of returning to it, and an arpeggio would leave the channel a few
    /// semitones sharp when the row ended. `volume_mod` is the same story for tremolo.
    period_mod: f64,
    volume_mod: i32,
    /// `Ixy` tremor's off phase. Also cleared every tick, for the same reason: it gates the channel
    /// rather than changing its volume, and left sticky it would silence a channel for good once
    /// the row carrying the effect passed on an off beat.
    tremor_muted: bool,
    /// Where tone portamento is heading.
    porta_target: f64,
    porta_speed: f64,
    volume: i32,
    pan: f32,
    playing: bool,
    /// The note last triggered on this channel, for tone portamento and note retriggers.
    note: u8,
    finetune: i8,
    c2spd: u32,

    vib_speed: i32,
    vib_depth: i32,
    vib_pos: i32,
    /// Waveform selector, 0 sine / 1 ramp down / 2 square, +4 = don't retrigger on a new note.
    vib_wave: u8,
    trem_speed: i32,
    trem_depth: i32,
    trem_pos: i32,
    trem_wave: u8,

    /// Last non-zero parameter, per effect that remembers one. Kept separate because a module
    /// relies on them not clobbering each other.
    mem_volslide: u8,
    mem_porta_up: u8,
    mem_porta_down: u8,
    mem_tone_porta: u8,
    mem_offset: u8,
    mem_retrig: u8,
    mem_tremor: u8,
    mem_vibrato: u8,

    arpeggio: u8,
    /// Ticks remaining before a scheduled `EDx` note starts, with the cell it will play.
    delay_ticks: i32,
    delayed: Option<Cell>,
    /// `ECx` / `SCx`: silence the channel after this many ticks.
    cut_after: i32,
    /// `Ixy` tremor: on/off tick counters and where in the cycle we are.
    tremor_pos: i32,
    retrig_count: i32,
}

impl Module {
    /// Hertz for a period, which is the one piece of pitch maths the two formats genuinely disagree
    /// about.
    ///
    /// MOD's period *is* the Amiga's audio divisor -- the hardware clocked one sample out every
    /// `period` cycles of a 3.546895 MHz timebase (PAL; NTSC machines ran slightly sharp, and
    /// PAL is what essentially all of this music was written on).
    ///
    /// S3M keeps period-shaped numbers so the same portamento arithmetic works, but scales them by
    /// the sample's own C-2 rate at trigger time, so the divisor is a fixed constant instead.
    fn period_to_hz(&self, period: f64) -> f64 {
        if period <= 0.0 {
            return 0.0
        }
        match self.format {
            Format::Mod => 7093789.2 / (period * 2.0),
            Format::S3m => 14317056.0 / period,
        }
    }

    /// Period for a freshly triggered note.
    fn note_period(&self, channel: &Channel, note: u8) -> Option<f64> {
        match self.format {
            Format::Mod => mod_note_period(note, channel.finetune),
            Format::S3m => {
                // S3M numbers notes as octave * 12 + semitone straight from the file's own nibbles,
                // and octave 4 semitone 0 is the note that sounds at the sample's C-2 rate.
                let idx = note as i32;
                if !(0..(12 * 9)).contains(&idx) {
                    return None
                }
                let octave = (idx / 12) as u32;
                let base = S3M_PERIODS[(idx % 12) as usize] as f64;
                // Halving in floating point rather than shifting the table entry: the entries are
                // small enough (907 at the top of the octave) that an integer shift would put the
                // highest octaves badly out of tune.
                Some((16.0 * 8363.0 * base) / (1u32 << octave.min(15)) as f64 / channel.c2spd.max(1) as f64)
            },
        }
    }

    /// Renders one pass through the song.
    pub fn render(&self, sample_rate: u32) -> Decoded {
        let mut player = Player::new(self, sample_rate);
        player.run();
        let frames = player.left.len();
        let mut planar = player.left;
        planar.append(&mut player.right);
        Decoded { channels: 2, sample_rate, planar_samples: planar, frames }
    }
}

struct Player<'a> {
    module: &'a Module,
    sample_rate: f64,
    channels: Vec<Channel>,
    left: Vec<f32>,
    right: Vec<f32>,

    order_pos: usize,
    row: usize,
    tick: u32,
    speed: u32,
    tempo: u32,
    global_volume: i32,

    /// Set by a position jump / pattern break during a row, applied when the row ends.
    next_order: Option<usize>,
    next_row: Option<usize>,
    /// `EEx` / `SEx`: repeat the current row this many more times before advancing.
    pattern_delay: u32,
    /// Whether the row currently being played is a `pattern_delay` repeat rather than its first
    /// pass, in which case tick 0 runs effects instead of re-triggering the row's notes.
    in_pattern_delay: bool,
    /// `E6x` / `SBx` pattern loop: the row it returns to and how many repeats are left.
    loop_row: usize,
    loop_count: i32,

    /// Every (order position, row) already played, so the render can stop where the song repeats.
    visited: Vec<bool>,
    done: bool,
}

impl<'a> Player<'a> {
    fn new(module: &'a Module, sample_rate: u32) -> Self {
        let channels = (0..module.channels)
            .map(|c| Channel { pan: module.panning.get(c).copied().unwrap_or(0.5), ..Default::default() })
            .collect();
        Player {
            module,
            sample_rate: sample_rate as f64,
            channels,
            left: Vec::new(),
            right: Vec::new(),
            order_pos: 0,
            row: 0,
            tick: 0,
            speed: module.initial_speed,
            tempo: module.initial_tempo,
            global_volume: module.global_volume,
            next_order: None,
            next_row: None,
            pattern_delay: 0,
            in_pattern_delay: false,
            loop_row: 0,
            loop_count: 0,
            visited: vec![false; module.order.len() * 64],
            done: false,
        }
    }

    fn run(&mut self) {
        let max_frames = MAX_RENDER_SECS * self.sample_rate as usize;
        while !self.done && self.left.len() < max_frames {
            // Vibrato, tremolo and arpeggio are recomputed from nothing each tick; see
            // `Channel::period_mod`.
            for ch in &mut self.channels {
                ch.period_mod = 0.0;
                ch.volume_mod = 0;
                ch.tremor_muted = false;
            }
            if self.tick == 0 && !self.in_pattern_delay {
                self.start_row();
            } else {
                // A pattern delay repeats the row's *ticks*, not the row: the notes on it are not
                // struck again, which is the whole difference between `EEx` and simply writing the
                // row out twice.
                self.tick_effects();
            }
            if self.done {
                break
            }
            // A "tick" is 2.5 / BPM seconds -- the tracker convention, from the Amiga's 50 Hz
            // vertical blank at the default tempo of 125.
            let frames = ((self.sample_rate * 2.5) / self.tempo.max(32) as f64).round().max(1.0) as usize;
            self.mix(frames);
            self.advance_tick();
        }
        // Never hand back a zero-length buffer: the caller turns this into an AudioBuffer, and
        // Web Audio rejects an empty one, which would look to the game like a failed load.
        if self.left.is_empty() {
            self.left.push(0.0);
            self.right.push(0.0);
        }
    }

    fn pattern(&self) -> &Pattern {
        let index = self.module.order.get(self.order_pos).copied().unwrap_or(0);
        &self.module.patterns[index.min(self.module.patterns.len() - 1)]
    }

    /// Processes tick 0 of a row: note triggers and the one-shot half of each effect.
    fn start_row(&mut self) {
        // Stopping here rather than at the end of the last order position is what makes a song
        // that jumps backwards render exactly one full pass.
        let key = self.order_pos * 64 + self.row;
        if key < self.visited.len() {
            if self.visited[key] {
                self.done = true;
                return
            }
            self.visited[key] = true;
        }

        let cells: Vec<Cell> = {
            let pattern = self.pattern();
            let width = self.module.channels;
            let row = self.row.min(pattern.rows.saturating_sub(1));
            pattern.cells[row * width..(row + 1) * width].to_vec()
        };
        for (i, cell) in cells.into_iter().enumerate() {
            self.start_cell(i, cell);
        }
    }

    fn start_cell(&mut self, index: usize, cell: Cell) {
        // Note delay has to be decided before anything else in the cell is applied: the whole
        // point is that none of it happens yet.
        let delay = match self.module.format {
            Format::Mod if cell.effect == 0x0E && cell.param >> 4 == 0x0D => (cell.param & 0x0F) as i32,
            Format::S3m if cell.effect == eff::S && cell.param >> 4 == 0x0D => (cell.param & 0x0F) as i32,
            _ => 0,
        };
        if delay > 0 {
            let ch = &mut self.channels[index];
            ch.delay_ticks = delay;
            ch.delayed = Some(cell);
            return
        }
        self.apply_cell(index, cell);
    }

    fn apply_cell(&mut self, index: usize, cell: Cell) {
        let module = self.module;
        // Tone portamento must not restart the sample -- it bends the note already sounding. The
        // two effects that mean it differ per format, hence resolving it before the note handling.
        let is_tone_porta = match module.format {
            Format::Mod => cell.effect == 0x03 || cell.effect == 0x05,
            Format::S3m => cell.effect == eff::G || cell.effect == eff::L,
        };

        if cell.instrument != 0 {
            let idx = cell.instrument as usize - 1;
            if let Some(sample) = module.samples.get(idx) {
                let ch = &mut self.channels[index];
                ch.sample = Some(idx);
                ch.volume = sample.volume;
                ch.finetune = sample.finetune;
                ch.c2spd = sample.c2spd;
            }
        }

        if cell.note == NOTE_OFF {
            self.channels[index].playing = false;
        } else if cell.note != NO_NOTE {
            // A note with no instrument column reuses whatever the channel already holds, which is
            // how a module retriggers the same instrument without respecifying it.
            let period = {
                let ch = &self.channels[index];
                module.note_period(ch, cell.note)
            };
            if let Some(period) = period {
                let ch = &mut self.channels[index];
                ch.note = cell.note;
                if is_tone_porta {
                    ch.porta_target = period;
                } else {
                    ch.period = period;
                    ch.pos = 0.0;
                    ch.playing = ch.sample.is_some();
                    ch.porta_target = period;
                    if ch.vib_wave & 4 == 0 {
                        ch.vib_pos = 0;
                    }
                    if ch.trem_wave & 4 == 0 {
                        ch.trem_pos = 0;
                    }
                }
            }
        }

        if cell.volume != NO_VOLUME {
            self.channels[index].volume = (cell.volume as i32).min(MAX_VOLUME);
        }

        match module.format {
            Format::Mod => self.mod_effect_row(index, cell),
            Format::S3m => self.s3m_effect_row(index, cell),
        }
    }

    fn advance_tick(&mut self) {
        self.tick += 1;
        if self.tick < self.speed.max(1) {
            return
        }
        self.tick = 0;
        if self.pattern_delay > 0 {
            // Stay on this row; `in_pattern_delay` makes its tick 0 run effects rather than
            // re-striking the notes.
            self.pattern_delay -= 1;
            self.in_pattern_delay = true;
            return
        }
        self.in_pattern_delay = false;

        let rows = self.pattern().rows;
        let (mut order, mut row) = (self.order_pos, self.row + 1);
        if let Some(next) = self.next_order.take() {
            order = next;
            row = self.next_row.take().unwrap_or(0);
        } else if let Some(next) = self.next_row.take() {
            order += 1;
            row = next;
        } else if row >= rows {
            order += 1;
            row = 0;
        }
        if order >= self.module.order.len() {
            // Past the end of the order list the song restarts. Whether it wraps to 0 or to a
            // stored restart position doesn't matter here: either way `visited` catches it on the
            // first row and ends the render.
            order = 0;
        }
        if order != self.order_pos {
            // A pattern loop is scoped to its pattern; carrying its state across would make a
            // later pattern jump back into an earlier one.
            self.loop_row = 0;
            self.loop_count = 0;
        }
        self.order_pos = order;
        self.row = row;
    }

    // -- effects ------------------------------------------------------------------------------

    /// Tick 0 of a MOD effect.
    ///
    /// Split in two because a handful of these are not channel effects at all -- they retarget the
    /// sequencer (jump, break, speed, pattern loop and delay) and so must run without a mutable
    /// borrow of the channel outstanding. Everything else is the channel's own.
    fn mod_effect_row(&mut self, index: usize, cell: Cell) {
        let (effect, param) = (cell.effect, cell.param);
        let (x, y) = (param >> 4, param & 0x0F);
        match (effect, x) {
            (0x0B, _) => {
                self.next_order = Some(param as usize);
                self.next_row = Some(0);
                return
            },
            (0x0D, _) => {
                // The parameter is read as two decimal digits, not as a byte -- D16 is row 16.
                self.next_row = Some((x as usize * 10 + y as usize).min(63));
                return
            },
            (0x0F, _) => {
                // One command for two clocks: below 0x20 it is ticks per row, at or above it is
                // beats per minute. The split is why no module can run faster than 31 ticks a row.
                if param < 0x20 {
                    self.speed = (param as u32).max(1);
                } else {
                    self.tempo = param as u32;
                }
                return
            },
            (0x0E, 0x06) => {
                self.pattern_loop(y);
                return
            },
            (0x0E, 0x0E) => {
                self.pattern_delay = y as u32;
                return
            },
            _ => {},
        }

        let ch = &mut self.channels[index];
        match effect {
            0x00 => ch.arpeggio = param,
            0x01 => ch.mem_porta_up = if param != 0 { param } else { ch.mem_porta_up },
            0x02 => ch.mem_porta_down = if param != 0 { param } else { ch.mem_porta_down },
            0x03 => {
                if param != 0 {
                    ch.mem_tone_porta = param;
                }
                ch.porta_speed = ch.mem_tone_porta as f64;
            },
            0x04 => {
                if x != 0 {
                    ch.vib_speed = x as i32;
                }
                if y != 0 {
                    ch.vib_depth = y as i32;
                }
            },
            0x05 | 0x06 | 0x0A => {
                if param != 0 {
                    ch.mem_volslide = param;
                }
            },
            0x07 => {
                if x != 0 {
                    ch.trem_speed = x as i32;
                }
                if y != 0 {
                    ch.trem_depth = y as i32;
                }
            },
            // 8xx is set-panning in the trackers that extended MOD past the Amiga. On a genuine
            // four-channel module it is usually a no-op left in by a converter, but honouring it
            // costs nothing and is right whenever it was meant.
            0x08 => ch.pan = (param as f32 / 255.0).clamp(0.0, 1.0),
            0x09 => {
                if param != 0 {
                    ch.mem_offset = param;
                }
                // Only when the row actually strikes a note. The note handling above has already
                // rewound to 0, so this moves the start of *that* note; applying it to a sample
                // already sounding would jump it mid-phrase.
                if cell.note != NO_NOTE {
                    ch.pos = (ch.mem_offset as f64) * 256.0;
                }
            },
            0x0C => ch.volume = (param as i32).min(MAX_VOLUME),
            0x0E => match x {
                0x01 => ch.period = (ch.period - y as f64).max(1.0),
                0x02 => ch.period += y as f64,
                0x04 => ch.vib_wave = y,
                0x05 => ch.finetune = ((y as i8) << 4) >> 4,
                0x07 => ch.trem_wave = y,
                0x09 => ch.mem_retrig = y,
                0x0A => ch.volume = (ch.volume + y as i32).min(MAX_VOLUME),
                0x0B => ch.volume = (ch.volume - y as i32).max(0),
                0x0C => ch.cut_after = y as i32,
                _ => {},
            },
            _ => {},
        }
    }

    /// `E6x` (MOD) / `SBx` (S3M): `x == 0` marks the row to come back to, and any other value
    /// jumps back to it that many times. The counter lives on the player rather than the channel
    /// because the jump is the pattern's, and it is reset when the order position changes.
    fn pattern_loop(&mut self, x: u8) {
        if x == 0 {
            self.loop_row = self.row;
            return
        }
        if self.loop_count == 0 {
            self.loop_count = x as i32;
        } else {
            self.loop_count -= 1;
        }
        if self.loop_count > 0 {
            self.next_order = Some(self.order_pos);
            self.next_row = Some(self.loop_row);
        }
    }

    fn tick_effects(&mut self) {
        for index in 0..self.channels.len() {
            // A delayed note fires mid-row and then behaves exactly like one placed on tick 0.
            let fire = {
                let ch = &mut self.channels[index];
                if ch.delay_ticks > 0 {
                    ch.delay_ticks -= 1;
                    if ch.delay_ticks == 0 { ch.delayed.take() } else { None }
                } else {
                    None
                }
            };
            if let Some(cell) = fire {
                self.apply_cell(index, cell);
                continue
            }

            let ch = &mut self.channels[index];
            if ch.cut_after > 0 {
                ch.cut_after -= 1;
                if ch.cut_after == 0 {
                    ch.volume = 0;
                }
            }

            let cell = self.row_cell(index);
            match self.module.format {
                Format::Mod => self.mod_effect_tick(index, cell),
                Format::S3m => self.s3m_effect_tick(index, cell),
            }
        }
    }

    fn row_cell(&self, index: usize) -> Cell {
        let pattern = self.pattern();
        let width = self.module.channels;
        let row = self.row.min(pattern.rows.saturating_sub(1));
        pattern.cells[row * width + index]
    }

    fn mod_effect_tick(&mut self, index: usize, cell: Cell) {
        let param = cell.param;
        match cell.effect {
            0x00 => {
                if param != 0 {
                    self.do_arpeggio(index, param)
                }
            },
            0x01 => {
                let step = self.channels[index].mem_porta_up as f64;
                let ch = &mut self.channels[index];
                ch.period = (ch.period - step).max(1.0);
            },
            0x02 => {
                let step = self.channels[index].mem_porta_down as f64;
                self.channels[index].period += step;
            },
            0x03 => self.do_tone_porta(index),
            0x04 => self.do_vibrato(index),
            0x05 => {
                self.do_tone_porta(index);
                let p = self.channels[index].mem_volslide;
                self.do_volume_slide(index, p);
            },
            0x06 => {
                self.do_vibrato(index);
                let p = self.channels[index].mem_volslide;
                self.do_volume_slide(index, p);
            },
            0x07 => self.do_tremolo(index),
            0x0A => {
                let p = self.channels[index].mem_volslide;
                self.do_volume_slide(index, p);
            },
            0x0E => {
                if param >> 4 == 0x09 {
                    let n = self.channels[index].mem_retrig as u32;
                    if n > 0 && self.tick % n == 0 {
                        self.channels[index].pos = 0.0;
                        self.channels[index].playing = self.channels[index].sample.is_some();
                    }
                }
            },
            _ => {},
        }
    }

    /// Tick 0 of an S3M effect. Split for the same reason as [`Self::mod_effect_row`].
    fn s3m_effect_row(&mut self, index: usize, cell: Cell) {
        // Effects arrive as 1 = A, 2 = B, ... which is how the file stores them.
        let (effect, param) = (cell.effect, cell.param);
        let (x, y) = (param >> 4, param & 0x0F);
        match (effect, x) {
            (e, _) if e == eff::A => {
                self.speed = (param as u32).max(1);
                return
            },
            (e, _) if e == eff::B => {
                self.next_order = Some(param as usize);
                self.next_row = Some(0);
                return
            },
            (e, _) if e == eff::C => {
                // Unlike MOD's Dxx, S3M's break parameter really is read as two decimal digits too.
                self.next_row = Some((x as usize * 10 + y as usize).min(63));
                return
            },
            (e, _) if e == eff::T => {
                self.tempo = (param as u32).max(32);
                return
            },
            (e, _) if e == eff::V => {
                self.global_volume = (param as i32).min(MAX_VOLUME);
                return
            },
            (e, 0x0B) if e == eff::S => {
                self.pattern_loop(y);
                return
            },
            (e, 0x0E) if e == eff::S => {
                self.pattern_delay = y as u32;
                return
            },
            _ => {},
        }

        let ch = &mut self.channels[index];
        // Almost every S3M effect shares one "last non-zero parameter" per effect letter, so the
        // memory update happens up front and the arms below read the remembered value.
        match effect {
            e if e == eff::D || e == eff::K || e == eff::L => {
                if param != 0 {
                    ch.mem_volslide = param;
                }
                // Fine slides happen once, on tick 0, and are encoded in the parameter's shape:
                // `DxF` slides up by x, `DFy` slides down by y. Kxy and Lxy carry a volume slide
                // in the same parameter; their vibrato/portamento halves keep their own memories
                // (H's and G's), which is why neither reads `param` for pitch here.
                let p = ch.mem_volslide;
                let (px, py) = (p >> 4, p & 0x0F);
                if py == 0x0F && px != 0 {
                    ch.volume = (ch.volume + px as i32).min(MAX_VOLUME);
                } else if px == 0x0F && py != 0 {
                    ch.volume = (ch.volume - py as i32).max(0);
                }
                if e == eff::L {
                    ch.porta_speed = ch.mem_tone_porta as f64;
                }
            },
            e if e == eff::E || e == eff::F => {
                if param != 0 {
                    if e == eff::E {
                        ch.mem_porta_down = param;
                    } else {
                        ch.mem_porta_up = param;
                    }
                }
                let p = if e == eff::E { ch.mem_porta_down } else { ch.mem_porta_up };
                let (px, py) = (p >> 4, p & 0x0F);
                // Exx/Fxx fold three effects into one parameter: Fx is fine (once per row), Ex is
                // extra-fine (a quarter step), anything else is the ordinary per-tick slide.
                let step = match px {
                    0x0F => py as f64,
                    0x0E => py as f64 / 4.0,
                    _ => 0.0,
                };
                if step != 0.0 {
                    if e == eff::E {
                        ch.period += step;
                    } else {
                        ch.period = (ch.period - step).max(1.0);
                    }
                }
            },
            e if e == eff::G => {
                if param != 0 {
                    ch.mem_tone_porta = param;
                }
                ch.porta_speed = ch.mem_tone_porta as f64;
            },
            e if e == eff::H || e == eff::U => {
                if param != 0 {
                    ch.mem_vibrato = param;
                }
                let p = ch.mem_vibrato;
                if p >> 4 != 0 {
                    ch.vib_speed = (p >> 4) as i32;
                }
                if p & 0x0F != 0 {
                    // Uxy is the same vibrato at a quarter of the depth; scaling here keeps one
                    // implementation of the oscillator.
                    ch.vib_depth = (p & 0x0F) as i32 * if e == eff::U { 1 } else { 4 };
                }
            },
            e if e == eff::I => {
                if param != 0 {
                    ch.mem_tremor = param;
                }
                ch.tremor_pos = 0;
            },
            e if e == eff::J => ch.arpeggio = param,
            e if e == eff::O => {
                if param != 0 {
                    ch.mem_offset = param;
                }
                // See MOD's 9xx.
                if cell.note != NO_NOTE {
                    ch.pos = (ch.mem_offset as f64) * 256.0;
                }
            },
            e if e == eff::Q => {
                if param != 0 {
                    ch.mem_retrig = param;
                }
                ch.retrig_count = 0;
            },
            e if e == eff::R => {
                if x != 0 {
                    ch.trem_speed = x as i32;
                }
                if y != 0 {
                    ch.trem_depth = y as i32;
                }
            },
            e if e == eff::S => match x {
                0x03 => ch.vib_wave = y,
                0x04 => ch.trem_wave = y,
                0x08 => ch.pan = y as f32 / 15.0,
                0x0B => {
                    if y == 0 {
                        self.loop_row = self.row;
                    } else {
                        if self.loop_count == 0 {
                            self.loop_count = y as i32;
                        } else {
                            self.loop_count -= 1;
                        }
                        if self.loop_count > 0 {
                            self.next_order = Some(self.order_pos);
                            self.next_row = Some(self.loop_row);
                        }
                    }
                    return
                },
                0x0C => ch.cut_after = y as i32,
                0x0E => {
                    self.pattern_delay = y as u32;
                    return
                },
                _ => {},
            },
            e if e == eff::T => {
                self.tempo = (param as u32).max(32);
                return
            },
            e if e == eff::V => {
                self.global_volume = (param as i32).min(MAX_VOLUME);
                return
            },
            e if e == eff::X => ch.pan = (param as f32 / 128.0).clamp(0.0, 1.0),
            _ => {},
        }
    }

    /// The per-tick half of S3M's `Dxy`, shared by `Kxy` and `Lxy` which carry the same slide.
    fn s3m_volume_slide(&mut self, index: usize) {
        let param = self.channels[index].mem_volslide;
        let (x, y) = (param >> 4, param & 0x0F);
        // The fine forms already ran on tick 0; running them again here would slide twice as far.
        if x != 0x0F && y != 0x0F {
            self.do_volume_slide(index, param);
        }
    }

    fn s3m_effect_tick(&mut self, index: usize, cell: Cell) {
        let effect = cell.effect;
        match effect {
            e if e == eff::D => self.s3m_volume_slide(index),
            e if e == eff::E || e == eff::F => {
                let p =
                    if e == eff::E { self.channels[index].mem_porta_down } else { self.channels[index].mem_porta_up };
                if p >> 4 < 0x0E {
                    let ch = &mut self.channels[index];
                    if e == eff::E {
                        ch.period += p as f64 * 4.0;
                    } else {
                        ch.period = (ch.period - p as f64 * 4.0).max(1.0);
                    }
                }
            },
            e if e == eff::G => self.do_tone_porta(index),
            e if e == eff::H || e == eff::U => self.do_vibrato(index),
            e if e == eff::I => {
                let p = self.channels[index].mem_tremor;
                let (on, off) = (((p >> 4) as i32).max(1), ((p & 0x0F) as i32).max(1));
                let ch = &mut self.channels[index];
                ch.tremor_pos = (ch.tremor_pos + 1) % (on + off);
                // Gates the channel rather than changing its volume, so the volume the row set is
                // still there when it comes back on.
                ch.tremor_muted = ch.tremor_pos >= on;
            },
            e if e == eff::J => {
                let p = self.channels[index].arpeggio;
                if p != 0 {
                    self.do_arpeggio(index, p)
                }
            },
            e if e == eff::K => {
                self.do_vibrato(index);
                self.s3m_volume_slide(index);
            },
            e if e == eff::L => {
                self.do_tone_porta(index);
                self.s3m_volume_slide(index);
            },
            e if e == eff::Q => {
                let p = self.channels[index].mem_retrig;
                let interval = ((p & 0x0F) as i32).max(1);
                let ch = &mut self.channels[index];
                ch.retrig_count += 1;
                if ch.retrig_count >= interval {
                    ch.retrig_count = 0;
                    ch.pos = 0.0;
                    ch.playing = ch.sample.is_some();
                    // The high nibble is a volume change applied at each retrigger: 1..5 subtract,
                    // 9..0xD add, and 6/7/0xE/0xF scale.
                    let vol = ch.volume;
                    ch.volume = match p >> 4 {
                        v @ 0x01..=0x05 => (vol - (1 << (v - 1))).max(0),
                        0x06 => vol * 2 / 3,
                        0x07 => vol / 2,
                        v @ 0x09..=0x0D => (vol + (1 << (v - 9))).min(MAX_VOLUME),
                        0x0E => (vol * 3 / 2).min(MAX_VOLUME),
                        0x0F => (vol * 2).min(MAX_VOLUME),
                        _ => vol,
                    };
                }
            },
            e if e == eff::R => self.do_tremolo(index),
            _ => {},
        }
    }

    // -- shared effect helpers ------------------------------------------------------------------

    /// Arpeggio: cycle the pitch between the note, +x semitones and +y semitones, one per tick.
    ///
    /// Expressed as an offset from the channel's own period rather than by overwriting it, so a
    /// portamento sharing the channel still slides from the real note.
    fn do_arpeggio(&mut self, index: usize, param: u8) {
        let step = match self.tick % 3 {
            0 => return, // the base note; `period_mod` was already cleared for this tick
            1 => param >> 4,
            _ => param & 0x0F,
        };
        let ch = &self.channels[index];
        let note = ch.note.saturating_add(step);
        if let (Some(base), Some(stepped)) = (self.module.note_period(ch, ch.note), self.module.note_period(ch, note)) {
            self.channels[index].period_mod = stepped - base;
        }
    }

    fn do_tone_porta(&mut self, index: usize) {
        // MOD's units are period steps; S3M's are quarter-steps, the same factor its plain
        // portamento uses.
        let scale = if matches!(self.module.format, Format::S3m) { 4.0 } else { 1.0 };
        let ch = &mut self.channels[index];
        if ch.porta_target <= 0.0 || ch.period <= 0.0 {
            return
        }
        let step = ch.porta_speed * scale;
        if ch.period < ch.porta_target {
            ch.period = (ch.period + step).min(ch.porta_target);
        } else {
            ch.period = (ch.period - step).max(ch.porta_target);
        }
    }

    /// Reads one of the four oscillator waveforms. Shared by vibrato and tremolo, which differ only
    /// in what they modulate and by how much.
    fn oscillator(wave: u8, pos: i32) -> i32 {
        let p = (pos & 63) as usize;
        match wave & 3 {
            // The sine table is a half-period; the second half is the same values negated.
            0 => {
                let v = SINE[p & 31];
                if p < 32 { v } else { -v }
            },
            // A falling sawtooth across the whole cycle, 255 down to -249 -- not a mirrored ramp.
            1 => 255 - (p as i32) * 8,
            // Square. Waveform 3 is "random" in ProTracker; the square is a closer stand-in than
            // silence, and no game seen so far selects it.
            _ => {
                if p < 32 {
                    255
                } else {
                    -255
                }
            },
        }
    }

    fn do_vibrato(&mut self, index: usize) {
        let ch = &mut self.channels[index];
        ch.period_mod = (Self::oscillator(ch.vib_wave, ch.vib_pos) * ch.vib_depth / 128) as f64;
        ch.vib_pos = (ch.vib_pos + ch.vib_speed) & 63;
    }

    fn do_tremolo(&mut self, index: usize) {
        let ch = &mut self.channels[index];
        ch.volume_mod = Self::oscillator(ch.trem_wave, ch.trem_pos) * ch.trem_depth / 64;
        ch.trem_pos = (ch.trem_pos + ch.trem_speed) & 63;
    }

    fn do_volume_slide(&mut self, index: usize, param: u8) {
        let (x, y) = ((param >> 4) as i32, (param & 0x0F) as i32);
        let ch = &mut self.channels[index];
        // Up wins when both nibbles are set -- a quirk of the original code every module inherits.
        ch.volume = if x > 0 { (ch.volume + x).min(MAX_VOLUME) } else { (ch.volume - y).max(0) };
    }

    // -- mixing -----------------------------------------------------------------------------------

    fn mix(&mut self, frames: usize) {
        let start = self.left.len();
        self.left.resize(start + frames, 0.0);
        self.right.resize(start + frames, 0.0);

        // Destructured so the loop can hold a mutable borrow of one channel and of the output
        // buffers at the same time; `&mut self.channels[i]` plus `self.left` would not compile.
        let Player { module, channels, left, right, sample_rate, global_volume, .. } = self;
        let (sample_rate, global_volume) = (*sample_rate, *global_volume);

        for ch in channels.iter_mut() {
            if !ch.playing || ch.tremor_muted || ch.period <= 0.0 {
                continue
            }
            let Some(sample) = ch.sample.and_then(|i| module.samples.get(i)) else { continue };
            if sample.data.is_empty() {
                continue
            }
            let hz = module.period_to_hz((ch.period + ch.period_mod).max(1.0));
            if hz <= 0.0 {
                continue
            }
            let step = hz / sample_rate;
            let volume = (ch.volume + ch.volume_mod).clamp(0, MAX_VOLUME);
            // The 1/4 keeps a four-channel module from clipping when every channel is at full
            // volume, which is what a tracker's own mixer does with its headroom.
            let gain = (volume as f32 / MAX_VOLUME as f32) * (global_volume as f32 / MAX_VOLUME as f32) * 0.25;
            // Equal-power panning: a centred channel keeps the loudness it has hard left, which
            // constant-gain panning would drop by 3 dB right where most S3M channels sit.
            let angle = ch.pan.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
            let (gl, gr) = (angle.cos() * gain, angle.sin() * gain);

            let end = if sample.loop_len > 0 { sample.loop_start + sample.loop_len } else { sample.data.len() };
            for i in 0..frames {
                if ch.pos >= end as f64 {
                    if sample.loop_len > 0 {
                        // Wrapping by the loop length rather than assigning `loop_start` keeps a
                        // very short loop (some are a handful of frames) in phase.
                        let over = ch.pos - sample.loop_start as f64;
                        ch.pos = sample.loop_start as f64 + over % sample.loop_len as f64;
                    } else {
                        ch.playing = false;
                        break
                    }
                }
                let idx = ch.pos as usize;
                let frac = (ch.pos - idx as f64) as f32;
                let a = sample.data[idx.min(sample.data.len() - 1)];
                // Linear interpolation, wrapping into the loop so a looped sample has no click at
                // the seam.
                let next = if idx + 1 < end {
                    idx + 1
                } else if sample.loop_len > 0 {
                    sample.loop_start
                } else {
                    idx
                };
                let b = sample.data[next.min(sample.data.len() - 1)];
                let v = a + (b - a) * frac;
                left[start + i] += v * gl;
                right[start + i] += v * gr;
                ch.pos += step;
            }
        }

        // Channels are summed with headroom rather than normalised, so this only catches a module
        // that genuinely asked for more than full scale.
        for v in left[start..].iter_mut().chain(right[start..].iter_mut()) {
            *v = v.clamp(-1.0, 1.0);
        }
    }
}
