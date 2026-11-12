pub mod dll;
mod dummy;
pub mod fmod;
pub mod gm82;
pub mod gm82buf;
pub mod gm82dx8;
pub mod gm82dx9;
pub mod gm82gltf;
pub mod httpdll;
pub mod maizemusic;
pub mod sgaudio;
pub mod ssound;
pub mod win32;
#[cfg(not(target_arch = "wasm32"))]
mod wow64;

use crate::{gml, gml::Function, types::ID};
use serde::{Deserialize, Serialize};

#[cfg(not(all(target_os = "windows", target_arch = "x86")))]
use dummy as native;
#[cfg(all(target_os = "windows", target_arch = "x86"))]
use win32 as native;

#[cfg(all(target_os = "windows", target_arch = "x86"))]
use dummy as ipc;
// No subprocess spawning exists in a browser; the wow64 helper-process backend cannot work there.
#[cfg(target_arch = "wasm32")]
use dummy as ipc;
#[cfg(not(any(all(target_os = "windows", target_arch = "x86"), target_arch = "wasm32")))]
use wow64 as ipc;

pub use native::NativeExternal;

pub enum Call {
    Dummy {
        value: gml::Value,
        /// `true` for the catch-all fallback in `make_call` -- a DLL (or a symbol on a partly
        /// reimplemented one) nobody has emulated yet, as opposed to the deliberate no-ops
        /// `should_dummy` returns. Only this kind warns, and only when the game actually *calls*
        /// it: plenty of games `external_define` an entire DLL's ~150-function surface up front
        /// (GM8-era wrapper libraries ship one `external_define` per export in a single
        /// `LoadXxx` script) and then call five of them, so warning at definition time would
        /// bury the handful that matter under a hundred that never run.
        unsupported: bool,
    },
    Emulated(Function),
    Native(NativeExternal),
    Ipc(ipc::IpcExternal),
}

pub struct External {
    pub call: Call,
    pub signature: dll::ExternalSignature,
}

pub struct ExternalManager {
    externals: Vec<Option<External>>,
    dummy_audio: bool,

    native_manager: native::NativeManager,
    ipc_manager: ipc::IpcManager,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ExternalState {
    signatures: Vec<Option<dll::ExternalSignature>>,
}

impl ExternalManager {
    pub fn new(dummy_audio: bool) -> Self {
        Self {
            externals: Vec::new(),
            dummy_audio,
            native_manager: native::NativeManager::new(),
            ipc_manager: ipc::IpcManager::new(),
        }
    }

