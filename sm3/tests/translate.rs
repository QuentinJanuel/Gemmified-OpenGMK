//! The four shaders that ship inside Delicious Level Design Contest, translated end to end.
//!
//! They are the whole point of the crate and between them they cover everything a GM8.2 game is
//! likely to need: the gm82dx9 pass-through pair is the minimum any shader-using game loads, and
//! the gm82gltf pair is a full PBR material with skinning, morph targets, an eight-light model and
//! fog -- which is to say relative constant addressing, `rep`, `ifc`/`else`, six samplers and 143
//! constant registers. Extracted with `cargo run --example dump_extensions -p gm8exe`.
//!
//! These are checks on *meaning*, not golden files. A snapshot of the emitted text would fail on
//! every whitespace change and would say nothing about whether the translation is right; asserting
//! that the skinning shader indexes its joint matrices through the address register does.

use sm3::{Error, Stage};

fn fixture(name: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    std::fs::read(format!("{}{}", path, name)).unwrap_or_else(|e| panic!("{}: {}", name, e))
}

fn translate(name: &str) -> sm3::Shader {
    sm3::translate(&fixture(name)).unwrap_or_else(|e| panic!("{} failed to translate: {}", name, e))
}

/// Everything the emitter produces has to open with these, or no WebGL2 driver will take it.
fn assert_well_formed(glsl: &str) {
    assert!(glsl.starts_with("#version 300 es\n"), "missing version directive");
    assert!(glsl.contains("precision highp float;"), "missing float precision");
    assert!(glsl.contains("void main() {"), "missing entry point");
    assert_eq!(glsl.matches('{').count(), glsl.matches('}').count(), "braces are unbalanced");
    // Every statement the emitter writes is terminated; an unterminated one means a code path
    // returned a bare expression where a line was expected.
    for line in glsl.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let structural =
            line.ends_with('{') || line.ends_with('}') || line.starts_with('#') || line.starts_with("} else");
        assert!(structural || line.ends_with(';'), "unterminated line: {}", line);
    }
}

#[test]
fn pass_through_vertex_shader() {
    let shader = translate("vs_pass.vs3");
    assert_well_formed(&shader.glsl);
    assert_eq!(shader.stage, Stage::Vertex);

    // The one thing this shader does is a world-view-projection multiply, which `fxc` expands into
    // a mul and three mads against four consecutive constant registers.
    let matrix = shader.constants.find("rMatrixWVP").expect("rMatrixWVP not in the constant table");
    assert_eq!((matrix.register_index, matrix.register_count), (0, 4));
    assert_eq!(shader.float_register_count, 4);

    // Position, texture coordinate and colour in; the last two straight back out as varyings.
    assert_eq!(shader.attributes.len(), 3);
    assert!(shader.glsl.contains("layout(location = 0) in vec4 vs_in_position0;"));
    assert!(shader.glsl.contains("out vec4 vary_texcoord0;"));
    assert!(shader.glsl.contains("out vec4 vary_color0;"));
    // The local an output is collected into is named after its register *file* as well as its
    // index, because shader model 2 spreads outputs over three files that all start at zero.
    assert!(shader.glsl.contains("gl_Position = o6_0;"));
}

#[test]
fn pass_through_pixel_shader() {
    let shader = translate("ps_pass.ps3");
    assert_well_formed(&shader.glsl);
    assert_eq!(shader.stage, Stage::Pixel);

    assert_eq!(shader.samplers.len(), 1);
    assert!(shader.glsl.contains("uniform sampler2D ps_s0;"));
    assert!(shader.glsl.contains("texture(ps_s0,"));
    assert!(shader.glsl.contains("layout(location = 0) out vec4 ps_colour0;"));

    // It samples a texture and multiplies by the vertex colour, and touches no float constant at
    // all -- so there must be no constant array. Declaring an empty-but-for-one-slot array here was
    // a real bug: every register it claimed would have been a uniform the engine had to allocate
    // and upload for nothing.
    assert_eq!(shader.float_register_count, 0);
    assert!(!shader.glsl.contains("uniform vec4 ps_c"), "declared a constant array it never reads");
}

