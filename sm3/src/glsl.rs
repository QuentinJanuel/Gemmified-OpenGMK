//! Emits GLSL ES 3.00 from a decoded program.
//!
//! The shape of the translation, and why:
//!
//! - **Every instruction becomes one assignment.** A source is turned into a `vec4` expression, the
//!   opcode wraps it in another `vec4` expression, and the write mask is applied to both sides:
//!   `r0.xyz = (r1 * r2 + r3).xyz;`. That single rule is correct for the component-wise opcodes
//!   *and* for the replicating ones (`dp3` produces `vec4(dot(...))`, so masking it still writes the
//!   same scalar to each selected component), which is exactly D3D's behaviour and is why no
//!   opcode needs its own mask handling.
//! - **Constants stay registers.** `c0..cN` becomes one `uniform vec4 vs_c[N]`, indexed by number,
//!   because that is how GM8.2's API sets them -- by address, from the constant table. Named
//!   uniforms would need a name lookup on every set, and there is no name to look up.
//! - **Stages are matched by semantic, not by register.** A vertex shader writes `o1` and a pixel
//!   shader reads `v0`; what makes those the same value is that both declared `texcoord0`. Naming
//!   the varying after the semantic is what lets a vertex and pixel shader compiled separately --
//!   which D3D9 pairs freely and GL does not -- link at all.
//! - **Outputs are written through locals.** A shader may fill `oPos` in pieces across several
//!   masked writes, and `gl_Position.xy = ...` is legal but `gl_Position` is not always assignable
//!   the same way across drivers. Collecting into a local and assigning once at the end of `main`
//!   sidesteps the whole question.

use crate::{
    Error,
    bytecode::{
        DclUsage, Dest, Instruction, Opcode, Program, Register, RegisterType, SamplerKind, Source, SourceModifier,
        Stage, mask_components, swizzle_components,
    },
    ctab::ConstantTable,
};
use std::collections::{BTreeMap, BTreeSet};

/// A vertex attribute the shader reads, and the D3D semantic it expects to be fed from.
#[derive(Clone, Debug)]
pub struct Attribute {
    /// `layout(location = _)`, and the `v#` register number it came from.
    pub location: u32,
    pub semantic: u8,
    pub semantic_index: u8,
    pub name: String,
}

/// A texture unit the shader samples.
#[derive(Clone, Debug)]
pub struct Sampler {
    /// `s#`, which is the stage number `texture_set_stage` addresses.
    pub index: u32,
    pub name: String,
    pub kind: SamplerKind,
}

/// The finished translation.
pub struct Shader {
    pub stage: Stage,
    pub glsl: String,
    /// Names to registers, for `shader_*_uniform_get_address` and friends.
    pub constants: ConstantTable,
    /// How many `vec4`s the `c#` uniform array holds -- the size the engine has to allocate.
    pub float_register_count: u32,
    pub samplers: Vec<Sampler>,
    /// Vertex stage only.
    pub attributes: Vec<Attribute>,
    /// The varyings this stage declares, by name. Both stages derive these from semantics, so a
    /// matched pair produces identical lists and the program links.
    pub varyings: Vec<String>,
}

/// The D3D semantic names, lowercased and suffixed with the semantic index, which is what a varying
/// is called on both sides of the link.
fn semantic_name(usage: u8, index: u8) -> String {
    let base = match usage {
        0 => "position",
        1 => "blendweight",
        2 => "blendindices",
        3 => "normal",
        4 => "psize",
        5 => "texcoord",
        6 => "tangent",
        7 => "binormal",
        8 => "tessfactor",
        9 => "positiont",
        10 => "color",
        11 => "fog",
        12 => "depth",
        13 => "sample",
        _ => "unknown",
    };
    format!("{}{}", base, index)
}

const USAGE_POSITION: u8 = 0;
const USAGE_PSIZE: u8 = 4;

/// The raw `D3DSPR_*` numbers for the three register files a shader model 2 vertex shader writes.
const FILE_RASTOUT: u8 = 4;
const FILE_ATTROUT: u8 = 5;
const FILE_TEXCRDOUT: u8 = 6;

/// Which output file a register belongs to, for keying `Emitter::outputs`.
fn output_file(kind: RegisterType) -> Option<u8> {
    match kind {
        RegisterType::RastOut => Some(FILE_RASTOUT),
        RegisterType::AttrOut => Some(FILE_ATTROUT),
        RegisterType::Output => Some(FILE_TEXCRDOUT),
        _ => None,
    }
}