    fn make_call(&mut self, signature: &dll::ExternalSignature) -> Result<Call, String> {
        if let Some(dummy) = self.should_dummy(&signature) {
            return Ok(Call::Dummy { value: dummy, unsupported: false })
        }
        if cfg!(all(target_os = "windows", target_arch = "x86")) {
            Ok(Call::Native(self.native_manager.define(&signature)?))
        } else if cfg!(target_arch = "wasm32") {
            // There's no execution model for arbitrary native DLL code in a browser at all --
            // unlike native's `Call::Ipc` (a real helper-process call that can fail but is at
            // least attempted), `game/external/dummy.rs`'s IPC stand-in `define()`s successfully
            // but panics the instant the game actually *calls* the external (`call_external` in
            // `game.rs`), which crashes the whole page the first time any GML code touches it.
            let dll = dll_basename(&signature.dll);
            if dll.eq_ignore_ascii_case("ssound.dll") || dll.eq_ignore_ascii_case("supersound.dll") {
                if let Some(func) = ssound::resolve(&signature.symbol) {
                    return Ok(Call::Emulated(func))
                }
            }
            // OpenAL-backed external music, shipped next to `OpenAL32.dll`/`wrap_oal.dll`. Its
            // emitter model is neither ssound's nor FMOD's, so it gets its own module -- see
            // `sgaudio.rs`.
            if dll.eq_ignore_ascii_case("sgaudio.dll") {
                if let Some(func) = sgaudio::resolve(&signature.symbol) {
                    return Ok(Call::Emulated(func))
                }
            }
            // BASS-backed external music, shipped as an extension package alongside `bass.dll`
            // and `bass_fx.dll`. Its handles are BASS channels and its whole model is "one file,
            // one playback state", like sgaudio's emitters but with no group layer -- see
            // `maizemusic.rs`.
            if dll.eq_ignore_ascii_case("maizemusic.dll") {
                if let Some(func) = maizemusic::resolve(&signature.symbol) {
                    return Ok(Call::Emulated(func))
                }
            }
            // GM8.2's replacement audio system re-exports GMFMODSimple's exact symbol names
            // (`FMODinit`, `FMODSoundAdd`, `FMODSoundPlay`, ...) from a differently-named DLL, so
            // it resolves against the same reimplementation rather than a parallel one.
            if dll.eq_ignore_ascii_case("gmfmodsimple.dll") || dll.eq_ignore_ascii_case("gm82snd.dll") {
                if let Some(func) = fmod::resolve(&signature.symbol) {
                    return Ok(Call::Emulated(func))
                }
            }
            // The GM8.2 fork's own DLLs. `gm82core` and `gm82dx9` (below) both refuse to initialize
            // unless a version handshake answers 820, so without these every GM8.2 game stops at
            // `show_error` before its first frame -- see `gm82.rs`.
            if dll.eq_ignore_ascii_case("gm82core.dll") || dll.eq_ignore_ascii_case("inst_meta.dll") {
                if let Some(func) = gm82::resolve(&signature.symbol) {
                    return Ok(Call::Emulated(func))
                }
            }
            // The graphics half: the same handshake, plus the matrix, render-state and vertex-buffer
            // surface GM8 itself never had. See `gm82dx9.rs` for what it deliberately does not do.
            if dll.eq_ignore_ascii_case("gm82dx9.dll") {
                if let Some(func) = gm82dx9::resolve(&signature.symbol) {
                    return Ok(Call::Emulated(func))
                }
            }
            // The same handshake again, for the DirectX 8 extension a game built against the older
            // GM8.2 IDE links *instead* of gm82dx9 (they are mutually exclusive). Its exports are
            // dx9's under older names, so `gm82dx8.rs` is mostly a translation table onto the
            // module above -- but it needs its own entry here, because a missing handshake is the
            // same fatal `show_error` before the first frame.
            if dll.eq_ignore_ascii_case("gm82dx8.dll") {
                if let Some(func) = gm82dx8::resolve(&signature.symbol) {
                    return Ok(Call::Emulated(func))
                }
            }
            // Not implemented, but its load *must* answer negatively rather than let the generic
            // no-op's 0 pass for a loaded scene -- see `gm82gltf.rs`.
            if dll.eq_ignore_ascii_case("gm82gltf.dll") {
                if let Some(func) = gm82gltf::resolve(&signature.symbol) {
                    return Ok(Call::Emulated(func))
                }
            }
            // The socket extension a patched game reaches its host through, to loopback only --
            // see `httpdll.rs`.
            if dll.eq_ignore_ascii_case("http_dll_2_3.dll") {
                if let Some(func) = httpdll::resolve(&signature.symbol) {
                    return Ok(Call::Emulated(func))
                }
            }
            // GM8.2's buffer library. Games build their entire save system on it -- Verve, the most
            // widely used GM8.2 fangame engine, writes saves as `buffer_write_hex` + `buffer_rc4` +
            // `buffer_save` -- so an unimplemented one is not a cosmetic gap: saving silently does
            // nothing and no progress survives a reload.
            if dll.eq_ignore_ascii_case("gm82buf.dll") {
                // Its socket half is Http Dll's, over these buffers -- see `httpdll.rs`.
                if let Some(func) =
                    gm82buf::resolve(&signature.symbol).or_else(|| httpdll::resolve_gm82_sockets(&signature.symbol))
                {
                    return Ok(Call::Emulated(func))
                }
            }
            // Not a DLL with a real reimplementation (or an unrecognized symbol on one that has
            // one) -- no-op with a value matching the declared return type instead of ever
            // reaching the panic above. Keeps the game running (whatever that external provided
            // just does nothing) rather than crashing on a DLL nobody's specifically
            // reimplemented yet, but flagged so the first actual call says so out loud: a
            // silent no-op here is indistinguishable from a working call, which is exactly how
            // an unregistered `FMODSoundPlay3d` presented as "the game has no music" with
            // nothing anywhere pointing at the missing symbol.
            Ok(Call::Dummy { value: generic_dummy(signature), unsupported: true })
        } else {
            Ok(Call::Ipc(self.ipc_manager.define(&signature)?))
        }
    }

    pub fn define(&mut self, signature: dll::ExternalSignature) -> Result<ID, String> {
        let external = External { call: self.make_call(&signature)?, signature };
        if let Some((id, cell)) = self.externals.iter_mut().enumerate().find(|(_, o)| o.is_none()) {
            *cell = Some(external);
            Ok(id as ID)
        } else {
            let id = self.externals.len();
            self.externals.push(Some(external));
            Ok(id as ID)
        }
    }

    pub fn get_external(&self, id: i32) -> Option<&External> {
        id.try_into().ok().and_then(|id: usize| self.externals.get(id).map(Option::as_ref).flatten())
    }

    pub fn call_native(&mut self, id: usize, args: &[dll::Value]) -> gml::Value {
        match &self.externals[id].as_ref().unwrap().call {
            Call::Native(ext) => self.native_manager.call(ext, args).into(),
            _ => unreachable!(),
        }
    }

