//! This engine's half of the plugin's internal wire (GEMS spec Annex A), in its worker.
//!
//! `host/worker.js` loads this module and hands every message to `on_message`; the host realm's
//! half is the `opengmk-host` crate. Requests are `init`, `start`, `tick` and `stats`; everything
//! the game tells the host goes out as a note. A blocking GML dialog blocks this thread on the
//! shared dialog buffer (A.5) until the player answers.

use crate::{gml::vfs, loopback::Event, wasm_entry::WasmGame};
use indexmap::IndexMap;
use js_sys::{Array, Int32Array, Object, Reflect, SharedArrayBuffer, Uint8Array};
use std::cell::{Cell, RefCell};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;

const ENGINE_ID: &str = "opengmk";
const API_VERSION: f64 = 5.0;
const SOUNDFONT: &str = "GeneralUser-GS.sf2";

#[derive(Default)]
struct Connection {
    /// Host -> game bytes since the last frame.
    inbox: Vec<u8>,
    closed: bool,
}

#[derive(Default)]
struct Session {
    initialized: bool,
    ended: bool,
    canvas: Option<web_sys::OffscreenCanvas>,
    asset_base: String,
    launch_args: Vec<String>,
    ports: Vec<u16>,
    settings: Option<Object>,
    dialog_buffer: Option<SharedArrayBuffer>,
    /// Input since the last frame, applied at the start of the next one.
    input: Vec<JsValue>,
    /// The game's loopback connections, by the game's own ids.
    connections: IndexMap<u32, Connection>,
    /// The size last reported to the host.
    size: (u32, u32),
}

thread_local! {
    // Never borrowed across a call into the game, which reaches back here mid-frame.
    static SESSION: RefCell<Session> = RefCell::new(Session::default());
    static GAME: RefCell<Option<WasmGame>> = const { RefCell::new(None) };
    static FULLSCREEN: Cell<bool> = const { Cell::new(false) };
    /// After `unload`: nothing more goes out or comes in.
    static CLOSED: Cell<bool> = const { Cell::new(false) };
}