/// The local a vertex output is collected into before `main` publishes it.
fn output_local(file: u8, index: u32) -> String {
    format!("o{}_{}", file, index)
}

struct Emitter<'a> {
    program: &'a Program,
    prefix: &'static str,
    body: String,
    indent: usize,

    /// Float constants written by `def`. Referenced by literal rather than through the uniform
    /// array: they are baked into the shader, so making the engine upload them would be inventing
    /// work for it, and would need array slots the constant table never mentions.
    defined_floats: BTreeMap<u32, [f32; 4]>,
    defined_ints: BTreeMap<u32, [i32; 4]>,
    defined_bools: BTreeMap<u32, bool>,

    temps: BTreeSet<u32>,
    uses_address_register: bool,

    /// Output register -> where its value ends up. Vertex stage only, and keyed by *file* as well
    /// as index: shader model 2 spreads its outputs over three register files that all start at
    /// zero (`oPos` is RastOut 0, `oD0` is AttrOut 0, `oT0` is TexCrdOut 0), so the index alone
    /// does not name one. Shader model 3 replaced all three with a single declared file.
    outputs: BTreeMap<(u8, u32), OutputTarget>,
    /// Input register -> varying name. Pixel stage only.
    inputs: BTreeMap<u32, String>,
    /// Shader model 2 pixel shaders read texture coordinates from a *separate* `t#` file, declared
    /// with a bare `dcl t3`. Shader model 3 folded them into `v#` with a semantic.
    texture_inputs: BTreeMap<u32, String>,
    attributes: Vec<Attribute>,
    samplers: BTreeMap<u32, Sampler>,
    varyings: Vec<String>,
    writes_depth: bool,
    colour_outputs: BTreeSet<u32>,
}

#[derive(Clone)]
enum OutputTarget {
    Position,
    PointSize,
    Varying(String),
}

pub fn translate(program: &Program, constants: ConstantTable) -> Result<Shader, Error> {
    let prefix = match program.version.stage {
        Stage::Vertex => "vs",
        Stage::Pixel => "ps",
    };
    let mut emitter = Emitter {
        program,
        prefix,
        body: String::new(),
        indent: 1,
        defined_floats: BTreeMap::new(),
        defined_ints: BTreeMap::new(),
        defined_bools: BTreeMap::new(),
        temps: BTreeSet::new(),
        uses_address_register: false,
        outputs: BTreeMap::new(),
        inputs: BTreeMap::new(),
        texture_inputs: BTreeMap::new(),
        attributes: Vec::new(),
        samplers: BTreeMap::new(),
        varyings: Vec::new(),
        writes_depth: false,
        colour_outputs: BTreeSet::new(),
    };

    emitter.collect_declarations()?;
    emitter.emit_body()?;
    emitter.finish(constants)
}

impl<'a> Emitter<'a> {
    /// First pass: declarations, literal constants, and which temporaries exist. Separate from
    /// emission because GLSL wants every uniform, attribute and local declared before use, while
    /// the bytecode simply mentions registers as it reaches them.
    fn collect_declarations(&mut self) -> Result<(), Error> {
        for instruction in &self.program.instructions {
            match instruction {
                Instruction::Def { register, value } => {
                    self.defined_floats.insert(*register, *value);
                },
                Instruction::DefI { register, value } => {
                    self.defined_ints.insert(*register, *value);
                },
                Instruction::DefB { register, value } => {
                    self.defined_bools.insert(*register, *value);
                },
                Instruction::Dcl { usage, dest } => self.declare(*usage, dest)?,
                Instruction::Op { dest, sources, .. } => {
                    if let Some(dest) = dest {
                        self.note_register(dest.register);
                    }
                    for source in sources {
                        self.note_register(source.register);
                        if let Some(relative) = source.relative {
                            self.note_register(relative.register);
                        }
                    }
                },
            }
        }
        Ok(())
    }

