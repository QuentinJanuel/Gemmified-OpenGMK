//! Minimal synchronous OGG/Vorbis decoder for `ssound.dll` support (`game/external/ssound.rs`),
//! using `symphonia` (pure Rust, no C toolchain). GM8's own built-in sound system never supports
//! OGG at all (only WAV/MP3) -- external DLLs like `ssound.dll` are the only source of OGG files
//! in this project, which is why this lives here rather than alongside `mp3.rs`/`wav.rs`.
//!
//! Unlike `mp3.rs`, this doesn't need a two-pass decode: MP3's second pass exists specifically to
//! replicate a documented native GM8/rmp3 bug (skip any frame whose channel count differs from the
//! first). Nothing analogous applies to `ssound.dll`'s OGG files, so a single pass accumulating
//! into growable per-channel buffers is enough.

use std::io::Cursor;
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{CODEC_TYPE_NULL, DecoderOptions},
    errors::Error as SymError,
    formats::FormatOptions,
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
};

pub struct Decoded {
    pub channels: u16,
    pub sample_rate: u32,
    /// Planar (per-channel contiguous), NOT interleaved.
    pub planar_samples: Vec<f32>,
    pub frames: usize,
}

pub fn decode(bytes: &[u8]) -> Option<Decoded> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes.to_vec())), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("ogg");
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?;
    let mut format = probed.format;
    let track_id = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL)?.id;
    let params = &format.tracks().iter().find(|t| t.id == track_id)?.codec_params;
    let mut decoder = symphonia::default::get_codecs().make(params, &DecoderOptions::default()).ok()?;

    let mut channels: Option<u16> = None;
    let mut sample_rate = 0u32;
    let mut per_channel: Vec<Vec<f32>> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let ch = spec.channels.count() as u16;
                if channels.is_none() {
                    channels = Some(ch);
                    sample_rate = spec.rate;
                    per_channel = vec![Vec::new(); usize::from(ch)];
                }
                if Some(ch) != channels {
                    continue
                }
                let this_frames = decoded.frames();
                let mut buf = SampleBuffer::<f32>::new(this_frames as u64, spec);
                buf.copy_planar_ref(decoded);
                for (c, out) in per_channel.iter_mut().enumerate() {
                    out.extend_from_slice(&buf.samples()[c * this_frames..(c + 1) * this_frames]);
                }
            },
            Err(SymError::IoError(_)) | Err(SymError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    let channels = channels?;
    let frames = per_channel.first().map(Vec::len).unwrap_or(0);
    if channels == 0 || frames == 0 {
        return None
    }

    let mut planar = Vec::with_capacity(usize::from(channels) * frames);
    for ch_data in &per_channel {
        planar.extend_from_slice(ch_data);
    }

    Some(Decoded { channels, sample_rate, planar_samples: planar, frames })
}
