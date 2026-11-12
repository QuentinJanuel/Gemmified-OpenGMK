//! The browser build's game object. `gems.rs` drives `WasmGame` one frame per `tick`, in place of
//! the native CLI's `main.rs` -> `Game::run()` (a blocking loop with an `Instant`/sleep-based frame limiter,
//! neither of which exist on wasm32).

use crate::game::{Game, PlayType, SceneChange};
use wasm_bindgen::prelude::*;

/// Per-call delta is clamped to this many milliseconds, so a backgrounded/throttled tab doesn't
/// try to "catch up" by simulating a huge number of frames in one `step()` call when it regains
/// focus (a classic fixed-timestep spiral-of-death guard).
const MAX_DELTA_MS: f64 = 250.0;

#[wasm_bindgen]
pub struct WasmGame {
    game: Game,
    /// Accumulated, not-yet-simulated time, in nanoseconds (matches `Game::room.speed`'s units).
    acc_ns: f64,
    last_ms: Option<f64>,
    /// Real-time start of the second currently being counted for the GML `fps` variable. See
    /// `step`'s frame-counter block; `None` until the first `step`.
    fps_window_start_ms: Option<f64>,
    /// Whether `Game::init` has run yet. It is deliberately *not* run by the constructor -- see
    /// the block at the top of `step` for why.
    initialized: bool,
}

/// One input event from the host, in the engine contract's `GameInput` shape: a `kind` plus
/// whatever that kind carries (GEMS spec §9). Pointer positions are in game
/// pixels, already mapped out of the letterbox by the host, and auto-repeats have already been
/// dropped -- see `window::input`.
///
/// **A free function, not a method.** It touches no game state at all -- it queues, and the next
/// frame reads the queue -- so it never needs a borrow of a `WasmGame`.
///
/// Silently does nothing for an event this engine has no meaning for, beyond one console warning:
/// a host is entitled to forward every key the player presses, and most of them are not GameMaker
/// keys.
#[wasm_bindgen]
pub fn push_input(input: &JsValue) {
    if !crate::window::input::push_from_host(input) {
        web_sys::console::warn_1(&"push_input: unrecognised input event, ignored".into());
    }
}

