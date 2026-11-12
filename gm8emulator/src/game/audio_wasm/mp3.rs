//! Minimal synchronous MP3 decoder for the wasm build, using `symphonia` (pure Rust, no C
//! toolchain) instead of `rmp3` (used natively -- its build.rs compiles a C file via `cc`, and no
//! wasm32-capable C compiler is available in this project's build environment).
//!
//! Deliberately replicates the same GM8 bug documented on the native `Mp3Player` (`game/audio/
//! mp3.rs`): the first successfully decoded frame's channel count and sample rate are assumed to
//! be authoritative for the whole file, and any later frame with a different channel count is
//! skipped entirely rather than erroring or resampling.

use std::io::Cursor;
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{CODEC_TYPE_NULL, DecoderOptions},
    errors::Error as SymError,
    formats::{FormatOptions, FormatReader},
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

fn make_reader(bytes: &[u8]) -> Option<(Box<dyn FormatReader>, u32)> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes.to_vec())), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?;
    let format = probed.format;
    let track_id = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL)?.id;
    Some((format, track_id))
}

fn make_decoder(format: &dyn FormatReader, track_id: u32) -> Option<Box<dyn symphonia::core::codecs::Decoder>> {
    let params = &format.tracks().iter().find(|t| t.id == track_id)?.codec_params;
    symphonia::default::get_codecs().make(params, &DecoderOptions::default()).ok()
}

pub fn decode(bytes: &[u8]) -> Option<Decoded> {
    // Pass 1: find the authoritative channel count + sample rate (the first successfully decoded
    // frame), and count total frames from every frame whose channel count matches it.
    let (mut format, track_id) = make_reader(bytes)?;
    let mut decoder = make_decoder(format.as_ref(), track_id)?;

    let mut channels: Option<u16> = None;
    let mut sample_rate = 0u32;
    let mut total_frames = 0usize;

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
                }
                if Some(ch) == channels {
                    total_frames += decoded.frames();
                }
            },
            Err(SymError::IoError(_)) | Err(SymError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    let channels = channels?;
    if channels == 0 || total_frames == 0 {
        return None
    }

    // Pass 2: symphonia's format reader is a forward-only stream (not rewindable in general), so
    // decode again from a fresh reader/decoder, this time actually copying samples for
    // channel-count-matching frames into a preallocated flat planar buffer.
    let (mut format, track_id) = make_reader(bytes)?;
    let mut decoder = make_decoder(format.as_ref(), track_id)?;

    let mut planar = vec![0.0f32; usize::from(channels) * total_frames];
    let mut frame_cursor = 0usize;

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
                if spec.channels.count() as u16 != channels {
                    continue
                }
                let this_frames = decoded.frames();
                if frame_cursor + this_frames > total_frames {
                    break
                }
                let mut buf = SampleBuffer::<f32>::new(this_frames as u64, spec);
                buf.copy_planar_ref(decoded);
                for ch in 0..usize::from(channels) {
                    let src = &buf.samples()[ch * this_frames..(ch + 1) * this_frames];
                    let dst_start = ch * total_frames + frame_cursor;
                    planar[dst_start..dst_start + this_frames].copy_from_slice(src);
                }
                frame_cursor += this_frames;
            },
            Err(SymError::IoError(_)) | Err(SymError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    Some(Decoded { channels, sample_rate, planar_samples: planar, frames: total_frames })
}
