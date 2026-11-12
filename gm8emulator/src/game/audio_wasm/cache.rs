//! Shared decoded-AudioBuffer LRU for the wasm AudioManager.
//!
//! Built-in GM8 sounds, FMOD, and ssound all go through this so one byte budget and one pin/evict
//! policy cover every path. Compressed bytes stay here for re-decode; decoded PCM for "music" is
//! capped; short SFX stay resident once decoded.

use crate::game::audio_proxy::AudioBuffer;
use std::{
    collections::{HashMap, VecDeque},
    rc::Rc,
};

/// Defaults for the `audio.*` settings `host/plugin.js` declares; a host that knows its device
/// overrides them.
pub const DEFAULT_SHORT_COMPRESSED_BYTES: usize = 256 * 1024;
pub const DEFAULT_SHORT_DURATION_SECS: f64 = 5.0;
pub const DEFAULT_MUSIC_BUDGET_BYTES: usize = 192 * 1024 * 1024;

/// Rate MIDI scores are rendered at. Unlike the three real codecs, whose rate is a property of the
/// file, a synthesised track's rate is ours to pick, and it sets both how long the render takes and
/// how much of the budget above the result occupies. A host may lower it on phones.
pub const DEFAULT_MIDI_SAMPLE_RATE: u32 = super::midi::DEFAULT_SAMPLE_RATE;

/// The most of the music budget a single rendered MIDI track may claim, as a divisor: a score
/// longer than this is truncated rather than allowed to evict the entire rest of the soundtrack
/// (or outright fail to allocate) for one song. Half a budget is already a ~4.5 minute track on a
/// phone at the reduced rate, which is longer than any GM8 MIDI loop in practice.
pub const MIDI_BUDGET_SHARE_DIVISOR: usize = 2;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum AudioCacheKey {
    EmbeddedSound { sound_id: i32 },
    ExternalFile { path: String },
    Tracker { path: String },
}

