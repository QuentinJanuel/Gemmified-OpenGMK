//! Reimplementation of `gm82buf.dll`, GameMaker 8.2's buffer library. Not specific to any one game:
//! the extension package ships with GM8.2 and games use it for save files, binary parsing, and
//! (through the socket half, not implemented here) networking.
//!
//! A buffer is just a byte vector plus a caret. Every operation reads or writes at the caret and
//! advances it, except the whole-buffer ones (`buffer_rc4_crypt`, the file load/save pair, and
//! `buffer_get_length`).
//!
//! Written against the extension's own documented interface -- the argument lists, the caret
//! behaviour, and the worked examples in its help text ("cat" in the buffer reads back as `636174`;
//! applying the same RC4 password a second time unscrambles) -- rather than from its source, which
//! is LGPL-3. RC4 is a published algorithm and hex is trivial, so nothing here needed transcribing.
//!
//! **Why this matters for real games:** the Verve engine, the most widely used GM8.2 fangame
//! framework, implements its *entire* save system on top of these six functions:
//!
//! ```text
//! buffer_write_hex(b, ds_map_write(save_map));   // ds_*_write returns a hex string
//! buffer_rc4(b, password);
//! buffer_save(b, file);
//! ```
//!
//! With these unimplemented, `buffer_save` silently did nothing: saving appeared to work in-session
//! (the state lives in a ds_map) but nothing ever reached disk, so no file appeared and no progress
//! survived a reload.
//!
//! Wired in from `external.rs`'s `make_call`, via the same `Call::Emulated(Function)` extension
//! point `ssound.rs`, `fmod.rs` and `gm82.rs` use.