    fn note_register(&mut self, register: Register) {
        match register.kind {
            RegisterType::Temp => {
                self.temps.insert(register.index);
            },
            RegisterType::AddrOrTexture if self.program.version.stage == Stage::Vertex => {
                self.uses_address_register = true;
            },
            RegisterType::ColorOut => {
                self.colour_outputs.insert(register.index);
            },
            // Shader model 2 does not declare its vertex outputs -- there is no `dcl` for them, the
            // register file itself says what each one means -- so they come into existence the
            // first time something writes one.
            RegisterType::RastOut | RegisterType::AttrOut | RegisterType::Output
                if self.program.version.stage == Stage::Vertex && self.program.version.major == 2 =>
            {
                let file = output_file(register.kind).unwrap_or(FILE_TEXCRDOUT);
                let target = match (file, register.index) {
                    // oPos, oFog, oPts.
                    (FILE_RASTOUT, 0) => OutputTarget::Position,
                    (FILE_RASTOUT, 2) => OutputTarget::PointSize,
                    // Fog is a varying like any other; the built-in program's fog is not available
                    // to a custom shader anyway, so a game wanting it computes its own.
                    (FILE_RASTOUT, _) => OutputTarget::Varying("vary_fog0".to_string()),
                    (FILE_ATTROUT, index) => OutputTarget::Varying(format!("vary_color{}", index)),
                    (_, index) => OutputTarget::Varying(format!("vary_texcoord{}", index)),
                };
                if let OutputTarget::Varying(name) = &target {
                    if !self.varyings.contains(name) {
                        self.varyings.push(name.clone());
                    }
                }
                self.outputs.entry((file, register.index)).or_insert(target);
            },
            RegisterType::DepthOut => self.writes_depth = true,
            _ => (),
        }
    }

    fn declare(&mut self, usage: DclUsage, dest: &Dest) -> Result<(), Error> {
        let register = dest.register;
        match usage {
            DclUsage::Sampler(kind) => {
                let name = format!("{}_s{}", self.prefix, register.index);
                self.samplers.insert(register.index, Sampler { index: register.index, name, kind });
            },
            // A pixel shader input with no semantic attached. In shader model 2 that is the norm
            // rather than an oddity: `dcl t3` declares texture coordinate 3, and the register
            // number *is* the semantic index -- there is no usage token to read it from. Shader
            // model 3 has no such form, so there it really is `vPos`/`vFace` and refusing is right.
            DclUsage::Misc if register.kind == RegisterType::Misc => {
                // `vPos` is `gl_FragCoord` and `vFace` is `gl_FrontFacing`; both are built in, so
                // declaring them is a no-op and only the reads in `source_expr` matter.
            },
            DclUsage::Misc => {
                if self.program.version.major == 2 {
                    let varying = format!("vary_texcoord{}", dest.register.index);
                    if !self.varyings.contains(&varying) {
                        self.varyings.push(varying.clone());
                    }
                    self.texture_inputs.insert(register.index, varying);
                } else {
                    return Err(Error::UnsupportedFeature("vPos/vFace input"));
                }
            },
            DclUsage::Semantic { usage, index } => {
                let name = semantic_name(usage, index);
                match (self.program.version.stage, register.kind) {
                    (Stage::Vertex, RegisterType::Input) => {
                        self.attributes.push(Attribute {
                            location: register.index,
                            semantic: usage,
                            semantic_index: index,
                            name: format!("{}_in_{}", self.prefix, name),
                        });
                    },
                    (Stage::Vertex, RegisterType::Output) => {
                        let target = match usage {
                            USAGE_POSITION => OutputTarget::Position,
                            USAGE_PSIZE => OutputTarget::PointSize,
                            _ => {
                                let varying = format!("vary_{}", name);
                                self.varyings.push(varying.clone());
                                OutputTarget::Varying(varying)
                            },
                        };
                        self.outputs.insert((FILE_TEXCRDOUT, register.index), target);
                    },
                    (Stage::Pixel, RegisterType::Input) => {
                        // Shader model 2's `dcl v0` carries no semantic either -- it is the
                        // interpolated colour, numbered by register. Only 3.0's declarations name
                        // one, and there the semantic is what links the two stages.
                        let varying = if self.program.version.major == 2 {
                            format!("vary_color{}", register.index)
                        } else {
                            format!("vary_{}", name)
                        };
                        if !self.varyings.contains(&varying) {
                            self.varyings.push(varying.clone());
                        }
                        self.inputs.insert(register.index, varying);
                    },
                    _ => return Err(Error::UnsupportedFeature("declaration of an unexpected register")),
                }
            },
        }
        Ok(())
    }

