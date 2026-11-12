//! Initial lazy-audio policy for AudioManager::new, from the host's settings (the `audio.*` keys
//! the plugin declares), so embedded sounds registered during Game::launch already follow it.

use wasm_bindgen::prelude::*;

use crate::game::audio::{
    AudioCachePolicy, DEFAULT_MIDI_SAMPLE_RATE, DEFAULT_MUSIC_BUDGET_BYTES, DEFAULT_SHORT_COMPRESSED_BYTES,
    DEFAULT_SHORT_DURATION_SECS,
};

fn emu_audio_cache_config() -> JsValue {
    let config = js_sys::Object::new();
    for key in ["lazy", "shortCompressedBytes", "shortDurationSecs", "musicBudgetBytes", "midiSampleRate"] {
        let _ = js_sys::Reflect::set(&config, &key.into(), &crate::gems::setting(&format!("audio.{key}")));
    }
    config.into()
}

pub fn initial_mode_and_policy() -> (bool, AudioCachePolicy) {
    let cfg = emu_audio_cache_config();
    let lazy = js_sys::Reflect::get(&cfg, &JsValue::from_str("lazy")).ok().and_then(|v| v.as_bool()).unwrap_or(false);
    let short_compressed_bytes = js_sys::Reflect::get(&cfg, &JsValue::from_str("shortCompressedBytes"))
        .ok()
        .and_then(|v| v.as_f64())
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_SHORT_COMPRESSED_BYTES);
    let short_duration_secs = js_sys::Reflect::get(&cfg, &JsValue::from_str("shortDurationSecs"))
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(DEFAULT_SHORT_DURATION_SECS);
    let music_budget_bytes = js_sys::Reflect::get(&cfg, &JsValue::from_str("musicBudgetBytes"))
        .ok()
        .and_then(|v| v.as_f64())
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MUSIC_BUDGET_BYTES);
    let midi_sample_rate = js_sys::Reflect::get(&cfg, &JsValue::from_str("midiSampleRate"))
        .ok()
        .and_then(|v| v.as_f64())
        .map(|n| n as u32)
        .unwrap_or(DEFAULT_MIDI_SAMPLE_RATE);
    (lazy, AudioCachePolicy { short_compressed_bytes, short_duration_secs, music_budget_bytes, midi_sample_rate })
}
