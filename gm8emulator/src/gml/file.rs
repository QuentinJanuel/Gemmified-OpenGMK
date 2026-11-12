use image::{AnimationDecoder, ImageError, ImageFormat, Pixel, RgbaImage, codecs::gif::GifDecoder};
use std::{
    fs::{File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

pub type Result<T> = std::result::Result<T, Error>;

// `TextHandle`/`BinaryHandle` need to hold EITHER a real-filesystem reader/writer (native, or wasm
// falling through to the always-failing `std::fs`) OR an in-memory `VirtualFile` (wasm: see below),
// two different concrete types per capability combination. The natural way to abstract over that is
// a `Read + Seek` (etc.) supertrait object, but converting such an object into a *different* trait
// object a sibling match arm already returns (e.g. `BinaryHandle::get_seeker` needs `&mut dyn Seek`
// across a `Read`/`Write`/`ReadWrite` match where each arm is a different concrete type) is trait
// object upcasting, stable only since Rust 1.86 -- this crate is pinned to 1.77 (rust-toolchain.toml).
// Each `Boxed*` type below sidesteps that by manually forwarding to a private dyn-safe trait and
// re-implementing the real `std::io` traits on a concrete newtype, so every call site coerces a
// concrete type into a trait object (always stable) instead of between two trait objects.
trait DynReadSeek {
    fn dyn_read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn dyn_seek(&mut self, pos: SeekFrom) -> io::Result<u64>;
}
impl<T: Read + Seek> DynReadSeek for T {
    fn dyn_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Read::read(self, buf)
    }

    fn dyn_seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        Seek::seek(self, pos)
    }
}

pub(crate) struct BoxedReadSeek(Box<dyn DynReadSeek>);
impl Read for BoxedReadSeek {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.dyn_read(buf)
    }
}
impl Seek for BoxedReadSeek {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.dyn_seek(pos)
    }
}

/// Truncates a writable file-like thing to `size` bytes, needed only by `BinaryHandle::clear()`
/// (GML's "rewrite" semantics). Not part of `std::io::Write` so `Write + Seek` alone doesn't cover
/// it; implemented per concrete type below instead of derived generically.
trait TruncateAt {
    fn truncate_at(&mut self, size: u64) -> io::Result<()>;
}
impl TruncateAt for File {
    fn truncate_at(&mut self, size: u64) -> io::Result<()> {
        self.set_len(size)
    }
}
impl TruncateAt for BufWriter<File> {
    fn truncate_at(&mut self, size: u64) -> io::Result<()> {
        self.get_ref().set_len(size)
    }
}

trait DynWriteSeek {
    fn dyn_write(&mut self, buf: &[u8]) -> io::Result<usize>;
    fn dyn_flush(&mut self) -> io::Result<()>;
    fn dyn_seek(&mut self, pos: SeekFrom) -> io::Result<u64>;
    fn dyn_set_len(&mut self, size: u64) -> io::Result<()>;
}
impl<T: Write + Seek + TruncateAt> DynWriteSeek for T {
    fn dyn_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Write::write(self, buf)
    }

    fn dyn_flush(&mut self) -> io::Result<()> {
        Write::flush(self)
    }

    fn dyn_seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        Seek::seek(self, pos)
    }

    fn dyn_set_len(&mut self, size: u64) -> io::Result<()> {
        TruncateAt::truncate_at(self, size)
    }
}

pub(crate) struct BoxedWriteSeek(Box<dyn DynWriteSeek>);
impl Write for BoxedWriteSeek {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.dyn_write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.dyn_flush()
    }
}
impl Seek for BoxedWriteSeek {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.dyn_seek(pos)
    }
}
impl BoxedWriteSeek {
    fn set_len(&mut self, size: u64) -> io::Result<()> {
        self.0.dyn_set_len(size)
    }
}

trait DynReadWriteSeek {
    fn dyn_read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn dyn_write(&mut self, buf: &[u8]) -> io::Result<usize>;
    fn dyn_flush(&mut self) -> io::Result<()>;
    fn dyn_seek(&mut self, pos: SeekFrom) -> io::Result<u64>;
    fn dyn_set_len(&mut self, size: u64) -> io::Result<()>;
}
impl<T: Read + Write + Seek + TruncateAt> DynReadWriteSeek for T {
    fn dyn_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Read::read(self, buf)
    }

    fn dyn_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Write::write(self, buf)
    }

    fn dyn_flush(&mut self) -> io::Result<()> {
        Write::flush(self)
    }

    fn dyn_seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        Seek::seek(self, pos)
    }

    fn dyn_set_len(&mut self, size: u64) -> io::Result<()> {
        TruncateAt::truncate_at(self, size)
    }
}

pub(crate) struct BoxedReadWriteSeek(Box<dyn DynReadWriteSeek>);
impl Read for BoxedReadWriteSeek {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.dyn_read(buf)
    }
}
impl Write for BoxedReadWriteSeek {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.dyn_write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.dyn_flush()
    }
}
impl Seek for BoxedReadWriteSeek {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.dyn_seek(pos)
    }
}
impl BoxedReadWriteSeek {
    fn set_len(&mut self, size: u64) -> io::Result<()> {
        self.0.dyn_set_len(size)
    }
}

pub enum TextHandle {
    Read(BoxedReadSeek),
    Write(BoxedWriteSeek),
}
impl std::fmt::Debug for TextHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(_) => f.write_str("TextHandle::Read(..)"),
            Self::Write(_) => f.write_str("TextHandle::Write(..)"),
        }
    }
}
pub enum BinaryHandle {
    Read(BoxedReadSeek),
    Write(BoxedWriteSeek),
    ReadWrite(BoxedReadWriteSeek),
}
impl std::fmt::Debug for BinaryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(_) => f.write_str("BinaryHandle::Read(..)"),
            Self::Write(_) => f.write_str("BinaryHandle::Write(..)"),
            Self::ReadWrite(_) => f.write_str("BinaryHandle::ReadWrite(..)"),
        }
    }
}

