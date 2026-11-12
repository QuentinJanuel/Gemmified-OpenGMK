//! The `CTAB` constant table: the block `fxc` hides in a comment token so a compiled shader still
//! knows what its own constant registers are called.
//!
//! This is what makes GM8.2's uniform API answerable. `shader_vertex_uniform_f(address, ...)` takes
//! an *address* the game got from `shader_vertex_uniform_get_address(shader, "lightpos")`, so
//! without this table there is no way to turn that name into the register the game meant, and the
//! engine's only honest answer is "no such uniform". The `0` it hands back today is exactly the
//! placebo this replaces.
//!
//! Layout, from `D3DXSHADER_CONSTANTTABLE` / `D3DXSHADER_CONSTANTINFO`. Every offset in the table
//! is a byte offset from the start of the table itself, which is the word *after* the `CTAB`
//! fourcc -- so the slice this module is handed must begin there and nowhere else.

use crate::Error;

/// Which register file a constant lives in. The float file is the one that matters; the others are
/// carried because a shader that declares them and gets silence back would be just as misleading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegisterSet {
    Bool,
    Int4,
    Float4,
    Sampler,
}

/// One named entry: what a game asks for by name and sets by register.
#[derive(Clone, Debug)]
pub struct Constant {
    pub name: String,
    pub set: RegisterSet,
    /// First register, and how many consecutive ones it occupies -- a `float4x4` is four.
    pub register_index: u32,
    pub register_count: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ConstantTable {
    pub constants: Vec<Constant>,
    /// The compile target string (`vs_3_0`), kept because it is the one field that can contradict
    /// the version token and is therefore worth being able to compare.
    pub target: Option<String>,
}

impl ConstantTable {
    /// One past the highest float register any constant occupies, which is how big the uniform
    /// array backing `c#` has to be.
    ///
    /// From the table rather than from the instruction stream on purpose: a shader that indexes
    /// `c[a0.x + 20]` never names its highest register anywhere an instruction walk could see, and
    /// sizing the array from what the walk found would cut the bone matrices off at whatever the
    /// first hardcoded index happened to be.
    pub fn float_register_count(&self) -> u32 {
        self.constants
            .iter()
            .filter(|c| c.set == RegisterSet::Float4)
            .map(|c| c.register_index + c.register_count)
            .max()
            .unwrap_or(0)
    }

    pub fn find(&self, name: &str) -> Option<&Constant> {
        self.constants.iter().find(|c| c.name == name)
    }
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32, Error> {
    data.get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(Error::MalformedConstantTable)
}

fn u16_at(data: &[u8], offset: usize) -> Result<u16, Error> {
    data.get(offset..offset + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or(Error::MalformedConstantTable)
}

/// A NUL-terminated string at a byte offset. The table stores every name this way, packed after the
/// fixed-size records.
fn string_at(data: &[u8], offset: usize) -> Result<String, Error> {
    let tail = data.get(offset..).ok_or(Error::MalformedConstantTable)?;
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    // Names come from HLSL identifiers, so they are ASCII in practice; decode leniently rather than
    // reject a whole shader over one byte.
    Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
}

pub fn parse(data: &[u8]) -> Result<ConstantTable, Error> {
    // size, creator, version, constant count, constant info offset, flags, target
    if data.len() < 28 {
        return Err(Error::MalformedConstantTable);
    }
    let count = u32_at(data, 12)? as usize;
    let info_offset = u32_at(data, 16)? as usize;
    let target = string_at(data, u32_at(data, 24)? as usize).ok();

    let mut constants = Vec::with_capacity(count);
    for i in 0..count {
        // Each D3DXSHADER_CONSTANTINFO is 20 bytes: name offset, register set (u16), register index
        // (u16), register count (u16), reserved (u16), type info offset, default value offset.
        let base = info_offset + i * 20;
        let name = string_at(data, u32_at(data, base)? as usize)?;
        let set = match u16_at(data, base + 4)? {
            0 => RegisterSet::Bool,
            1 => RegisterSet::Int4,
            2 => RegisterSet::Float4,
            3 => RegisterSet::Sampler,
            _ => return Err(Error::MalformedConstantTable),
        };
        constants.push(Constant {
            name,
            set,
            register_index: u32::from(u16_at(data, base + 6)?),
            register_count: u32::from(u16_at(data, base + 8)?),
        });
    }

    Ok(ConstantTable { constants, target })
}
