//! Direct3D 9 shader model 3 bytecode to GLSL ES 3.00.
//!
//! GameMaker 8.2's `shader_pixel_create_file` takes *compiled* D3D9 bytecode -- `fxc` output, shader
//! model 3, either as a `.ps3`/`.vs3` file or base64'd into the game -- so running a GM8.2 game's
//! shaders in WebGL2 means translating that bytecode at load time. That is what every compatibility
//! layer from Wine to DXVK to MojoShader does, and it is also the only approach that works for a
//! shader a game supplies itself, which `shader_pixel_create_base64` exists precisely to allow.
//!
//! ```no_run
//! let shader = sm3::translate(&std::fs::read("ps_pass.ps3").unwrap()).unwrap();
//! println!("{}", shader.glsl);
//! ```
//!
//! **This crate deliberately knows nothing about the emulator.** Its input is a byte slice and its
//! output is a string plus a description of the constant table -- no `Game`, no renderer, no wasm,
//! no `gml::Value`. That is what lets `cargo test` cover it in full on a machine with no GPU, which
//! makes it the one part of this workspace that is properly tested rather than merely compiled.
//!
//! It refuses rather than guesses. An opcode, modifier or register file that has not been checked
//! against what it means produces an `Error`, so the engine can answer "this shader did not load"
//! and the game falls back to its fixed-function look -- which is a far better outcome for a player
//! than a shader that renders confidently wrong.

mod bytecode;
mod ctab;
mod glsl;

pub use bytecode::{SamplerKind, Stage};
pub use ctab::{Constant, ConstantTable, RegisterSet};
pub use glsl::{Attribute, Sampler, Shader};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not a D3D9 shader at all -- no recognisable version token.
    NotShaderBytecode,
    /// A shader model this crate does not read. Below 3.0 is not a subset: shader model 1 has its
    /// own texture-addressing instruction set and no declared inputs.
    UnsupportedVersion {
        major: u8,
        minor: u8,
    },
    /// An opcode that has not been checked against its documented semantics.
    UnsupportedOpcode(u16),
    /// A feature that is understood but not implemented.
    UnsupportedFeature(&'static str),
    /// The token stream ended mid-instruction.
    UnexpectedEnd,
    /// An instruction whose operands did not match its opcode.
    MalformedInstruction(u16),
    /// `if`/`rep` without a matching `endif`/`endrep`.
    UnbalancedControlFlow,
    MalformedConstantTable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotShaderBytecode => write!(f, "not Direct3D 9 shader bytecode"),
            Self::UnsupportedVersion { major, minor } => {
                write!(f, "unsupported shader model {}.{} (only 3.0 is translated)", major, minor)
            },
            Self::UnsupportedOpcode(opcode) => write!(f, "unsupported opcode {:#x}", opcode),
            Self::UnsupportedFeature(what) => write!(f, "unsupported: {}", what),
            Self::UnexpectedEnd => write!(f, "bytecode ended mid-instruction"),
            Self::MalformedInstruction(opcode) => write!(f, "malformed instruction {:#x}", opcode),
            Self::UnbalancedControlFlow => write!(f, "unbalanced control flow"),
            Self::MalformedConstantTable => write!(f, "malformed constant table"),
        }
    }
}

impl std::error::Error for Error {}

/// Translates one compiled shader.
pub fn translate(bytes: &[u8]) -> Result<Shader, Error> {
    let program = bytecode::parse(bytes)?;
    // A shader with no constant table still translates; it just has no names to offer, so the
    // uniform array is sized from what the instructions reach instead.
    let constants = match &program.constant_table {
        Some(data) => ctab::parse(data)?,
        None => ConstantTable::default(),
    };
    glsl::translate(&program, constants)
}