// A browser has no filesystem, so every file a game reads or writes resolves through the
// virtual filesystem GEMS describes -- see `gml/vfs.rs` for the surface and the model.
//
// This module is only the *session* layer on top of it: a cache of open files, so that two handles
// on the same path share one buffer and a write through one is visible through the other. Path
// resolution, the bundle/store overlay, whiteouts and persistence all live in `gml/vfs` and are
// deliberately not reimplemented here.
//
// Two bugs that design rules out, both worth not reintroducing:
//
//   * saves were keyed by BASENAME, so `Save\game1` and `Backup\game1` were the same file, and
//     `working_directory + "\Save\x"` and `"Save\x"` were two different ones
//   * the read-only upload and the writable save store were separate registries with separate
//     keying, so `file_exists` and `file_bin_open` could disagree about the same path
#[cfg(target_arch = "wasm32")]
mod session_files {
    use crate::gml::vfs;
    use std::{cell::RefCell, collections::HashMap, rc::Rc};

    thread_local! {
        // Open files for the currently-running game, keyed by case-folded full path. Not a
        // persistence layer -- `vfs` is -- just the identity map that makes concurrent handles on
        // one file agree.
        static OPEN: RefCell<HashMap<String, Rc<RefCell<Vec<u8>>>>> = RefCell::new(HashMap::new());
    }

    fn cache_key(path: &str) -> String {
        path.replace('\\', "/").to_lowercase()
    }

    /// Drops the open-file cache. Called when a new game loads so its files can't inherit a
    /// previous game's buffers within the same page session.
    pub fn reset() {
        OPEN.with(|f| f.borrow_mut().clear());
    }

    /// Seeds a read-only, engine-provided file into the bundle: Game Maker's Include Files, and
    /// extension payloads. Not persisted -- these are re-created identically on every launch from
    /// the game file itself, and are not the player's data to delete.
    pub fn register(path: &str, data: &[u8]) {
        vfs::add_bundle(path, data);
    }

    /// The raw bytes at a path, or `None` if nothing is there. Used where a caller needs the whole
    /// file rather than a handle (`ssound.dll`'s `SS_LoadSound` hands them to its own decoders).
    pub fn lookup_bytes(path: &str) -> Option<Vec<u8>> {
        if let Some(buf) = OPEN.with(|f| f.borrow().get(&cache_key(path)).cloned()) {
            return Some(buf.borrow().clone())
        }
        vfs::read(path)
    }

    /// An already-open or on-disk file, for read-only opens. `None` means "does not exist", which
    /// callers treat the same as a failed open.
    ///
    /// **A read does not join `OPEN`.** It consults it -- so a read open sees what a write handle
    /// on the same path is holding, which is the whole point of the identity map -- and then falls
    /// through to a fresh `vfs::read`. Registering the result instead turned the handle map into a
    /// permanent read cache: the first read of a path pinned its bytes for the rest of the session,
    /// and every later read answered with them no matter what had been written since.
    ///
    /// That is wrong for *any* write the engine did not make through `get_or_create`, and the mod
    /// host makes exactly those -- it writes a picture into the VFS from the app side and has the
    /// game `sprite_add` it, over and over on one path. Each slot kept whichever picture it was
    /// given first (a mod drawing speech bubbles showed the session's first line on every one), and the
    /// stale buffer was reported as present by `exists`, so nothing even failed.
    pub fn lookup(path: &str) -> Option<(String, Rc<RefCell<Vec<u8>>>)> {
        if let Some(existing) = OPEN.with(|f| f.borrow().get(&cache_key(path)).cloned()) {
            return Some((path.to_string(), existing))
        }
        Some((path.to_string(), Rc::new(RefCell::new(vfs::read(path)?))))
    }

    /// Get-or-create, for write and read-write opens -- which GM8 always succeeds at regardless of
    /// whether the file existed. Also backs read opens of binary files, which GM8 auto-creates.
    pub fn get_or_create(path: &str, truncate: bool) -> Option<(String, Rc<RefCell<Vec<u8>>>)> {
        let key = cache_key(path);
        let buf = if let Some(existing) = OPEN.with(|f| f.borrow().get(&key).cloned()) {
            existing
        } else {
            let buf = Rc::new(RefCell::new(vfs::read(path).unwrap_or_default()));
            OPEN.with(|f| f.borrow_mut().insert(key, buf.clone()));
            buf
        };
        if truncate {
            buf.borrow_mut().clear();
            // Deliberately not persisted here. The handle's flush/Drop writes the final contents,
            // so persisting an empty buffer now would only open a window in which the store claims
            // the file is empty -- and if the real write then failed (quota), that placeholder is
            // all that would survive, shadowing the bundled original on every later load.
        }
        Some((path.to_string(), buf))
    }

    /// Writes a file's current contents through to the store. Called on flush/close.
    pub fn persist(path: &str, data: &[u8]) {
        vfs::write(path, data);
    }

    /// Publishes bytes at a path **in memory only**, for gm82buf's `buffer_make_pipe`: the game
    /// hands the resulting name to something that takes a filename (`d3d_model_load` reads a model
    /// straight out of a buffer this way) and expects to read the buffer's contents back.
    ///
    /// Deliberately not `register`: a pipe is not a file. Routing one through the bundle or the
    /// store would push a multi-megabyte model into localStorage, where it would blow the ~5MB
    /// quota and outlive the moment it existed for. Landing it in `OPEN` -- which `lookup` consults
    /// before the VFS -- makes it readable by every path that opens a file, and it is gone with the
    /// rest of the session.
    pub fn register_memory(path: &str, data: Vec<u8>) {
        OPEN.with(|f| f.borrow_mut().insert(cache_key(path), Rc::new(RefCell::new(data))));
    }

    pub fn exists(path: &str) -> bool {
        OPEN.with(|f| f.borrow().contains_key(&cache_key(path))) || vfs::exists(path)
    }

