//! Web Audio for an engine that cannot reach it: stand-ins for the five `web_sys` types
//! `audio_wasm.rs` is built on, backed by the plugin's audio command protocol (`host/src/audio.rs`).
//!
//! **Why this exists.** There is no `AudioContext` in a worker, and every engine ends up in one.
//! So the engine keeps the whole graph in its own head -- which node exists, what is connected to
//! what, how long each buffer is, when each source started -- and sends out only the operations
//! that actually produce sound. The host replays them against a real context on the thread that has
//! one (the `opengmk-host` crate's `audio` module).
//!
//! **Nothing above this file changed.** `audio_wasm.rs` is 2,858 lines of mixer, cache, FMOD and
//! ssound emulation written against `web_sys`; it compiles against these types unchanged, because
//! every method here has the signature `web_sys` gives it. That is the whole design: the port is a
//! swap of one `use` line, and a reader comparing this engine's audio against upstream's sees the
//! mixer they expect.
//!
//! **What an engine may not do through this, and the code above happens not to.**
//!
//! * `current_time` is **not** the host context's clock. It cannot be: a synchronous getter cannot
//!   round-trip to another thread. It is a monotonic clock in seconds, which is all the mixer wants
//!   -- every read is differenced against an earlier one to ask "how far into this sound are we".
//!   Scheduling against it would be wrong, so nothing does: every `start` above passes `when = 0`,
//!   meaning "now", and `start_at` says so again where it would bite.
//! * Nothing is ever read back out of an `AudioBuffer`. The mixer asks for `length`,
//!   `sample_rate` and `number_of_channels`, all of which it told us when it created the buffer,
//!   and never for samples.
//! * The host's context is the host's. `resume` asks; `close` does nothing, because closing a
//!   context that may also be carrying the page's own sound is not an engine's decision. A host
//!   tears its context down when it disposes of the session.

use std::{cell::Cell, rc::Rc};
use wasm_bindgen::prelude::*;

/// The destination node's reserved id. Never created, never
/// deleted.
const DESTINATION_ID: i32 = 0;

thread_local! {
    /// Ids for everything this engine creates. Node ids and buffer ids are separate namespaces in
    /// the protocol, but one counter serves both and makes a stray command easier to read in a log.
    static NEXT_ID: Cell<i32> = const { Cell::new(1) };
}

fn next_id() -> i32 {
    NEXT_ID.with(|id| {
        let next = id.get();
        id.set(next.wrapping_add(1).max(1));
        next
    })
}

/// Starts a command object. Every command is `{op, ...}`; `host/src/audio.rs` reads the fields each
/// op carries.
fn command(op: &str) -> js_sys::Object {
    let object = js_sys::Object::new();
    set(&object, "op", &JsValue::from_str(op));
    object
}

fn set(object: &js_sys::Object, key: &str, value: &JsValue) {
    // The only way this fails is a frozen object, which one we just made is not.
    let _ = js_sys::Reflect::set(object, &JsValue::from_str(key), value);
}

fn set_num(object: &js_sys::Object, key: &str, value: f64) {
    set(object, key, &JsValue::from_f64(value));
}

fn send(command: js_sys::Object) {
    crate::gems::audio(command.as_ref(), None);
}

/// The clock `current_time` reports and `linear_ramp_to_value_at_time` differences against. One
/// function so the two cannot drift apart, which would turn a fade's duration into a wrong number
/// rather than an obviously broken one.
fn now_seconds() -> f64 {
    crate::gml::datetime::monotonic_micros() / 1_000_000.0
}

// -- Nodes ----------------------------------------------------------------------------------------

/// What every node shares: an id, and the promise to tell the host when it is gone.
struct NodeInner {
    id: i32,
}

impl Drop for NodeInner {
    /// **The reason a node is reference-counted at all, and the engine's only teardown.** The
    /// executor holds every node it was told to create in a map, and a game that plays a sound
    /// effect creates one per play -- so without this, a long session leaves thousands of dead
    /// `AudioBufferSourceNode`s on the host's heap, each pinning the buffer it played. The mixer
    /// never disconnects anything by hand; it drops its handles, which is the same statement and a
    /// stronger one, since the host also stops holding the node.
    fn drop(&mut self) {
        if self.id == DESTINATION_ID {
            return
        }
        let message = command("deleteNode");
        set_num(&message, "id", f64::from(self.id));
        send(message);
    }
}