    fn line(&mut self, text: &str) {
        for _ in 0..self.indent {
            self.body.push_str("    ");
        }
        self.body.push_str(text);
        self.body.push('\n');
    }

    fn emit_body(&mut self) -> Result<(), Error> {
        for instruction in &self.program.instructions {
            let (opcode, control, dest, sources) = match instruction {
                // Declarations were handled in the first pass and produce no code.
                Instruction::Dcl { .. }
                | Instruction::Def { .. }
                | Instruction::DefI { .. }
                | Instruction::DefB { .. } => continue,
                Instruction::Op { opcode, control, dest, sources } => (*opcode, *control, dest, sources),
            };

            // Control flow first: these are the instructions that are statements rather than
            // assignments, and the only ones that move the indent.
            match opcode {
                Opcode::Nop => continue,
                Opcode::Rep => {
                    let count = self.loop_count(&sources[0])?;
                    // Shadowing `aL` in a nested loop is what the bytecode means: `aL` always names
                    // the innermost counter, and GLSL scoping gives that for free.
                    let text = format!("for (int aL = 0; aL < {}; ++aL) {{", count);
                    self.line(&text);
                    self.indent += 1;
                    continue;
                },
                Opcode::EndRep | Opcode::EndIf => {
                    self.indent = self.indent.saturating_sub(1);
                    self.line("}");
                    continue;
                },
                Opcode::Else => {
                    self.indent = self.indent.saturating_sub(1);
                    self.line("} else {");
                    self.indent += 1;
                    continue;
                },
                // `texkill` discards the fragment when any of x, y or z is negative. It reads its
                // operand through the *destination* token, which is how the encoding does it, and
                // writes nothing.
                Opcode::TexKill => {
                    let register = dest.as_ref().ok_or(Error::MalformedInstruction(0))?.register;
                    let operand = self.register_expr(register)?;
                    let text = format!("if (any(lessThan(({}).xyz, vec3(0.0)))) discard;", operand);
                    self.line(&text);
                    continue;
                },
                Opcode::Break => {
                    self.line("break;");
                    continue;
                },
                Opcode::BreakC => {
                    let comparison = Self::comparison(control)?;
                    let (a, b) = (self.source_expr(&sources[0])?, self.source_expr(&sources[1])?);
                    let text = format!("if (({}).x {} ({}).x) break;", a, comparison, b);
                    self.line(&text);
                    continue;
                },
                Opcode::If => {
                    let condition = self.bool_expr(&sources[0])?;
                    let text = format!("if ({}) {{", condition);
                    self.line(&text);
                    self.indent += 1;
                    continue;
                },
                Opcode::IfC => {
                    let comparison = Self::comparison(control)?;
                    let (a, b) = (self.source_expr(&sources[0])?, self.source_expr(&sources[1])?);
                    let text = format!("if (({}).x {} ({}).x) {{", a, comparison, b);
                    self.line(&text);
                    self.indent += 1;
                    continue;
                },
                _ => (),
            }

            let dest = dest.as_ref().ok_or(Error::MalformedInstruction(0))?;

            // `mova` is the one instruction whose destination is not a float register: it rounds
            // into the integer address register that relative constant addressing reads.
            if opcode == Opcode::Mova {
                let source = self.source_expr(&sources[0])?;
                let mask = mask_components(dest.write_mask);
                let text = format!("a0.{} = ivec4(round({})).{};", mask, source, mask);
                self.line(&text);
                continue;
            }

            let rhs = self.rhs_expr(opcode, sources)?;
            let rhs = if dest.saturate { format!("clamp({}, 0.0, 1.0)", rhs) } else { rhs };
            let target = self.dest_expr(dest)?;

            let text = if dest.write_mask == 0b1111 {
                format!("{} = {};", target, rhs)
            } else {
                let mask = mask_components(dest.write_mask);
                format!("{}.{} = ({}).{};", target, mask, rhs, mask)
            };
            self.line(&text);
        }

        if self.indent != 1 {
            return Err(Error::UnbalancedControlFlow);
        }
        Ok(())
    }