    /// Replaces a file's contents wholesale and persists them, for `file_copy`/`file_rename`.
    ///
    /// Goes through `get_or_create` rather than straight to `vfs::write` so that a handle already
    /// open on the destination ends up holding the new bytes -- otherwise its own flush on close
    /// would put the stale buffer back over the copy.
    pub fn overwrite(path: &str, data: Vec<u8>) {
        let Some((_, buf)) = get_or_create(path, false) else { return };
        *buf.borrow_mut() = data;
        persist(path, buf.borrow().as_slice());
    }

    pub fn remove(path: &str) {
        OPEN.with(|f| f.borrow_mut().remove(&cache_key(path)));
        vfs::remove(path);
    }

    pub fn dir_exists(path: &str) -> bool {
        vfs::dir_exists(path)
    }

    pub fn find(pattern: &str, include_dirs: bool) -> Vec<String> {
        vfs::find(pattern, include_dirs)
    }

    pub fn list_persisted() -> Vec<(String, usize)> {
        vfs::list_store()
    }
}

#[cfg(target_arch = "wasm32")]
pub use session_files::{
    find as find_files, list_persisted as list_store_files, register as register_bundle_file,
    register_memory as register_pipe_file, remove as delete_store_file, reset as reset_open_files,
};
// Reused by game/external/ssound.rs's SS_LoadSound, which needs the raw bytes rather than a handle.
#[cfg(target_arch = "wasm32")]
pub(crate) use session_files::lookup_bytes as read_file_bytes;

// The web harness's file-management panel (see `wasm_entry.rs`) has nothing to list/delete on
// native, where the real filesystem already holds everything -- only wasm32 needs these.
#[cfg(not(target_arch = "wasm32"))]
pub fn list_store_files() -> Vec<(String, usize)> {
    Vec::new()
}
#[cfg(not(target_arch = "wasm32"))]
pub fn delete_store_file(_path: &str) {}

