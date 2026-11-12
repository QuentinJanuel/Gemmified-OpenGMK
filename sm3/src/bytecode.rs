//! The Direct3D 9 bytecode container: a version token, a flat stream of instruction tokens, and an
//! `end`. Everything here is decoding little-endian words into the shapes `glsl.rs` emits from --
//! no GLSL, no opinions about what an instruction *means*.
//!
//! Layouts are from the `d3d9types.h` `D3DSI_*`/`D3DSP_*` masks. They are worth stating rather than
//! citing, because two of them are genuinely surprising:
//!
//! - **A register type is split across two bit ranges.** Three bits at 28 and two more at 11, so a
//!   naive `(token >> 28) & 7` silently reads sampler registers as temporaries.
//! - **Instruction length is in the token**, in DWORDs, at bits 24..27 -- which is what makes it
//!   possible to skip an instruction this crate does not understand without decoding it. Comment
//!   blocks carry their own 15-bit length instead, at bits 16..30.

use crate::Error;

/// `vs` or `ps`. The two differ in more than their name: the register files are not the same, and
/// neither are the output rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Vertex,
    Pixel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Version {
    pub stage: Stage,
    pub major: u8,
    pub minor: u8,
}

/// A register file. The names are the ones the D3D assembly listing uses, because that is what
/// anyone reading a disassembly next to this code will see.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegisterType {
    /// `r#` -- temporary.
    Temp,
    /// `v#` -- a vertex attribute in a vertex shader, an interpolated varying in a pixel shader.
    Input,
    /// `c#` -- float constant, the register file the whole uniform API addresses.
    Const,
    /// `a#` in a vertex shader (the address register `mova` writes); `t#` in a pixel shader.
    AddrOrTexture,
    /// `oPos`/`oFog`/`oPts` -- the fixed rasteriser outputs of shader models below 3.
    RastOut,
    /// `oD#` -- the colour outputs of shader models below 3.
    AttrOut,
    /// `o#` -- shader model 3's single, declared output file (and `oT#` below it).
    Output,
    /// `i#` -- integer constant, written by `defi` and read by `rep`.
    ConstInt,
    /// `oC#` -- pixel shader colour output.
    ColorOut,
    /// `oDepth` -- pixel shader depth output.
    DepthOut,
    /// `s#` -- sampler.
    Sampler,
    /// `b#` -- boolean constant, written by `defb`.
    ConstBool,
    /// `aL` -- the loop counter `rep`/`loop` maintains.
    Loop,
    /// `p#` -- predicate.
    Predicate,
    /// `vPos`/`vFace` -- a pixel shader's screen position and facing, which shader model 3 added as
    /// a register file of its own rather than as a semantic.
    Misc,
    /// Anything else the format allows and this crate has no name for.
    Other(u8),
}

impl RegisterType {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Temp,
            1 => Self::Input,
            2 => Self::Const,
            3 => Self::AddrOrTexture,
            4 => Self::RastOut,
            5 => Self::AttrOut,
            6 => Self::Output,
            7 => Self::ConstInt,
            8 => Self::ColorOut,
            9 => Self::DepthOut,
            10 => Self::Sampler,
            14 => Self::ConstBool,
            15 => Self::Loop,
            17 => Self::Misc,
            19 => Self::Predicate,
            other => Self::Other(other),
        }
    }
}

/// What a source operand does to its value before the instruction sees it. Only the three that
/// occur in real shader-model-3 output are named; the rest belong to shader model 1's texture
/// addressing and are rejected rather than guessed at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceModifier {
    None,
    Negate,
    Abs,
    AbsNegate,
}

