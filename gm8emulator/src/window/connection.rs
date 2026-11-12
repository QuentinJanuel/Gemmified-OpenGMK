use super::window::{Controls, Window};
use std::{cell::RefCell, fmt};
use web_sys::OffscreenCanvas;

thread_local! {
    /// The drawing surface a host handed this engine, parked between `hand_in_surface` and the
    /// `Builder::build` inside `Game::launch`.
    ///
    /// The engine does not look a canvas up by id: it is given one, which is what lets it render
    /// in a worker, where there is no document to look anything up in. An `OffscreenCanvas` either
    /// stands on its own or is a transferred placeholder `<canvas>`; the renderer cannot tell the
    /// difference, and a host that transferred one keeps showing whatever is drawn into it.
    ///
    /// A thread-local rather than an argument because the builder chain in `Game::launch` is shared
    /// with native, where the window comes from ramen and no canvas is named anywhere along it. One
    /// game per module instance, so there is never a second surface waiting its turn.
    static HANDED_IN: RefCell<Option<OffscreenCanvas>> = const { RefCell::new(None) };
}

/// Hands the engine the surface its next `Game::launch` will render into. Called by `wasm_entry`
/// immediately before launching, with what the host passed in.
pub fn hand_in_surface(canvas: OffscreenCanvas) {
    HANDED_IN.with(|cell| *cell.borrow_mut() = Some(canvas));
}

fn take_surface() -> Option<OffscreenCanvas> {
    HANDED_IN.with(|cell| cell.borrow_mut().take())
}

#[derive(Debug)]
pub struct Error(pub(crate) String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Error {}

/// Stand-in for ramen::connection::Connection. There's no OS display/connection concept in a
/// browser, so this only exists to give `Builder`/`Renderer::new` something to hang off of.
#[derive(Clone)]
pub struct Connection;

impl Connection {
    pub fn new() -> Result<Self, Error> {
        Ok(Self)
    }

    pub fn builder(&self) -> Builder {
        Builder::new()
    }
}

/// Stand-in for ramen::window::Builder. Everything except `size`/`title` is a pure no-op accepted
/// only so the exact same builder chain in Game::launch (game.rs) compiles unchanged.
pub struct Builder {
    size: (u16, u16),
    title: String,
}

impl Builder {
    fn new() -> Self {
        Self { size: (800, 600), title: String::from("a nice window") }
    }

    pub fn class_name<T: Into<String>>(self, _class_name: T) -> Self {
        self
    }

    pub fn visible(self, _visible: bool) -> Self {
        self
    }

    pub fn size(mut self, size: (u16, u16)) -> Self {
        self.size = size;
        self
    }

    pub fn borderless(self, _borderless: bool) -> Self {
        self
    }

    pub fn title<T: Into<String>>(mut self, title: T) -> Self {
        self.title = title.into();
        self
    }

    pub fn resizable(self, _resizable: bool) -> Self {
        self
    }

    pub fn controls(self, _controls: Option<Controls>) -> Self {
        self
    }

    pub fn build(self) -> Result<Window, Error> {
        let canvas = take_surface()
            .ok_or_else(|| Error("no drawing surface was handed in before the game was launched".into()))?;
        Window::new(canvas, self.size, &self.title)
    }
}
