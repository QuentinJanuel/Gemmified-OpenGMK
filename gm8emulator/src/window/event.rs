// Mirrors the subset of ramen::event::Event actually matched in Game::process_window_events
// (game.rs) — that match has a `_ => ()` wildcard arm, so variants ramen has that nothing here
// reads (Focus, Maximise, Minimise, Move, Visible, KeyboardRepeat, Input(char), MouseEnter/Leave)
// are simply omitted rather than stubbed out.
//
// `FocusLost` has no ramen equivalent -- it exists to clear stuck input state (see
// Game::process_window_events) whenever the page loses focus, e.g. switching browser tabs or
// alt-tabbing away while a key is held. It does NOT reliably cover a native browser dialog
// (window.alert/confirm/prompt, used by show_message/show_question/get_string/get_integer in
// gml/kernel.rs) stealing focus while open -- `blur` isn't guaranteed to fire for that case in
// every browser (confirmed not to in headless Chromium) -- so those call sites clear input state
// directly themselves instead of relying on this event.

use super::input::{Key, MouseButton};

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Event {
    CloseRequest,
    FocusLost,
    KeyboardDown(Key),
    KeyboardUp(Key),
    MouseMove((u16, u16)),
    MouseDown(MouseButton),
    MouseUp(MouseButton),
    ScrollUp,
    ScrollDown,
    Resize((u16, u16)),
}
