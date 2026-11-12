//! The host realm's half of OpenGMK's GEMS plugin: the `EngineSession` a host holds (the GEMS spec §8).
//!
//! The engine runs in a worker (`host/worker.js`, `gm8emulator/src/gems.rs`). This side is where
//! the things a worker cannot have live: the `AudioContext`, the shared buffer a blocking dialog is
//! answered through, and the `Worker` itself. The two halves speak the GEMS spec Annex A's reference wire.
//! It is a wasm module of its own, a small one, so the page never loads the engine.

#![cfg(target_arch = "wasm32")]

mod audio;

use js_sys::{Array, Function, Int32Array, Object, Promise, Reflect, SharedArrayBuffer, Uint8Array};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::Rc,
};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, future_to_promise, spawn_local};

const API_VERSION: f64 = 5.0;
const NAME: &str = "opengmk";
/// The dialog buffer (Annex A.5): signal, button, length, reserved, then the answer's UTF-8.
const DIALOG_BUFFER_BYTES: u32 = 64 * 1024;
const DIALOG_VALUE_OFFSET: u32 = 16;
const DIALOG_DISMISSED: i32 = -1;

pub(crate) fn get(object: &JsValue, key: &str) -> JsValue {
    Reflect::get(object, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

pub(crate) fn object(fields: &[(&str, JsValue)]) -> JsValue {
    let object = Object::new();
    for (key, value) in fields {
        let _ = Reflect::set(&object, &JsValue::from_str(key), value);
    }
    object.into()
}

/// `target[name](...args)`, with whatever it throws as the error.
pub(crate) fn call(target: &JsValue, name: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let method: Function = get(target, name).dyn_into()?;
    method.apply(target, &args.iter().collect::<Array>())
}

/// A call into the host whose failure nobody can act on, reported as an uncaught one would be.
fn call_host(target: &JsValue, name: &str, args: &[JsValue]) -> JsValue {
    call(target, name, args).unwrap_or_else(|error| {
        web_sys::console::error_1(&error);
        JsValue::UNDEFINED
    })
}

/// Swallows a promise's rejection.
pub(crate) fn ignore(promise: Result<Promise, JsValue>) {
    thread_local! {
        static NOTHING: Closure<dyn FnMut(JsValue)> = Closure::new(|_| {});
    }
    if let Ok(promise) = promise {
        NOTHING.with(|nothing| drop(promise.catch(nothing)));
    }
}

/// An error a host recognises by its brand (GEMS spec §12).
fn engine_error(code: &str, detail: &str) -> JsValue {
    let error = js_sys::Error::new(&format!("{code}: {detail}"));
    error.set_name("EngineError");
    let _ = Reflect::set(&error, &"engineError".into(), &true.into());
    let _ = Reflect::set(&error, &"code".into(), &code.into());
    let _ = Reflect::set(&error, &"detail".into(), &detail.into());
    error.into()
}

/// `String(value)`.
fn describe(value: &JsValue) -> String {
    match value.dyn_ref::<js_sys::Error>() {
        Some(error) => String::from(error.to_string()),
        None => value.as_string().unwrap_or_else(|| format!("{value:?}")),
    }
}

/// The envelope layer (Annex A.1), over the worker.
struct Wire {
    worker: web_sys::Worker,
    next_id: Cell<u32>,
    pending: RefCell<HashMap<u32, (Function, Function)>>,
    closed: Cell<bool>,
}

impl Wire {
    fn post(&self, message: &JsValue, transfer: Option<&Array>) -> Result<(), JsValue> {
        match transfer {
            Some(transfer) => self.worker.post_message_with_transfer(message, transfer),
            None => self.worker.post_message(message),
        }
    }

    fn notify(&self, kind: &str, fields: &[(&str, JsValue)], transfer: Option<&Array>) {
        if self.closed.get() {
            return
        }
        let body = Object::new();
        let _ = Reflect::set(&body, &"kind".into(), &kind.into());
        for (key, value) in fields {
            let _ = Reflect::set(&body, &JsValue::from_str(key), value);
        }
        if let Err(error) = self.post(&object(&[("w", "note".into()), ("body", body.into())]), transfer) {
            web_sys::console::error_1(&error);
        }
    }

    fn request(&self, body: JsValue, transfer: Option<&Array>) -> Promise {
        if self.closed.get() {
            return Promise::reject(&engine_error("internal", "the wire is closed"))
        }
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        Promise::new(&mut |resolve, reject| {
            let message = object(&[("w", "req".into()), ("id", id.into()), ("body", body.clone())]);
            match self.post(&message, transfer) {
                Ok(()) => drop(self.pending.borrow_mut().insert(id, (resolve, reject))),
                Err(error) => drop(reject.call1(&JsValue::NULL, &error)),
            }
        })
    }

    /// An answer from the worker.
    fn settle(&self, data: &JsValue, ok: bool) {
        let id = get(data, "id").as_f64().unwrap_or(0.0) as u32;
        let Some((resolve, reject)) = self.pending.borrow_mut().remove(&id) else { return };
        let _ = if ok {
            resolve.call1(&JsValue::NULL, &get(data, "body"))
        } else {
            let error = get(data, "error");
            let code = get(&error, "code").as_string().unwrap_or_else(|| "internal".into());
            let detail = get(&error, "detail").as_string().unwrap_or_default();
            reject.call1(&JsValue::NULL, &engine_error(&code, &detail))
        };
    }

    /// Ignores the worker from now on, and rejects every request still waiting.
    fn close(&self) {
        if self.closed.replace(true) {
            return
        }
        let error = engine_error("internal", "the wire closed before the reply arrived");
        for (_, (_, reject)) in self.pending.borrow_mut().drain() {
            let _ = reject.call1(&JsValue::NULL, &error);
        }
    }
}

/// An event listener this session added: its target, its event, and the closure.
type Listener = (JsValue, &'static str, Closure<dyn FnMut(JsValue)>);

struct Inner {
    host: JsValue,
    wire: Wire,
    context: web_sys::AudioContext,
    audio: RefCell<audio::Executor>,
    dialog_buffer: SharedArrayBuffer,
    /// Dialogs asked and not yet answered; one is shown at a time.
    dialogs: RefCell<VecDeque<JsValue>>,
    showing: Cell<bool>,
    /// Set once `init` was answered; notes before that are dropped.
    ready: Cell<bool>,
    /// The game ended itself, or the worker went away. Either way: no more frames.
    ended: Cell<bool>,
    /// A frame that has not finished. A frame stopped on a dialog has not finished.
    ticking: Cell<bool>,
    /// The game's loopback connections, by the engine's id (GEMS spec §14).
    connections: RefCell<Vec<(u32, JsValue)>>,
    /// The host's last known fullscreen state, so a change can be pushed rather than polled for.
    fullscreen: Cell<bool>,
    /// The engine's last answer about itself. Asked for now, shown next time; see `stats`.
    last_stats: RefCell<JsValue>,
    /// What this session hangs off the page and the worker, released by `release`.
    listeners: RefCell<Vec<Listener>>,
}

impl Inner {
    fn host(&self, path: &[&str]) -> JsValue {
        path.iter().fold(self.host.clone(), |at, key| get(&at, key))
    }

    /// A note arrived from the worker.
    fn note(self: &Rc<Self>, body: &JsValue) {
        let kind = get(body, "kind").as_string().unwrap_or_default();
        if kind == "dialog-asked" {
            return self.ask(get(body, "request"))
        }
        if !self.ready.get() {
            return
        }
        let field = |key| get(body, key);
        match kind.as_str() {
            "resize" => drop(call_host(&self.host(&["surface"]), "reportSize", &[field("width"), field("height")])),
            "title" => drop(call_host(&self.host(&["window"]), "setTitle", &[field("title")])),
            "set-fullscreen" => {
                call_host(&self.host(&["window"]), "setFullscreen", &[field("enabled")]);
                // Taken as read: the next `push_fullscreen_change` corrects it if the host declined.
                self.fullscreen.set(field("enabled").is_truthy());
            },
            "fs-set" => drop(call_host(&self.host(&["replica"]), "set", &[field("key"), field("value")])),
            "fs-remove" => drop(call_host(&self.host(&["replica"]), "remove", &[field("key")])),
            // The replica's own call, not `events.onSave`, which would count one save twice.
            "save" => drop(call_host(&self.host(&["replica"]), "save", &[])),
            "audio" => self.audio.borrow_mut().execute(&field("command")),
            "net-open" => {
                let id = field("id").as_f64().unwrap_or(0.0) as u32;
                let connection = call_host(&self.host(&["network"]), "connect", &[field("port")]);
                if connection.is_object() {
                    self.connections.borrow_mut().push((id, connection));
                } else {
                    self.wire.notify("net-close", &[("id", id.into())], None);
                }
            },
            "net-send" => {
                if let Some(connection) = self.connection(&field("id")) {
                    call_host(&connection, "send", &[Uint8Array::new(&field("bytes")).into()]);
                }
            },
            "net-end" => {
                if let Some(connection) = self.connection(&field("id")) {
                    call_host(&connection, "close", &[]);
                    let id = field("id").as_f64().unwrap_or(0.0) as u32;
                    self.connections.borrow_mut().retain(|(open, _)| *open != id);
                }
            },
            "error" => drop(call_host(&self.host(&["events"]), "onError", &[field("code"), field("detail")])),
            _ => (),
        }
    }

    fn connection(&self, id: &JsValue) -> Option<JsValue> {
        let id = id.as_f64().unwrap_or(0.0) as u32;
        self.connections.borrow().iter().find(|(open, _)| *open == id).map(|(_, c)| c.clone())
    }

    /// A blocking dialog: shown through the host, answered through the buffer the engine waits on.
    /// One at a time, so two answers can never land in one buffer.
    fn ask(self: &Rc<Self>, request: JsValue) {
        self.dialogs.borrow_mut().push_back(request);
        if self.showing.replace(true) {
            return
        }
        let inner = self.clone();
        spawn_local(async move {
            loop {
                let Some(request) = inner.dialogs.borrow_mut().pop_front() else { break };
                let (button, value) = inner.settle_dialog(&request).await;
                inner.answer(button, &value);
            }
            inner.showing.set(false);
        });
    }

    /// The host's answer, normalised: an unknown button is a dismissal, and the value is the
    /// field's text exactly when the request had a field. A host that fails dismisses.
    async fn settle_dialog(&self, request: &JsValue) -> (i32, String) {
        let has_input = get(request, "input").is_truthy();
        let shown = match call(&self.host(&["dialogs"]), "show", &[request.clone()]) {
            Ok(answer) => JsFuture::from(Promise::resolve(&answer)).await,
            Err(error) => Err(error),
        };
        let (button, value) = match shown {
            Ok(result) => {
                let button = get(&result, "button").as_f64();
                let known = Array::from(&get(request, "buttons")).iter().any(|b| get(&b, "value").as_f64() == button);
                let value = get(&result, "value").as_string().unwrap_or_default();
                (if known { button.unwrap_or(-1.0) as i32 } else { DIALOG_DISMISSED }, value)
            },
            Err(_) => (DIALOG_DISMISSED, String::new()),
        };
        // Before the answer: a key held when the dialog opened must not still be held after it.
        self.wire.notify("input", &[("input", object(&[("kind", "focus-lost".into())]))], None);
        (button, if has_input { value } else { String::new() })
    }

    /// Writes the answer, truncated to fit, then wakes the engine.
    fn answer(&self, button: i32, value: &str) {
        let control = Int32Array::new_with_byte_offset_and_length(&self.dialog_buffer, 0, 4);
        let bytes = &value.as_bytes()[..value.len().min((DIALOG_BUFFER_BYTES - DIALOG_VALUE_OFFSET) as usize)];
        Uint8Array::new_with_byte_offset_and_length(&self.dialog_buffer, DIALOG_VALUE_OFFSET, bytes.len() as u32)
            .copy_from(bytes);
        let _ = js_sys::Atomics::store(&control, 1, button);
        let _ = js_sys::Atomics::store(&control, 2, bytes.len() as i32);
        let _ = js_sys::Atomics::store(&control, 0, 1);
        let _ = js_sys::Atomics::notify(&control, 0);
    }

    /// The worker died without answering.
    fn fail(&self, detail: &str) {
        self.ended.set(true);
        call_host(&self.host(&["events"]), "onError", &["internal".into(), detail.into()]);
        self.wire.close();
    }

    /// Everything this session took hold of. Safe twice.
    fn release(&self) {
        for (target, kind, listener) in self.listeners.borrow_mut().drain(..) {
            let target: &web_sys::EventTarget = target.unchecked_ref();
            let _ = target.remove_event_listener_with_callback(kind, listener.as_ref().unchecked_ref());
        }
        self.audio.borrow_mut().dispose();
        // Told, then terminated: waiting for an acknowledgement would be waiting on an engine
        // already decided against.
        self.wire.notify("unload", &[], None);
        self.wire.close();
        self.wire.worker.terminate();
        // The context is this session's own, so closing it is this side's job.
        ignore(self.context.close());
    }

    fn listen(self: &Rc<Self>, target: JsValue, kind: &'static str, handler: impl Fn(&Rc<Self>, JsValue) + 'static) {
        let inner = Rc::downgrade(self);
        let listener = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
            if let Some(inner) = inner.upgrade() {
                handler(&inner, event);
            }
        });
        let _ = target
            .unchecked_ref::<web_sys::EventTarget>()
            .add_event_listener_with_callback(kind, listener.as_ref().unchecked_ref());
        self.listeners.borrow_mut().push((target, kind, listener));
    }
}

/// One loaded game, as the host holds it (GEMS spec §8).
#[wasm_bindgen]
pub struct Session(Rc<Inner>);

/// Starts the engine's worker and completes the `init` handshake. Anything short of a matched
/// contract rejects, with the worker, the audio device and the listeners released.
#[wasm_bindgen(js_name = createSession)]
pub async fn create_session(host: JsValue, worker_url: String) -> Result<Session, JsValue> {
    let options = web_sys::WorkerOptions::new();
    options.set_type(web_sys::WorkerType::Module);
    let worker = web_sys::Worker::new_with_options(&worker_url, &options)?;

    // The real AudioContext, which a worker cannot have; the engine sends the operations that make
    // sound and the executor replays them. Made in the host's realm, so whatever the host has done
    // to `AudioContext.prototype.destination` -- a master volume, say -- applies to it.
    if get(&js_sys::global(), "AudioContext").is_undefined() {
        worker.terminate();
        return Err(engine_error("unsupported-environment", "this realm has no AudioContext"))
    }
    // Requires cross-origin isolation, like the blocking dialogs it exists for.
    if get(&js_sys::global(), "SharedArrayBuffer").is_undefined() {
        worker.terminate();
        return Err(engine_error("internal", "SharedArrayBuffer is not defined"))
    }
    let context = web_sys::AudioContext::new()?;

    let inner = Rc::new(Inner {
        audio: RefCell::new(audio::Executor::new(context.clone())),
        context,
        dialog_buffer: SharedArrayBuffer::new(DIALOG_BUFFER_BYTES),
        wire: Wire { worker, next_id: Cell::new(1), pending: RefCell::default(), closed: Cell::new(false) },
        fullscreen: Cell::new(call_host(&get(&host, "window"), "isFullscreen", &[]).is_truthy()),
        host,
        dialogs: RefCell::default(),
        showing: Cell::new(false),
        ready: Cell::new(false),
        ended: Cell::new(false),
        ticking: Cell::new(false),
        connections: RefCell::default(),
        last_stats: RefCell::new(Object::new().into()),
        listeners: RefCell::default(),
    });

    // Browsers only let audio start from a user gesture. A realm with no document simply has no
    // gestures to wait for.
    if let Some(document) = web_sys::window().and_then(|window| window.document()) {
        for kind in ["pointerdown", "keydown"] {
            inner.listen(document.clone().into(), kind, |inner, _| ignore(inner.context.resume()));
        }
    }
    let worker: JsValue = inner.wire.worker.clone().into();
    inner.listen(worker.clone(), "message", |inner, event| {
        if inner.wire.closed.get() {
            return
        }
        let data = get(&event, "data");
        match get(&data, "w").as_string().as_deref() {
            Some("note") => inner.note(&get(&data, "body")),
            Some("ok") => inner.wire.settle(&data, true),
            Some("err") => inner.wire.settle(&data, false),
            _ => (),
        }
    });
    // A worker that fails to even start answers nothing; closing the wire rejects what waits on it.
    inner.listen(worker, "error", |inner, event| {
        if inner.ready.get() {
            inner.fail(&format!("the engine's worker failed: {}", describe(&get(&event, "message"))));
        } else {
            inner.wire.close();
        }
    });

    let host = &inner.host;
    let canvas = inner.host(&["surface", "canvas"]);
    let init = object(&[
        ("apiVersion", API_VERSION.into()),
        ("canvas", canvas.clone()),
        ("assetBase", inner.host(&["assets", "base"])),
        ("settings", get(host, "settings")),
        // The replica's own scope: the host keyed its storage by this and seeds from it.
        ("gameId", get(host, "game")),
        ("storeSeed", call_host(&get(host, "replica"), "seed", &[])),
        ("dialogBuffer", inner.dialog_buffer.clone().into()),
        ("launchArgs", get(host, "launchArgs")),
        ("ports", inner.host(&["network", "ports"])),
    ]);
    let handshake = inner.wire.request(object(&[("kind", "init".into()), ("init", init)]), Some(&Array::of1(&canvas)));
    if let Err(error) = JsFuture::from(handshake).await {
        inner.release();
        return Err(error)
    }
    inner.ready.set(true);

    // Host-side changes to the filesystem, straight into the running game's replica.
    let (put, drop_key) = (Rc::downgrade(&inner), Rc::downgrade(&inner));
    let put = Closure::<dyn Fn(JsValue, Uint8Array)>::new(move |path: JsValue, bytes: Uint8Array| {
        if let Some(inner) = put.upgrade() {
            // A copy made for exactly this, so it is moved rather than cloned.
            let buffer = bytes.buffer();
            inner.wire.notify(
                "fs-put",
                &[("path", path), ("bytes", buffer.clone().into())],
                Some(&Array::of1(&buffer)),
            );
        }
    });
    let drop_key = Closure::<dyn Fn(JsValue)>::new(move |key: JsValue| {
        if let Some(inner) = drop_key.upgrade() {
            inner.wire.notify("fs-drop", &[("key", key)], None);
        }
    });
    let sink = object(&[("put", put.into_js_value()), ("drop", drop_key.into_js_value())]);
    call_host(&get(&inner.host, "replica"), "watch", &[sink]);

    Ok(Session(inner))
}

#[wasm_bindgen]
impl Session {
    /// Hands the game's files to the worker. Read here, where a host's `File` handles can be opened,
    /// and moved rather than copied: a game is routinely hundreds of megabytes.
    pub fn start(&self, files: JsValue) -> Promise {
        let inner = self.0.clone();
        future_to_promise(async move {
            let wire_files = Array::new();
            let transfer = Array::new();
            for file in Array::from(&get(&files, "files")).iter() {
                let bytes = JsFuture::from(Promise::resolve(&call(&file, "read", &[])?)).await?;
                // A fresh buffer: the host's may be a view into something it still owns.
                let copy = bytes.unchecked_into::<Uint8Array>().slice(0, u32::MAX).buffer();
                wire_files.push(&object(&[("path", get(&file, "path")), ("bytes", copy.clone().into())]));
                transfer.push(&copy);
            }
            let start = object(&[("kind", "start".into()), ("files", wire_files.into())]);
            JsFuture::from(inner.wire.request(start, Some(&transfer))).await
        })
    }

    /// One frame. A frame suspended on a dialog outlives several of the host's callbacks, so a
    /// second call while one is pending answers "still going" rather than stacking ticks.
    pub fn step(&self, now: f64) -> JsValue {
        let inner = &self.0;
        if inner.ended.get() {
            return false.into()
        }
        if inner.ticking.get() {
            return true.into()
        }
        self.push_fullscreen_change();
        self.relay_from_host();
        let pending = inner.wire.request(object(&[("kind", "tick".into()), ("now", now.into())]), None);
        inner.ticking.set(true);
        let inner = inner.clone();
        future_to_promise(async move {
            let result = JsFuture::from(pending).await;
            inner.ticking.set(false);
            Ok(match result {
                Ok(answer) => {
                    let running = get(&answer, "running");
                    if !running.is_truthy() {
                        inner.ended.set(true);
                    }
                    running
                },
                // A frame whose answer can never arrive is what a dead worker looks like from here.
                Err(error) => {
                    inner.ended.set(true);
                    let detail = format!("the engine stopped answering: {}", describe(&error));
                    call_host(&inner.host(&["events"]), "onError", &["internal".into(), detail.into()]);
                    false.into()
                },
            })
        })
        .into()
    }

    #[wasm_bindgen(js_name = sendInput)]
    pub fn send_input(&self, input: JsValue) {
        self.0.wire.notify("input", &[("input", input)], None);
    }

    #[wasm_bindgen(js_name = setSetting)]
    pub fn set_setting(&self, key: JsValue, value: JsValue) {
        self.0.wire.notify("setting", &[("key", key), ("value", value)], None);
    }

    /// Asked for now, shown next time: `stats` is synchronous and the engine a round trip away.
    pub fn stats(&self) -> JsValue {
        let inner = self.0.clone();
        let pending = inner.wire.request(object(&[("kind", "stats".into())]), None);
        spawn_local(async move {
            if let Ok(answer) = JsFuture::from(pending).await {
                *inner.last_stats.borrow_mut() = answer;
            }
        });
        self.0.last_stats.borrow().clone()
    }

    pub fn dispose(&self) {
        self.0.ended.set(true);
        self.0.release();
    }

    /// Pulled once a frame, because `WindowServices` has no event for it and the game has to be
    /// answered mid-frame.
    fn push_fullscreen_change(&self) {
        let now = call_host(&self.0.host(&["window"]), "isFullscreen", &[]).is_truthy();
        if self.0.fullscreen.replace(now) != now {
            self.0.wire.notify("fullscreen-changed", &[("enabled", now.into())], None);
        }
    }

    /// What the host sent on each loopback connection since the last frame.
    fn relay_from_host(&self) {
        let connections = self.0.connections.borrow().clone();
        for (id, connection) in connections {
            let bytes: Uint8Array = call_host(&connection, "receive", &[]).unchecked_into();
            if bytes.length() > 0 {
                let buffer = bytes.slice(0, bytes.length()).buffer();
                self.0.wire.notify(
                    "net-data",
                    &[("id", id.into()), ("bytes", buffer.clone().into())],
                    Some(&Array::of1(&buffer)),
                );
            }
            if get(&connection, "closed").is_truthy() {
                self.0.wire.notify("net-close", &[("id", id.into())], None);
                self.0.connections.borrow_mut().retain(|(open, _)| *open != id);
            }
        }
    }
}