#[derive(Clone, Copy, Debug)]
pub struct Register {
    pub kind: RegisterType,
    pub index: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Dest {
    pub register: Register,
    /// Which of `xyzw` the instruction writes, as a 4-bit mask. Never 0 in valid bytecode.
    pub write_mask: u8,
    pub saturate: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Source {
    pub register: Register,
    /// Two bits per destination component, low bits first: which source component feeds `x`, then
    /// `y`, and so on. `0xE4` is the identity, `xyzw`.
    pub swizzle: u8,
    pub modifier: SourceModifier,
    /// `c[a0.x + n]`. Carries the register the index is read from and which of its components --
    /// this is how a skinning shader reaches its bone matrices, so it is not an exotic corner.
    pub relative: Option<RelativeAddress>,
}

#[derive(Clone, Copy, Debug)]
pub struct RelativeAddress {
    pub register: Register,
    /// Which component of the address register to read, 0..=3.
    pub component: u8,
}

/// One decoded instruction. `Dcl`/`Def`/`DefI`/`DefB` are declarations rather than code and carry
/// their payload directly; everything else is an opcode plus operands, and `glsl.rs` decides what
/// to do with it.
#[derive(Clone, Debug)]
pub enum Instruction {
    Dcl { usage: DclUsage, dest: Dest },
    Def { register: u32, value: [f32; 4] },
    DefI { register: u32, value: [i32; 4] },
    DefB { register: u32, value: bool },
    Op { opcode: Opcode, control: u8, dest: Option<Dest>, sources: Vec<Source> },
}

/// What a `dcl` says the register is for.
///
/// A vertex shader declares its inputs and outputs by *semantic* -- position, texcoord 3, colour 1
/// -- and the pixel shader declares the same semantics on the way in. Matching the two stages up by
/// semantic rather than by register number is what makes a vertex and pixel shader that were
/// compiled separately link together, which is the whole reason D3D9 can pair them freely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DclUsage {
    /// A vertex attribute or varying: the D3D semantic (`D3DDECLUSAGE`) plus its index.
    Semantic { usage: u8, index: u8 },
    /// A pixel shader sampler declaration, carrying the texture type it will be read as.
    Sampler(SamplerKind),
    /// A pixel shader input with no semantic of its own (`vPos`, `vFace`).
    Misc,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SamplerKind {
    TwoD,
    Cube,
    Volume,
}

/// The opcodes this crate understands. Anything outside the list is an `Error::UnsupportedOpcode`
/// rather than a guess: emitting plausible-looking GLSL for an instruction whose semantics were not
/// checked produces a shader that renders confidently wrong, which is worse for a player than a
/// game that falls back to its fixed-function look.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Opcode {
    Mov,
    Mova,
    Add,
    Sub,
    Mul,
    Mad,
    Lrp,
    Dp3,
    Dp4,
    Min,
    Max,
    Slt,
    Sge,
    Rcp,
    Rsq,
    Frc,
    Exp,
    Log,
    Abs,
    Nrm,
    Pow,
    Cmp,
    SinCos,
    TexLd,
    TexLdL,
    TexKill,
    Dp2Add,
    Break,
    BreakC,
    Rep,
    EndRep,
    If,
    IfC,
    Else,
    EndIf,
    Nop,
}

impl Opcode {
    fn from_bits(bits: u16) -> Option<Self> {
        Some(match bits {
            0 => Self::Nop,
            1 => Self::Mov,
            2 => Self::Add,
            3 => Self::Sub,
            4 => Self::Mad,
            5 => Self::Mul,
            6 => Self::Rcp,
            7 => Self::Rsq,
            8 => Self::Dp3,
            9 => Self::Dp4,
            10 => Self::Min,
            11 => Self::Max,
            12 => Self::Slt,
            13 => Self::Sge,
            14 => Self::Exp,
            15 => Self::Log,
            18 => Self::Lrp,
            19 => Self::Frc,
            32 => Self::Pow,
            35 => Self::Abs,
            36 => Self::Nrm,
            37 => Self::SinCos,
            38 => Self::Rep,
            39 => Self::EndRep,
            40 => Self::If,
            41 => Self::IfC,
            42 => Self::Else,
            43 => Self::EndIf,
            46 => Self::Mova,
            44 => Self::Break,
            45 => Self::BreakC,
            65 => Self::TexKill,
            66 => Self::TexLd,
            95 => Self::TexLdL,
            88 => Self::Cmp,
            90 => Self::Dp2Add,
            _ => return None,
        })
    }

