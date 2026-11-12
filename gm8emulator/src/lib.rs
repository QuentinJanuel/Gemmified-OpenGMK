#![cfg(target_arch = "wasm32")]

// This lib target exists solely to produce the wasm32 cdylib for the browser build. The native CLI binary (main.rs) declares its own, independent copy of the same module
// tree rather than depending on this crate -- see main.rs's top-of-file comment for why.

mod action;
mod asset;
mod audio_cache_bridge;
mod dialog;
mod dom;
mod fullscreen;
mod game;
mod gems;
mod gml;
mod handleman;
mod input;
mod instance;
mod instancelist;
mod loopback;
mod math;
mod render;
mod spatial_grid;
mod tile;
mod types;
mod util;
mod wasm_entry;
mod window;

pub use wasm_entry::WasmGame;
