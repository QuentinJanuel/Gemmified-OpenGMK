//! Loopback TCP answered by the host, as the engine protocol's `HostServices.network` describes it.
//!
//! A game's own networking (an extension DLL's sockets) reaches whatever listens on `127.0.0.1`.
//! Here that is the host: a connection to one of `ports` becomes a byte pipe the JS side drains and
//! fills through `wasm_entry.rs`, and a connection to anything else fails. What the bytes mean is no
//! business of this engine's.

use std::collections::HashMap;

/// Something the host has to be told, in order.
pub enum Event {
    Open { id: u32, port: u16 },
    Send { id: u32, bytes: Vec<u8> },
    End { id: u32 },
}

#[derive(Default)]
struct Connection {
    /// Host -> game bytes not yet taken.
    inbox: Vec<u8>,
    closed: bool,
}

/// Held on `Game`. Not part of a savestate: the host lives outside the game and keeps its own state.
#[derive(Default)]
pub struct Loopback {
    ports: Vec<u16>,
    next_id: u32,
    connections: HashMap<u32, Connection>,
    events: Vec<Event>,
}

impl Loopback {
    pub fn new(ports: Vec<u16>) -> Self {
        Self { ports, next_id: 1, ..Default::default() }
    }

    /// Opens a connection to `port`, or `None` when the host does not answer it.
    pub fn connect(&mut self, port: u16) -> Option<u32> {
        if !self.ports.contains(&port) {
            return None
        }
        let id = self.next_id;
        self.next_id += 1;
        self.connections.insert(id, Connection::default());
        self.events.push(Event::Open { id, port });
        Some(id)
    }

    pub fn send(&mut self, id: u32, bytes: Vec<u8>) {
        if bytes.is_empty() || self.is_closed(id) {
            return
        }
        self.events.push(Event::Send { id, bytes });
    }

    /// Everything the host sent on `id` since the last call.
    pub fn take(&mut self, id: u32) -> Vec<u8> {
        self.connections.get_mut(&id).map(|c| std::mem::take(&mut c.inbox)).unwrap_or_default()
    }

    pub fn is_closed(&self, id: u32) -> bool {
        self.connections.get(&id).map_or(true, |c| c.closed)
    }

    /// Closes `id` from the game's side and forgets it.
    pub fn close(&mut self, id: u32) {
        if let Some(c) = self.connections.remove(&id) {
            if !c.closed {
                self.events.push(Event::End { id });
            }
        }
    }

    // -- the host's side ---------------------------------------------------------------------------

    pub fn push(&mut self, id: u32, bytes: &[u8]) {
        if let Some(c) = self.connections.get_mut(&id) {
            c.inbox.extend_from_slice(bytes);
        }
    }

    pub fn closed_by_host(&mut self, id: u32) {
        if let Some(c) = self.connections.get_mut(&id) {
            c.closed = true;
        }
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}