    /// How many source operands the instruction takes, as an inclusive range. Used to sanity-check
    /// what was decoded, so a misread length turns into an error here instead of nonsense GLSL.
    ///
    /// A range rather than a number because of `sincos`: shader model 2 passes it two extra
    /// constant registers holding the Taylor coefficients the hardware of the day needed, and 3.0
    /// dropped them. The extra two are ignored either way -- GLSL has `sin` and `cos`.
    pub fn source_count(self) -> (usize, usize) {
        let exact = |n| (n, n);
        match self {
            Self::SinCos => (1, 3),
            Self::TexKill => exact(0),
            Self::Break => exact(0),
            Self::BreakC => exact(2),
            Self::Dp2Add => exact(3),
            Self::Nop | Self::Else | Self::EndIf | Self::EndRep => exact(0),
            Self::Mov
            | Self::Mova
            | Self::Rcp
            | Self::Rsq
            | Self::Frc
            | Self::Exp
            | Self::Log
            | Self::Abs
            | Self::Nrm
            | Self::Rep
            | Self::If => exact(1),
            Self::Add
            | Self::Sub
            | Self::Mul
            | Self::Dp3
            | Self::Dp4
            | Self::Min
            | Self::Max
            | Self::Slt
            | Self::Sge
            | Self::Pow
            | Self::TexLd
            | Self::TexLdL
            | Self::IfC => exact(2),
            Self::Mad | Self::Lrp | Self::Cmp => exact(3),
        }
    }

    /// Whether the instruction takes a destination token. `texkill`'s operand is encoded as one
    /// even though it only reads it, which is why this is about the encoding, not about writing.
    pub fn has_dest(self) -> bool {
        !matches!(
            self,
            Self::Nop
                | Self::Rep
                | Self::EndRep
                | Self::If
                | Self::IfC
                | Self::Else
                | Self::EndIf
                | Self::Break
                | Self::BreakC
        )
    }
}

/// The whole decoded program, plus the raw `CTAB` block for `ctab.rs` to read separately.
pub struct Program {
    pub version: Version,
    pub instructions: Vec<Instruction>,
    pub constant_table: Option<Vec<u8>>,
}

const OPCODE_MASK: u32 = 0x0000_FFFF;
const OPCODE_COMMENT: u32 = 0xFFFE;
const OPCODE_END: u32 = 0xFFFF;
const INSTRUCTION_PREDICATED: u32 = 0x1000_0000;

fn register(token: u32) -> Register {
    // The two halves of the register type, at bits 28..30 and 11..12. Splitting a field like this
    // is what lets `D3DSP_REGTYPE` grow past 8 values without moving anything already encoded.
    let kind = (((token >> 28) & 0x7) | ((token >> 8) & 0x18)) as u8;
    Register { kind: RegisterType::from_bits(kind), index: token & 0x7FF }
}

/// Reads a token stream, one word at a time, so every decode step can say where it ran out.
struct Reader<'a> {
    words: &'a [u32],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn next(&mut self) -> Result<u32, Error> {
        let word = self.words.get(self.pos).copied().ok_or(Error::UnexpectedEnd)?;
        self.pos += 1;
        Ok(word)
    }

    fn peek(&self) -> Option<u32> {
        self.words.get(self.pos).copied()
    }
}

