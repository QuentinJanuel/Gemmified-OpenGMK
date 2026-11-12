// Facade over the platform windowing/input layer. Natively this is just `ramen` (window creation,
// event pump, key/button types). On wasm32 there is no OS window at all — `window`/`connection`/
// `event`/`input` are hand-rolled stand-ins bound to a `<canvas>`, matching only the subset of
// ramen's API surface actually used elsewhere in the crate (see each wasm submodule for specifics).

#[cfg(not(target_arch = "wasm32"))]
pub use ramen::{connection, event, input, window};

#[cfg(target_arch = "wasm32")]
pub mod connection;
#[cfg(target_arch = "wasm32")]
pub mod event;
#[cfg(target_arch = "wasm32")]
pub mod input;
#[cfg(target_arch = "wasm32")]
pub mod window;