impl AudioCacheKey {
    pub fn label(&self) -> String {
        match self {
            Self::EmbeddedSound { sound_id } => format!("sound:{sound_id}"),
            Self::ExternalFile { path } => format!("file:{path}"),
            Self::Tracker { path } => format!("tracker:{path}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioKind {
    Sfx,
    Music,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheEventType {
    Decode,
    Evict,
    Pin,
    Unpin,
}

#[derive(Clone, Debug)]
pub struct CacheEvent {
    pub kind: CacheEventType,
    pub id: String,
    pub at_ms: f64,
}

#[derive(Clone, Debug, Default)]
pub struct AudioCacheStats {
    pub lazy: bool,
    pub decoded_music_count: u32,
    pub decoded_music_bytes: u64,
    pub decoded_sfx_count: u32,
    pub compressed_bytes: u64,
    pub pinned_count: u32,
    pub last_event: Option<CacheEvent>,
}

#[derive(Clone)]
pub struct AudioCachePolicy {
    pub short_compressed_bytes: usize,
    pub short_duration_secs: f64,
    pub music_budget_bytes: usize,
    pub midi_sample_rate: u32,
}

impl Default for AudioCachePolicy {
    fn default() -> Self {
        Self {
            short_compressed_bytes: DEFAULT_SHORT_COMPRESSED_BYTES,
            short_duration_secs: DEFAULT_SHORT_DURATION_SECS,
            music_budget_bytes: DEFAULT_MUSIC_BUDGET_BYTES,
            midi_sample_rate: DEFAULT_MIDI_SAMPLE_RATE,
        }
    }
}

struct CacheEntry {
    buffer: Option<Rc<AudioBuffer>>,
    compressed: Option<Box<[u8]>>,
    kind: AudioKind,
    format: SoundFormat,
    approx_bytes: usize,
    pinned: u32,
    /// True once we've decided this is SFX for good (never LRU-evicted).
    permanent: bool,
    /// Set by `register_streamed`: this file is never held permanently, whatever the mode says.
    /// Sticky across `store_decoded`, which otherwise re-decides permanence on every decode.
    streamed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoundFormat {
    Mp3,
    Wav,
    Ogg,
    /// Not a codec: a MIDI score, rendered through a SoundFont rather than decoded. See
    /// `super::midi`, and `AudioManager::add_midi` for why one never registers as SFX.
    Midi,
}

pub struct AudioBufferCache {
    lazy: bool,
    policy: AudioCachePolicy,
    entries: HashMap<AudioCacheKey, CacheEntry>,
    /// Music keys in LRU order (front = least recently used).
    music_lru: VecDeque<AudioCacheKey>,
    last_event: Option<CacheEvent>,
}

impl AudioBufferCache {
    pub fn new(lazy: bool, policy: AudioCachePolicy) -> Self {
        Self { lazy, policy, entries: HashMap::new(), music_lru: VecDeque::new(), last_event: None }
    }

    pub fn set_mode(&mut self, lazy: bool, policy: AudioCachePolicy) {
        self.lazy = lazy;
        self.policy = policy;
    }

    pub fn lazy(&self) -> bool {
        self.lazy
    }

    pub fn policy(&self) -> &AudioCachePolicy {
        &self.policy
    }

    fn now_ms() -> f64 {
        js_sys::Date::now()
    }

    fn record(&mut self, kind: CacheEventType, id: String) {
        self.last_event = Some(CacheEvent { kind, id, at_ms: Self::now_ms() });
    }

    pub fn classify_compressed(&self, compressed_len: usize) -> AudioKind {
        if compressed_len <= self.policy.short_compressed_bytes { AudioKind::Sfx } else { AudioKind::Music }
    }

    /// Register compressed bytes. In eager mode (or for SFX) the caller should decode immediately
    /// via `put_decoded`. In lazy mode for music, bytes sit here until first play.
    pub fn register(&mut self, key: AudioCacheKey, compressed: Box<[u8]>, format: SoundFormat) -> AudioKind {
        let kind = self.classify_compressed(compressed.len());
        let permanent = kind == AudioKind::Sfx || !self.lazy;
        self.entries.insert(key.clone(), CacheEntry {
            buffer: None,
            compressed: Some(compressed),
            kind,
            format,
            approx_bytes: 0,
            pinned: 0,
            permanent,
            streamed: false,
        });
        kind
    }

    /// Registers compressed bytes for a file whose real DLL *streams* it from disk rather than
    /// loading it: nothing is decoded now, and the decoded PCM stays LRU-evictable afterwards
    /// even in eager mode.
    ///
    /// Both halves of that matter, and only for this class of file. `MaizeMusic.dll` is BASS, and
    /// a game built on it loads its whole soundtrack up front and holds every handle for the rest
    /// of the session -- Irkara Needle's startup loop `mm_music_load`s 46 OGG files, 55 MB on
    /// disk and some 2.5 GB once decoded to `f32` PCM, which is past the wasm heap's own 2 GB
    /// ceiling several times over. On real Windows that costs nothing because a BASS stream holds
    /// no decoded audio at all; here the equivalent is to keep the bytes, decode on first play,
    /// and let the music budget decide how many of those decodes stay resident. A plain
    /// `register` would instead decode all 46 at load (eager mode) and mark every one permanent,
    /// i.e. exempt from the very budget that exists to prevent this.
    pub fn register_streamed(&mut self, key: AudioCacheKey, compressed: Box<[u8]>, format: SoundFormat) {
        self.entries.insert(key, CacheEntry {
            buffer: None,
            compressed: Some(compressed),
            kind: AudioKind::Music,
            format,
            approx_bytes: 0,
            pinned: 0,
            permanent: false,
            streamed: true,
        });
    }

    pub fn format(&self, key: &AudioCacheKey) -> Option<SoundFormat> {
        self.entries.get(key).map(|e| e.format)
    }

    pub fn compressed(&self, key: &AudioCacheKey) -> Option<&[u8]> {
        self.entries.get(key).and_then(|e| e.compressed.as_deref())
    }

    pub fn take_compressed(&mut self, key: &AudioCacheKey) -> Option<Box<[u8]>> {
        self.entries.get_mut(key).and_then(|e| e.compressed.take())
    }

    pub fn restore_compressed(&mut self, key: &AudioCacheKey, bytes: Box<[u8]>) {
        if let Some(e) = self.entries.get_mut(key) {
            e.compressed = Some(bytes);
        }
    }

    pub fn get_buffer(&self, key: &AudioCacheKey) -> Option<Rc<AudioBuffer>> {
        self.entries.get(key).and_then(|e| e.buffer.clone())
    }

    pub fn put_decoded(&mut self, key: AudioCacheKey, buffer: Rc<AudioBuffer>, duration_secs: f64) -> Rc<AudioBuffer> {
        self.store_decoded(key, buffer, duration_secs, true)
    }

    /// Insert a decoded buffer. When `keep_compressed` is true and the entry is music under lazy
    /// mode, compressed bytes (if present) are retained so the buffer can be re-decoded after eviction.
    pub fn store_decoded(
        &mut self,
        key: AudioCacheKey,
        buffer: Rc<AudioBuffer>,
        duration_secs: f64,
        keep_compressed: bool,
    ) -> Rc<AudioBuffer> {
        let approx = (buffer.length() as usize).saturating_mul(buffer.number_of_channels() as usize).saturating_mul(4);
        let label = key.label();
        let prev = self.entries.remove(&key);
        let pinned = prev.as_ref().map(|e| e.pinned).unwrap_or(0);
        let format = prev.as_ref().map(|e| e.format).unwrap_or(SoundFormat::Mp3);
        let compressed_len = prev.as_ref().and_then(|e| e.compressed.as_ref()).map(|c| c.len()).unwrap_or(0);
        let streamed = prev.as_ref().map(|e| e.streamed).unwrap_or(false);
        let compressed = if keep_compressed { prev.and_then(|e| e.compressed) } else { None };

        let short_by_size = compressed_len > 0 && compressed_len <= self.policy.short_compressed_bytes;
        let short_by_duration = duration_secs <= self.policy.short_duration_secs;
        let (kind, permanent) = if streamed {
            // `register_streamed`'s promise, which outlives this decode: evictable whatever the
            // mode, and whatever the file's size or duration says.
            (AudioKind::Music, false)
        } else if !self.lazy || short_by_size || short_by_duration {
            (if short_by_size || short_by_duration { AudioKind::Sfx } else { AudioKind::Music }, true)
        } else {
            (AudioKind::Music, false)
        };

        self.music_lru.retain(|k| k != &key);
        if kind == AudioKind::Music && !permanent {
            self.music_lru.push_back(key.clone());
        }

        self.entries.insert(key, CacheEntry {
            buffer: Some(buffer.clone()),
            compressed: if permanent { None } else { compressed },
            kind,
            format,
            approx_bytes: approx,
            pinned,
            permanent,
            streamed,
        });
        self.record(CacheEventType::Decode, label);
        self.evict_until_under_budget();
        buffer
    }

    pub fn touch(&mut self, key: &AudioCacheKey) {
        if let Some(e) = self.entries.get(key) {
            if e.kind == AudioKind::Music && !e.permanent {
                self.music_lru.retain(|k| k != key);
                self.music_lru.push_back(key.clone());
            }
        }
    }

    pub fn pin(&mut self, key: &AudioCacheKey) {
        if let Some(e) = self.entries.get_mut(key) {
            e.pinned = e.pinned.saturating_add(1);
            let id = key.label();
            self.record(CacheEventType::Pin, id);
        }
        self.touch(key);
    }

    pub fn unpin(&mut self, key: &AudioCacheKey) {
        if let Some(e) = self.entries.get_mut(key) {
            e.pinned = e.pinned.saturating_sub(1);
            let id = key.label();
            self.record(CacheEventType::Unpin, id);
        }
        self.evict_until_under_budget();
    }

    pub fn discard_decoded(&mut self, key: &AudioCacheKey) {
        if let Some(e) = self.entries.get_mut(key) {
            if e.pinned > 0 {
                return;
            }
            e.buffer = None;
            e.approx_bytes = 0;
            self.music_lru.retain(|k| k != key);
            self.record(CacheEventType::Evict, key.label());
        }
    }

    fn music_decoded_bytes(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.kind == AudioKind::Music && e.buffer.is_some() && !e.permanent)
            .map(|e| e.approx_bytes)
            .sum()
    }

    pub fn evict_until_under_budget(&mut self) {
        while self.music_decoded_bytes() > self.policy.music_budget_bytes {
            let victim = self
                .music_lru
                .iter()
                .find(|k| {
                    self.entries.get(*k).map(|e| e.pinned == 0 && e.buffer.is_some() && !e.permanent).unwrap_or(false)
                })
                .cloned();
            let Some(key) = victim else { break };
            if let Some(e) = self.entries.get_mut(&key) {
                e.buffer = None;
                e.approx_bytes = 0;
            }
            self.music_lru.retain(|k| k != &key);
            self.record(CacheEventType::Evict, key.label());
        }
    }

    pub fn stats(&self) -> AudioCacheStats {
        let mut decoded_music_count = 0u32;
        let mut decoded_music_bytes = 0u64;
        let mut decoded_sfx_count = 0u32;
        let mut compressed_bytes = 0u64;
        let mut pinned_count = 0u32;
        for e in self.entries.values() {
            compressed_bytes += e.compressed.as_ref().map(|c| c.len() as u64).unwrap_or(0);
            if e.pinned > 0 {
                pinned_count += 1;
            }
            if e.buffer.is_some() {
                // Count by classification (music vs sfx), not permanence -- eager mode marks
                // everything permanent, but long tracks are still music for the stats panel.
                if e.kind == AudioKind::Music {
                    decoded_music_count += 1;
                    decoded_music_bytes += e.approx_bytes as u64;
                } else {
                    decoded_sfx_count += 1;
                }
            }
        }
        AudioCacheStats {
            lazy: self.lazy,
            decoded_music_count,
            decoded_music_bytes,
            decoded_sfx_count,
            compressed_bytes,
            pinned_count,
            last_event: self.last_event.clone(),
        }
    }

    pub fn remove(&mut self, key: &AudioCacheKey) {
        self.entries.remove(key);
        self.music_lru.retain(|k| k != key);
    }
}