#[wasm_bindgen]
impl WasmGame {
    /// Parses `bytes` as a GM8 `.exe`/`.gm81` and launches it. `bytes` is consumed entirely up
    /// front (gm8exe::reader::from_exe only needs the raw bytes, not a filesystem path) and is not
    /// retained afterward.
    ///
    /// `extra_files` is a JS array of `{name, data}` objects (`name` a relative path string,
    /// `data` a `Uint8Array`) -- loose files from a folder upload (e.g. a game's `BGM/` folder, or
    /// an external DLL a game only `file_exists()`-checks for). Visible to both the general GML
    /// file API (`gml::file`'s `file_exists`/`file_bin_open`/etc.) and `ssound.dll`'s `SS_LoadSound`
    /// reimplementation (`game/external/ssound.rs`), which share one registry. Pass an empty array
    /// for the common case of a single self-contained .exe.
    ///
    /// `launch_args` is the command line after the program (`parameter_string(1..)`), and `ports`
    /// the loopback ports the host answers -- see `crate::loopback` and `drain_loopback` below.
    ///
    /// `canvas` is the surface to render into, handed in rather than looked up: the engine used to
    /// find a `<canvas>` by a hardcoded id, which only a main-thread host with a document could
    /// satisfy. An `OffscreenCanvas` is what both a worker and a transferred placeholder element
    /// present, so this one argument is what lets the same build render in either.
    ///
    /// Input does not appear here at all: the host captures it and pushes it in through the
    /// `push_input` function below, which is why this engine registers no DOM listener and needs no
    /// element.
    #[wasm_bindgen(constructor)]
    pub fn new(
        canvas: web_sys::OffscreenCanvas,
        mut bytes: Vec<u8>,
        extra_files: js_sys::Array,
        launch_args: Vec<String>,
        ports: Vec<u16>,
    ) -> Result<WasmGame, JsValue> {
        console_error_panic_hook::set_once();

        // Drop any open-file buffers from a previously-loaded game. Scoping saves to *this* game,
        // and seeding the bundle from `extra_files`, both happen before this constructor is reached
        // (`gems.rs` starts the `Vfs` on `init` and fills the bundle on `start`) -- this only owns
        // the session's open handles. `extra_files` is accepted and ignored so the constructor
        // signature stays stable for callers.
        let _ = &extra_files;
        crate::gml::file::reset_open_files();
        // Input the host pushed before this game existed -- a key released while the previous one
        // was being torn down -- belongs to nobody.
        crate::window::input::clear_pushed();
        // A password is per game, not per page: without this, a second game loaded into the same
        // page would try to decrypt its audio with the first game's key.
        crate::game::external::fmod::reset_password();
        crate::game::external::gm82buf::reset();
        crate::game::external::httpdll::reset();
        crate::game::external::gm82dx8::reset();
        crate::game::external::gm82dx9::reset();

        // Some GM8 games (this one included) write a "temp" scratch file before game_restart() to
        // smuggle state past the global-variable wipe that causes, then delete it once resumed --
        // relying on a graceful in-game quit (Escape/Alt+F4 key handler) to clean up any leftover
        // marker from an aborted run. A browser tab has no equivalent of that: a refresh or close
        // runs no GML at all, so the file would otherwise persist in localStorage forever and make
        // every future load skip straight back into the last save, never showing the title screen.
        // Deleting it here -- which runs exactly once per real page load, never on the in-session
        // restart-and-resume path itself (that's `Game::restart()`, a separate call) -- treats each
        // fresh load as what it actually is: a new process start, the only time this file should be
        // considered stale.
        crate::gml::file::delete("temp").ok();

        // `multithread: false` -- gm8exe's parallel asset-decompression path uses `rayon`, which
        // needs real OS threads unavailable on wasm32-unknown-unknown without additional Worker +
        // cross-origin-isolation plumbing.
        let assets = gm8exe::reader::from_exe(&mut bytes, None::<fn(&str)>, false, false)
            .map_err(|e| JsValue::from_str(&format!("failed to parse game file: {}", e)))?;

        // `program_directory` -- which `working_directory` also reports -- is the shared VFS's
        // `game` mount root, NOT an empty string.
        //
        // An empty string would cause a whole bug family. GML builds paths by
        // concatenation (`working_directory + "\Save\save1"`), so an empty root produced
        // `"\Save\save1"`: rooted, backslashed, and meaningless on a unix-semantics wasm target,
        // leaving every layer downstream to guess what it referred to -- and they guessed
        // differently. With a real root the same idiom yields `"/game/Save/save1"`, which the VFS
        // parses unambiguously into a mount plus segments.
        //
        // What is passed here is the path of the game *executable*, not the directory: `Game::launch`
        // derives `program_directory` from it by popping the last component. Handing it the bare
        // root popped the mount name off and left `working_directory` reporting `"/"`, so every
        // path a game built from it came out one level too high. Saves still landed correctly --
        // `parsePath` treats an unrecognised root as game-relative -- but anything the game showed
        // or re-parsed was wrong: Not Another Magic Tower Game prompts with
        // `working_directory + "\BackUps"` and that read `"/\BackUps"`.
        // Parked for `Game::launch`, which reaches the window builder several layers down a chain
        // shared with native and so cannot be passed a canvas directly. See `hand_in_surface`.
        crate::window::connection::hand_in_surface(canvas);

        let exe = format!("{}/game.exe", crate::gml::vfs::GAME_ROOT);
        let mut game = Game::launch(
            assets,
            std::path::PathBuf::from(&exe),
            // `parameter_string(0)` is the program itself, as on a real command line.
            std::iter::once(exe.clone()).chain(launch_args).collect(),
            None,
            // Matches main.rs's native default -- most GM8 games (especially Japanese ones) store
            // embedded strings as Shift-JIS regardless of OS locale; this was mistakenly UTF_8.
            encoding_rs::SHIFT_JIS,
            false, // frame_limiter: our own step() accumulator paces frames instead
            0,
            false, // capture_recording: no subprocess spawning exists in a browser
            PlayType::Normal,
        )
        .map_err(|e| JsValue::from_str(&format!("failed to launch game: {}", e)))?;
        game.loopback = crate::loopback::Loopback::new(ports);

        // `Game::launch` leaves the clock spoofed at `SpoofedNanos(0)`, and native's main.rs (never
        // run on wasm) is what normally swaps in a real one. Left alone, every date the game can
        // read is 1970-01-01 plus however long the run has lasted: `current_hour` is 0, the date
        // variables are the epoch, and `date_current_datetime` is 25569 days off. Games do read
        // these -- I Wanna Kill the Kamilia 3's prism rooms hang a working wall clock on
        // `current_hour`/`current_minute`, and it sat frozen at midnight.
        //
        // Native uses `StartupEpoch(Instant)` here, which wasm can't (`Instant::now()` panics), so
        // `Realtime` reads the browser's clock on each access instead. See `gml/datetime.rs`.
        game.clock = crate::game::GameClock::Realtime;

        // NOTE: `init()` is deliberately not called here -- `step` runs it on its first call. See
        // the `initialized` block there.
        Ok(WasmGame { game, acc_ns: 0.0, last_ms: None, fps_window_start_ms: None, initialized: false })
    }