/// The pair only links if both stages agree on the varyings, which they can only do by semantic --
/// the vertex shader writes `o1..o4` and the pixel shader reads `v0..v4`, and nothing but the
/// declared semantic connects the two.
#[test]
fn stages_agree_on_varyings() {
    for (vertex, pixel) in [("vs_pass.vs3", "ps_pass.ps3"), ("gltf_vertex.vs3", "gltf_pixel.ps3")] {
        let (vs, ps) = (translate(vertex), translate(pixel));
        for varying in &ps.varyings {
            assert!(vs.varyings.contains(varying), "{} reads {} but {} never writes it", pixel, varying, vertex);
            assert!(vs.glsl.contains(&format!("out vec4 {};", varying)));
            assert!(ps.glsl.contains(&format!("in vec4 {};", varying)));
        }
    }
}

#[test]
fn gltf_vertex_shader_skins_through_the_address_register() {
    let shader = translate("gltf_vertex.vs3");
    assert_well_formed(&shader.glsl);

    // 128 registers of bone matrices, which is what forces relative addressing to exist: the shader
    // cannot know at compile time which joint a vertex is weighted to.
    let joints = shader.constants.find("uJointMatrix").expect("uJointMatrix not in the constant table");
    assert_eq!((joints.register_index, joints.register_count), (0, 128));
    assert_eq!(shader.float_register_count, 143);

    assert!(shader.glsl.contains("ivec4 a0"), "no address register declared");
    assert!(shader.glsl.contains("a0.x = ivec4(round("), "mova did not round into the address register");
    assert!(shader.glsl.contains("vs_c[a0.x + "), "joint matrices are not indexed relatively");

    // Skinning is behind a branch on whether the mesh has any, and morph targets arrive as extra
    // position/normal/tangent streams.
    assert!(shader.glsl.contains("} else {"));
    assert!(shader.attributes.iter().any(|a| a.name.contains("blendweight")));
    assert!(shader.attributes.iter().any(|a| a.name.contains("position1")), "no morph target stream");
}

#[test]
fn gltf_pixel_shader_lights_and_samples() {
    let shader = translate("gltf_pixel.ps3");
    assert_well_formed(&shader.glsl);

    // Base colour, normal, emissive, occlusion, roughness, environment.
    assert_eq!(shader.samplers.len(), 6);
    for index in 0..6 {
        assert!(shader.glsl.contains(&format!("uniform sampler2D ps_s{};", index)));
    }

    // GM's eight-light model, re-implemented in the shader because a custom shader gets none of the
    // fixed-function state -- which is exactly why the extension asks the game for the light data.
    let colours = shader.constants.find("uLightColor").unwrap();
    assert_eq!((colours.register_index, colours.register_count), (0, 8));

    // `rep`/`endrep` over the lights, and `cmp` for the per-component selects a PBR material is
    // full of. `aL` is the loop counter the bytecode names.
    assert!(shader.glsl.contains("for (int aL = 0;"), "the light loop did not survive");
    assert!(shader.glsl.contains("mix("), "cmp/lrp did not lower to a select");
}

#[test]
fn refuses_what_it_has_not_checked() {
    // Shader model 1 is not a subset of 2 or 3: its own texture-addressing instruction set, no
    // declared inputs, no usable constant table. Answering "no shader" is what lets a game fall
    // back to its fixed-function look instead of rendering something confidently wrong.
    // `.err()` rather than comparing the whole `Result`: a `Shader` holds its entire GLSL, and a
    // failing assert that dumped it would bury the one line that matters.
    let mut sm1 = fixture("ps_pass.ps3");
    sm1[1] = 1; // the version token's major byte -> ps_1_0
    assert_eq!(sm3::translate(&sm1).err(), Some(Error::UnsupportedVersion { major: 1, minor: 0 }));

    assert_eq!(sm3::translate(&[]).err(), Some(Error::NotShaderBytecode));
    assert_eq!(sm3::translate(b"not a shader at all").err(), Some(Error::NotShaderBytecode));
    // A version token and nothing else is structurally fine and simply produces an empty shader;
    // a *truncated instruction* is the case that has to be caught.
    assert_eq!(sm3::translate(&[0x00, 0x03, 0xFE, 0xFF, 0x01, 0x00, 0x00, 0x02]).err(), Some(Error::UnexpectedEnd));
}