/// Stand-in for `web_sys::AudioNode`: the type every node derefs to, and the one `connect` speaks.
#[derive(Clone)]
pub struct AudioNode {
    inner: Rc<NodeInner>,
}

impl AudioNode {
    fn create(kind: &str) -> Self {
        let id = next_id();
        let message = command("create");
        set_num(&message, "id", f64::from(id));
        set(&message, "type", &JsValue::from_str(kind));
        set(&message, "args", js_sys::Array::new().as_ref());
        send(message);
        Self { inner: Rc::new(NodeInner { id }) }
    }

    fn id(&self) -> i32 {
        self.inner.id
    }

    pub fn connect_with_audio_node(&self, destination: &AudioNode) -> Result<AudioNode, JsValue> {
        let message = command("connect");
        set_num(&message, "id", f64::from(self.id()));
        set_num(&message, "target", f64::from(destination.id()));
        send(message);
        // `web_sys` answers with the destination, so a chain of connects reads left to right.
        Ok(destination.clone())
    }

    /// One `AudioParam` of this node, addressed by name. Nothing is sent until a value is set.
    fn param(&self, name: &'static str) -> AudioParam {
        AudioParam { node: self.clone(), name, value: Cell::new(0.0) }
    }

    fn set_prop(&self, prop: &str, value: &JsValue) {
        let message = command("prop");
        set_num(&message, "id", f64::from(self.id()));
        set(&message, "prop", &JsValue::from_str(prop));
        set(&message, "value", value);
        send(message);
    }
}

/// Generates the newtype, the `Deref` to `AudioNode` and the `AsRef` that `web_sys` gives these
/// types -- which is what lets `source.connect_with_audio_node(gain.as_ref())` above compile
/// against either implementation.
macro_rules! node_type {
    ($name:ident) => {
        #[derive(Clone)]
        pub struct $name(AudioNode);

        impl std::ops::Deref for $name {
            type Target = AudioNode;

            fn deref(&self) -> &AudioNode {
                &self.0
            }
        }

        impl AsRef<AudioNode> for $name {
            fn as_ref(&self) -> &AudioNode {
                &self.0
            }
        }
    };
}

node_type!(GainNode);
node_type!(StereoPannerNode);
node_type!(AudioBufferSourceNode);
node_type!(AudioDestinationNode);

impl GainNode {
    pub fn gain(&self) -> AudioParam {
        self.0.param("gain")
    }
}

impl StereoPannerNode {
    pub fn pan(&self) -> AudioParam {
        self.0.param("pan")
    }
}

impl AudioBufferSourceNode {
    pub fn set_buffer(&self, buffer: Option<&AudioBuffer>) {
        let message = command("setBuffer");
        set_num(&message, "id", f64::from(self.0.id()));
        // `None` would mean "detach", which the protocol has no way to say and no caller here asks
        // for. Sent as the reserved id -- never a real buffer's, since ids start at 1 -- so a host
        // reports it as addressing a buffer that does not exist rather than quietly doing something
        // else. If a caller ever appears, that report is the first thing it will produce.
        set_num(&message, "bufferId", f64::from(buffer.map_or(0, AudioBuffer::id)));
        send(message);
    }

    pub fn set_loop(&self, looping: bool) {
        self.0.set_prop("loop", &JsValue::from_bool(looping));
    }

    pub fn set_loop_start(&self, seconds: f64) {
        self.0.set_prop("loopStart", &JsValue::from_f64(seconds));
    }

    pub fn set_loop_end(&self, seconds: f64) {
        self.0.set_prop("loopEnd", &JsValue::from_f64(seconds));
    }

    pub fn playback_rate(&self) -> AudioParam {
        self.0.param("playbackRate")
    }

    pub fn start(&self) -> Result<(), JsValue> {
        self.start_at(0.0, 0.0)
    }

    pub fn start_with_when_and_grain_offset(&self, when: f64, offset: f64) -> Result<(), JsValue> {
        self.start_at(when, offset)
    }