    /// Advances the game by however many logic frames have elapsed since the last call, given the
    /// current `now_ms` timestamp (as provided by `requestAnimationFrame` -- i.e. `DOMHighResTimeStamp`,
    /// milliseconds since the page navigation started). Returns `false` once the game has ended
    /// (or hit a runtime error); the caller should stop scheduling further calls when that
    /// happens.
    pub fn step(&mut self, now_ms: f64) -> bool {
        // `Game::launch` only sets up a placeholder room; the actual first room (with its
        // instances) is loaded by `init()` -- library init strings, extension initializers, then
        // `load_room()` for real. Native `Game::run()`/`replay()` both do `self.init()?;
        // handle_scene_change!(self);` before their loop starts.
        //
        // Run here rather than in the constructor because all three stages run GML, and GML can
        // open a dialog ("I Wanna Break The Series Z 3 EZ" does from its first room's create
        // event); a frame is where the host expects the engine to block on one.
        if !self.initialized {
            self.initialized = true;
            if let Err(e) = self.game.init() {
                log_error(&format!("failed to initialize game: {}", e));
                return false
            }
            if !self.handle_scene_change() {
                return false
            }
        }

        let last_ms = self.last_ms.unwrap_or(now_ms);
        self.last_ms = Some(now_ms);
        let delta_ms = (now_ms - last_ms).clamp(0.0, MAX_DELTA_MS);
        self.acc_ns += delta_ms * 1_000_000.0;

        // How long this call may spend simulating before it gives up on the backlog. See the loop
        // below for why an unbounded catch-up is worse than no catch-up at all. Taken from the room
        // speed on entry: a room change mid-loop moves the step length, but the budget is a
        // wall-clock allowance and only needs to be about one frame's worth.
        let deadline_ms =
            crate::gml::datetime::monotonic_micros() / 1000.0 + 1000.0 / f64::from(self.game.room.speed.max(1));

        loop {
            let step_ns = 1_000_000_000.0 / self.game.room.speed.max(1) as f64;
            if self.acc_ns < step_ns {
                break true
            }
            self.acc_ns -= step_ns;

            // The GML `fps` variable: simulated frames completed in the last real second. Native's
            // `run()` loop maintains this; nothing on wasm did, so `Game::fps` sat at its initial 0
            // for the entire run and every game that reads `fps` saw zero forever.
            //
            // Not cosmetic. Games use `fps` to compensate for lag, and dividing by it or scaling
            // by it turns a stuck 0 into a stuck 0: one real game rate-matches its music to the
            // frame rate every step (`FMODInstanceSetPitch(bgm, fps*(1/50))`), which pinned the
            // playback rate at 0 -- music correctly loaded, correctly playing, and completely
            // inaudible, with nothing in the audio path at fault.
            //
            // Counted per *simulated* frame and published once per *real* second, exactly like
            // native. GM8 discards whatever time is left over past the second rather than carrying
            // it forward, so the window restarts at `now_ms` instead of advancing by exactly 1000ms
            // -- native does the same (`time_last = time_now`), and matching it keeps a laggy frame
            // from making the next second's count read high to compensate.
            let window_start = *self.fps_window_start_ms.get_or_insert(now_ms);
            if now_ms - window_start >= 1000.0 {
                self.fps_window_start_ms = Some(now_ms);
                self.game.fps = self.game.frame_counter;
                self.game.frame_counter = 0;
            }
            self.game.frame_counter += 1;

            self.game.process_window_events();
            if let Err(e) = self.game.frame() {
                log_error(&format!("runtime error: {}", e));
                break false
            }
            if !self.handle_scene_change() {
                break false
            }
            if self.game.close_requested {
                if let Err(e) = self.game.run_game_end_events() {
                    log_error(&format!("runtime error while ending game: {}", e));
                }
                break false
            }

            // Catch-up has to be bounded by *wall time already spent here*, not just by how much
            // game time is owed, or a game that can't hit its room speed collapses instead of
            // merely running slow.
            //
            // Say a frame costs 40ms at room_speed 50 (a 20ms budget). Each callback is then
            // scheduled ~40ms after the last, hands the accumulator 40ms, and so simulates two
            // frames -- taking 80ms, which buys four frames next time, then eight. It settles at
            // `MAX_DELTA_MS` worth (12 frames, ~480ms) per callback: the game still simulates ~25
            // frames a second, but the browser only composites once per callback, so the player
            // sees two frames a second and their input is sampled twice a second. The engine
            // spends 12x the work to present 1/12th as often, and every dropped frame makes the
            // next one worse.
            //
            // Native does not do this. `Game::run`'s limiter runs exactly one frame per iteration
            // and sleeps out the remainder; when a frame overruns it simply starts the next one
            // late, so the game runs *slower* and `fps` falls below `room_speed`. That is also what
            // GM8 itself does, and what this game's players are used to seeing when it lags.
            //
            // So: keep the accumulator (a room speed above the display's refresh rate genuinely
            // needs more than one frame per callback), but the moment this call has already spent a
            // frame's worth of real time, drop whatever is still owed rather than paying it back.
            if crate::gml::datetime::monotonic_micros() / 1000.0 >= deadline_ms {
                self.acc_ns = 0.0;
                break true
            }
        }
    }

