//! `gm82gltf.dll`, GameMaker 8.2's glTF loader -- not implemented, but not left to the generic
//! no-op either, because the value that no-op returns is a lie this particular API acts on.
//!
//! The extension loads a `.glb`/`.gltf` scene and draws it through vertex buffers and its own
//! compiled shaders (`gltf_vertex.vs3`, `gltf_pixel.ps3`). Its GML entry point opens with:
//!
//! ```text
//! __gltf = __gltf_load(working_directory, argument0)
//! if (__gltf < 0) return __gltf         // the API's own "this did not load"
//! ```
//!
//! `external.rs`'s fallback answers **0**, which is a perfectly good handle, so the wrapper walks
//! straight past its own failure check and builds a scene out of a file that was never read: every
//! count comes back 0, every id it hands the game is bogus, and the first thing to index an array
//! with one dies. In this state the game is not "missing its glTF models", it is corrupt.
//!
//! So the one function here reports the failure the interface already has a way to express. The
//! rest of the 91 exports stay on the warn-once dummy, which is honest for them: nothing reaches
//! those once the load has said no.
//!
//! Implementing this for real is a project rather than a mapping. Parsing glTF is the easy half (a
//! pure-Rust crate would build for wasm); the rendering half runs on shader-model-3 bytecode, the
//! same wall `gm82dx9.rs`'s shader block documents.

use crate::{
    game::Game,
    gml::{self, Context, Function, Value},
};

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;

    /// `__gltf_load(working_directory, filename)` -- negative means the scene did not load, which
    /// is the truth here and stops the GML wrapper before it invents ids for it.
    fn load_failed(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        Ok(Value::Real((-1).into()))
    }

    pub fn resolve(symbol: &str) -> Option<Function> {
        match symbol {
            "__gltf_load" => Some(Function::Runtime(load_failed as super::ContextFn)),
            _ => None,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;

    pub fn resolve(_symbol: &str) -> Option<Function> {
        None
    }
}

// The bare fn-pointer type `Function::Runtime` wraps (its own `ContextFunctionPtr` type alias in
// `gml.rs` is private, so this names the same signature structurally instead of by that name).
type ContextFn = fn(&mut Game, &mut Context, &[Value]) -> gml::Result<Value>;

pub use imp::resolve;