use crate::{
    game::Game,
    gml::{self, Context, Function, Value},
};

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use std::{cell::RefCell, collections::HashMap};

    fn arg_str(v: &Value) -> std::borrow::Cow<'_, str> {
        crate::game::decode_gml_string(<&Value as Into<&[u8]>>::into(v))
    }

    fn arg_f64(v: &Value) -> f64 {
        match v {
            Value::Real(r) => (*r).into(),
            Value::Str(_) => arg_str(v).parse().unwrap_or(0.0),
        }
    }

    fn arg_id(v: Option<&Value>) -> i32 {
        v.map(arg_f64).unwrap_or(-1.0) as i32
    }

    fn ok_num(n: f64) -> gml::Result<Value> {
        Ok(Value::Real(n.into()))
    }

    fn ok_str(s: String) -> gml::Result<Value> {
        Ok(Value::from(s))
    }

    struct Buf {
        data: Vec<u8>,
        pos: usize,
    }

    thread_local! {
        static BUFFERS: RefCell<HashMap<i32, Buf>> = RefCell::new(HashMap::new());
        // Ids start at 1, never 0. GML treats 0 as false, and this extension family cares: gm82core's
        // own initializer creates and leaks one of every ds_* type purely so real ones start at 1.
        // A buffer id of 0 would make the common `if (buffer)` guard read as "creation failed".
        static NEXT_ID: RefCell<i32> = RefCell::new(1);
        // Names a `buffer_make_pipe` apart from every other one, so two live pipes never collide.
        static NEXT_PIPE: RefCell<u32> = RefCell::new(0);
    }

    fn with_buf<R>(id: i32, f: impl FnOnce(&mut Buf) -> R) -> Option<R> {
        BUFFERS.with(|b| b.borrow_mut().get_mut(&id).map(f))
    }

    /// `buffer_get_address(buffer)` -- where the buffer's bytes actually are.
    ///
    /// This extension family hands buffers to *other* DLLs as a bare pointer plus a length rather
    /// than as an id: `vertex_buffer_create`, `buffer_get_surface` and `shader_pixel_create_buffer`
    /// are all `__gm82dx9_...(buffer_get_address(b), buffer_get_size(b))`. So an address has to be
    /// something the receiving side can turn back into these bytes, and since both sides are this
    /// one wasm module, the honest answer is the real address: `read_at`/`write_at` find the buffer
    /// whose allocation contains it. That also makes the `address + offset` arithmetic games do on
    /// one work.
    fn get_address(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        ok_num(
            with_buf(arg_id(args.first()), |b| {
                // An empty `Vec` owns no allocation and every one of them reports the same dangling
                // address, which would make two empty buffers indistinguishable to a lookup. A byte
                // of capacity costs nothing and gives each its own.
                if b.data.capacity() == 0 {
                    b.data.reserve(1);
                }
                b.data.as_ptr() as usize as f64
            })
            .unwrap_or(0.0),
        )
    }

    fn with_addr<R>(addr: f64, f: impl FnOnce(&mut Buf, usize) -> R) -> Option<R> {
        if !(addr > 0.0) {
            return None
        }
        let addr = addr as usize;
        BUFFERS.with(|b| {
            let mut buffers = b.borrow_mut();
            let buf = buffers.values_mut().find(|buf| {
                let base = buf.data.as_ptr() as usize;
                // `.max(1)` so a zero-length buffer still answers for its own base address.
                addr >= base && addr < base + buf.data.len().max(1)
            })?;
            let offset = addr - buf.data.as_ptr() as usize;
            Some(f(buf, offset))
        })
    }

    /// The `len` bytes at `addr`, for a caller that was handed a `buffer_get_address`. Stops at the
    /// end of the buffer rather than reading into whatever is next in memory.
    /// A buffer's whole contents, for `httpdll.rs`'s sockets: gm82buf ships Http Dll's socket
    /// functions alongside its own buffers, and a message is written from one of these.
    pub fn contents(id: i32) -> Vec<u8> {
        with_buf(id, |b| b.data.clone()).unwrap_or_default()
    }

    /// Replaces a buffer's contents and rewinds it -- what a socket read does to the buffer it fills.
    pub fn fill(id: i32, data: Vec<u8>) {
        with_buf(id, |b| {
            b.data = data;
            b.pos = 0;
        });
    }

    pub fn read_at(addr: f64, len: usize) -> Option<Vec<u8>> {
        with_addr(addr, |buf, offset| {
            let end = (offset + len).min(buf.data.len());
            buf.data[offset.min(end)..end].to_vec()
        })
    }

    /// Writes over the bytes at `addr`, clamped to the end of the buffer -- a real DLL writing
    /// through this pointer could not grow the caller's allocation either, and every GML wrapper
    /// that passes an address sizes the buffer first.
    pub fn write_at(addr: f64, data: &[u8]) -> bool {
        with_addr(addr, |buf, offset| {
            let end = (offset + data.len()).min(buf.data.len());
            if end > offset {
                buf.data[offset..end].copy_from_slice(&data[..end - offset]);
            }
        })
        .is_some()
    }

    fn create(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        let id = NEXT_ID.with(|n| {
            let mut n = n.borrow_mut();
            let id = *n;
            *n += 1;
            id
        });
        BUFFERS.with(|b| b.borrow_mut().insert(id, Buf { data: Vec::new(), pos: 0 }));
        ok_num(f64::from(id))
    }

    fn destroy(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        BUFFERS.with(|b| b.borrow_mut().remove(&id));
        ok_num(1.0)
    }

    fn exists(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        ok_num(f64::from(u8::from(BUFFERS.with(|b| b.borrow().contains_key(&id)))))
    }

    fn get_length(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        ok_num(with_buf(id, |b| b.data.len() as f64).unwrap_or(0.0))
    }

    fn get_pos(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        ok_num(with_buf(id, |b| b.pos as f64).unwrap_or(0.0))
    }

    fn set_pos(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let pos = args.get(1).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        with_buf(id, |b| {
            // Seeking past the end grows the buffer, so a later write lands where the game asked
            // rather than silently earlier -- the caret is an index into a resizable buffer, not a
            // cursor over a fixed-length file.
            if pos > b.data.len() {
                b.data.resize(pos, 0);
            }
            b.pos = pos;
        });
        ok_num(1.0)
    }

    fn set_size(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let size = args.get(1).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        with_buf(id, |b| {
            b.data.resize(size, 0);
            b.pos = b.pos.min(b.data.len());
        });
        ok_num(1.0)
    }

    fn clear(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        with_buf(id, |b| {
            b.data.clear();
            b.pos = 0;
        });
        ok_num(1.0)
    }

    fn at_end(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        ok_num(f64::from(u8::from(with_buf(id, |b| b.pos >= b.data.len()).unwrap_or(true))))
    }

    /// `buffer_make_pipe(buffer)` -- a name that can be passed anywhere a *filename* goes, and
    /// reads back as this buffer's contents. On Windows it is a real named pipe; the idiom it
    /// exists for is feeding a GML function that only takes a path from memory instead:
    ///
    /// ```text
    /// buffer_inflate(b);                          // d3d_model_load_g3z, verbatim
    /// d3d_model_load(model, buffer_make_pipe(b));
    /// ```
    ///
    /// The name keeps the `\\.\pipe\` prefix because games test for it (`d3d_model_create_and_load`
    /// skips its `file_exists` check when it sees one), and the bytes are published in memory only
    /// -- see `file::register_pipe_file` for why they must not reach the filesystem layer.
    ///
    /// A snapshot, not a live view: the reader gets the buffer as it is now, and later writes to
    /// the buffer do not change it. Every use of this loads a finished buffer immediately.
    fn make_pipe(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let data = match with_buf(id, |b| b.data.clone()) {
            Some(data) => data,
            None => return Ok(Value::from("")),
        };
        let name = NEXT_PIPE.with(|n| {
            let mut n = n.borrow_mut();
            *n += 1;
            format!("\\\\.\\pipe\\gm82buf_{}", *n)
        });
        crate::gml::file::register_pipe_file(&name, data);
        Ok(Value::from(name))
    }

    // -- raw byte access ---------------------------------------------------------------------

    fn write_bytes(b: &mut Buf, bytes: &[u8]) {
        let end = b.pos + bytes.len();
        if end > b.data.len() {
            b.data.resize(end, 0);
        }
        b.data[b.pos..end].copy_from_slice(bytes);
        b.pos = end;
    }

    fn read_bytes(b: &mut Buf, len: usize) -> Vec<u8> {
        // A short read returns what is actually there rather than failing: GML has no way to signal
        // a partial read here, and every caller treats the result as "the data".
        let end = (b.pos + len).min(b.data.len());
        let out = if b.pos < end { b.data[b.pos..end].to_vec() } else { Vec::new() };
        b.pos = end;
        out
    }

    fn write_uint(id: i32, value: f64, width: usize) -> gml::Result<Value> {
        let raw = (value as i64) as u64;
        with_buf(id, |b| write_bytes(b, &raw.to_le_bytes()[..width]));
        ok_num(1.0)
    }

    fn read_uint(id: i32, width: usize, signed: bool) -> gml::Result<Value> {
        let bytes = with_buf(id, |b| read_bytes(b, width)).unwrap_or_default();
        let mut raw = [0u8; 8];
        raw[..bytes.len()].copy_from_slice(&bytes);
        let unsigned = u64::from_le_bytes(raw);
        let value = if signed && width < 8 {
            // Sign-extend from the declared width.
            let shift = 64 - width * 8;
            ((unsigned << shift) as i64 >> shift) as f64
        } else {
            unsigned as f64
        };
        ok_num(value)
    }

    // -- hex -------------------------------------------------------------------------------------
    // The pair the save systems actually use. GM8's own `ds_*_write` produces a hex string, so a
    // game round-trips a data structure through `buffer_write_hex` on save and `buffer_read_hex` on
    // load. Per the extension's own example: the bytes for "cat" read back as "636174".

    fn write_hex(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let text = args.get(1).map(arg_str).unwrap_or_default();
        let digits: Vec<u8> = text.bytes().filter(|c| c.is_ascii_hexdigit()).collect();
        let mut bytes = Vec::with_capacity(digits.len() / 2);
        for pair in digits.chunks_exact(2) {
            let hi = (pair[0] as char).to_digit(16).unwrap_or(0) as u8;
            let lo = (pair[1] as char).to_digit(16).unwrap_or(0) as u8;
            bytes.push(hi << 4 | lo);
        }
        with_buf(id, |b| write_bytes(b, &bytes));
        ok_num(1.0)
    }

    fn read_hex(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let len = args.get(1).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        let bytes = with_buf(id, |b| read_bytes(b, len)).unwrap_or_default();
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            out.push_str(&format!("{byte:02x}"));
        }
        ok_str(out)
    }

    // -- RC4 -------------------------------------------------------------------------------------

    /// Standard RC4 over the whole buffer, ignoring the caret. Symmetric, which is what the
    /// extension's "apply the same password again to unscramble" contract means and what lets a
    /// game call it once on save and once on load.
    ///
    /// Not a security boundary -- RC4 is long broken, and the password sits in the game's own GML
    /// in plaintext anyway. It is here because save files on disk were scrambled with it and have
    /// to be unscrambled byte-identically.
    fn rc4_crypt(data: &mut [u8], key: &[u8]) {
        if key.is_empty() {
            return
        }
        let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }
        let (mut i, mut j) = (0u8, 0u8);
        for byte in data.iter_mut() {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
            *byte ^= k;
        }
    }

    fn rc4(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let key = args.get(1).map(arg_str).unwrap_or_default().into_owned();
        with_buf(id, |b| rc4_crypt(&mut b.data, key.as_bytes()));
        ok_num(1.0)
    }

    /// `buffer_rc4_buffer(buffer, key_buffer)` -- the same cipher with the key taken from another
    /// buffer, so it can contain bytes a GML string cannot carry.
    fn rc4_buffer(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (id, key_id) = (arg_id(args.first()), arg_id(args.get(1)));
        let key = with_buf(key_id, |b| b.data.clone()).unwrap_or_default();
        with_buf(id, |b| rc4_crypt(&mut b.data, &key));
        ok_num(1.0)
    }

    // -- file I/O --------------------------------------------------------------------------------
    // Through the shared VFS, exactly like every other file operation: a load resolves the store
    // over the bundle (so a save the player made wins over one shipped with the game), and a save
    // lands in the store.

    fn load_file(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let path = args.get(1).map(arg_str).unwrap_or_default().into_owned();
        let data = crate::gml::file::read_file_bytes(&path).unwrap_or_default();
        // "Loads the entire file into the buffer, resizing it to fit. Sets the caret to the start."
        // The caret reset is load-bearing: a game's very next call is typically a read of the whole
        // buffer, which would come back empty if the caret stayed at the end.
        with_buf(id, |b| {
            b.data = data;
            b.pos = 0;
        });
        ok_num(f64::from(id))
    }

    /// `buffer_load_part(buffer, file, offset, size)` -- `size` bytes from `offset` into the file,
    /// written at the caret. Unlike `buffer_load` this adds to what is already in the buffer rather
    /// than replacing it, which is the point: it is how a game reads one record out of a pack file.
    fn load_file_part(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let path = args.get(1).map(arg_str).unwrap_or_default().into_owned();
        let offset = args.get(2).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        let size = args.get(3).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        let data = crate::gml::file::read_file_bytes(&path).unwrap_or_default();
        let start = offset.min(data.len());
        let end = (offset + size).min(data.len());
        with_buf(id, |b| write_bytes(b, &data[start..end]));
        ok_num(f64::from(id))
    }

    fn save_file(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let path = args.get(1).map(arg_str).unwrap_or_default().into_owned();
        let data = with_buf(id, |b| b.data.clone()).unwrap_or_default();
        crate::gml::vfs::write(&path, &data);
        ok_num(1.0)
    }

    /// `buffer_save_append(buffer, file)` -- adds to the end of an existing file instead of
    /// replacing it. Read-modify-write because the VFS stores whole files, which is also how
    /// `file.rs`'s handle layer does an append.
    fn save_file_append(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let path = args.get(1).map(arg_str).unwrap_or_default().into_owned();
        let mut data = crate::gml::file::read_file_bytes(&path).unwrap_or_default();
        data.extend_from_slice(&with_buf(id, |b| b.data.clone()).unwrap_or_default());
        crate::gml::vfs::write(&path, &data);
        ok_num(1.0)
    }

    // -- strings and raw data --------------------------------------------------------------------

    /// Raw GML string bytes, not a decoded `String`: a buffer is binary, and round-tripping through
    /// the game's text encoding would mangle any byte that isn't valid in it.
    fn arg_raw(v: &Value) -> Vec<u8> {
        <&Value as Into<&[u8]>>::into(v).to_vec()
    }

    fn write_string(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let mut bytes = args.get(1).map(arg_raw).unwrap_or_default();
        bytes.push(0); // null-terminated, per the extension's contract
        with_buf(id, |b| write_bytes(b, &bytes));
        ok_num(1.0)
    }

    fn read_string(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let bytes = with_buf(id, |b| {
            let start = b.pos;
            let end = b.data[start.min(b.data.len())..].iter().position(|&c| c == 0).map(|n| start + n);
            match end {
                Some(nul) => {
                    let out = b.data[start..nul].to_vec();
                    b.pos = nul + 1; // step past the terminator
                    out
                },
                // No terminator: take the rest, matching a C reader walking off the end of the data
                // it was given rather than failing.
                None => {
                    let out = b.data[start.min(b.data.len())..].to_vec();
                    b.pos = b.data.len();
                    out
                },
            }
        })
        .unwrap_or_default();
        Ok(Value::from(bytes.as_slice()))
    }

    /// `buffer_write_data`/`buffer_read_data` -- the same bytes as the string pair but with an
    /// explicit length and no terminator, which is what a game uses when its data can contain a
    /// zero byte.
    fn write_data(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let bytes = args.get(1).map(arg_raw).unwrap_or_default();
        with_buf(id, |b| write_bytes(b, &bytes));
        ok_num(1.0)
    }

    fn read_data(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let len = args.get(1).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        let bytes = with_buf(id, |b| read_bytes(b, len)).unwrap_or_default();
        Ok(Value::from(bytes.as_slice()))
    }

    // -- floats ----------------------------------------------------------------------------------
    // NOTE: the extension's own doc comments on write_float32/write_float64 are swapped ("writes a
    // double" sits on the 32-bit one). The code is right and the comments are not; these follow the
    // code -- float32 is four bytes, float64 is eight.
    //
    // Reads sanitize NaN and infinity to 0, which the original does deliberately: GML has no way to
    // represent either, and letting one through poisons every later arithmetic op silently.

    fn write_f32(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let v = args.get(1).map(arg_f64).unwrap_or(0.0) as f32;
        with_buf(id, |b| write_bytes(b, &v.to_le_bytes()));
        ok_num(1.0)
    }

    /// `buffer_write_float2/3/4/12` -- several float32s in one call. They exist for vertex buffers:
    /// gm82dx9's `buffer_write_format_position` is `buffer_write_float4(b, x, y, z, 1)`, and the
    /// twelve-wide one writes a 4x3 matrix for skinning.
    fn write_f32s(id: i32, args: &[Value], count: usize) -> gml::Result<Value> {
        let mut bytes = Vec::with_capacity(count * 4);
        for i in 0..count {
            bytes.extend_from_slice(&(args.get(i + 1).map(arg_f64).unwrap_or(0.0) as f32).to_le_bytes());
        }
        with_buf(id, |b| write_bytes(b, &bytes));
        ok_num(1.0)
    }

    fn write_f64(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let v = args.get(1).map(arg_f64).unwrap_or(0.0);
        with_buf(id, |b| write_bytes(b, &v.to_le_bytes()));
        ok_num(1.0)
    }

    fn sane(v: f64) -> f64 {
        if v.is_finite() { v } else { 0.0 }
    }

    fn read_f32(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let bytes = with_buf(arg_id(args.first()), |b| read_bytes(b, 4)).unwrap_or_default();
        let mut raw = [0u8; 4];
        raw[..bytes.len().min(4)].copy_from_slice(&bytes[..bytes.len().min(4)]);
        ok_num(sane(f64::from(f32::from_le_bytes(raw))))
    }

    fn read_f64(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let bytes = with_buf(arg_id(args.first()), |b| read_bytes(b, 8)).unwrap_or_default();
        let mut raw = [0u8; 8];
        raw[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
        ok_num(sane(f64::from_le_bytes(raw)))
    }

    // -- random access and copying ---------------------------------------------------------------

    /// Reads one byte at an absolute position without moving the caret.
    fn peek(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let pos = args.get(1).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        ok_num(with_buf(id, |b| b.data.get(pos).copied().unwrap_or(0)).unwrap_or(0).into())
    }

    /// Writes one byte at an absolute position without moving the caret, growing the buffer if the
    /// position is past the end.
    fn poke(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let id = arg_id(args.first());
        let pos = args.get(1).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        let byte = args.get(2).map(arg_f64).unwrap_or(0.0) as i64 as u8;
        with_buf(id, |b| {
            if pos >= b.data.len() {
                b.data.resize(pos + 1, 0);
            }
            b.data[pos] = byte;
        });
        ok_num(1.0)
    }

    /// `buffer_copy(to, from)` -- writes the whole source buffer into the destination. Note the
    /// argument order: destination first.
    fn copy(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (to, from) = (arg_id(args.first()), arg_id(args.get(1)));
        let src = with_buf(from, |b| b.data.clone()).unwrap_or_default();
        with_buf(to, |b| write_bytes(b, &src));
        ok_num(1.0)
    }

    /// `buffer_copy_part(to, from, offset, size)` -- the same, but `size` bytes taken from an
    /// absolute `offset` in the source rather than all of it. The source's own caret is not moved,
    /// which is why every caller advances it by hand afterwards:
    ///
    /// ```text
    /// buffer_copy_part(tmp, b, buffer_get_pos(b), size);   // gm82dx9's bundle loader, verbatim
    /// buffer_set_pos(b, buffer_get_pos(b) + size);
    /// ```
    ///
    /// Load-bearing well beyond copying bytes about: this is how a model bundle unpacks its
    /// textures, so without it every bundled texture arrived as a zero-byte PNG, `background_add`
    /// answered -1, and the game died indexing a built-in array with it.
    fn copy_part(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let (to, from) = (arg_id(args.first()), arg_id(args.get(1)));
        let offset = args.get(2).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        let size = args.get(3).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        let src = with_buf(from, |b| {
            let start = offset.min(b.data.len());
            let end = (offset + size).min(b.data.len());
            b.data[start..end].to_vec()
        })
        .unwrap_or_default();
        with_buf(to, |b| write_bytes(b, &src));
        ok_num(1.0)
    }

    // -- base64 and checksums --------------------------------------------------------------------

    fn encode_base64(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        use base64::Engine;
        let id = arg_id(args.first());
        let len = args.get(1).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        let bytes = with_buf(id, |b| read_bytes(b, len)).unwrap_or_default();
        ok_str(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    /// `buffer_decode_base64(buffer, string)`.
    ///
    /// Whitespace is stripped first, and that is load-bearing rather than tidy. Games paste
    /// compiled shaders into GML as base64 through `shader_pixel_create_base64`, and a GML string
    /// literal spanning several source lines keeps every newline and every space of indentation --
    /// which is exactly how all 48 of Delicious Level Design Contest's shaders are stored. Rust's
    /// `STANDARD` engine rejects those outright, so the decode returned nothing, the inflate that
    /// follows had nothing to work on, and every shader in the game arrived at the translator as
    /// zero bytes. Real base64 decoders skip whitespace; this one has to as well.
    fn decode_base64(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        use base64::Engine;
        let id = arg_id(args.first());
        let text = args.get(1).map(arg_str).unwrap_or_default().into_owned();
        let text: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD.decode(text).unwrap_or_default();
        with_buf(id, |b| write_bytes(b, &bytes));
        ok_num(1.0)
    }

    /// CRC-32 (IEEE, the zlib/PNG polynomial). Implemented inline rather than pulled from a crate:
    /// the workspace's `crc32fast` is scoped to Windows-only dependencies, and this is ten lines.
    /// Checked against the standard vector -- CRC32("123456789") == 0xCBF43926.
    fn crc32_of(bytes: &[u8]) -> f64 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        f64::from(!crc)
    }

    fn crc32_buffer(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let bytes = with_buf(arg_id(args.first()), |b| b.data.clone()).unwrap_or_default();
        ok_num(crc32_of(&bytes))
    }

    fn crc32_string(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        ok_num(crc32_of(&args.first().map(arg_raw).unwrap_or_default()))
    }

    fn crc32_file(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        let path = args.first().map(arg_str).unwrap_or_default().into_owned();
        ok_num(crc32_of(&crate::gml::file::read_file_bytes(&path).unwrap_or_default()))
    }

    // -- compression -----------------------------------------------------------------------------
    // Whole-buffer, in place. The caret is clamped rather than reset: the two real flows both stay
    // correct that way -- "load, inflate, read from the start" keeps a caret of 0, and "write,
    // deflate, save" doesn't care where the caret ends up since the save takes the whole buffer.

    fn deflate(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        use flate2::{Compression, write::ZlibEncoder};
        use std::io::Write;
        let id = arg_id(args.first());
        let ok = with_buf(id, |b| {
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            if enc.write_all(&b.data).is_err() {
                return false
            }
            match enc.finish() {
                Ok(out) => {
                    b.data = out;
                    b.pos = b.pos.min(b.data.len());
                    true
                },
                Err(_) => false,
            }
        })
        .unwrap_or(false);
        ok_num(f64::from(u8::from(ok)))
    }

    /// Returns whether the data actually inflated -- the original reports this, and a game that
    /// checks it is asking "was this really compressed?", so a failure must not look like success
    /// with an empty buffer.
    fn inflate(_: &mut Game, _: &mut Context, args: &[Value]) -> gml::Result<Value> {
        use flate2::write::ZlibDecoder;
        use std::io::Write;
        let id = arg_id(args.first());
        let ok = with_buf(id, |b| {
            let mut dec = ZlibDecoder::new(Vec::new());
            if dec.write_all(&b.data).is_err() {
                return false
            }
            match dec.finish() {
                Ok(out) => {
                    b.data = out;
                    b.pos = b.pos.min(b.data.len());
                    true
                },
                Err(_) => false,
            }
        })
        .unwrap_or(false);
        ok_num(f64::from(u8::from(ok)))
    }

    // -- hashes ----------------------------------------------------------------------------------
    // All return a lowercase hex string, matching the extension's own BinToHex. Games compare these
    // as strings, so the case is part of the contract, not cosmetic.

    fn to_hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    fn md5_of(bytes: &[u8]) -> String {
        use md5::{Digest, Md5};
        let mut h = Md5::new();
        h.update(bytes);
        to_hex(&h.finalize())
    }

    fn sha1_of(bytes: &[u8]) -> String {
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(bytes);
        to_hex(&h.finalize())
    }

    /// The `*_buffer_part` variants read an absolute slice, independent of the caret.
    fn buffer_slice(args: &[Value]) -> Vec<u8> {
        let id = arg_id(args.first());
        let pos = args.get(1).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        let len = args.get(2).map(arg_f64).unwrap_or(0.0).max(0.0) as usize;
        with_buf(id, |b| {
            let start = pos.min(b.data.len());
            let end = (pos + len).min(b.data.len());
            b.data[start..end].to_vec()
        })
        .unwrap_or_default()
    }

    fn whole_buffer(args: &[Value]) -> Vec<u8> {
        with_buf(arg_id(args.first()), |b| b.data.clone()).unwrap_or_default()
    }

    fn file_bytes(args: &[Value]) -> Vec<u8> {
        let path = args.first().map(arg_str).unwrap_or_default().into_owned();
        crate::gml::file::read_file_bytes(&path).unwrap_or_default()
    }

    pub fn resolve(symbol: &str) -> Option<Function> {
        // Matched on the DLL's exported name (the extension's `extname`), which is what reaches
        // here -- several differ from the GML name games call, e.g. `buffer_load` exports as
        // `buffer_read_from_file` and `buffer_get_size` as `buffer_get_length`.
        let f: super::ContextFn = match symbol {
            "__gm82buf_buffer_create" => create,
            "buffer_destroy" => destroy,
            "buffer_exists" => exists,
            "buffer_get_length" => get_length,
            "buffer_get_address" => get_address,
            "buffer_get_pos" => get_pos,
            "buffer_set_pos" => set_pos,
            "buffer_set_size" => set_size,
            "buffer_clear" => clear,
            "buffer_at_end" => at_end,
            "buffer_make_pipe" => make_pipe,

            "buffer_write_hex" => write_hex,
            "buffer_read_hex" => read_hex,
            "buffer_rc4_crypt" => rc4,
            "buffer_rc4_crypt_buffer" => rc4_buffer,
            "buffer_read_from_file" => load_file,
            "buffer_read_from_file_part" => load_file_part,
            "buffer_write_to_file" => save_file,
            "buffer_append_to_file" => save_file_append,

            "buffer_write_uint8" | "buffer_write_int8" => |_: &mut Game, _: &mut Context, a: &[Value]| {
                write_uint(arg_id(a.first()), a.get(1).map(arg_f64).unwrap_or(0.0), 1)
            },
            "buffer_write_uint16" | "buffer_write_int16" => |_: &mut Game, _: &mut Context, a: &[Value]| {
                write_uint(arg_id(a.first()), a.get(1).map(arg_f64).unwrap_or(0.0), 2)
            },
            "buffer_write_uint32" | "buffer_write_int32" => |_: &mut Game, _: &mut Context, a: &[Value]| {
                write_uint(arg_id(a.first()), a.get(1).map(arg_f64).unwrap_or(0.0), 4)
            },
            "buffer_read_uint8" => |_: &mut Game, _: &mut Context, a: &[Value]| read_uint(arg_id(a.first()), 1, false),
            "buffer_read_int8" => |_: &mut Game, _: &mut Context, a: &[Value]| read_uint(arg_id(a.first()), 1, true),
            "buffer_read_uint16" => |_: &mut Game, _: &mut Context, a: &[Value]| read_uint(arg_id(a.first()), 2, false),
            "buffer_read_int16" => |_: &mut Game, _: &mut Context, a: &[Value]| read_uint(arg_id(a.first()), 2, true),
            "buffer_read_uint32" => |_: &mut Game, _: &mut Context, a: &[Value]| read_uint(arg_id(a.first()), 4, false),
            "buffer_read_int32" => |_: &mut Game, _: &mut Context, a: &[Value]| read_uint(arg_id(a.first()), 4, true),
            "buffer_write_uint64" | "buffer_write_int64" => |_: &mut Game, _: &mut Context, a: &[Value]| {
                write_uint(arg_id(a.first()), a.get(1).map(arg_f64).unwrap_or(0.0), 8)
            },
            "buffer_read_uint64" => |_: &mut Game, _: &mut Context, a: &[Value]| read_uint(arg_id(a.first()), 8, false),
            "buffer_read_int64" => |_: &mut Game, _: &mut Context, a: &[Value]| read_uint(arg_id(a.first()), 8, true),

            "buffer_write_string" => write_string,
            "buffer_read_string" => read_string,
            "buffer_write_data" => write_data,
            "buffer_read_data" => read_data,
            "buffer_write_float32" => write_f32,
            "buffer_write_float64" => write_f64,
            "buffer_read_float32" => read_f32,
            "buffer_read_float64" => read_f64,
            "buffer_peek" => peek,
            "buffer_poke" => poke,
            "buffer_write_buffer" => copy,
            "buffer_write_buffer_part" => copy_part,
            "buffer_write_float32x2" => {
                |_: &mut Game, _: &mut Context, a: &[Value]| write_f32s(arg_id(a.first()), a, 2)
            },
            "buffer_write_float32x3" => {
                |_: &mut Game, _: &mut Context, a: &[Value]| write_f32s(arg_id(a.first()), a, 3)
            },
            "buffer_write_float32x4" => {
                |_: &mut Game, _: &mut Context, a: &[Value]| write_f32s(arg_id(a.first()), a, 4)
            },
            "buffer_write_float32x12" => {
                |_: &mut Game, _: &mut Context, a: &[Value]| write_f32s(arg_id(a.first()), a, 12)
            },
            "buffer_encode_base64" => encode_base64,
            "buffer_decode_base64" => decode_base64,
            "crc32_buffer" => crc32_buffer,
            "crc32_string" => crc32_string,
            "crc32_file" => crc32_file,
            "crc32_buffer_part" => |_: &mut Game, _: &mut Context, a: &[Value]| ok_num(crc32_of(&buffer_slice(a))),
            "buffer_zlib_compress" => deflate,
            "buffer_zlib_uncompress" => inflate,
            "md5_string" => {
                |_: &mut Game, _: &mut Context, a: &[Value]| ok_str(md5_of(&a.first().map(arg_raw).unwrap_or_default()))
            },
            "md5_buffer" => |_: &mut Game, _: &mut Context, a: &[Value]| ok_str(md5_of(&whole_buffer(a))),
            "md5_buffer_part" => |_: &mut Game, _: &mut Context, a: &[Value]| ok_str(md5_of(&buffer_slice(a))),
            "md5_file" => |_: &mut Game, _: &mut Context, a: &[Value]| ok_str(md5_of(&file_bytes(a))),
            "sha1_string" => |_: &mut Game, _: &mut Context, a: &[Value]| {
                ok_str(sha1_of(&a.first().map(arg_raw).unwrap_or_default()))
            },
            "sha1_buffer" => |_: &mut Game, _: &mut Context, a: &[Value]| ok_str(sha1_of(&whole_buffer(a))),
            "sha1_buffer_part" => |_: &mut Game, _: &mut Context, a: &[Value]| ok_str(sha1_of(&buffer_slice(a))),
            "sha1_file" => |_: &mut Game, _: &mut Context, a: &[Value]| ok_str(sha1_of(&file_bytes(a))),

            _ => return None,
        };
        Some(Function::Runtime(f))
    }

    /// Dropped when a new game loads, so one game's buffers can't leak into another's ids within
    /// the same page session.
    pub fn reset() {
        BUFFERS.with(|b| b.borrow_mut().clear());
        NEXT_ID.with(|n| *n.borrow_mut() = 1);
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;

    pub fn resolve(_symbol: &str) -> Option<Function> {
        None
    }
    pub fn reset() {}
}

// The bare fn-pointer type `Function::Runtime` wraps (its own `ContextFunctionPtr` type alias in
// `gml.rs` is private, so this names the same signature structurally instead of by that name).
type ContextFn = fn(&mut Game, &mut Context, &[Value]) -> gml::Result<Value>;

pub use imp::{reset, resolve};

/// Reaching a buffer's bytes through a `buffer_get_address` -- how `gm82dx9.rs` receives one.
#[cfg(target_arch = "wasm32")]
pub use imp::{contents, fill, read_at, write_at};
