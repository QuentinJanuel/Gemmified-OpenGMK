//! The host's audio device, driven by an engine in a worker, which has no `AudioContext`.
//!
//! The engine keeps its own graph (`gm8emulator/src/game/audio_proxy.rs`) and sends only the
//! operations that make sound, as Annex A.4's commands; this replays them on the real context.
//! **No command may fail a frame.** The engine's graph and this mirror of it can legitimately
//! disagree (a source already stopped, a property this browser lacks); the sound is wrong at worst,
//! so what did nothing is logged once per kind and the game goes on.

use crate::{call, get, ignore};
use js_sys::Reflect;
use std::collections::{HashMap, HashSet};
use wasm_bindgen::JsValue;

/// Node ids the protocol reserves: the device's output and its listener. Never created.
const DESTINATION: i32 = 0;
const LISTENER: i32 = -1;

pub struct Executor {
    context: web_sys::AudioContext,
    nodes: HashMap<i32, JsValue>,
    buffers: HashMap<i32, JsValue>,
    /// Kinds of command already reported as doing nothing; some arrive every frame.
    unsupported: HashSet<String>,
}

impl Executor {
    pub fn new(context: web_sys::AudioContext) -> Self {
        Self { context, nodes: HashMap::new(), buffers: HashMap::new(), unsupported: HashSet::new() }
    }

    /// Read through the getter every time: a host's master volume may patch `destination` later.
    fn node(&self, id: i32) -> Option<JsValue> {
        if id == DESTINATION { Some(get(&self.context, "destination")) } else { self.nodes.get(&id).cloned() }
    }

