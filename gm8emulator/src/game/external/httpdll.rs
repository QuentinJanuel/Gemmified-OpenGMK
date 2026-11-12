//! Reimplementation of `http_dll_2_3.dll`, Maarten Baert's Http Dll 2.3: the socket and buffer
//! extension a GameMaker 8 game uses to reach a TCP peer.
//!
//! Only the subset a socket client needs is here -- connect, read and write length-prefixed
//! messages, and the buffers those messages travel in -- written against the extension's own
//! documentation (`sockets.txt`, `buffers.txt`). Every other export falls through to the generic
//! dummy in `external.rs`, which says so the first time a game calls it.
//!
//! A socket here connects to the host's loopback (`crate::loopback`), and only to a port the host
//! answers: anywhere else ends in `socket_state_error`, as a machine with nothing listening would.
//!
//! GameMaker 8.2's `gm82buf.dll` ships the same socket functions over its own buffers, and a game
//! that has it keeps it rather than gaining a second copy -- so the socket half is generic over
//! whose buffers a message is read from and written into (`Buffers`).
//!
//! Wired in from `external.rs`'s `make_call`, like `gm82buf.rs`.

use crate::{
    game::Game,
    gml::{self, Context, Function, Value},
};

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use std::{
        cell::RefCell,
        collections::{HashMap, VecDeque},
    };

    // `socket_get_state`'s values, from `sockets.txt`.
    const NOT_CONNECTED: u8 = 0;
    const CONNECTING: u8 = 1;
    const CONNECTED: u8 = 2;
    const CLOSED: u8 = 4;
    const ERROR: u8 = 5;

    /// A message claiming more than this is a desynchronised stream, not a message. Matches the host.
    const MAX_MESSAGE: usize = 1 << 20;

    fn arg_f64(v: Option<&Value>) -> f64 {
        match v {
            Some(Value::Real(r)) => (*r).into(),
            Some(Value::Str(s)) => std::str::from_utf8(s.as_ref()).ok().and_then(|t| t.parse().ok()).unwrap_or(0.0),
            None => 0.0,
        }
    }

    fn arg_id(v: Option<&Value>) -> i32 {
        arg_f64(v) as i32
    }

    fn arg_raw(v: Option<&Value>) -> Vec<u8> {
        v.map(|v| <&Value as Into<&[u8]>>::into(v).to_vec()).unwrap_or_default()
    }

    fn ok(n: f64) -> gml::Result<Value> {
        Ok(Value::Real(n.into()))
    }

    #[derive(Default)]
    struct Buf {
        data: Vec<u8>,
        pos: usize,
    }

    #[derive(Default)]
    struct Socket {
        state: u8,
        connection: Option<u32>,
        /// Framed bytes waiting for `socket_update_write`.
        tx: Vec<u8>,
        /// Raw bytes received but not yet a whole message.
        rx: Vec<u8>,
        messages: VecDeque<Vec<u8>>,
    }

    #[derive(Default)]
    struct State {
        buffers: HashMap<i32, Buf>,
        sockets: HashMap<i32, Socket>,
        // Ids start at 1: GML reads 0 as false, and an `if (socket)` guard would misread it.
        next_id: i32,
    }

    thread_local! {
        static STATE: RefCell<State> = RefCell::new(State { next_id: 1, ..Default::default() });
    }

    fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
        STATE.with(|s| f(&mut s.borrow_mut()))
    }

    fn next_id(s: &mut State) -> i32 {
        let id = s.next_id;
        s.next_id += 1;
        id
    }

    // -- buffers -------------------------------------------------------------------------------

    fn buffer_create(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok(f64::from(with(|s| {
            let id = next_id(s);
            s.buffers.insert(id, Buf::default());
            id
        })))
    }

    fn buffer_destroy(_: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        with(|s| s.buffers.remove(&arg_id(a.first())));
        ok(0.0)
    }

    fn buffer_clear(_: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        with(|s| s.buffers.insert(arg_id(a.first()), Buf::default()).map(|_| ()));
        ok(0.0)
    }

    fn buffer_set_pos(_: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        let pos = arg_f64(a.get(1)).max(0.0) as usize;
        with(|s| s.buffers.get_mut(&arg_id(a.first())).map(|b| b.pos = pos.min(b.data.len())));
        ok(0.0)
    }

    /// Appends the text and its terminator: writes go at the end, reads at the position.
    fn buffer_write_string(_: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        let mut bytes = arg_raw(a.get(1));
        bytes.push(0);
        with(|s| s.buffers.get_mut(&arg_id(a.first())).map(|b| b.data.extend_from_slice(&bytes)));
        ok(0.0)
    }

    fn buffer_read_string(_: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        let bytes = with(|s| {
            let b = s.buffers.get_mut(&arg_id(a.first()))?;
            let start = b.pos.min(b.data.len());
            let end = b.data[start..].iter().position(|&c| c == 0).map_or(b.data.len(), |n| start + n);
            b.pos = (end + 1).min(b.data.len());
            Some(b.data[start..end].to_vec())
        });
        Ok(Value::Str(gml::String::from(bytes.unwrap_or_default())))
    }

    // -- sockets -------------------------------------------------------------------------------

    /// Whose buffers a socket reads a message from and writes one into.
    pub trait Buffers {
        fn contents(id: i32) -> Vec<u8>;
        fn fill(id: i32, data: Vec<u8>);
    }

    /// This DLL's own.
    pub struct Own;
    impl Buffers for Own {
        fn contents(id: i32) -> Vec<u8> {
            with(|s| s.buffers.get(&id).map(|b| b.data.clone()).unwrap_or_default())
        }

        fn fill(id: i32, data: Vec<u8>) {
            with(|s| s.buffers.insert(id, Buf { data, pos: 0 }));
        }
    }

    /// `gm82buf.dll`'s.
    pub struct Gm82;
    impl Buffers for Gm82 {
        fn contents(id: i32) -> Vec<u8> {
            super::super::gm82buf::contents(id)
        }

        fn fill(id: i32, data: Vec<u8>) {
            super::super::gm82buf::fill(id, data)
        }
    }

    fn socket_create(_: &mut Game, _: &mut Context, _: &[Value]) -> gml::Result<Value> {
        ok(f64::from(with(|s| {
            let id = next_id(s);
            s.sockets.insert(id, Socket::default());
            id
        })))
    }

    fn socket_destroy(game: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        if let Some(connection) = with(|s| s.sockets.remove(&arg_id(a.first())).and_then(|k| k.connection)) {
            game.loopback.close(connection);
        }
        ok(0.0)
    }

    fn socket_get_state(_: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        ok(f64::from(with(|s| s.sockets.get(&arg_id(a.first())).map_or(NOT_CONNECTED, |k| k.state))))
    }

    /// Asynchronous in Http Dll: the connection completes in a later `socket_update_read`.
    fn socket_connect(game: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        let address = arg_raw(a.get(1));
        let port = arg_f64(a.get(2));
        let loopback = matches!(address.as_slice(), b"127.0.0.1" | b"localhost" | b"::1");
        let connection =
            if loopback && (1.0..=65535.0).contains(&port) { game.loopback.connect(port as u16) } else { None };
        with(|s| {
            if let Some(k) = s.sockets.get_mut(&arg_id(a.first())) {
                k.connection = connection;
                k.state = if connection.is_some() { CONNECTING } else { ERROR };
            }
        });
        ok(0.0)
    }

    fn socket_write_message<B: Buffers>(_: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        let body = B::contents(arg_id(a.get(1)));
        with(|s| {
            if let Some(k) = s.sockets.get_mut(&arg_id(a.first())) {
                k.tx.extend(super::encode_uintv(body.len()));
                k.tx.extend(body);
            }
        });
        ok(0.0)
    }

    /// Writes held while connecting go out once connected, as the real DLL does.
    fn socket_update_write(game: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        let pending = with(|s| {
            let k = s.sockets.get_mut(&arg_id(a.first()))?;
            if k.state != CONNECTED || k.tx.is_empty() {
                return None
            }
            Some((k.connection?, std::mem::take(&mut k.tx)))
        });
        if let Some((connection, bytes)) = pending {
            game.loopback.send(connection, bytes);
        }
        ok(0.0)
    }

    fn socket_update_read(game: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        let id = arg_id(a.first());
        let Some(connection) = with(|s| s.sockets.get(&id).and_then(|k| k.connection)) else { return ok(0.0) };
        let incoming = game.loopback.take(connection);
        let closed = game.loopback.is_closed(connection);
        with(|s| {
            let Some(k) = s.sockets.get_mut(&id) else { return };
            if k.state == CONNECTING {
                k.state = CONNECTED;
            }
            k.rx.extend(incoming);
            while let Some((len, width)) = super::decode_uintv(&k.rx) {
                if len > MAX_MESSAGE {
                    k.state = ERROR;
                    k.rx.clear();
                    return
                }
                if k.rx.len() < width + len {
                    break
                }
                let message = k.rx[width..width + len].to_vec();
                k.rx.drain(..width + len);
                k.messages.push_back(message);
            }
            if closed && k.state == CONNECTED {
                k.state = CLOSED;
            }
        });
        ok(0.0)
    }

    /// Replaces the buffer's contents with the next message and rewinds it; 1 if there was one.
    fn socket_read_message<B: Buffers>(_: &mut Game, _: &mut Context, a: &[Value]) -> gml::Result<Value> {
        let message = with(|s| s.sockets.get_mut(&arg_id(a.first()))?.messages.pop_front());
        let got = message.is_some();
        if let Some(message) = message {
            B::fill(arg_id(a.get(1)), message);
        }
        ok(if got { 1.0 } else { 0.0 })
    }

    pub fn resolve(symbol: &str) -> Option<Function> {
        // The extension declares no external names, so these are its GML names.
        let f: super::ContextFn = match symbol {
            "buffer_create" => buffer_create,
            "buffer_destroy" => buffer_destroy,
            "buffer_clear" => buffer_clear,
            "buffer_set_pos" => buffer_set_pos,
            "buffer_write_string" => buffer_write_string,
            "buffer_read_string" => buffer_read_string,
            _ => return sockets::<Own>(symbol),
        };
        Some(Function::Runtime(f))
    }

    /// The socket functions alone, over `gm82buf.dll`'s buffers.
    pub fn resolve_gm82_sockets(symbol: &str) -> Option<Function> {
        sockets::<Gm82>(symbol)
    }

    fn sockets<B: Buffers>(symbol: &str) -> Option<Function> {
        let f: super::ContextFn = match symbol {
            "socket_create" => socket_create,
            "socket_destroy" => socket_destroy,
            "socket_get_state" => socket_get_state,
            "socket_connect" => socket_connect,
            "socket_write_message" => socket_write_message::<B>,
            "socket_update_write" => socket_update_write,
            "socket_update_read" => socket_update_read,
            "socket_read_message" => socket_read_message::<B>,
            _ => return None,
        };
        Some(Function::Runtime(f))
    }

    /// Dropped when a new game loads, so one game's ids never reach another's.
    pub fn reset() {
        with(|s| *s = State { next_id: 1, ..Default::default() });
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;

    pub fn resolve(_symbol: &str) -> Option<Function> {
        None
    }
    pub fn resolve_gm82_sockets(_symbol: &str) -> Option<Function> {
        None
    }
    pub fn reset() {}
}