    /// `when` is an **absolute time on the host's context clock**, which this engine does not have
    /// and must not guess at -- see this module's header. Every caller passes `0`, meaning "now",
    /// which is the one value that needs no clock at all. Anything else is a scheduling decision
    /// made against the wrong timeline, so it is clamped to "now" and said out loud rather than
    /// silently landing somewhere unintended.
    fn start_at(&self, when: f64, offset: f64) -> Result<(), JsValue> {
        if when != 0.0 {
            web_sys::console::warn_1(
                &"audio: a source was scheduled for a future time, which this engine cannot do; starting it now".into(),
            );
        }
        let message = command("start");
        set_num(&message, "id", f64::from(self.0.id()));
        set_num(&message, "when", 0.0);
        set_num(&message, "offset", offset);
        send(message);
        Ok(())
    }

    pub fn stop(&self) -> Result<(), JsValue> {
        let message = command("stop");
        set_num(&message, "id", f64::from(self.0.id()));
        send(message);
        Ok(())
    }
}

// -- Params ---------------------------------------------------------------------------------------

/// Stand-in for `web_sys::AudioParam`, addressed by its node and name rather than held as an object
/// of its own: the protocol has no handle for a param, and nothing above ever stores one.
///
/// `value` is remembered here because the mixer reads it back (`gain().value()` when ducking one
/// sound under another), and a getter cannot ask the host. Every write goes through this type, so
/// what it remembers is what the host was told.
pub struct AudioParam {
    node: AudioNode,
    name: &'static str,
    value: Cell<f32>,
}

impl AudioParam {
    pub fn value(&self) -> f32 {
        self.value.get()
    }

    pub fn set_value(&self, value: f32) {
        self.value.set(value);
        let message = command("param");
        set_num(&message, "id", f64::from(self.node.id()));
        set(&message, "param", &JsValue::from_str(self.name));
        set_num(&message, "value", f64::from(value));
        send(message);
    }

    /// The scheduled form. For the same reason as `start_at`, there is no shared clock to schedule
    /// against, and every caller passes a time it has just read from `current_time` -- so this is
    /// the immediate set, which is what those calls meant.
    pub fn set_value_at_time(&self, value: f32, _when: f64) -> Result<AudioParam, JsValue> {
        self.set_value(value);
        Ok(self.clone_param())
    }

    /// A fade: the gain idiom behind `sga_fade_volume` and `mmMusicSlideVolume`, where a game asks
    /// for a volume change over a stretch of time rather than at an instant.
    ///
    /// `end_time` is on `current_time`'s clock, which the host does not share -- so what crosses is
    /// the **duration**, recovered here by differencing against the same clock the caller read. The
    /// host anchors the ramp at the value the param actually has, which is what makes a fade
    /// interrupting another fade continue from where the sound is rather than jump.
    pub fn linear_ramp_to_value_at_time(&self, value: f32, end_time: f64) -> Result<AudioParam, JsValue> {
        let seconds = end_time - now_seconds();
        // Whatever the host settles on, this is where the value ends up, and a later `value()` read
        // should say so rather than reporting where the fade started.
        self.value.set(value);
        let message = command("ramp");
        set_num(&message, "id", f64::from(self.node.id()));
        set(&message, "param", &JsValue::from_str(self.name));
        set_num(&message, "value", f64::from(value));
        set_num(&message, "seconds", seconds.max(0.0));
        send(message);
        Ok(self.clone_param())
    }

    /// Drops any ramp still on the timeline, leaving the value where it is.
    ///
    /// Called before writing a param that may have been ramped: a scheduled event would otherwise
    /// win over the write. `_start_time` is on the unshared clock again, and means "now" at every
    /// call site.
    pub fn cancel_scheduled_values(&self, _start_time: f64) -> Result<AudioParam, JsValue> {
        let message = command("cancelRamp");
        set_num(&message, "id", f64::from(self.node.id()));
        set(&message, "param", &JsValue::from_str(self.name));
        send(message);
        Ok(self.clone_param())
    }

    /// `web_sys` answers these with the param itself, so calls chain. Ours is a handle rather than
    /// an object, so a copy of the handle is the same thing.
    fn clone_param(&self) -> AudioParam {
        AudioParam { node: self.node.clone(), name: self.name, value: Cell::new(self.value.get()) }
    }
}

// -- Buffers --------------------------------------------------------------------------------------

struct BufferInner {
    id: i32,
    frames: u32,
    channels: u32,
    rate: f32,
}

impl Drop for BufferInner {
    fn drop(&mut self) {
        let message = command("deleteBuffer");
        set_num(&message, "id", f64::from(self.id));
        send(message);
    }
}