/// Translating is a load-time step, so it has to be cheap enough to be invisible next to parsing
/// the game itself. The big pair is ~800 tokens; anything in the millisecond range would mean a
/// mistake rather than a cost worth optimising.
#[test]
fn translation_is_not_quadratic() {
    let bytes = fixture("gltf_vertex.vs3");
    let start = std::time::Instant::now();
    for _ in 0..100 {
        sm3::translate(&bytes).unwrap();
    }
    let each = start.elapsed() / 100;
    assert!(each.as_millis() < 50, "one translation took {:?}", each);
}

/// Shader model 2, which is what real games are mostly made of: `fxc` targets the lowest model a
/// shader's features allow, so a screen-space effect compiles to `ps_2_x` and only the genuinely
/// demanding ones reach 3.0. Of the 48 shaders inside Delicious Level Design Contest, 37 are model
/// 2 -- so a translator that took only 3.0 would have refused three quarters of the game.
///
/// The two models differ in more than a version number, which is the point of these:
#[test]
fn shader_model_2_pixel_shader() {
    let shader = translate("ps2_blur.ps2");
    assert_well_formed(&shader.glsl);
    assert_eq!(shader.stage, Stage::Pixel);

    // Model 2 reads texture coordinates from a separate `t#` file declared with a bare `dcl t0` --
    // no semantic anywhere, the register number *is* the index. Model 3 folded them into `v#`.
    // Both have to arrive at the same varying name or a mixed-model pair would not link.
    assert!(shader.glsl.contains("in vec4 vary_texcoord0;"), "t0 did not become texcoord0");
    assert!(shader.glsl.contains("texture(ps_s0,"));
    assert!(shader.constants.find("resolution").is_some(), "resolution uniform missing");
}

#[test]
fn shader_model_2_vertex_shader() {
    let shader = translate("vs2_shadow.vs2");
    assert_well_formed(&shader.glsl);
    assert_eq!(shader.stage, Stage::Vertex);

    // Model 2 has no `dcl` for its outputs at all: three separate register files say what each one
    // means, and they all start at zero -- `oPos` is RastOut 0, `oD0` is AttrOut 0, `oT0` is
    // TexCrdOut 0. They exist only because something writes them.
    assert!(shader.glsl.contains("gl_Position = o4_0;"), "oPos did not reach gl_Position");
    assert!(shader.varyings.iter().any(|v| v == "vary_color0"), "oD0 did not become color0");
    assert!(shader.varyings.iter().any(|v| v == "vary_texcoord0"), "oT0 did not become texcoord0");

    // `sincos` takes two extra constant registers in model 2 -- Taylor coefficients the hardware of
    // the day needed -- and one argument in model 3. Both lower to GLSL's own `sin`/`cos`.
    assert!(shader.glsl.contains("sin("), "sincos did not lower");

    let matrix = shader.constants.find("rMatrixVP").expect("rMatrixVP not in the constant table");
    assert_eq!((matrix.register_index, matrix.register_count), (0, 4));
}