type Failure = (&'static str, String);

fn get(object: &JsValue, key: &str) -> JsValue {
    Reflect::get(object, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

fn object(fields: &[(&str, JsValue)]) -> JsValue {
    let object = Object::new();
    for (key, value) in fields {
        let _ = Reflect::set(&object, &JsValue::from_str(key), value);
    }
    object.into()
}

/// `String(value)`, for an error's detail.
fn describe(value: &JsValue) -> String {
    match value.dyn_ref::<js_sys::Error>() {
        Some(error) => String::from(error.to_string()),
        None => value.as_string().unwrap_or_else(|| format!("{value:?}")),
    }
}

fn post(message: &JsValue, transfer: Option<&Array>) {
    let scope: web_sys::DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let _ = match transfer {
        Some(transfer) => scope.post_message_with_transfer(message, transfer),
        None => scope.post_message(message),
    };
}

/// A note to the host realm's half: `{w: "note", body: {kind, ...fields}}`.
fn notify(kind: &str, fields: &[(&str, JsValue)], transfer: Option<&Array>) {
    if CLOSED.with(Cell::get) {
        return
    }
    let body = Object::new();
    let _ = Reflect::set(&body, &"kind".into(), &kind.into());
    for (key, value) in fields {
        let _ = Reflect::set(&body, &JsValue::from_str(key), value);
    }
    post(&object(&[("w", "note".into()), ("body", body.into())]), transfer);
}

fn reply(id: &JsValue, result: Result<JsValue, Failure>) {
    post(
        &match result {
            Ok(body) => object(&[("w", "ok".into()), ("id", id.clone()), ("body", body)]),
            Err((code, detail)) => object(&[
                ("w", "err".into()),
                ("id", id.clone()),
                ("error", object(&[("engineError", true.into()), ("code", code.into()), ("detail", detail.into())])),
            ]),
        },
        None,
    );
}

/// Every message from the host realm's half. Anything that is not an envelope is ignored.
#[wasm_bindgen(js_name = onMessage)]
pub fn on_message(data: JsValue) {
    if CLOSED.with(Cell::get) {
        return
    }
    let body = get(&data, "body");
    match get(&data, "w").as_string().as_deref() {
        Some("note") => note(&body),
        Some("req") => {
            let id = get(&data, "id");
            match get(&body, "kind").as_string().as_deref() {
                Some("init") => reply(&id, init(&get(&body, "init"))),
                // Answers itself, once the SoundFont is in or has failed.
                Some("start") => start(id, &body),
                Some("tick") => reply(&id, tick(get(&body, "now").as_f64().unwrap_or(0.0))),
                Some("stats") => reply(
                    &id,
                    Ok(GAME
                        .with(|game| game.borrow().as_ref().map(WasmGame::get_audio_cache_stats))
                        .unwrap_or_else(|| Object::new().into())),
                ),
                _ => reply(&id, Ok(JsValue::UNDEFINED)),
            }
        },
        _ => (),
    }
}

fn init(init: &JsValue) -> Result<JsValue, Failure> {
    if SESSION.with(|s| s.borrow().initialized) {
        return Err(("internal", "init arrived twice".into()))
    }
    let api = get(init, "apiVersion");
    if api.as_f64() != Some(API_VERSION) {
        let theirs = api.as_f64().map(|n| n.to_string()).unwrap_or_else(|| describe(&api));
        return Err((
            "unsupported-environment",
            format!("host implements contract {theirs}, this engine implements {API_VERSION}"),
        ))
    }

    let seed = Object::entries(&get(init, "storeSeed").unchecked_into())
        .iter()
        .filter_map(|entry| {
            let entry: Array = entry.unchecked_into();
            Some((entry.get(0).as_string()?, entry.get(1).as_string()?))
        })
        .collect();
    vfs::start(get(init, "gameId").as_string().unwrap_or_default(), seed, store_changed, saved);

    SESSION.with(|s| {
        let mut s = s.borrow_mut();
        s.initialized = true;
        s.canvas = get(init, "canvas").dyn_into().ok();
        s.asset_base = get(init, "assetBase").as_string().unwrap_or_default();
        s.launch_args = Array::from(&get(init, "launchArgs")).iter().filter_map(|a| a.as_string()).collect();
        s.ports = Array::from(&get(init, "ports")).iter().filter_map(|p| p.as_f64()).map(|p| p as u16).collect();
        s.settings = Some(Object::assign(&Object::new(), &get(init, "settings").unchecked_into()));
        s.dialog_buffer = get(init, "dialogBuffer").dyn_into().ok();
    });
    Ok(object(&[("engineId", ENGINE_ID.into())]))
}

fn start(id: JsValue, body: &JsValue) {
    match load(&get(body, "files")) {
        Ok(true) => wasm_bindgen_futures::spawn_local(async move {
            // MIDI needs a ~32 MiB SoundFont bank, so it is fetched only for a game that has MIDI.
            // A missing bank costs the music, not the game.
            if let Err(error) = install_soundfont().await {
                web_sys::console::warn_1(&format!("opengmk: no SoundFont, MIDI will be silent: {error}").into());
            }
            reply(&id, Ok(started()));
        }),
        Ok(false) => reply(&id, Ok(started())),
        Err(failure) => reply(&id, Err(failure)),
    }
}

/// Fills the bundle and builds the game. `true` when the game wants the SoundFont.
fn load(files: &JsValue) -> Result<bool, Failure> {
    let files = Array::from(files);
    let path_of = |file: &JsValue| get(file, "path").as_string().unwrap_or_default();
    let is_exe = |path: &str| {
        let path = path.to_ascii_lowercase();
        path.ends_with(".exe") || path.ends_with(".gm81") || path.ends_with(".gmk")
    };
    let entry = files
        .iter()
        .position(|file| is_exe(&path_of(&file)))
        .ok_or(("unsupported-game", "no .exe, .gm81 or .gmk".to_owned()))?;

    // The game's loose files (music folders, DLLs) are relative to the executable's directory.
    let entry_path = path_of(&files.get(entry as u32));
    let dir = &entry_path[..entry_path.rfind('/').map_or(0, |at| at + 1)];
    for (index, file) in files.iter().enumerate() {
        if index == entry {
            continue
        }
        let path = path_of(&file);
        let path = path.strip_prefix(dir).unwrap_or(&path);
        vfs::add_bundle_js(&format!("{}/{path}", vfs::GAME_ROOT), Uint8Array::new(&get(&file, "bytes")));
    }

    let bytes = Uint8Array::new(&get(&files.get(entry as u32), "bytes")).to_vec();
    let (canvas, launch_args, ports) = SESSION.with(|s| {
        let s = s.borrow();
        (s.canvas.clone(), s.launch_args.clone(), s.ports.clone())
    });
    let canvas = canvas.ok_or(("internal", "the host has not sent init yet".to_owned()))?;
    let game = WasmGame::new(canvas, bytes, Array::new(), launch_args, ports)
        .map_err(|error| ("corrupt-game", describe(&error)))?;
    let needs_soundfont = game.needs_soundfont();
    GAME.with(|cell| *cell.borrow_mut() = Some(game));
    Ok(needs_soundfont)
}

async fn install_soundfont() -> Result<(), String> {
    let url = format!("{}/{SOUNDFONT}", SESSION.with(|s| s.borrow().asset_base.clone()));
    let scope: web_sys::WorkerGlobalScope = js_sys::global().unchecked_into();
    let response: web_sys::Response =
        JsFuture::from(scope.fetch_with_str(&url)).await.map_err(|e| describe(&e))?.unchecked_into();
    if !response.ok() {
        return Err(format!("Error: HTTP {}", response.status()))
    }
    let buffer = JsFuture::from(response.array_buffer().map_err(|e| describe(&e))?).await.map_err(|e| describe(&e))?;
    let bytes = Uint8Array::new(&buffer).to_vec();
    match GAME.with(|game| game.borrow_mut().as_mut().map(|game| game.install_soundfont(&bytes))) {
        Some(false) => Err("Error: not a usable SoundFont bank".into()),
        _ => Ok(()),
    }
}

fn started() -> JsValue {
    let (width, height) = report_size();
    object(&[("width", width.into()), ("height", height.into())])
}

/// The resolution the game draws at, which the host cannot read off a transferred canvas.
fn report_size() -> (u32, u32) {
    let (now, before) = SESSION.with(|s| {
        let mut s = s.borrow_mut();
        let now = s.canvas.as_ref().map_or((0, 0), |c| (c.width(), c.height()));
        (now, std::mem::replace(&mut s.size, now))
    });
    if now != before {
        notify("resize", &[("width", now.0.into()), ("height", now.1.into())], None);
    }
    now
}

fn tick(now: f64) -> Result<JsValue, Failure> {
    if SESSION.with(|s| s.borrow().ended) {
        return Ok(object(&[("running", false.into())]))
    }
    let input = SESSION.with(|s| std::mem::take(&mut s.borrow_mut().input));
    for input in &input {
        crate::wasm_entry::push_input(input);
    }
    let running = GAME
        .with(|game| game.borrow_mut().as_mut().map(|game| game.step(now)))
        .ok_or(("internal", "the game has not started".to_owned()))?;
    relay_loopback();
    report_size();
    if !running {
        SESSION.with(|s| s.borrow_mut().ended = true);
    }
    Ok(object(&[("running", running.into())]))
}

/// Between frames: what the game did on loopback goes out, what the host sent goes in.
fn relay_loopback() {
    let events = GAME.with(|game| game.borrow_mut().as_mut().map(WasmGame::drain_loopback).unwrap_or_default());
    for event in events {
        match event {
            // The game's own `Loopback` only opens one of the host's ports.
            Event::Open { id, port } => {
                SESSION.with(|s| s.borrow_mut().connections.insert(id, Connection::default()));
                notify("net-open", &[("id", id.into()), ("port", port.into())], None);
            },
            Event::Send { id, bytes } => {
                let open = SESSION.with(|s| s.borrow().connections.get(&id).map_or(false, |c| !c.closed));
                if open && !bytes.is_empty() {
                    let buffer = Uint8Array::from(bytes.as_slice()).buffer();
                    notify(
                        "net-send",
                        &[("id", id.into()), ("bytes", buffer.clone().into())],
                        Some(&Array::of1(&buffer)),
                    );
                }
            },
            Event::End { id } => {
                if SESSION.with(|s| s.borrow_mut().connections.shift_remove(&id)).map_or(false, |c| !c.closed) {
                    notify("net-end", &[("id", id.into())], None);
                }
            },
        }
    }
    let incoming: Vec<(u32, Vec<u8>, bool)> = SESSION.with(|s| {
        let mut s = s.borrow_mut();
        let incoming = s.connections.iter_mut().map(|(id, c)| (*id, std::mem::take(&mut c.inbox), c.closed)).collect();
        s.connections.retain(|_, c| !c.closed);
        incoming
    });
    GAME.with(|game| {
        if let Some(game) = game.borrow_mut().as_mut() {
            for (id, bytes, closed) in incoming {
                if !bytes.is_empty() {
                    game.loopback_push(id, &bytes);
                }
                if closed {
                    game.loopback_close(id);
                }
            }
        }
    });
}

fn note(body: &JsValue) {
    let kind = get(body, "kind").as_string().unwrap_or_default();
    match kind.as_str() {
        "input" => SESSION.with(|s| s.borrow_mut().input.push(get(body, "input"))),
        "setting" => {
            let key = get(body, "key").as_string().unwrap_or_default();
            if let Some(settings) = SESSION.with(|s| s.borrow().settings.clone()) {
                let _ = Reflect::set(&settings, &key.as_str().into(), &get(body, "value"));
            }
            if key.starts_with("audio.") {
                let number = |key: &str| setting(key).as_f64().unwrap_or(f64::NAN);
                let lazy = setting("audio.lazy").is_truthy();
                GAME.with(|game| {
                    if let Some(game) = game.borrow_mut().as_mut() {
                        game.set_audio_cache_mode(
                            lazy,
                            number("audio.shortCompressedBytes"),
                            number("audio.shortDurationSecs"),
                            number("audio.musicBudgetBytes"),
                            number("audio.midiSampleRate"),
                        );
                    }
                });
            }
        },
        "net-data" => {
            let id = get(body, "id").as_f64().unwrap_or(0.0) as u32;
            let bytes = Uint8Array::new(&get(body, "bytes")).to_vec();
            SESSION.with(|s| s.borrow_mut().connections.get_mut(&id).map(|c| c.inbox.extend(bytes)));
        },
        "net-close" => {
            let id = get(body, "id").as_f64().unwrap_or(0.0) as u32;
            SESSION.with(|s| s.borrow_mut().connections.get_mut(&id).map(|c| c.closed = true));
        },
        // A file the host added goes straight into the filesystem the game reads.
        "fs-put" => {
            if SESSION.with(|s| s.borrow().initialized) {
                let path = get(body, "path").as_string().unwrap_or_default();
                vfs::put(&path, Uint8Array::new(&get(body, "bytes")).to_vec());
            }
        },
        // The host deleted an entry: the running game stops seeing it, and the host is not told back.
        "fs-drop" => vfs::drop_key(&get(body, "key").as_string().unwrap_or_default()),
        "fullscreen-changed" => FULLSCREEN.with(|f| f.set(get(body, "enabled").is_truthy())),
        "unload" => {
            // Dropping the game is what stops its sound.
            SESSION.with(|s| s.borrow_mut().ended = true);
            drop(GAME.with(|game| game.borrow_mut().take()));
            CLOSED.with(|c| c.set(true));
        },
        _ => (),
    }
}

// -- What the game reaches the host through ------------------------------------------------------

/// A declared setting's current value (GEMS spec §11).
pub fn setting(key: &str) -> JsValue {
    SESSION.with(|s| s.borrow().settings.as_ref().map_or(JsValue::UNDEFINED, |settings| get(settings, key)))
}

fn store_changed(key: &str, value: Option<&str>) {
    match value {
        Some(value) => notify("fs-set", &[("key", key.into()), ("value", value.into())], None),
        None => notify("fs-remove", &[("key", key.into())], None),
    }
}

fn saved() {
    notify("save", &[], None);
}

/// One operation for the host's `AudioContext` (A.4). `transfer` moves sample buffers.
pub fn audio(command: &JsValue, transfer: Option<&Array>) {
    notify("audio", &[("command", command.clone())], transfer);
}

pub fn set_title(title: &str) {
    notify("title", &[("title", title.into())], None);
}

pub fn set_fullscreen(enabled: bool) {
    notify("set-fullscreen", &[("enabled", enabled.into())], None);
}

/// The host's last reported state, so it answers correctly mid-frame.
pub fn is_fullscreen() -> bool {
    FULLSCREEN.with(Cell::get)
}

/// Shows a dialog and **blocks this thread** until the player answers: the button's index (or -1,
/// dismissed) and the input field's text. `labels` joined by U+001F; `kind` 0 none, 1 string, 2
/// integer; `cancel` -1 means not dismissible.
pub fn show_dialog(text: &str, labels: &str, primary: i32, cancel: i32, kind: i32, initial: &str) -> (i32, String) {
    let Some(buffer) = SESSION.with(|s| s.borrow().dialog_buffer.clone()) else {
        notify(
            "error",
            &[
                ("code", "unsupported-environment".into()),
                ("detail", "blocking dialogs need a handshake buffer".into()),
            ],
            None,
        );
        return (-1, String::new())
    };
    let buttons: Array = if labels.is_empty() { Vec::new() } else { labels.split('\u{1f}').collect::<Vec<_>>() }
        .into_iter()
        .enumerate()
        .map(|(i, label)| {
            let kind = if i as i32 == primary { "primary".into() } else { JsValue::UNDEFINED };
            object(&[("label", label.into()), ("value", (i as u32).into()), ("kind", kind)])
        })
        .collect();
    let input = match kind {
        0 => JsValue::UNDEFINED,
        _ => object(&[("kind", if kind == 2 { "integer" } else { "string" }.into()), ("initial", initial.into())]),
    };
    let request = object(&[
        ("text", text.into()),
        ("buttons", buttons.into()),
        ("input", input),
        ("dismissible", (cancel != -1).into()),
    ]);

    // signal, button, length, reserved; then the answer's UTF-8. Marked waiting *before* the
    // question goes out, so an instant answer cannot be missed.
    let control = Int32Array::new_with_byte_offset_and_length(&buffer, 0, 4);
    let _ = js_sys::Atomics::store(&control, 0, 0);
    notify("dialog-asked", &[("request", request)], None);
    let _ = js_sys::Atomics::wait(&control, 0, 0);
    let length = (js_sys::Atomics::load(&control, 2).unwrap_or(0).max(0) as u32).min(64 * 1024 - 16);
    let value = Uint8Array::new_with_byte_offset_and_length(&buffer, 16, length).to_vec();
    (js_sys::Atomics::load(&control, 1).unwrap_or(-1), String::from_utf8_lossy(&value).into_owned())
}