/// Http Dll's `uintv`: a little-endian prefix code whose low bits give the width. See
/// `buffers.txt`.
const OFFSETS: [u64; 4] = [0, 0x80, 0x4080, 0x20_4080];
const USED_BITS: [u32; 4] = [1, 2, 3, 3];
const PREFIX_BITS: [u64; 4] = [1, 2, 4, 0];

pub fn encode_uintv(value: usize) -> Vec<u8> {
    let value = value as u64;
    let width = OFFSETS.iter().rposition(|&o| value >= o).unwrap_or(0) + 1;
    let encoded = ((value - OFFSETS[width - 1]) << USED_BITS[width - 1]) + PREFIX_BITS[width - 1];
    encoded.to_le_bytes()[..width].to_vec()
}

/// `(value, width)`, or `None` when `bytes` does not yet hold the whole prefix.
pub fn decode_uintv(bytes: &[u8]) -> Option<(usize, usize)> {
    let first = *bytes.first()?;
    let width = if first & 1 != 0 {
        1
    } else if first & 2 != 0 {
        2
    } else if first & 4 != 0 {
        3
    } else {
        4
    };
    if bytes.len() < width {
        return None
    }
    let raw = bytes[..width].iter().rev().fold(0u64, |n, &b| (n << 8) | u64::from(b));
    Some(((raw >> USED_BITS[width - 1]) as usize + OFFSETS[width - 1] as usize, width))
}

#[cfg(test)]
mod tests {
    use super::*;

    // From I Wanna Play Online's server encoder, as the host's own tests use.
    #[test]
    fn uintv_matches_the_reference() {
        for (value, hex) in [(0, "01"), (127, "ff"), (128, "0200"), (16512, "040000"), (2113664, "00000000")] {
            let bytes = encode_uintv(value);
            let got: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
            assert_eq!(got, hex);
            assert_eq!(decode_uintv(&bytes), Some((value, bytes.len())));
        }
    }
}

// See `gm82buf.rs`: `Function::Runtime`'s pointer type, named structurally.
type ContextFn = fn(&mut Game, &mut Context, &[Value]) -> gml::Result<Value>;

pub use imp::{reset, resolve, resolve_gm82_sockets};