    /// One `AudioParam` by node and name, checked by shape, or why not.
    fn param(&self, id: i32, name: &str) -> Result<JsValue, &'static str> {
        let params = if id == LISTENER { Some(get(&self.context, "listener")) } else { self.node(id) };
        let param = get(&params.ok_or("unknown-node")?, name);
        match param.is_object() && Reflect::has(&param, &"value".into()).unwrap_or(false) {
            true => Ok(param),
            false => Err("unknown-param"),
        }
    }

    pub fn execute(&mut self, command: &JsValue) {
        let op = get(command, "op").as_string().unwrap_or_default();
        let id = get(command, "id").as_f64().unwrap_or(0.0) as i32;
        let field = |key| get(command, key);
        let result: Result<(), &'static str> = match op.as_str() {
            "create" => {
                let kind = get(command, "type").as_string().unwrap_or_default();
                let method = match kind.as_str() {
                    "gain" => "createGain",
                    "stereoPanner" => "createStereoPanner",
                    "panner" => "createPanner",
                    "bufferSource" => "createBufferSource",
                    "biquadFilter" => "createBiquadFilter",
                    "dynamicsCompressor" => "createDynamicsCompressor",
                    "delay" => "createDelay",
                    "oscillator" => "createOscillator",
                    "convolver" => "createConvolver",
                    _ => "",
                };
                if method.is_empty() {
                    Err("node-type")
                } else {
                    // The one creator that reads an argument; a bad one falls back to the default.
                    let args = match get(&get(command, "args"), "0").as_f64() {
                        Some(max_delay) if kind == "delay" => vec![max_delay.into()],
                        _ if kind == "delay" => vec![1.into()],
                        _ => vec![],
                    };
                    call(&self.context, method, &args)
                        .map(|node| drop(self.nodes.insert(id, node)))
                        .map_err(|_| "rejected")
                }
            },
            "connect" => {
                let target = field("target").as_f64().unwrap_or(0.0) as i32;
                match (self.node(id), self.node(target)) {
                    (Some(from), Some(to)) => call(&from, "connect", &[to]).map(drop).map_err(|_| "rejected"),
                    _ => Err("unknown-node"),
                }
            },
            // Already started or stopped is routine for a source node, which is single-use.
            "start" => {
                self.node(id).ok_or("unknown-node").map(|n| drop(call(&n, "start", &[field("when"), field("offset")])))
            },
            "stop" => self.node(id).ok_or("unknown-node").map(|n| drop(call(&n, "stop", &[]))),
            "param" => self.param(id, &param_name(command)).and_then(|p| assign(&p, "value", &field("value"))),
            "ramp" => self.param(id, &param_name(command)).and_then(|param| {
                // Anchored where the value is now, so a fade started mid-fade continues.
                let now: JsValue = self.context.current_time().into();
                let seconds = field("seconds").as_f64().unwrap_or(0.0);
                call(&param, "cancelScheduledValues", &[now.clone()])
                    .and_then(|_| call(&param, "setValueAtTime", &[get(&param, "value"), now.clone()]))
                    .map_err(|_| "rejected")?;
                if seconds > 0.0 {
                    let end = self.context.current_time() + seconds;
                    call(&param, "linearRampToValueAtTime", &[field("value"), end.into()])
                        .map(drop)
                        .map_err(|_| "rejected")
                } else {
                    assign(&param, "value", &field("value"))
                }
            }),
            "cancelRamp" => self.param(id, &param_name(command)).and_then(|param| {
                let now: JsValue = self.context.current_time().into();
                let held = get(&param, "value");
                call(&param, "cancelScheduledValues", &[now.clone()])
                    .and_then(|_| call(&param, "setValueAtTime", &[held, now]))
                    .map(drop)
                    .map_err(|_| "rejected")
            }),
            "deleteNode" => {
                if let Some(node) = self.nodes.remove(&id) {
                    let _ = call(&node, "disconnect", &[]);
                }
                Ok(())
            },
            "prop" => {
                let prop = get(command, "prop").as_string().unwrap_or_default();
                self.node(id).ok_or("unknown-node").and_then(|node| assign(&node, &prop, &field("value")))
            },
            "createBuffer" => {
                let args = [field("channels"), field("frames"), field("rate")];
                call(&self.context, "createBuffer", &args)
                    .map(|buffer| drop(self.buffers.insert(id, buffer)))
                    .map_err(|_| "rejected")
            },
            // Copied into the real buffer here and now; the samples were moved, not shared.
            "bufferData" => match self.buffers.get(&id) {
                Some(buffer) => {
                    let start = field("start").as_f64().unwrap_or(0.0);
                    call(buffer, "copyToChannel", &[field("data"), field("ch"), start.into()])
                        .map(drop)
                        .map_err(|_| "rejected")
                },
                None => Err("unknown-node"),
            },
            "setBuffer" => {
                let buffer = self.buffers.get(&(field("bufferId").as_f64().unwrap_or(0.0) as i32)).cloned();
                match (self.node(id), buffer) {
                    (Some(node), Some(buffer)) => assign(&node, "buffer", &buffer),
                    _ => Err("unknown-node"),
                }
            },
            "deleteBuffer" => {
                self.buffers.remove(&id);
                Ok(())
            },
            // A context the browser will not start without a gesture; the host retries on the next.
            "resume" => {
                ignore(self.context.resume());
                Ok(())
            },
            _ => Ok(()),
        };
        if let Err(reason) = result {
            let param = get(command, "param").as_string().unwrap_or_default();
            let kind = format!("{op} {param} ({reason})");
            if self.unsupported.insert(kind.clone()) {
                web_sys::console::warn_1(&format!("{}: audio command {kind} did nothing", crate::NAME).into());
            }
        }
    }

    /// Drops every node and buffer. The context itself is the session's to close.
    pub fn dispose(&mut self) {
        for node in self.nodes.values() {
            let _ = call(node, "disconnect", &[]);
        }
        self.nodes.clear();
        self.buffers.clear();
    }
}

fn param_name(command: &JsValue) -> String {
    get(command, "param").as_string().unwrap_or_default()
}

/// `target[key] = value`, or why not: read-only, or refused by the browser.
fn assign(target: &JsValue, key: &str, value: &JsValue) -> Result<(), &'static str> {
    match Reflect::set(target, &key.into(), value) {
        Ok(true) => Ok(()),
        _ => Err("rejected"),
    }
}
