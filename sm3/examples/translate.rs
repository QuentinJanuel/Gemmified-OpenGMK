//! Prints the GLSL for one compiled shader, for eyeballing a translation next to its disassembly.
//!
//! Usage: cargo run --example translate -p sm3 -- <path-to-.vs3-or-.ps3>

fn main() {
    let path = std::env::args().nth(1).expect("usage: translate <shader file>");
    let bytes = std::fs::read(&path).expect("couldn't read shader");
    match sm3::translate(&bytes) {
        Ok(shader) => {
            println!("// {:?}, {} float registers", shader.stage, shader.float_register_count);
            for constant in &shader.constants.constants {
                println!(
                    "// {} -> {:?} c{}[{}]",
                    constant.name, constant.set, constant.register_index, constant.register_count
                );
            }
            println!("{}", shader.glsl);
        },
        Err(e) => {
            eprintln!("{}: {}", path, e);
            std::process::exit(1);
        },
    }
}
