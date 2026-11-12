// Mirrors ramen::input::{Key, MouseButton} exactly (same variant names) so match arms elsewhere
// in the crate (input.rs's ramen2vk/ramen2mb/TryFrom impls) compile unchanged on both targets.

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum Key {
    // common
    LeftAlt,
    RightAlt,
    Applications,
    Backspace,
    CapsLock,
    LeftControl,
    RightControl,
    Delete,
    End,
    Escape,
    Home,
    Insert,
    NumLock,
    PageDown,
    PageUp,
    Pause,
    PrintScreen,
    Return,
    ScrollLock,
    Sleep,
    Space,
    LeftShift,
    RightShift,
    LeftSuper,
    RightSuper,
    Tab,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,

    // alphanumeric
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Alpha0,
    Alpha1,
    Alpha2,
    Alpha3,
    Alpha4,
    Alpha5,
    Alpha6,
    Alpha7,
    Alpha8,
    Alpha9,

    // OEM keys which may not be present on all keyboards
    Minus,
    Plus,
    Equals,
    Period,
    Comma,
    Hash,
    BracketLeft,
    BracketRight,
    BraceLeft,
    BraceRight,
    Grave,
    Apostrophe,
    Semicolon,
    Colon,
    Underscore,
    Pipe,
    Slash,
    Backslash,
    QuestionMark,
    LessThan,
    GreaterThan,
    Quote,

    // numpad
    KeypadAdd,
    KeypadSubtract,
    KeypadMultiply,
    KeypadDivide,
    KeypadDecimal,
    KeypadSeparator,
    Keypad0,
    Keypad1,
    Keypad2,
    Keypad3,
    Keypad4,
    Keypad5,
    Keypad6,
    Keypad7,
    Keypad8,
    Keypad9,

    // arrow
    LeftArrow,
    RightArrow,
    UpArrow,
    DownArrow,

    // media
    MediaPreviousTrack,
    MediaNextTrack,
    MediaPlayPause,
    MediaStop,
    MediaVolumeDown,
    MediaVolumeUp,
    MediaVolumeMute,

    // ibm related nonsense (never produced by a browser KeyboardEvent, kept for match-arm parity)
    Attn,
    Clear,
    CrSel,
    EraseEof,
    Execute,
    ExSel,
    OemReset,
    OemJump,
    Oem102,
    OemPa1,
    OemPa2,
    OemPa3,
    OemWsCtrl,
    OemClear,
    OemCuSel,
    OemAttn,
    OemFinish,
    OemCopy,
    OemAuto,
    OemEnlw,
    OemBackTab,
    Pa1,
    Print,
    Select,
    Zoom,

    // ime input stuff
    ImeAccept,
    ImeConvert,
    ImeNonConvert,
    ImeFinal,
    ImeModeChangeRequest,
    ImeProcess,
    ImeOn,
    ImeOff,
    ImeKanaOrHangul,
    ImeHanjaOrKanji,
    ImeJunja,

    // microsoft nonsense
    BrowserBack,
    BrowserFavourites,
    BrowserForward,
    BrowserHome,
    BrowserRefresh,
    BrowserSearch,
    BrowserStop,
    Help,
    LaunchApplication1,
    LaunchApplication2,
    LaunchMail,
    LaunchMediaSelect,
    Play,
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Maps a browser `KeyboardEvent.code` (physical, locale-independent) string to our [`Key`].
/// Returns `None` for keys we have no mapping for (silently ignored by the caller).
pub(crate) fn key_from_code(code: &str) -> Option<Key> {
    Some(match code {
        "AltLeft" => Key::LeftAlt,
        "AltRight" => Key::RightAlt,
        "ContextMenu" => Key::Applications,
        "Backspace" => Key::Backspace,
        "CapsLock" => Key::CapsLock,
        "ControlLeft" => Key::LeftControl,
        "ControlRight" => Key::RightControl,
        "Delete" => Key::Delete,
        "End" => Key::End,
        "Escape" => Key::Escape,
        "Home" => Key::Home,
        "Insert" => Key::Insert,
        "NumLock" => Key::NumLock,
        "PageDown" => Key::PageDown,
        "PageUp" => Key::PageUp,
        "Pause" => Key::Pause,
        "PrintScreen" => Key::PrintScreen,
        "Enter" | "NumpadEnter" => Key::Return,
        "ScrollLock" => Key::ScrollLock,
        "Space" => Key::Space,
        "ShiftLeft" => Key::LeftShift,
        "ShiftRight" => Key::RightShift,
        "MetaLeft" | "OSLeft" => Key::LeftSuper,
        "MetaRight" | "OSRight" => Key::RightSuper,
        "Tab" => Key::Tab,

        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        "F13" => Key::F13,
        "F14" => Key::F14,
        "F15" => Key::F15,
        "F16" => Key::F16,
        "F17" => Key::F17,
        "F18" => Key::F18,
        "F19" => Key::F19,
        "F20" => Key::F20,
        "F21" => Key::F21,
        "F22" => Key::F22,
        "F23" => Key::F23,
        "F24" => Key::F24,

        "KeyA" => Key::A,
        "KeyB" => Key::B,
        "KeyC" => Key::C,
        "KeyD" => Key::D,
        "KeyE" => Key::E,
        "KeyF" => Key::F,
        "KeyG" => Key::G,
        "KeyH" => Key::H,
        "KeyI" => Key::I,
        "KeyJ" => Key::J,
        "KeyK" => Key::K,
        "KeyL" => Key::L,
        "KeyM" => Key::M,
        "KeyN" => Key::N,
        "KeyO" => Key::O,
        "KeyP" => Key::P,
        "KeyQ" => Key::Q,
        "KeyR" => Key::R,
        "KeyS" => Key::S,
        "KeyT" => Key::T,
        "KeyU" => Key::U,
        "KeyV" => Key::V,
        "KeyW" => Key::W,
        "KeyX" => Key::X,
        "KeyY" => Key::Y,
        "KeyZ" => Key::Z,

        "Digit0" => Key::Alpha0,
        "Digit1" => Key::Alpha1,
        "Digit2" => Key::Alpha2,
        "Digit3" => Key::Alpha3,
        "Digit4" => Key::Alpha4,
        "Digit5" => Key::Alpha5,
        "Digit6" => Key::Alpha6,
        "Digit7" => Key::Alpha7,
        "Digit8" => Key::Alpha8,
        "Digit9" => Key::Alpha9,

        "Minus" => Key::Minus,
        "Equal" => Key::Equals,
        "Period" => Key::Period,
        "Comma" => Key::Comma,
        "Backquote" => Key::Grave,
        "Quote" => Key::Apostrophe,
        "Semicolon" => Key::Semicolon,
        "Slash" => Key::Slash,
        "Backslash" => Key::Backslash,
        "BracketLeft" => Key::BracketLeft,
        "BracketRight" => Key::BracketRight,
        "IntlBackslash" => Key::Oem102,

        "NumpadAdd" => Key::KeypadAdd,
        "NumpadSubtract" => Key::KeypadSubtract,
        "NumpadMultiply" => Key::KeypadMultiply,
        "NumpadDivide" => Key::KeypadDivide,
        "NumpadDecimal" => Key::KeypadDecimal,
        "NumpadComma" => Key::KeypadSeparator,
        "Numpad0" => Key::Keypad0,
        "Numpad1" => Key::Keypad1,
        "Numpad2" => Key::Keypad2,
        "Numpad3" => Key::Keypad3,
        "Numpad4" => Key::Keypad4,
        "Numpad5" => Key::Keypad5,
        "Numpad6" => Key::Keypad6,
        "Numpad7" => Key::Keypad7,
        "Numpad8" => Key::Keypad8,
        "Numpad9" => Key::Keypad9,

        "ArrowLeft" => Key::LeftArrow,
        "ArrowRight" => Key::RightArrow,
        "ArrowUp" => Key::UpArrow,
        "ArrowDown" => Key::DownArrow,

        "MediaTrackPrevious" => Key::MediaPreviousTrack,
        "MediaTrackNext" => Key::MediaNextTrack,
        "MediaPlayPause" => Key::MediaPlayPause,
        "MediaStop" => Key::MediaStop,
        "AudioVolumeDown" => Key::MediaVolumeDown,
        "AudioVolumeUp" => Key::MediaVolumeUp,
        "AudioVolumeMute" => Key::MediaVolumeMute,

        "BrowserBack" => Key::BrowserBack,
        "BrowserFavorites" => Key::BrowserFavourites,
        "BrowserForward" => Key::BrowserForward,
        "BrowserHome" => Key::BrowserHome,
        "BrowserRefresh" => Key::BrowserRefresh,
        "BrowserSearch" => Key::BrowserSearch,
        "BrowserStop" => Key::BrowserStop,
        "Help" => Key::Help,
        "LaunchApp1" => Key::LaunchApplication1,
        "LaunchApp2" => Key::LaunchApplication2,
        "LaunchMail" => Key::LaunchMail,
        "LaunchMediaPlayer" | "MediaSelect" => Key::LaunchMediaSelect,
        "Sleep" => Key::Sleep,

        _ => return None,
    })
}

// -- Input pushed in by the host ------------------------------------------------------------------
//
// The engine registers no DOM listener any more. The host captures everything, normalises it and
// pushes it here as the `GameInput` of the engine contract (GEMS spec §9's `sendInput`),
// already mapped into game pixels. Two bugs went away with the listeners, each of which existed in
// one engine and was fixed in the other: auto-repeats are dropped before they arrive (GameMaker
// derives "pressed" from an edge on the held state, so a repeat read as a fresh press), and a
// release that happened while something else had focus arrives as `focus-lost`.
//
// **The queue is a thread-local rather than a field, on purpose.** A host pushes from a listener,
// which fires whenever the player does something -- including in the middle of a frame, where an
// export taking `&self` would take a borrow of a `WasmGame` the running `step` already holds and
// panic ("recursive use of an object"). Nothing here touches the game at all: it queues, and the
// next `poll_events` reads the queue.

use super::event::Event;
use std::{cell::RefCell, collections::VecDeque};
use wasm_bindgen::JsValue;

thread_local! {
    static PUSHED: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
}

/// Everything pushed since the last call, oldest first. `Window::poll_events` is the only caller:
/// draining on the pull is what replicates ramen's "one batch per frame" contract.
pub(crate) fn take_pushed() -> Vec<Event> {
    PUSHED.with(|queue| queue.borrow_mut().drain(..).collect())
}

/// Drops anything queued but never read. Called when a game is constructed, so a key pushed at the
/// end of one game's life is not delivered to the next one loaded into the same module.
pub(crate) fn clear_pushed() {
    PUSHED.with(|queue| queue.borrow_mut().clear());
}

fn enqueue(event: Event) {
    PUSHED.with(|queue| queue.borrow_mut().push_back(event));
}

fn field(input: &JsValue, key: &str) -> JsValue {
    js_sys::Reflect::get(input, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

fn coordinate(input: &JsValue, key: &str) -> Option<u16> {
    // Out-of-range rather than clamped: GML lets a game see a cursor outside the room and games read
    // it, but `Event::MouseMove` carries `u16`s, so a position this type cannot express is dropped
    // rather than reported as a different one. Negative coordinates (a pointer above or left of the
    // picture) are the common case here.
    let value = field(input, key).as_f64()?;
    if (0.0..=f64::from(u16::MAX)).contains(&value) { Some(value as u16) } else { None }
}

fn button(input: &JsValue) -> Option<MouseButton> {
    match field(input, "button").as_string()?.as_str() {
        "left" => Some(MouseButton::Left),
        "middle" => Some(MouseButton::Middle),
        "right" => Some(MouseButton::Right),
        _ => None,
    }
}

/// Translates one `GameInput` and queues it. Returns `false` for an event this engine has no
/// meaning for -- an unknown `kind`, an unmapped key, a button GameMaker does not have -- which the
/// caller reports rather than failing the frame over.
pub(crate) fn push_from_host(input: &JsValue) -> bool {
    let Some(kind) = field(input, "kind").as_string() else { return false };
    match kind.as_str() {
        "key-down" | "key-up" => {
            let Some(code) = field(input, "code").as_string() else { return false };
            // An unmapped key is not an error: `key_from_code` covers what GameMaker has keys for,
            // and the host forwards every physical key it sees.
            let Some(key) = key_from_code(&code) else { return true };
            enqueue(if kind == "key-down" { Event::KeyboardDown(key) } else { Event::KeyboardUp(key) });
        },
        "pointer-move" | "pointer-down" => {
            // The position first, and a press carries one too: a game reading `mouse_x` in its
            // press event must see where the click landed rather than where the pointer was last
            // seen moving. A position outside what `Event::MouseMove` can carry is simply not
            // reported -- the pointer is off the picture, which is ordinary, so the event is
            // understood and dropped rather than refused.
            if let (Some(x), Some(y)) = (coordinate(input, "x"), coordinate(input, "y")) {
                enqueue(Event::MouseMove((x, y)));
            }
            if kind == "pointer-down" {
                let Some(button) = button(input) else { return false };
                enqueue(Event::MouseDown(button));
            }
        },
        "pointer-up" => {
            let Some(button) = button(input) else { return false };
            enqueue(Event::MouseUp(button));
        },
        // Only the sign reaches a game: GameMaker has a wheel-up key and a wheel-down key and
        // nothing in between.
        "wheel" => match field(input, "delta").as_f64() {
            Some(delta) if delta < 0.0 => enqueue(Event::ScrollUp),
            Some(delta) if delta > 0.0 => enqueue(Event::ScrollDown),
            _ => return false,
        },
        "focus-lost" => enqueue(Event::FocusLost),
        _ => return false,
    }
    true
}