    /// Applies a queued scene change (room load / restart / end / load-savestate), mirroring
    /// game.rs's private `handle_scene_change!` macro (not reusable across module boundaries).
    /// Returns `false` if the game should stop running (`SceneChange::End`, or an error).
    fn handle_scene_change(&mut self) -> bool {
        match self.game.scene_change.take() {
            Some(SceneChange::Room(id)) => {
                if let Err(e) = self.game.load_room(id) {
                    log_error(&format!("runtime error while loading room: {}", e));
                    return false
                }
            },
            Some(SceneChange::Restart) => {
                if let Err(e) = self.game.restart() {
                    log_error(&format!("runtime error while restarting: {}", e));
                    return false
                }
            },
            Some(SceneChange::End) => {
                if let Err(e) = self.game.run_game_end_events() {
                    log_error(&format!("runtime error while ending game: {}", e));
                }
                return false
            },
            Some(SceneChange::Load(path)) => {
                if let Err(e) = self.game.load_gm_save(path) {
                    log_error(&format!("runtime error while loading save: {}", e));
                    return false
                }
            },
            None => (),
        }
        true
    }

    /// Lists every file (save data, screenshot thumbnails, config, etc.) persisted to
    /// localStorage for *this* game -- backs the web harness's file-management panel below the
    /// canvas, since a shared browser origin otherwise accumulates saves from every game ever
    /// loaded with no way from JS alone to tell which belong to which. Returns a JS array of
    /// `{name, size}` objects (`size` in bytes).
    pub fn list_files(&self) -> js_sys::Array {
        let arr = js_sys::Array::new();
        for (path, size) in crate::gml::file::list_store_files() {
            let obj = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("path"), &JsValue::from_str(&path));
            let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("size"), &JsValue::from_f64(size as f64));
            arr.push(&obj);
        }
        arr
    }

    /// Bytes the host sent on loopback connection `id`.
    pub fn loopback_push(&mut self, id: u32, bytes: &[u8]) {
        self.game.loopback.push(id, bytes);
    }

    /// The host closed loopback connection `id`.
    pub fn loopback_close(&mut self, id: u32) {
        self.game.loopback.closed_by_host(id);
    }

    /// Deletes one persisted file by the full path `list_files` reported. Goes through the same
    /// path real GML `file_delete` does, so this drops the session's open-file buffer as well as
    /// the persisted copy -- otherwise a still-open handle would resurrect it on its own `Drop`.
    pub fn delete_file(&self, path: &str) {
        crate::gml::file::delete_store_file(path);
    }

    /// Whether this game embeds MIDI music and so needs a SoundFont bank to play it.
    ///
    /// Asked after construction rather than bundled with the engine: the bank is tens of megabytes
    /// and the overwhelming majority of games have no MIDI in them at all, so the host fetches it
    /// only when this says yes. Until `install_soundfont` lands, MIDI sounds are registered and
    /// silent, so the game runs either way and nothing has to block on the fetch.
    pub fn needs_soundfont(&self) -> bool {
        self.game.audio.needs_soundfont()
    }

    /// Installs the fetched `.sf2`. `false` means the bytes were not a usable bank, which leaves
    /// MIDI silent rather than failing the game.
    pub fn install_soundfont(&mut self, bytes: &[u8]) -> bool {
        self.game.audio.set_soundfont(bytes)
    }

    pub fn set_audio_cache_mode(
        &mut self,
        lazy: bool,
        short_compressed_bytes: f64,
        short_duration_secs: f64,
        music_budget_bytes: f64,
        midi_sample_rate: f64,
    ) {
        use crate::game::audio::AudioCachePolicy;
        self.game.audio.set_audio_cache_mode(lazy, AudioCachePolicy {
            short_compressed_bytes: short_compressed_bytes as usize,
            short_duration_secs,
            music_budget_bytes: music_budget_bytes as usize,
            midi_sample_rate: midi_sample_rate as u32,
        });
    }

    /// Live audio-cache stats, for a host's debug display.
    pub fn get_audio_cache_stats(&self) -> JsValue {
        use crate::game::audio::CacheEventType;
        let s = self.game.audio.audio_cache_stats();
        let obj = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&obj, &"lazy".into(), &JsValue::from_bool(s.lazy));
        let _ =
            js_sys::Reflect::set(&obj, &"decodedMusicCount".into(), &JsValue::from_f64(s.decoded_music_count as f64));
        let _ =
            js_sys::Reflect::set(&obj, &"decodedMusicBytes".into(), &JsValue::from_f64(s.decoded_music_bytes as f64));
        let _ = js_sys::Reflect::set(&obj, &"decodedSfxCount".into(), &JsValue::from_f64(s.decoded_sfx_count as f64));
        let _ = js_sys::Reflect::set(&obj, &"compressedBytes".into(), &JsValue::from_f64(s.compressed_bytes as f64));
        let _ = js_sys::Reflect::set(&obj, &"pinnedCount".into(), &JsValue::from_f64(s.pinned_count as f64));
        let last = match &s.last_event {
            Some(ev) => {
                let e = js_sys::Object::new();
                let ty = match ev.kind {
                    CacheEventType::Decode => "decode",
                    CacheEventType::Evict => "evict",
                    CacheEventType::Pin => "pin",
                    CacheEventType::Unpin => "unpin",
                };
                let _ = js_sys::Reflect::set(&e, &"type".into(), &JsValue::from_str(ty));
                let _ = js_sys::Reflect::set(&e, &"id".into(), &JsValue::from_str(&ev.id));
                let _ = js_sys::Reflect::set(&e, &"at".into(), &JsValue::from_f64(ev.at_ms));
                JsValue::from(e)
            },
            None => JsValue::NULL,
        };
        let _ = js_sys::Reflect::set(&obj, &"lastEvent".into(), &last);
        obj.into()
    }
}

impl WasmGame {
    /// Everything the game did on loopback since the last call, in order. Called between `step()`s,
    /// like the two above.
    pub(crate) fn drain_loopback(&mut self) -> Vec<crate::loopback::Event> {
        self.game.loopback.drain_events()
    }
}

fn log_error(message: &str) {
    web_sys::console::error_1(&message.into());
}
