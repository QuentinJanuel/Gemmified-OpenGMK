#!/usr/bin/env node
// Runs a Rust test binary built for wasm32-wasi. Wired up as cargo's runner for that target in
// ../.cargo/config.toml, so `cargo test --target wasm32-wasi` just works.
//
// Why this exists at all: gm8emulator's unit tests cannot be built for the host. The crate pulls
// in `udon` for desktop audio on any non-wasm target, and udon's libsoundio-sys build script
// unwraps pkg-config lookups for alsa, libpulse *and* jack under a blanket `cfg!(unix)` -- it
// never tries CoreAudio, so the crate does not build on macOS at all. Targeting wasm32 sidesteps
// that entirely (the audio backend is cfg'd out), and has the side benefit of running the
// arithmetic on the architecture this engine actually ships to.
//
// Node rather than wasmtime deliberately: the build already needs Node, so every machine and CI
// job has a runtime capable of this. Adding wasmtime would be a third toolchain to install
// and pin for no gain.
//
// Two things to know about wasm32 test binaries, both consequences of panic=abort:
//   - A failing assertion aborts the process rather than being caught and reported per-test, so a
//     red run stops at the first failure. Isolate one with `--exact <name>`.
//   - `#[should_panic]` tests cannot work and the harness silently counts them as ignored. Do not
//     add one here expecting it to gate anything (gml::value::tests::op_add_invalid is one).
import { readFile } from "node:fs/promises";
import { argv, env, exit } from "node:process";

// node:wasi emits an ExperimentalWarning on import, which would land in every CI log. Filter just
// that one rather than running with --no-warnings, which would hide genuine warnings too. The
// import has to be dynamic so this is installed before the module is evaluated.
const emitWarning = process.emitWarning;
process.emitWarning = (warning, ...rest) => {
  if (String(warning).includes("WASI is an experimental feature")) return;
  return emitWarning.call(process, warning, ...rest);
};
const { WASI } = await import("node:wasi");

// cargo appends the test binary path, then any harness arguments (--exact, --list, ...).
const [, , wasmPath, ...testArgs] = argv;

if (!wasmPath) {
  console.error("usage: wasi-run.mjs <test.wasm> [harness args...]");
  exit(2);
}

const wasi = new WASI({
  version: "preview1",
  // argv[0] is conventionally the program name; the harness parses the rest.
  args: [wasmPath, ...testArgs],
  env,
  // Without this, a non-zero exit from the guest throws instead of giving us the code.
  returnOnExit: true,
});

const module = await WebAssembly.compile(await readFile(wasmPath));
const instance = await WebAssembly.instantiate(module, wasi.getImportObject());

exit(wasi.start(instance));