    /// The comparison an `ifc`/`breakc` carries in its control bits.
    fn comparison(control: u8) -> Result<&'static str, Error> {
        Ok(match control {
            1 => ">",
            2 => "==",
            3 => ">=",
            4 => "<",
            5 => "!=",
            6 => "<=",
            _ => return Err(Error::UnsupportedFeature("comparison mode")),
        })
    }

    /// The `vec4`-valued right-hand side for one instruction.
    fn rhs_expr(&self, opcode: Opcode, sources: &[Source]) -> Result<String, Error> {
        let source = |index: usize| self.source_expr(&sources[index]);
        Ok(match opcode {
            Opcode::Mov => source(0)?,
            Opcode::Add => format!("{} + {}", source(0)?, source(1)?),
            Opcode::Sub => format!("{} - {}", source(0)?, source(1)?),
            Opcode::Mul => format!("{} * {}", source(0)?, source(1)?),
            Opcode::Mad => format!("{} * {} + {}", source(0)?, source(1)?, source(2)?),
            // `lrp dst, a, b, c` is `c + a * (b - c)`, which is `mix` with the arguments the other
            // way round from how they read here.
            Opcode::Lrp => format!("mix({}, {}, {})", source(2)?, source(1)?, source(0)?),
            Opcode::Dp3 => format!("vec4(dot(({}).xyz, ({}).xyz))", source(0)?, source(1)?),
            // Shader model 2's two-component dot with a scalar added -- the cheap way to do a
            // length or a bilinear weight when `dp3` would be a wasted component.
            Opcode::Dp2Add => {
                format!("vec4(dot(({}).xy, ({}).xy) + ({}).x)", source(0)?, source(1)?, source(2)?)
            },
            Opcode::Dp4 => format!("vec4(dot({}, {}))", source(0)?, source(1)?),
            Opcode::Min => format!("min({}, {})", source(0)?, source(1)?),
            Opcode::Max => format!("max({}, {})", source(0)?, source(1)?),
            Opcode::Slt => format!("vec4(lessThan({}, {}))", source(0)?, source(1)?),
            Opcode::Sge => format!("vec4(greaterThanEqual({}, {}))", source(0)?, source(1)?),
            Opcode::Rcp => format!("vec4(1.0 / ({}).x)", source(0)?),
            // D3D takes the reciprocal square root of the *magnitude*; a negative input is defined,
            // not undefined, and shaders rely on that.
            Opcode::Rsq => format!("vec4(inversesqrt(abs(({}).x)))", source(0)?),
            Opcode::Frc => format!("fract({})", source(0)?),
            // `exp`/`log` are base 2 in D3D, which is the one detail that silently changes a
            // shader's output rather than breaking it.
            Opcode::Exp => format!("vec4(exp2(({}).x))", source(0)?),
            Opcode::Log => format!("vec4(log2(abs(({}).x)))", source(0)?),
            Opcode::Abs => format!("abs({})", source(0)?),
            // `nrm` normalises the vector part and leaves w alone; nothing reads the w it produces.
            Opcode::Nrm => format!("vec4(normalize(({}).xyz), 0.0)", source(0)?),
            Opcode::Pow => format!("vec4(pow(abs(({}).x), ({}).x))", source(0)?, source(1)?),
            // `cmp` selects per component on `src0 >= 0`, which is `mix` with a boolean vector.
            Opcode::Cmp => format!("mix({}, {}, greaterThanEqual({}, vec4(0.0)))", source(2)?, source(1)?, source(0)?),
            Opcode::SinCos => {
                let angle = source(0)?;
                format!("vec4(cos(({}).x), sin(({}).x), 0.0, 0.0)", angle, angle)
            },
            // `texldl` picks the mip level explicitly, from the coordinate's w. A vertex shader
            // has no derivatives to work one out from, which is exactly why the instruction exists
            // and why vertex texture fetch always uses it.
            Opcode::TexLdL => {
                let sampler = sources[1].register;
                let declared = self
                    .samplers
                    .get(&sampler.index)
                    .ok_or(Error::UnsupportedFeature("sample from an undeclared sampler"))?;
                let coord = source(0)?;
                let components = match declared.kind {
                    SamplerKind::TwoD => "xy",
                    SamplerKind::Cube | SamplerKind::Volume => "xyz",
                };
                format!("textureLod({}, ({}).{}, ({}).w)", declared.name, coord, components, coord)
            },
            Opcode::TexLd => {
                let sampler = sources[1].register;
                let declared = self
                    .samplers
                    .get(&sampler.index)
                    .ok_or(Error::UnsupportedFeature("sample from an undeclared sampler"))?;
                let coord = source(0)?;
                match declared.kind {
                    SamplerKind::TwoD => format!("texture({}, ({}).xy)", declared.name, coord),
                    SamplerKind::Cube | SamplerKind::Volume => {
                        format!("texture({}, ({}).xyz)", declared.name, coord)
                    },
                }
            },
            // Handled as statements before this point.
            Opcode::Mova
            | Opcode::TexKill
            | Opcode::Break
            | Opcode::BreakC
            | Opcode::Rep
            | Opcode::EndRep
            | Opcode::If
            | Opcode::IfC
            | Opcode::Else
            | Opcode::EndIf
            | Opcode::Nop => return Err(Error::MalformedInstruction(0)),
        })
    }

    fn dest_expr(&self, dest: &Dest) -> Result<String, Error> {
        Ok(match dest.register.kind {
            RegisterType::Temp => format!("r{}", dest.register.index),
            // Only an output this emitter knows about has anywhere to go: `finish` publishes the
            // locals it created, so writing an unknown one would emit GLSL referring to a variable
            // that never existed rather than anything a driver could compile.
            RegisterType::RastOut | RegisterType::AttrOut | RegisterType::Output
                if self.program.version.stage == Stage::Vertex
                    && output_file(dest.register.kind)
                        .is_some_and(|file| self.outputs.contains_key(&(file, dest.register.index))) =>
            {
                output_local(output_file(dest.register.kind).unwrap_or(FILE_TEXCRDOUT), dest.register.index)
            },
            RegisterType::ColorOut => format!("oC{}", dest.register.index),
            RegisterType::DepthOut => "oDepth".to_string(),
            RegisterType::Predicate => return Err(Error::UnsupportedFeature("predicate register")),
            _ => return Err(Error::UnsupportedFeature("write to an unexpected register")),
        })
    }

    fn source_expr(&self, source: &Source) -> Result<String, Error> {
        let base = match source.register.kind {
            RegisterType::Temp => format!("r{}", source.register.index),
            RegisterType::Const => self.const_expr(source)?,
            RegisterType::ConstInt => {
                let value = self
                    .defined_ints
                    .get(&source.register.index)
                    .ok_or(Error::UnsupportedFeature("integer constant set by the application"))?;
                format!("vec4({}.0, {}.0, {}.0, {}.0)", value[0], value[1], value[2], value[3])
            },
            RegisterType::ConstBool => {
                let value = self
                    .defined_bools
                    .get(&source.register.index)
                    .ok_or(Error::UnsupportedFeature("boolean constant set by the application"))?;
                format!("vec4({})", if *value { "1.0" } else { "0.0" })
            },
            RegisterType::Input => match self.program.version.stage {
                Stage::Vertex => self
                    .attributes
                    .iter()
                    .find(|a| a.location == source.register.index)
                    .map(|a| a.name.clone())
                    .ok_or(Error::UnsupportedFeature("read of an undeclared input"))?,
                Stage::Pixel => self
                    .inputs
                    .get(&source.register.index)
                    .cloned()
                    .ok_or(Error::UnsupportedFeature("read of an undeclared input"))?,
            },
            RegisterType::Loop => "vec4(float(aL))".to_string(),
            // `vPos` -- the fragment's position in the render target.
            //
            // Measured against the real runner (a probe driving
            // a shader whose output hue is a function of its own `vPos`): Direct3D 9 numbers the
            // top-left pixel **(0, 0)**, integers, counting down. GL's `gl_FragCoord` is the centre
            // of the pixel, so it is that plus a half in both axes -- hence the subtraction.
            //
            // The direction needs no correction, though it takes two flips to get there: this
            // renderer's projection matrices carry a `-2/h` on y and `update_matrix` multiplies in
            // another `-1`, so world y ends up increasing *down* the GL framebuffer just as it does
            // in D3D. (That is also why `read_pixels` hands back correctly-oriented images without
            // flipping anything.) A shader reading `vPos` mirrored would mean one of those two
            // flips has moved.
            RegisterType::Misc if source.register.index == 0 => "(gl_FragCoord - vec4(0.5, 0.5, 0.0, 0.0))".to_string(),
            // `vFace`: positive for a front face, negative for a back one.
            RegisterType::Misc => "vec4(gl_FrontFacing ? 1.0 : -1.0)".to_string(),
            // Shader model 2's pixel-shader texture coordinate file. In a vertex shader the same
            // register number is the address register, which `const_expr` reaches instead.
            RegisterType::AddrOrTexture if self.program.version.stage == Stage::Pixel => self
                .texture_inputs
                .get(&source.register.index)
                .cloned()
                .ok_or(Error::UnsupportedFeature("read of an undeclared texture coordinate"))?,
            RegisterType::Sampler => {
                // Only ever the second operand of a sample, which `rhs_expr` reads directly.
                return Err(Error::MalformedInstruction(0));
            },
            _ => return Err(Error::UnsupportedFeature("read of an unexpected register")),
        };

        let swizzled = format!("{}{}", base, swizzle_components(source.swizzle));
        Ok(match source.modifier {
            SourceModifier::None => swizzled,
            SourceModifier::Negate => format!("-({})", swizzled),
            SourceModifier::Abs => format!("abs({})", swizzled),
            SourceModifier::AbsNegate => format!("-abs({})", swizzled),
        })
    }

    /// One register with no swizzle or modifier, for an operand encoded as a destination but only
    /// read -- which is `texkill` and nothing else.
    fn register_expr(&self, register: Register) -> Result<String, Error> {
        self.source_expr(&Source { register, swizzle: 0xE4, modifier: SourceModifier::None, relative: None })
    }

    fn const_expr(&self, source: &Source) -> Result<String, Error> {
        if let Some(relative) = source.relative {
            // `c[a0.x + n]`. The address register is the only thing that can index the array, and
            // this is the path skinning takes to reach its bone matrices.
            if relative.register.kind != RegisterType::AddrOrTexture && relative.register.kind != RegisterType::Loop {
                return Err(Error::UnsupportedFeature("relative addressing from an unexpected register"));
            }
            let index = if relative.register.kind == RegisterType::Loop {
                "aL".to_string()
            } else {
                format!("a0{}", ['x', 'y', 'z', 'w'][relative.component as usize % 4]).replace("a0", "a0.")
            };
            return Ok(format!("{}_c[{} + {}]", self.prefix, index, source.register.index));
        }
        // A `def`-ed register is a literal baked into the shader, not something the application can
        // set, so emit the value rather than an array slot that would never be written.
        if let Some(value) = self.defined_floats.get(&source.register.index) {
            return Ok(format!(
                "vec4({}, {}, {}, {})",
                float_literal(value[0]),
                float_literal(value[1]),
                float_literal(value[2]),
                float_literal(value[3])
            ));
        }
        Ok(format!("{}_c[{}]", self.prefix, source.register.index))
    }

    /// The repeat count of a `rep`, which is always an integer constant register.
    fn loop_count(&self, source: &Source) -> Result<String, Error> {
        match source.register.kind {
            RegisterType::ConstInt => {
                let value = self
                    .defined_ints
                    .get(&source.register.index)
                    .ok_or(Error::UnsupportedFeature("loop count set by the application"))?;
                Ok(value[0].to_string())
            },
            _ => Err(Error::UnsupportedFeature("loop count from an unexpected register")),
        }
    }

    fn bool_expr(&self, source: &Source) -> Result<String, Error> {
        match source.register.kind {
            RegisterType::ConstBool => {
                let value = self
                    .defined_bools
                    .get(&source.register.index)
                    .ok_or(Error::UnsupportedFeature("boolean set by the application"))?;
                Ok(value.to_string())
            },
            _ => Err(Error::UnsupportedFeature("branch on an unexpected register")),
        }
    }

    fn finish(self, constants: ConstantTable) -> Result<Shader, Error> {
        let stage = self.program.version.stage;
        let mut out = String::new();
        out.push_str("#version 300 es\n");
        // GLSL ES demands an explicit default, and `highp` is the only one that will not quietly
        // change what a shader computes. Cutting it back is an optimisation to make deliberately,
        // with something to compare against.
        out.push_str("precision highp float;\nprecision highp int;\n\n");

        // Sized from the constant table rather than from what the instructions mention -- see
        // `ConstantTable::float_register_count`. The instruction walk is only a backstop, and has
        // to be able to say "none at all": a shader that samples a texture and touches no float
        // constant would otherwise get a one-element array nothing ever reads or writes.
        let float_register_count =
            constants.float_register_count().max(self.max_static_const().map_or(0, |max| max + 1));
        if float_register_count > 0 {
            out.push_str(&format!("uniform vec4 {}_c[{}];\n", self.prefix, float_register_count));
        }
        for sampler in self.samplers.values() {
            let kind = match sampler.kind {
                SamplerKind::TwoD => "sampler2D",
                SamplerKind::Cube => "samplerCube",
                SamplerKind::Volume => "sampler3D",
            };
            out.push_str(&format!("uniform {} {};\n", kind, sampler.name));
        }
        out.push('\n');

        match stage {
            Stage::Vertex => {
                for attribute in &self.attributes {
                    out.push_str(&format!("layout(location = {}) in vec4 {};\n", attribute.location, attribute.name));
                }
                for varying in &self.varyings {
                    out.push_str(&format!("out vec4 {};\n", varying));
                }
            },
            Stage::Pixel => {
                for varying in &self.varyings {
                    out.push_str(&format!("in vec4 {};\n", varying));
                }
                for index in &self.colour_outputs {
                    out.push_str(&format!("layout(location = {0}) out vec4 ps_colour{0};\n", index));
                }
            },
        }
        out.push_str("\nvoid main() {\n");

        for temp in &self.temps {
            out.push_str(&format!("    vec4 r{} = vec4(0.0);\n", temp));
        }
        if self.uses_address_register {
            out.push_str("    ivec4 a0 = ivec4(0);\n");
        }
        for (file, index) in self.outputs.keys() {
            out.push_str(&format!("    vec4 {} = vec4(0.0);\n", output_local(*file, *index)));
        }
        for index in &self.colour_outputs {
            out.push_str(&format!("    vec4 oC{} = vec4(0.0);\n", index));
        }
        if self.writes_depth {
            out.push_str("    vec4 oDepth = vec4(0.0);\n");
        }
        if !self.temps.is_empty() || !self.outputs.is_empty() || !self.colour_outputs.is_empty() {
            out.push('\n');
        }

        out.push_str(&self.body);

        // Outputs are collected in locals and published once, at the end -- see the module comment.
        out.push('\n');
        for ((file, index), target) in &self.outputs {
            let local = output_local(*file, *index);
            match target {
                OutputTarget::Position => out.push_str(&format!("    gl_Position = {};\n", local)),
                OutputTarget::PointSize => out.push_str(&format!("    gl_PointSize = {}.x;\n", local)),
                OutputTarget::Varying(name) => out.push_str(&format!("    {} = {};\n", name, local)),
            }
        }
        for index in &self.colour_outputs {
            out.push_str(&format!("    ps_colour{0} = oC{0};\n", index));
        }
        if self.writes_depth {
            out.push_str("    gl_FragDepth = oDepth.x;\n");
        }
        out.push_str("}\n");

        Ok(Shader {
            stage,
            glsl: out,
            constants,
            float_register_count,
            samplers: self.samplers.into_values().collect(),
            attributes: self.attributes,
            varyings: self.varyings,
        })
    }

    /// The highest `c#` mentioned outright, or `None` if the shader reads no application-set float
    /// constant at all. A backstop for a shader whose constant table is missing or empty: without
    /// it the uniform array would be declared with no elements and every read out of bounds.
    fn max_static_const(&self) -> Option<u32> {
        let mut max = None;
        for instruction in &self.program.instructions {
            if let Instruction::Op { sources, .. } = instruction {
                for source in sources {
                    if source.register.kind == RegisterType::Const
                        && source.relative.is_none()
                        && !self.defined_floats.contains_key(&source.register.index)
                    {
                        max = Some(max.map_or(source.register.index, |m: u32| m.max(source.register.index)));
                    }
                }
            }
        }
        max
    }
}

/// A float that GLSL will accept: it insists on a decimal point, and `1e30` has none.
fn float_literal(value: f32) -> String {
    if value.is_nan() || value.is_infinite() {
        // Neither has a GLSL literal spelling. These come out of `fxc` as sentinels rather than as
        // values anything computes with, so the nearest finite value is closer to the intent than a
        // shader that fails to compile.
        return if value.is_sign_negative() { "-3.402823466e+38".to_string() } else { "3.402823466e+38".to_string() };
    }
    let text = format!("{:?}", value);
    if text.contains('.') || text.contains('e') { text } else { format!("{}.0", text) }
}