/// An in-memory stand-in for a real file, shared (via `Rc<RefCell<_>>`) with the virtual-files
/// registry so writes are immediately visible to any other handle opened on the same name. Carries
/// its own `name` so writes can persist themselves to localStorage (see `session_files::persist`)
/// without needing every call site to also track and pass the filename around.
#[cfg(target_arch = "wasm32")]
struct VirtualFile {
    name: String,
    buf: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    pos: usize,
    /// Set for handles opened in `AccessMode::Read` -- a pure reader never mutates `buf`, so it has
    /// nothing new to persist, and `Drop` must not persist on its behalf regardless (see `Drop`'s
    /// doc comment). Without this, closing/dropping a read handle whose buffer was ever fabricated
    /// empty (e.g. by a since-fixed bug in `get_or_create`, or simply a file that genuinely doesn't
    /// exist yet) would write that empty buffer into localStorage -- permanently shadowing the real
    /// file (a save from a previous session, or a loose upload from `external_files`) the *next*
    /// time this same name is opened, since `get_or_create`/`lookup` check localStorage before
    /// falling back to anything else. A one-off bug in the read path would otherwise leave a
    /// lingering trap for every future load even after the read path itself was fixed.
    read_only: bool,
}
#[cfg(target_arch = "wasm32")]
impl Read for VirtualFile {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let buf = self.buf.borrow();
        // `pos` may sit *past* the end, so clamp it before it reaches a slice index. `n` alone does
        // not protect the slice: at `pos` 25 on a 24-byte file `n` is correctly 0, but the
        // expression `&buf[25..25]` still panics ("range end index 25 out of range for slice of
        // length 24") because a range's *start* must also be in bounds. Two ways to get there, both
        // legal in GML: `file_bin_seek` takes any offset, and a read past the end leaves the
        // position beyond it (see `BinaryHandle::read_byte`).
        let start = self.pos.min(buf.len());
        let n = (buf.len() - start).min(out.len());
        out[..n].copy_from_slice(&buf[start..start + n]);
        self.pos += n;
        Ok(n)
    }
}
#[cfg(target_arch = "wasm32")]
impl Write for VirtualFile {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut buf = self.buf.borrow_mut();
        let end = self.pos + data.len();
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[self.pos..end].copy_from_slice(data);
        self.pos = end;
        // Deliberately NOT persisted here -- see `Drop`'s doc comment for why.
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        session_files::persist(&self.name, &self.buf.borrow());
        Ok(())
    }
}
#[cfg(target_arch = "wasm32")]
impl Drop for VirtualFile {
    /// Persists once more on drop, in addition to `flush` (called by `file_bin_close`/
    /// `file_text_close` et al) -- a safety net for a handle that never gets explicitly closed
    /// (e.g. a script erroring out partway through a long write sequence). Without this, `write`
    /// not persisting per-call (see below) would mean an unclosed handle's writes are lost
    /// entirely instead of just deferred.
    ///
    /// Persisting the whole current buffer to localStorage on *every single* `write` would be
    /// harmless for the odd one-shot write, but a real game's save function can call
    /// `file_bin_write_byte` many hundreds of times sequentially on one handle (one per save-data
    /// field). That turns one logical save into hundreds of synchronous, unbatched
    /// `localStorage.setItem` calls, any one of which silently failing (`persist`'s `let _ =` --
    /// intentional, since a game itself never checks GML file-write calls for failure either) or
    /// otherwise not completing left a stale, truncated copy in localStorage from partway through
    /// the sequence -- invisible until the *next* load, which would then see a save file shorter
    /// than the game expects and hit "failed to fill whole buffer" reading past its end. Matches
    /// the same fix already applied to `save_image`'s BMP encoder for the identical reason:
    /// persist the complete result once, not incrementally.
    ///
    /// Skipped entirely for `read_only` handles -- see that field's doc comment.
    fn drop(&mut self) {
        if !self.read_only {
            session_files::persist(&self.name, &self.buf.borrow());
        }
    }
}
#[cfg(target_arch = "wasm32")]
impl Seek for VirtualFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = self.buf.borrow().len() as i64;
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(p) => len + p,
            SeekFrom::Current(p) => self.pos as i64 + p,
        };
        if new_pos < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek to a negative position"))
        }
        self.pos = new_pos as usize;
        Ok(self.pos as u64)
    }
}
#[cfg(target_arch = "wasm32")]
impl TruncateAt for VirtualFile {
    fn truncate_at(&mut self, size: u64) -> io::Result<()> {
        let mut buf = self.buf.borrow_mut();
        buf.truncate(size as usize);
        session_files::persist(&self.name, &buf);
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn open_virtual_read(path: &str) -> Option<BoxedReadSeek> {
    // One lookup covers both layers now: the VFS returns the store's copy if the game has written
    // one, the bundled/uploaded original otherwise, and nothing if it was deleted.
    let (name, buf) = session_files::lookup(path)?;
    Some(BoxedReadSeek(Box::new(VirtualFile { name, buf, pos: 0, read_only: true })))
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn open_virtual_read(_path: &str) -> Option<BoxedReadSeek> {
    None
}

/// Reads a whole file's bytes, preferring the wasm virtual filesystem (e.g. a bundled Include
/// File) and falling back to a real filesystem read -- `Some`
/// only ever on wasm, so native behavior here is completely unchanged. Used by GML's `sound_add`/
/// `sound_replace` (`gml/kernel.rs`), which otherwise always fail on wasm via a raw
/// `std::fs::read` with no filesystem to read from.
pub(crate) fn read_bytes(path: &str) -> io::Result<Vec<u8>> {
    if let Some(mut r) = open_virtual_read(path) {
        let mut buf = Vec::new();
        r.read_to_end(&mut buf)?;
        return Ok(buf)
    }
    std::fs::read(path)
}

#[cfg(target_arch = "wasm32")]
fn open_virtual_read_or_create(path: &str) -> Option<BoxedReadSeek> {
    let (name, buf) = session_files::get_or_create(path, false)?;
    Some(BoxedReadSeek(Box::new(VirtualFile { name, buf, pos: 0, read_only: true })))
}
#[cfg(not(target_arch = "wasm32"))]
fn open_virtual_read_or_create(_path: &str) -> Option<BoxedReadSeek> {
    None
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn open_virtual_write(path: &str, truncate: bool, append: bool) -> Option<BoxedWriteSeek> {
    let (name, buf) = session_files::get_or_create(path, truncate)?;
    let pos = if append { buf.borrow().len() } else { 0 };
    Some(BoxedWriteSeek(Box::new(VirtualFile { name, buf, pos, read_only: false })))
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn open_virtual_write(_path: &str, _truncate: bool, _append: bool) -> Option<BoxedWriteSeek> {
    None
}

#[cfg(target_arch = "wasm32")]
fn open_virtual_readwrite(path: &str) -> Option<BoxedReadWriteSeek> {
    let (name, buf) = session_files::get_or_create(path, false)?;
    Some(BoxedReadWriteSeek(Box::new(VirtualFile { name, buf, pos: 0, read_only: false })))
}
#[cfg(not(target_arch = "wasm32"))]
fn open_virtual_readwrite(_path: &str) -> Option<BoxedReadWriteSeek> {
    None
}

#[derive(Clone, Copy, Debug)]
pub enum AccessMode {
    Read,
    Write,
    Special, // 'append' for text files, 'read-write' for binary
}

#[derive(Debug)]
pub enum Error {
    LegacyFileUnopened,
    InvalidFile(i32),
    CantRead,
    CantWrite,
    IOError(io::Error),
    ImageError(ImageError),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::IOError(e)
    }
}

impl From<ImageError> for Error {
    fn from(e: ImageError) -> Self {
        Self::ImageError(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LegacyFileUnopened => write!(f, "file is not opened"),
            Self::InvalidFile(handle) => write!(f, "invalid file handle {}", handle),
            Self::CantRead => write!(f, "file is not open for reading"),
            Self::CantWrite => write!(f, "file is not open for writing"),
            Self::IOError(err) => write!(f, "io error: {}", err),
            Self::ImageError(err) => write!(f, "image error: {}", err),
        }
    }
}

// Helper functions

fn read_until<P>(file: impl Read, mut end_pred: P) -> io::Result<Vec<u8>>
where
    P: FnMut(u8) -> bool,
{
    let mut out = Vec::new();
    for byte_maybe in file.bytes() {
        let byte = byte_maybe?;
        out.push(byte);
        if end_pred(byte) {
            break
        }
    }
    Ok(out)
}

// Returns Ok(false) on EOF
fn skip_until<P>(file: impl Read, end_pred: P) -> io::Result<bool>
where
    P: Fn(u8) -> bool,
{
    for byte in file.bytes() {
        if end_pred(byte?) {
            return Ok(true)
        }
    }
    Ok(false)
}

/// Converts a GML string into a valid filepath by replacing backslashes with file separators
pub fn to_path(path: &str) -> std::borrow::Cow<str> {
    path.replace("\\", std::path::MAIN_SEPARATOR_STR).into()
}

impl TextHandle {
    pub fn open(path: &str, mode: AccessMode) -> io::Result<Self> {
        match mode {
            AccessMode::Read => {
                if let Some(r) = open_virtual_read(path) {
                    return Ok(TextHandle::Read(r))
                }
            },
            AccessMode::Write | AccessMode::Special => {
                if let Some(w) =
                    open_virtual_write(path, matches!(mode, AccessMode::Write), matches!(mode, AccessMode::Special))
                {
                    return Ok(TextHandle::Write(w))
                }
            },
        }

        #[rustfmt::skip]
        let (read, write, append) = match mode {
            AccessMode::Read    => (true,  false, false),
            AccessMode::Write   => (false, true,  false),
            AccessMode::Special => (false, true,  true ),
        };

        let file = OpenOptions::new()
            .create(!read)
            .read(read)
            .write(write)
            .append(append)
            .truncate(write && !append)
            .open(path)?;

        Ok(match mode {
            AccessMode::Read => TextHandle::Read(BoxedReadSeek(Box::new(BufReader::new(file)))),
            AccessMode::Write | AccessMode::Special => {
                TextHandle::Write(BoxedWriteSeek(Box::new(BufWriter::new(file))))
            },
        })
    }

    fn get_reader(&mut self) -> Result<&mut BoxedReadSeek> {
        match self {
            Self::Read(f) => Ok(f),
            _ => Err(Error::CantRead),
        }
    }

    fn get_writer(&mut self) -> Result<&mut BoxedWriteSeek> {
        match self {
            Self::Write(f) => Ok(f),
            _ => Err(Error::CantWrite),
        }
    }

    pub fn read_real(&mut self) -> Result<f64> {
        Ok(read_real(self.get_reader()?)?)
    }

    pub fn read_string(&mut self) -> Result<Vec<u8>> {
        let mut f = self.get_reader()?;
        let mut bytes = read_until(&mut f, |c| c == 0x0a)?;
        if bytes.last() == Some(&0x0a) {
            // LF
            bytes.pop();
            f.seek(SeekFrom::Current(-1))?;
            if bytes.last() == Some(&0x0d) {
                // CR
                bytes.pop();
                f.seek(SeekFrom::Current(-1))?;
            }
        }
        Ok(bytes)
    }

    pub fn write_real(&mut self, real: f64) -> Result<()> {
        let text = if real.fract() == 0.0 { format!(" {:.0}", real) } else { format!(" {:.6}", real) };
        self.get_writer()?.write_all(text.as_bytes())?;
        Ok(())
    }

    pub fn write_string(&mut self, text: &[u8]) -> Result<()> {
        self.get_writer()?.write_all(text)?;
        Ok(())
    }

    pub fn write_newline(&mut self) -> Result<()> {
        self.get_writer()?.write_all(b"\r\n")?;
        Ok(())
    }

    pub fn skip_line(&mut self) -> Result<()> {
        Ok(skip_line(&mut self.get_reader()?)?)
    }

    pub fn is_eof(&mut self) -> Result<bool> {
        let f = self.get_reader()?;
        let mut buf: [u8; 1] = [0];
        let last_pos = f.stream_position()?;
        let bytes_read = f.read(&mut buf)?;
        f.seek(SeekFrom::Start(last_pos))?;
        Ok(bytes_read == 0)
    }

    pub fn is_eoln(&mut self) -> Result<bool> {
        let f = self.get_reader()?;
        let mut buf: [u8; 2] = [0, 0];
        let last_pos = f.stream_position()?;
        let bytes_read = f.read(&mut buf)?;
        f.seek(SeekFrom::Start(last_pos))?;
        Ok(bytes_read == 0 || (buf[0] == 0x0d && buf[1] == 0x0a))
    }

    pub fn flush(&mut self) -> Result<()> {
        match self {
            Self::Read(_) => Ok(()),
            Self::Write(f) => f.flush().map_err(|e| e.into()),
        }
    }
}

/// Says once per session that a game read past the end of a binary file. Once, because the games
/// that do this read a whole record that way -- the one this was found on reads 24 bytes past the
/// end, every launch -- and a line per byte would bury everything else in the log. It is worth
/// saying at all because the game is relying on something real GameMaker does not do (see
/// `BinaryHandle::read_byte`), which usually means its last save was cut short.
fn warn_read_past_end() {
    thread_local! {
        static WARNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    WARNED.with(|warned| {
        if warned.replace(true) {
            return
        }
        let message = "file_bin_read_byte read past the end of a file -- returning 0, where real \
                       GameMaker aborts. The file is shorter than the game expects, usually \
                       because a previous run was cut off partway through writing it.";
        // `target_os = "unknown"` narrows this to the browser build, for the reason spelled out on
        // `warn_unimplemented_once` in kernel.rs: the unit tests run on wasm32-wasi, which has no
        // JS imports, so a bare `target_arch = "wasm32"` would kill any test reaching this line.
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        web_sys::console::warn_1(&message.into());
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        eprintln!("{}", message);
    });
}

impl BinaryHandle {
    pub fn open(path: &str, mode: AccessMode) -> io::Result<Self> {
        match mode {
            // Binary files are auto-created (empty) if missing even in Read mode -- see the doc
            // comment on `_open` below -- so the virtual fallback matches via get-or-create too.
            AccessMode::Read => {
                if let Some(r) = open_virtual_read_or_create(path) {
                    return Ok(Self::Read(r))
                }
            },
            // Deliberately NOT truncated: real GM8's binary write mode opens (and creates, if
            // missing) a file for writing *in place*, preserving any existing longer content
            // beyond wherever this particular write actually reaches -- confirmed directly by the
            // native `_open` below, whose write branch has no `.truncate()` call at all. A game
            // can rely on this to open an already-larger save file, `file_bin_seek` partway in,
            // and overwrite just one field without disturbing the rest (a real, commonly-used
            // pattern: e.g. a "just update the death/playtime counters" save function distinct
            // from the "write the whole save from scratch" one, both opening the same file in
            // this same mode). Truncating unconditionally here would silently wipe out everything
            // after the partial writer's own last byte, right after the "whole save" writer had
            // just written it, shrinking the save to its first few fields.
            // A game that genuinely wants a fresh empty file calls `file_bin_rewrite`
            // (`BinaryHandle::clear`) explicitly, same as on native.
            AccessMode::Write => {
                if let Some(w) = open_virtual_write(path, false, false) {
                    return Ok(Self::Write(w))
                }
            },
            AccessMode::Special => {
                if let Some(rw) = open_virtual_readwrite(path) {
                    return Ok(Self::ReadWrite(rw))
                }
            },
        }

        let file = Self::_open(path, mode)?;
        match mode {
            AccessMode::Read => Ok(Self::Read(BoxedReadSeek(Box::new(BufReader::new(file))))),
            AccessMode::Write => Ok(Self::Write(BoxedWriteSeek(Box::new(BufWriter::new(file))))),
            AccessMode::Special => Ok(Self::ReadWrite(BoxedReadWriteSeek(Box::new(file)))),
        }
    }

    // Binary files are always created by GM if they doesn't exist, but in such
    // cases it opens them in read-write mode rather than specified, so both the
    // file_bin_read_byte() and file_bin_write_byte() works. However, we can't
    // simply specify the .create(true) because it's explicitly disallowed in the
    // code of std::fs for read-only mode. We also don't use Path::is_file()
    // because it may theoretically fail if someone will create a file with the
    // same name between testing and opening, and also would require an additional
    // function call every time. Instead, when a read-only or write-only mode was
    // requested for a file, we try first to create it and fail if it's exists.
    fn _open(path: &str, mode: AccessMode) -> io::Result<File> {
        let mut opts = OpenOptions::new();

        #[rustfmt::skip]
        let (read, write) = match mode {
            AccessMode::Read    => (true,  false),
            AccessMode::Write   => (false, true ),
            AccessMode::Special => (true,  true ),
        };

        if !(read && write) {
            // We don't return on other errors (that is, not AlreadyExists) here
            // because the second call to .open() may give us a more exact one.
            if let r @ Ok(_) = opts.create_new(true).read(true).write(true).open(path) {
                return r
            };

            opts.create_new(false);
        };

        // Note that .create() is necessary here not only for read-write case but
        // also to behave correctly if file was erased between .open() calls. Same
        // is also the reason why this opening attempt is secondary, not primary.
        opts.create(write) // not .create(true), read the initial comment why!
            .read(read)
            .write(write)
            .open(path)
    }

    fn get_reader(&mut self) -> Result<&mut dyn Read> {
        match self {
            Self::Read(f) => Ok(f),
            Self::Write(_) => Err(Error::CantRead),
            Self::ReadWrite(f) => Ok(f),
        }
    }

    fn get_writer(&mut self) -> Result<&mut dyn Write> {
        match self {
            Self::Read(_) => Err(Error::CantWrite),
            Self::Write(f) => Ok(f),
            Self::ReadWrite(f) => Ok(f),
        }
    }

    fn get_seeker(&mut self) -> &mut dyn Seek {
        match self {
            Self::Read(f) => f,
            Self::Write(f) => f,
            Self::ReadWrite(f) => f,
        }
    }

    pub fn clear(&mut self) -> Result<()> {
        match self {
            Self::Read(_) => return Err(Error::CantWrite),
            Self::Write(f) => {
                f.seek(SeekFrom::Start(0))?;
                f.set_len(0)?;
            },
            Self::ReadWrite(f) => {
                f.seek(SeekFrom::Start(0))?;
                f.set_len(0)?;
            },
        }
        Ok(())
    }

    /// Reading past the end yields 0 (and still advances the position) instead of failing. This is
    /// a DELIBERATE DIVERGENCE, and the same one `file_bin_read_byte` already makes in OpenGM's C#
    /// -- see the note there.
    ///
    /// Real GM8 does *not* forgive it. Measured on 2026-09-27 under wine32on64 against "I wanna
    /// stop the MELTDOWN", whose own `loadDeathTime` creates a 24-byte file on a first run and
    /// reads 48 bytes back on every run after: with a 48-byte file the game reached its `init`
    /// object (observable: it writes `controls`) in 8 seconds, and with the 24-byte one it never
    /// did across two runs of 40s and 120s, sitting at 0% CPU on the runner's error dialog.
    ///
    /// We diverge because *we* are what puts games into that state. On Windows that game rewrites
    /// the file to its full length whenever it ends through its own quit handlers -- which all
    /// call `saveDeathTime` -- so a player never meets the short file again. A browser tab closes
    /// with no such chance, so the short file persists and bricks every later launch: the read
    /// lands in room 0's create events, which is `Game::init`, so it surfaces as "failed to
    /// initialize game: file_bin_read_byte: io error: failed to fill whole buffer" and the game
    /// never starts again. Returning 0 lets it boot and rewrite the file itself, and costs
    /// fidelity only where GameMaker itself simply fails.
    pub fn read_byte(&mut self) -> Result<u8> {
        let mut buf: [u8; 1] = [0];
        match self.get_reader()?.read_exact(&mut buf) {
            Ok(()) => Ok(buf[0]),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                warn_read_past_end();
                // `read_exact` consumed nothing, so step over the byte it could not read: a game
                // that reads a fixed-size record and then asks `file_bin_position` sees the same
                // offset it would have on a long-enough file (and the same one OpenGM reports).
                let _ = self.get_seeker().seek(SeekFrom::Current(1));
                Ok(0)
            },
            Err(e) => Err(e.into()),
        }
    }

    pub fn write_byte(&mut self, byte: u8) -> Result<()> {
        self.get_writer()?.write_all(&[byte])?;
        Ok(())
    }

    pub fn tell(&mut self) -> Result<u64> {
        Ok(self.get_seeker().stream_position()?)
    }

    pub fn seek(&mut self, pos: i32) -> Result<()> {
        self.get_seeker().seek(SeekFrom::Start(pos as u64))?;
        Ok(())
    }

    pub fn size(&mut self) -> Result<u64> {
        // TODO seek_stream_len feature
        let seeker = self.get_seeker();
        let pos = seeker.stream_position()?;
        seeker.seek(SeekFrom::End(0))?;
        let size = seeker.stream_position()?;
        seeker.seek(SeekFrom::Start(pos))?;
        Ok(size)
    }

    pub fn flush(&mut self) -> Result<()> {
        match self {
            Self::Read(_) => Ok(()),
            Self::Write(f) => f.flush(),
            Self::ReadWrite(f) => f.flush(),
        }
        .map_err(|e| e.into())
    }
}

pub fn read_real(mut f: impl Read + Seek) -> io::Result<f64> {
    // Read digits and at most one period or comma, plus one extra character
    let mut period_seen = false;
    let mut nonspace_seen = false;
    let mut bytes = read_until(&mut f, |b| {
        // If you read spaces or dashes at the start, skip them
        if b == 0x20 || b == 0x2d {
            return nonspace_seen
        }
        nonspace_seen = true;
        // Comma or period
        if b == 0x2e || b == 0x2c {
            if period_seen {
                true
            } else {
                period_seen = true;
                false
            }
        } else {
            b < 0x30 || b > 0x39
        }
    })?;
    // read_until leaves a trailing character, so remove that
    if let Some(&b) = bytes.last() {
        if b < 0x30 || b > 0x39 {
            // Remove the trailing character and step back if it's a CR
            if bytes.pop().unwrap() == 0x0d {
                f.seek(SeekFrom::Current(-1))?;
            }
        }
    }
    // Having done that, there may still be a trailing dot, so remove that
    if let Some(&b) = bytes.last() {
        if b == 0x2e || b == 0x2c {
            bytes.pop();
        }
    }
    // These bytes are guaranteed to be UTF-8 so no worries here
    let mut text = String::from_utf8_lossy(bytes.as_slice()).replace(",", ".");
    // Remove spaces and all dashes but one
    let mut minus_seen = false;
    text.retain(|c| {
        if c == '-' {
            if minus_seen {
                return false
            }
            minus_seen = true;
        }
        c != ' '
    });
    text.parse().or(Ok(0.0))
}

pub fn skip_line(f: impl Read) -> io::Result<()> {
    skip_until(f, |c| c == 0x0a)?;
    Ok(())
}

pub fn file_exists(path: &str) -> bool {
    #[cfg(target_arch = "wasm32")]
    if session_files::exists(path) {
        return true
    }
    // A DLL this build reimplements answers yes even with no such file anywhere: the game is about
    // to `external_define` it and every one of those calls will work. See
    // `crate::game::external::is_emulated_dll` for why the stock wrapper-script sanity check
    // otherwise opened a "file not found" box on games whose audio was fine.
    #[cfg(target_arch = "wasm32")]
    if crate::game::external::is_emulated_dll(path) {
        return true
    }
    Path::new(path).is_file()
}

// `file_rename` and `file_copy` go through the VFS like every other file call; bare `std::fs` has
// no filesystem under it at all on wasm32-unknown-unknown. Games rotate saves by moving them ("copy
// the old file to its new home, then delete it"): Not Another Needle Game migrates `Achievements`
// to `Data/Achievements` on the title screen that way.
//
// GM8's own semantics, which these keep: a copy overwrites the destination, a rename does not and
// just does nothing if something is already there (Windows `MoveFile`), and both report success to
// GML regardless -- `file_copy`/`file_rename` return nothing and the callers above fail silently.
pub fn rename(from: &str, to: &str) -> Result<()> {
    #[cfg(target_arch = "wasm32")]
    {
        if session_files::exists(to) {
            return Ok(())
        }
        let bytes =
            session_files::lookup_bytes(from).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        session_files::overwrite(to, bytes);
        session_files::remove(from);
        return Ok(())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if !Path::new(to).exists() {
            std::fs::rename(from, to)?;
        }
        Ok(())
    }
}

pub fn copy(from: &str, to: &str) -> Result<()> {
    #[cfg(target_arch = "wasm32")]
    {
        let bytes =
            session_files::lookup_bytes(from).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        session_files::overwrite(to, bytes);
        return Ok(())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::fs::copy(from, to)?;
        Ok(())
    }
}

pub fn dir_exists(path: &str) -> bool {
    // A folder upload's files are the only "directories" a browser build has; consult those before
    // the (always-false on wasm) real filesystem.
    #[cfg(target_arch = "wasm32")]
    if session_files::dir_exists(path) {
        return true
    }
    Path::new(path).is_dir()
}

// Records the directory in the VFS store rather than no-op'ing.
//
// The VFS normally *infers* a directory from the files inside it, which covers the common idiom
// ("make this folder, then write into it" -- the write is what establishes it). What inference
// cannot represent is an empty directory, so a game that checks its own `directory_create` worked
//
//     directory_create(dir);
//     if (!directory_exists(dir)) show_error("could not create save directory");
//
// would be told the folder it just made does not exist if this were a bare `Ok(())`.
#[cfg(target_arch = "wasm32")]
pub fn dir_create(path: &str) -> Result<()> {
    crate::gml::vfs::create_dir(path);
    Ok(())
}
#[cfg(not(target_arch = "wasm32"))]
pub fn dir_create(path: &str) -> Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

pub fn delete(path: &str) -> Result<()> {
    #[cfg(target_arch = "wasm32")]
    session_files::remove(path);
    if Path::new(path).exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn load_image(path: &str) -> Result<RgbaImage> {
    // Check the virtual filesystem first -- round-trips with `save_image`'s wasm implementation
    // above (a screen_save/sprite_save screenshot), and also picks up any matching file from a
    // folder upload. `open_virtual_read` is always `None` on native, so this is a no-op there.
    if let Some(mut r) = open_virtual_read(path) {
        let mut bytes = Vec::new();
        r.read_to_end(&mut bytes)?;
        // eprintln! from callers like sprite_add goes nowhere on wasm (no stdout/stderr ->
        // console bridge is set up), so surface a decode failure directly -- otherwise silent.
        return match image::load_from_memory(&bytes) {
            Ok(img) => Ok(img.into_rgba8()),
            Err(e) => {
                #[cfg(target_arch = "wasm32")]
                web_sys::console::error_1(
                    &format!("load_image: decode failed for {:?} ({} bytes): {}", path, bytes.len(), e).into(),
                );
                Err(e.into())
            },
        }
    }
    Ok(image::open(path)?.into_rgba8())
}

pub fn load_animation(path: &str, imgnumb: usize) -> Result<Vec<RgbaImage>> {
    if ImageFormat::from_path(path)? == ImageFormat::Gif {
        GifDecoder::new(BufReader::new(File::open(path)?))?
            .into_frames()
            .map(|r| r.map(|f| f.into_buffer()).map_err(Error::from))
            .collect()
    } else {
        let image = load_image(path.as_ref())?;
        let sprite_width = image.width() as usize / imgnumb;
        let sprite_height = image.height() as usize;
        // get pixel data for each frame
        if imgnumb > 1 {
            let mut images = Vec::with_capacity(imgnumb);
            for i in 0..imgnumb {
                let mut pixels = Vec::with_capacity(sprite_width * sprite_height * 4);
                for row in image.rows() {
                    for p in row.skip(i * sprite_width).take(sprite_width) {
                        pixels.extend_from_slice(p.channels());
                    }
                }
                images.push(RgbaImage::from_vec(sprite_width as _, sprite_height as _, pixels).unwrap());
            }
            Ok(images)
        } else {
            Ok(vec![image])
        }
    }
}

// No real filesystem on wasm32 to save a snapshot/screenshot image to (e.g. screen_save/
// sprite_save, commonly used for a "continue" thumbnail showing the room the player last saved
// in) -- encode it in memory instead and write it through the same virtual filesystem/localStorage
// path everything else in this module uses, so it persists across a page reload exactly like save
// data does, and round-trips correctly with `load_image` below (which checks the same registry).
#[cfg(target_arch = "wasm32")]
pub fn save_image<P: AsRef<Path>>(path: P, image: RgbaImage) -> Result<()> {
    let format =
        if path.as_ref().extension().and_then(|s| s.to_str()).map(|s| s.eq_ignore_ascii_case("png")).unwrap_or(false) {
            ImageFormat::Png
        } else {
            ImageFormat::Bmp
        };
    // Encode fully in memory first, then write the complete result in one shot -- image encoders
    // (the BMP one especially) issue many small sequential writes (one header field at a time,
    // then one chunk per pixel row), and VirtualFile::write persists to localStorage on every
    // single call. Anything interrupting that sequence partway (a panic elsewhere, a page close)
    // would leave a corrupted file behind: its header (written first) claims the full size, but
    // the pixel data trails off wherever the write sequence stopped. Writing once, atomically,
    // means a reader never observes a partial state, and is also far fewer localStorage round
    // trips than one per pixel.
    let mut encoded = std::io::Cursor::new(Vec::new());
    image.write_to(&mut encoded, format)?;
    let path_str = path.as_ref().to_string_lossy();
    let mut writer = open_virtual_write(&path_str, true, false).ok_or(Error::CantWrite)?;
    writer.write_all(&encoded.into_inner())?;
    Ok(())
}
#[cfg(not(target_arch = "wasm32"))]
pub fn save_image<P: AsRef<Path>>(path: P, image: RgbaImage) -> Result<()> {
    // save to png if the filename is .png otherwise bmp regardless of filename
    if path.as_ref().extension().and_then(|s| s.to_str()).map(|s| s.eq_ignore_ascii_case("png")).unwrap_or(false) {
        image.save_with_format(path, ImageFormat::Png)?;
    } else {
        image.save_with_format(path, ImageFormat::Bmp)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lets an in-memory cursor stand in for a file in these tests -- `BoxedWriteSeek` needs it,
    /// and `std::fs::File` is no use on the wasm32-wasi target the crate's tests run on.
    impl TruncateAt for io::Cursor<Vec<u8>> {
        fn truncate_at(&mut self, size: u64) -> io::Result<()> {
            self.get_mut().truncate(size as usize);
            Ok(())
        }
    }

    fn binary_reader(bytes: &[u8]) -> BinaryHandle {
        BinaryHandle::Read(BoxedReadSeek(Box::new(io::Cursor::new(bytes.to_vec()))))
    }

    /// The shape that brought this in: "I wanna stop the MELTDOWN" writes a 24-byte `DeathTime`
    /// and reads 48 bytes back from it on every later launch, inside room 0's create events. Real
    /// GM8 aborts on the 25th byte; we keep going, or the game can never start again. See
    /// `BinaryHandle::read_byte`.
    #[test]
    fn read_byte_past_end_yields_zero() {
        let mut f = binary_reader(&[1, 2, 3]);
        assert_eq!(f.read_byte().unwrap(), 1);
        assert_eq!(f.read_byte().unwrap(), 2);
        assert_eq!(f.read_byte().unwrap(), 3);
        for _ in 0..4 {
            assert_eq!(f.read_byte().unwrap(), 0);
        }
    }

    /// Reads past the end still advance, so a game that reads a fixed-size record off the end of a
    /// short file and then asks where it is gets the answer it would have on a long-enough file.
    #[test]
    fn read_byte_past_end_advances_position() {
        let mut f = binary_reader(&[1]);
        f.read_byte().unwrap();
        f.read_byte().unwrap();
        f.read_byte().unwrap();
        assert_eq!(f.tell().unwrap(), 3);
    }

    /// An empty file is the same case with nothing to read first -- the state a run cut off before
    /// it wrote anything leaves behind.
    #[test]
    fn read_byte_on_empty_file_yields_zero() {
        let mut f = binary_reader(&[]);
        assert_eq!(f.read_byte().unwrap(), 0);
        assert_eq!(f.tell().unwrap(), 1);
    }

    /// The browser's in-memory file, read from a position past its end. Worth its own test because
    /// it is the one that actually took the game down: `read_byte` answering 0 is no use if the
    /// read underneath it panics, and a `file_bin_seek` beyond the end reaches the same line.
    #[cfg(target_arch = "wasm32")]
    #[test]
    fn virtual_file_reads_from_past_the_end() {
        let mut f = VirtualFile {
            name: "DeathTime".into(),
            buf: std::rc::Rc::new(std::cell::RefCell::new(vec![1, 2, 3])),
            pos: 9,
            // Keeps `Drop` from persisting, which would call into a JS bridge the test host
            // does not have -- see the field's doc comment.
            read_only: true,
        };
        let mut out = [0u8; 4];
        assert_eq!(f.read(&mut out).unwrap(), 0);
        assert_eq!(f.pos, 9);
    }

    /// The forgiveness is specific to reading past the end: a handle opened for writing still
    /// refuses reads rather than quietly answering 0.
    #[test]
    fn read_byte_on_write_handle_still_errors() {
        let mut f = BinaryHandle::Write(BoxedWriteSeek(Box::new(io::Cursor::new(Vec::new()))));
        assert!(matches!(f.read_byte(), Err(Error::CantRead)));
    }
}