pub fn parse(bytes: &[u8]) -> Result<Program, Error> {
    if bytes.len() < 8 || bytes.len() % 4 != 0 {
        return Err(Error::NotShaderBytecode);
    }
    let words: Vec<u32> = bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();

    let mut reader = Reader { words: &words, pos: 0 };
    let version_token = reader.next()?;
    let stage = match version_token >> 16 {
        0xFFFE => Stage::Vertex,
        0xFFFF => Stage::Pixel,
        _ => return Err(Error::NotShaderBytecode),
    };
    let version = Version { stage, major: ((version_token >> 8) & 0xFF) as u8, minor: (version_token & 0xFF) as u8 };
    // Shader models 2 and 3. Not 1.x, which is genuinely a different language: its own
    // texture-addressing instruction set, no declared inputs, and no constant table worth the name.
    //
    // 2 earns its place rather than being thrown in. It is *most* of what real games ship -- 37 of
    // the 48 shaders inside Delicious Level Design Contest are `ps_2_1`/`vs_2_1`, against 11 at 3.0
    // -- because `fxc` targets the lowest model a shader's features allow, and a screen-space
    // effect rarely needs 3.0. Refusing 2 would mean refusing three quarters of the shaders seen.
    if !(2..=3).contains(&version.major) {
        return Err(Error::UnsupportedVersion { major: version.major, minor: version.minor });
    }

    let mut instructions = Vec::new();
    let mut constant_table = None;

    loop {
        let token = match reader.peek() {
            Some(token) => token,
            // Running out of words without an `end` is a truncated file, but everything decoded so
            // far is still valid, so treat it the same as `end` rather than throwing it away.
            None => break,
        };
        let opcode_bits = token & OPCODE_MASK;

        if opcode_bits == OPCODE_END {
            break;
        }

        if opcode_bits == OPCODE_COMMENT {
            reader.next()?;
            let length = ((token >> 16) & 0x7FFF) as usize;
            let start = reader.pos;
            let end = start.checked_add(length).ok_or(Error::UnexpectedEnd)?;
            if end > words.len() {
                return Err(Error::UnexpectedEnd);
            }
            // The constant table rides in a comment block, which is the only reason a stripped
            // shader still knows its own uniform names. `fxc` writes at most one.
            if length >= 2 && words[start] == u32::from_le_bytes(*b"CTAB") {
                constant_table = Some(bytes[(start + 1) * 4..end * 4].to_vec());
            }
            reader.pos = end;
            continue;
        }

        reader.next()?;
        if token & INSTRUCTION_PREDICATED != 0 {
            // `(!p0) instr ...` -- a per-component predicate on top of the write mask. Rare in fxc
            // output, and getting it silently wrong would mean writing components the shader asked
            // to leave alone.
            return Err(Error::UnsupportedFeature("predicated instruction"));
        }

        let length = ((token >> 24) & 0xF) as usize;
        let operand_end = reader.pos + length;
        if operand_end > words.len() {
            return Err(Error::UnexpectedEnd);
        }

        let opcode_bits = opcode_bits as u16;
        let instruction = match opcode_bits {
            31 => {
                // dcl: a usage token, then the register being declared.
                let usage_token = reader.next()?;
                let dest = read_dest(&mut reader)?;
                Instruction::Dcl { usage: dcl_usage(usage_token, stage, dest.register), dest }
            },
            81 => {
                let dest = read_dest(&mut reader)?;
                let mut value = [0.0f32; 4];
                for slot in value.iter_mut() {
                    *slot = f32::from_bits(reader.next()?);
                }
                Instruction::Def { register: dest.register.index, value }
            },
            48 => {
                let dest = read_dest(&mut reader)?;
                let mut value = [0i32; 4];
                for slot in value.iter_mut() {
                    *slot = reader.next()? as i32;
                }
                Instruction::DefI { register: dest.register.index, value }
            },
            47 => {
                let dest = read_dest(&mut reader)?;
                let value = reader.next()? != 0;
                Instruction::DefB { register: dest.register.index, value }
            },
            _ => {
                let opcode = Opcode::from_bits(opcode_bits).ok_or(Error::UnsupportedOpcode(opcode_bits))?;
                let dest = if opcode.has_dest() { Some(read_dest(&mut reader)?) } else { None };
                let mut sources = Vec::with_capacity(opcode.source_count().1);
                while reader.pos < operand_end {
                    sources.push(read_source(&mut reader)?);
                }
                let (min_sources, max_sources) = opcode.source_count();
                if !(min_sources..=max_sources).contains(&sources.len()) {
                    return Err(Error::MalformedInstruction(opcode_bits));
                }
                Instruction::Op { opcode, control: ((token >> 16) & 0x7) as u8, dest, sources }
            },
        };

        // Trust the declared length over the operand walk: a `dcl` in particular carries a token
        // this decoder reads and one it does not, and skipping to the end keeps the stream aligned
        // regardless.
        reader.pos = operand_end;
        instructions.push(instruction);
    }

    Ok(Program { version, instructions, constant_table })
}