    pub fn call_ipc(&mut self, id: usize, args: &[dll::Value]) -> gml::Value {
        match &self.externals[id].as_ref().unwrap().call {
            Call::Ipc(ext) => self.ipc_manager.call(ext, args).into(),
            _ => unreachable!(),
        }
    }

    pub fn free(&mut self, dll: &str) {
        for option in &mut self.externals {
            if let Some(external) = option.as_ref() {
                if external.signature.dll.eq_ignore_ascii_case(dll) {
                    // ipc externals need the manager so free those with that
                    match option.take().unwrap().call {
                        Call::Ipc(e) => self.ipc_manager.free(e),
                        _ => (),
                    }
                }
            }
        }
    }

    pub fn save_state(&self) -> ExternalState {
        let signatures = self.externals.iter().map(|o| o.as_ref().map(|e| e.signature.clone())).collect();
        ExternalState { signatures }
    }

    pub fn load_state(&mut self, mut state: ExternalState) {
        self.externals.clear();
        for opt in state.signatures.drain(..) {
            let external = opt.map(|s| External { call: self.make_call(&s).unwrap(), signature: s });
            self.externals.push(external);
        }
    }

    fn should_dummy(&self, signature: &dll::ExternalSignature) -> Option<gml::Value> {
        let dll = &signature.dll;
        let sym = &signature.symbol;
        let dll = dll_basename(dll);

        let mut dummy = None;
        if self.dummy_audio {
            if dll.eq_ignore_ascii_case("gmfmodsimple.dll") {
                if sym == "FMODSoundAdd" {
                    dummy = Some(gml::Value::Real(1.into()));
                } else {
                    dummy = Some(gml::Value::Real(0.into()));
                }
            } else if dll.eq_ignore_ascii_case("ssound.dll") || dll.eq_ignore_ascii_case("supersound.dll") {
                if sym == "SS_Init" {
                    dummy = Some(gml::Value::Str("Yes".into()));
                } else {
                    dummy = Some(gml::Value::Real(0.into()));
                }
            } else if dll.eq_ignore_ascii_case("sgaudio.dll") {
                // `sga_Init` answering 0 is specifically how the real DLL reports *failure*, and a
                // game is entitled to bail on it -- so the silent stand-in still says it succeeded.
                // The two emitter-producing calls answer -1 rather than 0, which would name a
                // perfectly plausible emitter and leave every later call looking live; -1 is the
                // sentinel games already initialize their own emitter variables to.
                dummy = Some(match sym.as_str() {
                    "sga_Init" => gml::Value::Real(1.into()),
                    "sga_CreateEmitter" | "sga_TrackPlay" => gml::Value::Real((-1).into()),
                    _ => gml::Value::Real(0.into()),
                });
            } else if dll.eq_ignore_ascii_case("maizemusic.dll") {
                // `mmInit` answering 0 is how the real DLL reports that BASS could not open the
                // output device, and a game is entitled to bail on it -- so the silent stand-in
                // still says it initialized. A loader answering 0 is BASS's own "no stream", which
                // every later call then validates and ignores, so none of them need an answer of
                // their own; `mmMusicIsActive` must read as *stopped* rather than paused, which 0
                // already is.
                dummy = Some(match sym.as_str() {
                    "mmInit" | "mmFree" => gml::Value::Real(1.into()),
                    "mmGetDeviceName" => gml::Value::Str("".into()),
                    _ => gml::Value::Real(0.into()),
                });
            } else if dll.eq_ignore_ascii_case("sxms-3.dll") {
                dummy = Some(gml::Value::Real(0.into()));
            } else if dll.eq_ignore_ascii_case("caster.dll") {
                if sym == "caster_error_message" || sym == "caster_version" {
                    dummy = Some(gml::Value::Str("".into()));
                } else {
                    dummy = Some(gml::Value::Real(0.into()));
                }
            }
        }

        if dll.eq_ignore_ascii_case("gmeffect_0.1.dll") {
            // TODO: don't
            // ^ floogle's original comment, whatever it may mean
            // ^ viri's original comment, made without consulting floogle
            dummy = Some(gml::Value::Real(0.into()));
        }

        dummy
    }
}

/// Logs, once per `dll!symbol` for the lifetime of the page, that a game just called an external
/// nothing reimplements -- the call itself still returns `generic_dummy`'s inert value and the
/// game keeps running.
///
/// Once, not per call: an unsupported external is very often called from a Step event, so a
/// per-call log would be tens of thousands of identical lines a second and would itself become a
/// performance problem. The set is keyed by symbol rather than by external id so re-`define`-ing
/// the same function (which `load_state` does for every external at once) doesn't re-warn.
#[cfg(target_arch = "wasm32")]
pub fn warn_unsupported_once(dll: &str, symbol: &str) {
    use std::{cell::RefCell, collections::HashSet};
    thread_local! {
        static WARNED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    }
    let key = format!("{}!{}", dll_basename(dll).to_lowercase(), symbol);
    if WARNED.with(|w| w.borrow_mut().insert(key)) {
        web_sys::console::warn_1(
            &format!(
                "unimplemented external: {}:{} was called and did nothing (returned 0/\"\"). \
                 Whatever it does in the real game will be missing.",
                dll_basename(dll),
                symbol,
            )
            .into(),
        );
    }
}

/// Native builds reach real DLL code (or the wow64 helper process) rather than the dummy
/// fallback, so there is nothing to report and no console to report it to.
#[cfg(not(target_arch = "wasm32"))]
pub fn warn_unsupported_once(_dll: &str, _symbol: &str) {}

/// Games sometimes declare an external by a full path (`"C:\Games\ssound.dll"`, or -- far more
/// often -- `working_directory + "\ssound.dll"`); match by filename only, same as a real Windows
/// DLL search would effectively do for a same-directory DLL.
///
/// Splits on both separators by hand rather than using `Path::file_name`, which follows the *host*
/// platform's rules: on wasm (unix semantics) a backslash is an ordinary filename character, so
/// `"\ssound.dll"` came back whole and matched none of the DLL names below -- silently dropping a
/// game's entire reimplemented sound system into the generic no-op fallback.
fn dll_basename(dll: &str) -> &str {
    dll.rsplit(['\\', '/']).next().unwrap_or(dll)
}

/// Whether this build supplies the named DLL's functionality itself, so a game that ships the DLL
/// beside its `.exe` needs no such file here.
///
/// GM8-era DLL wrapper scripts almost all open with a sanity check --
/// `if !file_exists('GMFMODSimple.dll') show_message("File not found: ...")` is the stock
/// GMFMODSimple preamble, and ssound's and sgaudio's read the same way. On wasm the bare `.exe` is
/// often all a game's download holds, so that check finds nothing and every one of those games
/// would open with a "file not found" box -- while `external_define` on the very same DLL resolves
/// perfectly well against the reimplementations above. The box would be wrong about the only thing
/// it is really asking: whether these functions are going to work.
///
/// So `gml::file::file_exists` consults this (see its wasm branch). Listed here rather than there
/// because this is the same question `make_call` answers, and the two must not drift apart.
///
/// The companion runtimes (`fmodex.dll`, OpenAL's) are included deliberately: a game never
/// `external_define`s them -- they are what the wrapper DLL itself loads -- so a game checking for
/// one is asking whether its audio backend is present, and the reimplementation it is really
/// talking to subsumes them.
#[cfg(target_arch = "wasm32")]
pub fn is_emulated_dll(path: &str) -> bool {
    const EMULATED: &[&str] = &[
        // Wrappers with a real reimplementation, mirroring `make_call`'s dispatch above.
        "ssound.dll",
        "supersound.dll",
        "sgaudio.dll",
        "gmfmodsimple.dll",
        "gm82snd.dll",
        "maizemusic.dll",
        "gm82core.dll",
        "inst_meta.dll",
        "gm82dx9.dll",
        "gm82dx8.dll",
        "gm82buf.dll",
        // Backends the above load rather than the game itself. `gm82gltf.dll` is deliberately
        // absent: it has no reimplementation, only a load that answers negatively on purpose.
        "fmodex.dll",
        "openal32.dll",
        "wrap_oal.dll",
        // MaizeMusic's. Its own GML initializer copies these two out of the extension's temp
        // folder into the working directory `if !file_exists(...)` first, so answering yes is
        // also what keeps that copy from happening at all.
        "bass.dll",
        "bass_fx.dll",
    ];
    let name = dll_basename(path);
    EMULATED.iter().any(|dll| name.eq_ignore_ascii_case(dll))
}

/// A harmless placeholder value matching an external function's *declared* return type, for a DLL
/// `should_dummy` doesn't recognize specifically. Matching the declared type (rather than e.g.
/// always `Real(0)`) matters because plenty of GM8-era DLL wrapper scripts compare a call's result
/// against a string constant (`if external_call(...) = "No"` is exactly what this game's own
/// `ss_Init.gml` does) -- a type-mismatched dummy could make that comparison behave unpredictably
/// instead of just falling through as "no, nothing happened."
fn generic_dummy(signature: &dll::ExternalSignature) -> gml::Value {
    match signature.type_return {
        dll::ValueType::Real => gml::Value::Real(0.into()),
        dll::ValueType::Str => gml::Value::Str("".into()),
    }
}
