use super::{connection::Error, event::Event};
use wasm_bindgen::JsCast;
use web_sys::{OffscreenCanvas, WebGl2RenderingContext};

/// Stand-in for ramen::window::Cursor. Setting the cursor is a no-op on wasm (as it already is
/// on native non-Windows platforms — ramen's own `set_cursor` does nothing outside `cfg(windows)`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Cursor {
    Arrow,
    Blank,
    Cross,
    Hand,
    Help,
    IBeam,
    Progress,
    ResizeNESW,
    ResizeNS,
    ResizeNWSE,
    ResizeWE,
    ResizeAll,
    Unavailable,
    Wait,
}

/// Stand-in for ramen::window::Controls. Meaningless in a browser (no OS window chrome), kept only
/// so the builder chain in Game::launch (game.rs) compiles unchanged.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct Controls {
    minimise: bool,
    maximise: bool,
}

impl Controls {
    pub const fn new() -> Self {
        Self { minimise: true, maximise: true }
    }

    pub const fn minimise(self, minimise: bool) -> Self {
        Self { minimise, ..self }
    }

    pub const fn maximise(self, maximise: bool) -> Self {
        Self { maximise, ..self }
    }
}

/// Stand-in for ramen::window::Window, bound to the drawing surface the host handed in instead of
/// an OS window. `poll_events`/`events` replicate ramen's "pull batch, then read batch until next
/// pull" contract over the input the host has pushed (see `super::input`).
///
/// Nothing here touches a document, which is what lets the engine run in a worker.
pub struct Window {
    canvas: OffscreenCanvas,
    gl: WebGl2RenderingContext,
    current_batch: Vec<Event>,
}

fn js_err(context: &str, err: wasm_bindgen::JsValue) -> Error {
    Error(format!("{}: {:?}", context, err))
}

impl Window {
    pub(crate) fn new(canvas: OffscreenCanvas, size: (u16, u16), title: &str) -> Result<Self, Error> {
        // The engine runs in a worker, which has no document: the caption goes to the host.
        crate::gems::set_title(title);

        // The surface's own size, not an element attribute: a transferred placeholder `<canvas>`
        // cannot have its width/height set any more (the bitmap belongs to the OffscreenCanvas, and
        // the DOM setter throws), and the host sizes the element it shows with CSS regardless. A
        // host that wants to follow the game's resolution reads it back off the surface it handed
        // in, which is the same object this writes to.
        canvas.set_width(size.0 as u32);
        canvas.set_height(size.1 as u32);

        // `antialias` defaults to `true` for `getContext("webgl2")`, which makes the browser back
        // the canvas's default framebuffer with an implicit multisample buffer. Blitting from our
        // own (non-multisampled) offscreen render target into a multisampled default framebuffer
        // is only legal when the source/destination rects are pixel-identical -- true 1:1, never
        // true when the room is scaled to fit the window (`present()` in render/webgl.rs) -- and
        // raises GL_INVALID_OPERATION otherwise. Disable it so the default framebuffer behaves
        // like the native GL context's (also non-multisampled).
        let context_options = js_sys::Object::new();
        js_sys::Reflect::set(&context_options, &"antialias".into(), &wasm_bindgen::JsValue::FALSE)
            .map_err(|e| js_err("building WebGL2 context options", e))?;
        let gl = canvas
            .get_context_with_context_options("webgl2", &context_options)
            .map_err(|e| js_err("canvas.getContext(\"webgl2\")", e))?
            .ok_or_else(|| Error("canvas.getContext(\"webgl2\") returned null (WebGL2 unsupported?)".into()))?
            .dyn_into::<WebGl2RenderingContext>()
            .map_err(|_| Error("canvas.getContext(\"webgl2\") did not return a WebGl2RenderingContext".into()))?;

        Ok(Self { canvas, gl, current_batch: Vec::new() })
    }

    /// The WebGL2 context bound to this window's surface, for the wasm renderer (render/webgl.rs).
    pub fn webgl2_context(&self) -> WebGl2RenderingContext {
        self.gl.clone()
    }

    /// Pulls any new events into the buffer, discarding whatever was in it before — matches
    /// ramen's contract (see `events()` below). What it pulls from is whatever the host has pushed
    /// since the last frame; the engine itself produces no events.
    pub fn poll_events(&mut self) {
        self.current_batch = super::input::take_pushed();
    }

    /// Returns the batch of events captured by the last `poll_events()` call.
    pub fn events(&self) -> &Vec<Event> {
        &self.current_batch
    }

    pub fn set_visible(&self, _visible: bool) {
        // Canvas visibility is the page's CSS's job, not ours.
    }

    /// Resizes the drawing surface, which is what a game's `window_set_size` and every room change
    /// ultimately mean. A host showing the surface through a placeholder element learns the new
    /// size from the surface itself; nothing in the DOM changes here.
    pub fn set_size(&self, size: (u16, u16)) {
        self.canvas.set_width(size.0 as u32);
        self.canvas.set_height(size.1 as u32);
    }

    pub fn set_title(&self, title: &str) {
        crate::gems::set_title(title);
    }

    pub fn set_borderless(&self, _borderless: bool) {}

    pub fn set_resizable(&self, _resizable: bool) {}

    pub fn set_cursor(&self, _cursor: Cursor) {}
}