fn read_dest(reader: &mut Reader) -> Result<Dest, Error> {
    let token = reader.next()?;
    let write_mask = ((token >> 16) & 0xF) as u8;
    let modifier = (token >> 20) & 0xF;
    // Bit 0 is `_sat`. Bit 1 is `_pp` (partial precision), which is a hint about how much precision
    // the hardware may drop and has no GLSL equivalent worth inventing -- ignoring it is what every
    // translator does. Bit 2 is the multisample centroid hint, equally inert here.
    if modifier & !0b111 != 0 {
        return Err(Error::UnsupportedFeature("destination modifier"));
    }
    // The `_x2`/`_d2` result shifts of shader model 1. fxc does not emit them for SM3.
    if (token >> 24) & 0xF != 0 {
        return Err(Error::UnsupportedFeature("destination result shift"));
    }
    Ok(Dest { register: register(token), write_mask, saturate: modifier & 0b1 != 0 })
}

fn read_source(reader: &mut Reader) -> Result<Source, Error> {
    let token = reader.next()?;
    let modifier = match (token >> 24) & 0xF {
        0 => SourceModifier::None,
        1 => SourceModifier::Negate,
        11 => SourceModifier::Abs,
        12 => SourceModifier::AbsNegate,
        _ => return Err(Error::UnsupportedFeature("source modifier")),
    };
    let relative = if token & 0x2000 != 0 {
        let address_token = reader.next()?;
        Some(RelativeAddress { register: register(address_token), component: ((address_token >> 16) & 0x3) as u8 })
    } else {
        None
    };
    Ok(Source { register: register(token), swizzle: ((token >> 16) & 0xFF) as u8, modifier, relative })
}

fn dcl_usage(token: u32, stage: Stage, declared: Register) -> DclUsage {
    // The same token means two different things depending on what is being declared: a sampler
    // declaration puts the texture type in the high bits, everything else carries a semantic.
    if declared.kind == RegisterType::Sampler {
        return DclUsage::Sampler(match (token >> 27) & 0xF {
            3 => SamplerKind::Volume,
            4 => SamplerKind::Cube,
            _ => SamplerKind::TwoD,
        });
    }
    // Shader model 3's `vPos`/`vFace` live in a register file of their own and carry no semantic.
    if declared.kind == RegisterType::Misc {
        return DclUsage::Misc;
    }
    // Shader model 2's pixel inputs have no semantic either: `dcl t3` declares texture coordinate
    // 3, and the register number is the index -- there is no usage token to read it from.
    if stage == Stage::Pixel && declared.kind == RegisterType::AddrOrTexture {
        return DclUsage::Misc;
    }
    DclUsage::Semantic { usage: (token & 0x1F) as u8, index: ((token >> 16) & 0xF) as u8 }
}

/// The `xyzw` component names a swizzle or write mask selects, e.g. `0b1011` -> `"xyw"`.
pub fn mask_components(mask: u8) -> String {
    let mut out = String::with_capacity(4);
    for (bit, name) in [(0, 'x'), (1, 'y'), (2, 'z'), (3, 'w')] {
        if mask & (1 << bit) != 0 {
            out.push(name);
        }
    }
    out
}

/// A source swizzle as GLSL, or the empty string when it is the identity -- `.xyzw` on a `vec4` is
/// noise in generated code that a human has to read to debug it.
pub fn swizzle_components(swizzle: u8) -> String {
    if swizzle == 0xE4 {
        return String::new();
    }
    let names = ['x', 'y', 'z', 'w'];
    let mut out = String::with_capacity(5);
    out.push('.');
    for component in 0..4 {
        out.push(names[((swizzle >> (component * 2)) & 0x3) as usize]);
    }
    out
}
