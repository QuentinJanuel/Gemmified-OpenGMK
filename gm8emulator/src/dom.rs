//! The browser globals the engine still reaches for itself.
//!
//! The host owns every event listener -- input and the audio-unlock gesture alike -- so what is
//! left is one question about the realm the engine was loaded into, and nothing that outlives a
//! call.

use std::cell::RefCell;
use wasm_bindgen::JsCast;

/// `performance`, from whichever global scope the engine was loaded into.
///
/// Not `web_sys::window()`: that is `None` in a worker, which has no `Window` at all, and every
/// caller here would silently fall back to a clock stuck at zero -- `current_time`, `get_timer`
/// and the frame pacing's own deadline among them. Every global scope does have a `performance`,
/// so ask `globalThis` for it instead of asking the document's side of the platform.
///
/// Resolved once: the monotonic clock is read several times a frame, and GML's `sleep()` spins on
/// it.
pub fn performance() -> Option<web_sys::Performance> {
    thread_local! {
        static PERFORMANCE: RefCell<Option<Option<web_sys::Performance>>> = const { RefCell::new(None) };
    }
    PERFORMANCE.with(|cell| {
        cell.borrow_mut()
            .get_or_insert_with(|| {
                js_sys::Reflect::get(&js_sys::global(), &wasm_bindgen::JsValue::from_str("performance"))
                    .ok()
                    .and_then(|value| value.dyn_into::<web_sys::Performance>().ok())
            })
            .clone()
    })
}