/// Stand-in for `web_sys::AudioBuffer`. The samples live on the host; what is here is what the
/// engine asked for, which is also everything it ever asks back.
#[derive(Clone)]
pub struct AudioBuffer {
    inner: Rc<BufferInner>,
}

impl AudioBuffer {
    fn id(&self) -> i32 {
        self.inner.id
    }

    pub fn length(&self) -> u32 {
        self.inner.frames
    }

    pub fn sample_rate(&self) -> f32 {
        self.inner.rate
    }

    pub fn number_of_channels(&self) -> u32 {
        self.inner.channels
    }

    pub fn copy_to_channel(&self, data: &[f32], channel: i32) -> Result<(), JsValue> {
        self.copy_to_channel_with_start_in_channel(data, channel, 0)
    }

    /// Fills part of a channel, which is the form that matters: a MIDI render is streamed into its
    /// buffer in chunks precisely so the whole thing never exists in linear memory at once (a
    /// 223-second score is ~75MB, and wasm memory never shrinks). The protocol carries the offset
    /// so that property survives the port -- without it this would have to accumulate the channel
    /// on this side and send it whole, which is the allocation the chunking exists to avoid.
    ///
    pub fn copy_to_channel_with_start_in_channel(&self, data: &[f32], channel: i32, start: u32) -> Result<(), JsValue> {
        let message = command("bufferData");
        set_num(&message, "id", f64::from(self.id()));
        set_num(&message, "ch", f64::from(channel));
        set_num(&message, "start", f64::from(start));
        // A copy, moved rather than cloned to the host: a view would clone all of wasm memory.
        let data = js_sys::Float32Array::from(data);
        set(&message, "data", data.as_ref());
        crate::gems::audio(message.as_ref(), Some(&js_sys::Array::of1(&data.buffer())));
        Ok(())
    }
}

// -- The context ----------------------------------------------------------------------------------

/// Stand-in for `web_sys::AudioContext`. It owns no device and no clock the host shares; it is the
/// factory the mixer builds its graph through, plus the monotonic clock that graph measures itself
/// against.
#[derive(Clone)]
pub struct AudioContext {
    destination: AudioDestinationNode,
}

impl AudioContext {
    pub fn new() -> Result<Self, JsValue> {
        Ok(Self {
            // Never created and never deleted: the reserved id is what the executor resolves to the
            // real context's `destination` -- through its getter, every time, so the host's master
            // volume applies to this engine like anything else that reaches the speakers.
            destination: AudioDestinationNode(AudioNode { inner: Rc::new(NodeInner { id: DESTINATION_ID }) }),
        })
    }

    pub fn create_gain(&self) -> Result<GainNode, JsValue> {
        Ok(GainNode(AudioNode::create("gain")))
    }

    pub fn create_stereo_panner(&self) -> Result<StereoPannerNode, JsValue> {
        Ok(StereoPannerNode(AudioNode::create("stereoPanner")))
    }

    pub fn create_buffer_source(&self) -> Result<AudioBufferSourceNode, JsValue> {
        Ok(AudioBufferSourceNode(AudioNode::create("bufferSource")))
    }

    pub fn create_buffer(&self, channels: u32, frames: u32, rate: f32) -> Result<AudioBuffer, JsValue> {
        let id = next_id();
        let message = command("createBuffer");
        set_num(&message, "id", f64::from(id));
        set_num(&message, "channels", f64::from(channels));
        set_num(&message, "frames", f64::from(frames));
        set_num(&message, "rate", f64::from(rate));
        send(message);
        Ok(AudioBuffer { inner: Rc::new(BufferInner { id, frames, channels, rate }) })
    }

    pub fn destination(&self) -> AudioDestinationNode {
        self.destination.clone()
    }

    /// A monotonic clock in seconds. **Not the host context's clock** -- see this module's header
    /// for why there cannot be one, and why every reader here differences it rather than scheduling
    /// against it.
    pub fn current_time(&self) -> f64 {
        now_seconds()
    }

    pub fn resume(&self) -> Result<(), JsValue> {
        send(command("resume"));
        Ok(())
    }

    /// Does nothing, deliberately. The context belongs to the host and may be carrying sound this
    /// game did not make; an engine going away is not a reason to close it. What actually has to
    /// stop is this game's own sound, and `AudioManager::drop` stops that itself before getting
    /// here -- then every node it was holding drops, and each one tells the host to forget it.
    pub fn close(&self) -> Result<(), JsValue> {
        Ok(())
    }
}