/// `vPos` is the fragment's position in the render target, which GLSL has natively. Six of this
/// game's shaders read it, and before it was recognised they were refused as "declaration of an
/// unexpected register" -- it lives in a register file of its own rather than being a semantic.
///
/// It is `gl_FragCoord` *minus half a pixel in each axis*, and that half is not a detail: measured
/// against the real runner, Direct3D 9 numbers the top-left pixel (0, 0) while GL puts
/// `gl_FragCoord` at the pixel's centre. A shader keying an effect off its own screen position --
/// which is what all six do -- lands half a pixel out otherwise, and one that quantises the value
/// lands a whole pixel out.
#[test]
fn reads_the_fragment_position() {
    let shader = translate("foggers.ps3");
    assert_well_formed(&shader.glsl);
    assert!(shader.glsl.contains("gl_FragCoord"), "vPos did not become gl_FragCoord");
    assert!(
        shader.glsl.contains("(gl_FragCoord - vec4(0.5, 0.5, 0.0, 0.0))"),
        "vPos is not offset to D3D9's integer pixel numbering",
    );
    assert_eq!(shader.samplers.len(), 2);
}

/// Vertex texture fetch: a sampler in the *vertex* stage, read with `texldl` because a vertex
/// shader has no derivatives to work a mip level out from. Without it the game's `rStwinkle`
/// uniform lookup fails and stops the game during room load.
#[test]
fn samples_a_texture_from_the_vertex_stage() {
    let shader = translate("twinkle.vs3");
    assert_well_formed(&shader.glsl);
    assert_eq!(shader.stage, Stage::Vertex);

    // Named `vs_s0`, not `ps_s0`: the two stages have separate sampler registers in D3D9, so the
    // prefix is what stops a vertex `s0` and a pixel `s0` being taken for the same thing.
    assert!(shader.glsl.contains("uniform sampler2D vs_s0;"));
    assert!(shader.glsl.contains("textureLod("), "texldl did not lower to an explicit-LOD fetch");
    assert!(shader.constants.find("rStwinkle").is_some(), "the sampler is not in the constant table");
}

/// The contract every shaded draw in a GM8.2 game rests on: a game's pixel shader consumes exactly
/// the varyings `vs_pass` produces. `shader_vertex_set_passthrough` pairs that vertex shader with
/// whatever pixel shader the game set, and GL links the two by name -- so a varying the pixel half
/// reads and the vertex half never writes is not a link error, it is a silently undefined value.
/// `sh_FUCKINGSAND` is the game's simplest real material: sample a texture, modulate by the vertex
/// colour, keep alpha. If its texture coordinate were not fed, it would render one flat colour.
#[test]
fn a_game_material_pairs_with_the_pass_through_vertex_shader() {
    let (vertex, pixel) = (translate("vs_pass.vs3"), translate("sand.ps2"));
    assert_well_formed(&pixel.glsl);
    assert_eq!(pixel.stage, Stage::Pixel);

    for varying in ["vary_texcoord0", "vary_color0"] {
        assert!(pixel.glsl.contains(&format!("in vec4 {};", varying)), "{} is not read", varying);
        assert!(vertex.glsl.contains(&format!("out vec4 {};", varying)), "{} is never written", varying);
    }
    // The texture coordinate reaches the sampler unmodified -- this shader does no maths on it.
    assert!(pixel.glsl.contains("texture(ps_s0, (vary_texcoord0).xy)"));
}

/// A colour-grading lookup table, and the reason a sampler's *name* has to survive translation:
/// `table` is declared without an explicit register, so the game can only reach it through
/// `texture_set_stage("table", ...)`, which resolves the name against the constant table. Losing
/// the name means the lookup silently reads whatever was left in that stage.
#[test]
fn resolves_a_sampler_declared_without_a_register() {
    let shader = translate("lut.ps2");
    assert_well_formed(&shader.glsl);
    assert_eq!(shader.samplers.len(), 2, "expected the source texture and the table");

    let table = shader.constants.find("table").expect("`table` is not in the constant table");
    let source = shader.constants.find("rSampler").expect("`rSampler` is not in the constant table");
    // `rSampler` claims s0 explicitly, so the unregistered one must have landed after it.
    assert_eq!(source.register_index, 0);
    assert_ne!(table.register_index, source.register_index, "both samplers resolved to one stage");
    assert!(shader.glsl.contains(&format!("uniform sampler2D ps_s{};", table.register_index)));
}
