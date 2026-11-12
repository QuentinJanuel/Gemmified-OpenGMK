//! Minimal synchronous WAV (RIFF/PCM) decoder. Ported from `udon::wav::WavPlayer`'s parsing logic
//! (same GPL-2.0-only license as this crate) since `udon` itself wraps `libsoundio` and isn't
//! portable to wasm32 -- but decodes the whole file eagerly into a flat per-channel buffer instead
//! of streaming sample-by-sample, since a Web Audio `AudioBuffer` needs the whole thing up front
//! anyway (see `super::decoded_to_buffer`).

pub struct Decoded {
    pub channels: u16,
    pub sample_rate: u32,
    /// Planar (per-channel contiguous), NOT interleaved: channel 0's samples, then channel 1's.
    pub planar_samples: Vec<f32>,
    pub frames: usize,
}

#[derive(Clone, Copy)]
enum Format {
    U8,
    I16,
    I24,
    I32,
    F32,
}

fn find_section(data: &[u8], section_name: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = 12;
    let len = loop {
        if data.len() < pos + 8 {
            return None
        }
        let is_match = data[pos..(pos + 4)] == section_name[..];
        let len = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]) as usize;
        pos += 8;
        if is_match {
            break len
        } else {
            pos = pos.checked_add(len)?;
        }
    };
    Some((pos, len))
}

fn get_sample(format: Format, data: &[u8]) -> f32 {
    match format {
        Format::U8 => {
            let sample = i16::from(data[0]) - 0x80;
            f32::from(sample) / f32::from(i8::MAX)
        },
        Format::I16 => f32::from(i16::from_le_bytes([data[0], data[1]])) / f32::from(i16::MAX),
        Format::I24 => {
            let raw = i32::from_le_bytes([data[0], data[1], data[2], 0]);
            let sign_mask = 1 << 23;
            let sample = (raw ^ sign_mask).wrapping_sub(sign_mask);
            (f64::from(sample) / 8388608.0) as f32 // 2^23, the imaginary i24::MAX
        },
        Format::I32 => {
            (f64::from(i32::from_le_bytes([data[0], data[1], data[2], data[3]])) / f64::from(i32::MAX)) as f32
        },
        Format::F32 => f32::from_le_bytes([data[0], data[1], data[2], data[3]]),
    }
}

pub fn decode(file: &[u8]) -> Option<Decoded> {
    if file.get(0..4) != Some(b"RIFF") || file.get(8..12) != Some(b"WAVE") {
        return None
    }

    let (fmt_start, fmt_len) = find_section(file, b"fmt ")?;
    if fmt_len < 16 {
        return None
    }
    let fmt = file.get(fmt_start..(fmt_start + fmt_len))?;

    let audio_format = i16::from_le_bytes([fmt[0], fmt[1]]);
    let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
    let sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
    let sample_bits = u16::from_le_bytes([fmt[14], fmt[15]]);
    let ext_guid = fmt.get(24..40);

    // WAVE_FORMAT_EXTENSIBLE (-2) sub-format GUIDs for PCM and IEEE float, matching the two GM8
    // actually emits/accepts (same bytes as `udon::wav::WavPlayer`).
    let data_format_pcm: &[u8] = &[1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113];
    let data_format_float: &[u8] = &[3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113];
    let format = match (audio_format, sample_bits) {
        (1, 8) => Format::U8,
        (1, 16) => Format::I16,
        (1, 24) => Format::I24,
        (1, 32) => Format::I32,
        (3, 32) => Format::F32,
        (-2, 8) if ext_guid == Some(data_format_pcm) => Format::U8,
        (-2, 16) if ext_guid == Some(data_format_pcm) => Format::I16,
        (-2, 24) if ext_guid == Some(data_format_pcm) => Format::I24,
        (-2, 32) if ext_guid == Some(data_format_pcm) => Format::I32,
        (-2, 32) if ext_guid == Some(data_format_float) => Format::F32,
        _ => return None,
    };

    if channels == 0 || sample_rate == 0 {
        return None
    }

    let (data_start, data_len) = find_section(file, b"data")?;
    let data = match data_start.checked_add(data_len) {
        Some(end) if end <= file.len() => &file[data_start..end],
        _ => file.get(data_start..)?,
    };

    let sample_bytes = usize::from(sample_bits / 8);
    if sample_bytes == 0 {
        return None
    }
    let channels_usize = usize::from(channels);
    let frames = (data.len() / sample_bytes) / channels_usize;

    let mut planar = vec![0.0f32; channels_usize * frames];
    for frame in 0..frames {
        for ch in 0..channels_usize {
            let offset = (frame * channels_usize + ch) * sample_bytes;
            planar[ch * frames + frame] = get_sample(format, &data[offset..offset + sample_bytes]);
        }
    }

    Some(Decoded { channels, sample_rate, planar_samples: planar, frames })
}
