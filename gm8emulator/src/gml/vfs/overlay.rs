//! The filesystem GEMS describes (the GEMS spec §13): the overlay of §13.2, over the replica of §13.3,
//! keyed as §13.4 says. The keys it mints are the player's saves, so they must never change:
//! `tests` pins them, quirks included.
//!
//! - **bundle**: the game's own files, read-only, rebuilt every session. Kept as JS arrays, as
//!   before: a game's loose files can run to hundreds of megabytes, and wasm memory never shrinks.
//! - **store**: everything the game wrote. Persisted entries are base64 strings by key, mirrored to
//!   the host on every change; the `temp` mount is session-only and never leaves this worker.
//! - a read checks store, then whiteouts, then bundle; a write lands in the store; deleting a bundle
//!   file leaves a whiteout.
//!
//! Store keys keep the case the game wrote (only the bundle and directory checks fold case), so
//! `Save\a` and `save\a` are two store entries. That is how every existing save was written.

use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use indexmap::{IndexMap, IndexSet};

pub const MOUNTS: [&str; 3] = ["game", "appdata", "temp"];

#[derive(Clone, Debug, PartialEq)]
pub struct VPath {
    pub mount: &'static str,
    pub segments: Vec<String>,
}

impl VPath {
    /// The string form (§13.1), which is what the game is handed back.
    pub fn format(&self) -> String {
        format!("/{}/{}", self.mount, self.segments.join("/"))
    }

    /// Identity, case-insensitively (§13.1).
    fn key(&self) -> String {
        format!("{}:{}", self.mount, self.segments.join("/").to_lowercase())
    }

    pub fn parent(&self) -> VPath {
        VPath { mount: self.mount, segments: self.segments[..self.segments.len().saturating_sub(1)].to_vec() }
    }

    pub fn base_name(&self) -> &str {
        self.segments.last().map_or("", String::as_str)
    }

    /// Whether `path` sits somewhere inside this directory.
    fn contains(&self, path: &VPath) -> bool {
        self.mount == path.mount
            && path.segments.len() > self.segments.len()
            && self.segments.iter().zip(&path.segments).all(|(a, b)| a.to_lowercase() == b.to_lowercase())
    }
}

/// Splits on both separators; `..` pops, and one that would climb above the root is dropped.
fn clean_segments(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for segment in raw.split(['\\', '/']) {
        match segment {
            "" | "." => (),
            ".." => {
                out.pop();
            },
            s => out.push(s.to_owned()),
        }
    }
    out
}

const TEMP_MARKERS: [&str; 2] = ["appdata/local/temp/", "local settings/temp/"];
const APPDATA_MARKERS: [&str; 4] =
    ["appdata/local/", "appdata/roaming/", "local settings/application data/", "application data/"];

/// Where in `lower` the first of `markers` (by position, then list order) ends.
fn after_first(lower: &str, markers: &[&str]) -> Option<usize> {
    let mut first = usize::MAX;
    let mut end = None;
    for marker in markers {
        if let Some(at) = lower.find(marker) {
            if at < first {
                first = at;
                end = Some(at + marker.len());
            }
        }
    }
    end
}

/// A game's path string, as GEMS spec §13.5's Windows mapping resolves it.
pub fn parse_path(raw: &str) -> VPath {
    let unified = raw.replace('\\', "/");
    let lower = unified.to_lowercase();
    // The TypeScript found a position in the lowercased string and sliced the original at it, in
    // UTF-16 units. Lowercasing can lengthen a string ("İ"), so the two can disagree; slicing the same
    // way keeps every such key where it always was.
    let units: Vec<u16> = unified.encode_utf16().collect();
    let from = |at: usize| {
        let start = lower[..at].encode_utf16().count();
        String::from_utf16_lossy(units.get(start..).unwrap_or(&[]))
    };

    for mount in MOUNTS {
        let bare = format!("/{mount}");
        if lower == bare || lower.starts_with(&format!("{bare}/")) {
            return VPath { mount, segments: clean_segments(&from(bare.len())) }
        }
    }
    if let Some(end) = after_first(&lower, &TEMP_MARKERS) {
        return VPath { mount: "temp", segments: clean_segments(&from(end)) }
    }
    if let Some(end) = after_first(&lower, &APPDATA_MARKERS) {
        return VPath { mount: "appdata", segments: clean_segments(&from(end)) }
    }
    if lower == "c:/game" || lower.starts_with("c:/game/") {
        return VPath { mount: "game", segments: clean_segments(&from("c:/game".len())) }
    }
    // Another drive is dropped: the path stays inside the game's own folder.
    let bytes = unified.as_bytes();
    let without_drive =
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' { &unified[2..] } else { &unified };
    VPath { mount: "game", segments: clean_segments(without_drive) }
}

// -- key codec (§13.4) ----------------------------------------------------------------------------

/// ECMAScript's `encodeURIComponent`.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// ECMAScript's `decodeURIComponent`, or `None` where it would throw.
fn decode_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

const FILE: &str = "file";
const GONE: &str = "gone";
const DIR: &str = "dir";

fn store_key(tag: &str, game: &str, path: &VPath) -> String {
    let encoded: Vec<String> = path.segments.iter().map(|s| encode_component(s)).collect();
    format!("gems:{tag}:{game}:{}:{}", path.mount, encoded.join("/"))
}

/// The inverse of `store_key`, or `None` for any key that is not one of ours.
fn parse_key(key: &str) -> Option<(&str, &str, VPath)> {
    let parts: Vec<&str> = key.split(':').collect();
    if parts.len() < 5 || parts[0] != "gems" || ![FILE, GONE, DIR].contains(&parts[1]) {
        return None
    }
    let mount = *MOUNTS.iter().find(|m| **m == parts[3])?;
    let encoded = parts[4..].join(":");
    let segments = if encoded.is_empty() {
        Vec::new()
    } else {
        encoded.split('/').map(decode_component).collect::<Option<_>>()?
    };
    Some((parts[1], parts[2], VPath { mount, segments }))
}

/// `btoa`'s output, and `atob`'s forgiveness on the way back in.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let clean: String = value.chars().filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')).collect();
    BASE64.decode(clean).ok()
}

/// The game's filesystem: one per session.
pub struct Vfs {
    game: String,
    bundle: IndexMap<String, (VPath, js_sys::Uint8Array)>,
    /// The replica of the host's persisted entries, in the host's order.
    backend: IndexMap<String, String>,
    /// `temp` files, whiteouts and directory marks: this session only, keyed like the rest.
    session: IndexMap<String, Vec<u8>>,
    session_flags: IndexSet<String>,
    /// Every change to `backend`, for the host (`None` is a removal).
    mirror: fn(&str, Option<&str>),
    /// A real save landed.
    saved: fn(),
}

impl Vfs {
    pub fn new(
        game: String,
        seed: impl IntoIterator<Item = (String, String)>,
        mirror: fn(&str, Option<&str>),
        saved: fn(),
    ) -> Self {
        Self {
            game,
            bundle: IndexMap::new(),
            backend: seed.into_iter().collect(),
            session: IndexMap::new(),
            session_flags: IndexSet::new(),
            mirror,
            saved,
        }
    }

    fn set(&mut self, key: String, value: String) {
        (self.mirror)(&key, Some(&value));
        self.backend.insert(key, value);
    }

    /// Mirrored whether or not the key was there, as it always was.
    fn unset(&mut self, key: &str) {
        self.backend.shift_remove(key);
        (self.mirror)(key, None);
    }

    /// The host deleted an entry: dropped here without telling it back.
    pub fn drop_key(&mut self, key: &str) {
        self.backend.shift_remove(key);
    }

    pub fn add_bundle(&mut self, path: VPath, data: js_sys::Uint8Array) {
        self.bundle.insert(path.key(), (path, data));
    }

    // -- the store layer --

    fn stored(&self, path: &VPath) -> Option<Vec<u8>> {
        let key = store_key(FILE, &self.game, path);
        if path.mount == "temp" {
            return self.session.get(&key).cloned()
        }
        decode_base64(self.backend.get(&key)?)
    }

    fn has_stored(&self, path: &VPath) -> bool {
        let key = store_key(FILE, &self.game, path);
        if path.mount == "temp" { self.session.contains_key(&key) } else { self.backend.contains_key(&key) }
    }

    fn is_whited_out(&self, path: &VPath) -> bool {
        let key = store_key(GONE, &self.game, path);
        if path.mount == "temp" { self.session_flags.contains(&key) } else { self.backend.contains_key(&key) }
    }

    fn is_marked_directory(&self, path: &VPath) -> bool {
        let key = store_key(DIR, &self.game, path);
        if path.mount == "temp" { self.session_flags.contains(&key) } else { self.backend.contains_key(&key) }
    }

    fn marked_directories(&self) -> Vec<VPath> {
        self.backend
            .keys()
            .chain(self.session_flags.iter())
            .filter_map(|key| parse_key(key))
            .filter(|(tag, game, _)| *tag == DIR && *game == self.game)
            .map(|(_, _, path)| path)
            .collect()
    }

    /// Every file in the store, persisted then session, with its size.
    pub fn store_list(&self) -> Vec<(VPath, usize)> {
        let persisted = self.backend.iter().filter_map(|(key, value)| match parse_key(key) {
            // The decoded length as the TypeScript estimated it, padding and all.
            Some((FILE, game, path)) if game == self.game => Some((path, value.len() * 3 / 4)),
            _ => None,
        });
        let session = self.session.iter().filter_map(|(key, data)| match parse_key(key) {
            Some((FILE, game, path)) if game == self.game => Some((path, data.len())),
            _ => None,
        });
        persisted.chain(session).collect()
    }

    // -- the overlay --

    pub fn read(&self, path: &VPath) -> Option<Vec<u8>> {
        if let Some(data) = self.stored(path) {
            return Some(data)
        }
        if self.is_whited_out(path) {
            return None
        }
        self.bundle.get(&path.key()).map(|(_, data)| data.to_vec())
    }

    pub fn exists(&self, path: &VPath) -> bool {
        if self.has_stored(path) {
            return true
        }
        !self.is_whited_out(path) && self.bundle.contains_key(&path.key())
    }

    /// Writing a path also lifts any whiteout on it.
    pub fn write(&mut self, path: &VPath, data: Vec<u8>) {
        let file = store_key(FILE, &self.game, path);
        let gone = store_key(GONE, &self.game, path);
        if path.mount == "temp" {
            self.session_flags.shift_remove(&gone);
            self.session.insert(file, data);
            return
        }
        self.unset(&gone);
        self.set(file, BASE64.encode(data));
        (self.saved)();
    }

    /// Whether anything was there to delete, which is what GML's `file_delete` reports.
    pub fn remove(&mut self, path: &VPath) -> bool {
        let existed = self.exists(path);
        let in_bundle = self.bundle.contains_key(&path.key());
        let file = store_key(FILE, &self.game, path);
        let gone = store_key(GONE, &self.game, path);
        if path.mount == "temp" {
            self.session.shift_remove(&file);
            if in_bundle {
                self.session_flags.insert(gone);
            }
        } else {
            self.unset(&file);
            if in_bundle {
                self.set(gone, "1".into());
            }
            (self.saved)();
        }
        existed
    }

    /// GML's `directory_create`: recorded, so it exists while empty. A mount root always exists.
    pub fn create_dir(&mut self, path: &VPath) {
        if path.segments.is_empty() {
            return
        }
        let key = store_key(DIR, &self.game, path);
        if path.mount == "temp" {
            self.session_flags.insert(key);
        } else {
            self.set(key, "1".into());
        }
    }

    /// Every visible file, both layers, the store shadowing the bundle.
    fn list_all(&self) -> Vec<(VPath, usize)> {
        let mut merged: IndexMap<String, (VPath, usize)> = IndexMap::new();
        for (key, (path, data)) in &self.bundle {
            if !self.is_whited_out(path) {
                merged.insert(key.clone(), (path.clone(), data.length() as usize));
            }
        }
        for (path, size) in self.store_list() {
            merged.insert(path.key(), (path, size));
        }
        merged.into_values().collect()
    }

    pub fn dir_exists(&self, dir: &VPath) -> bool {
        dir.segments.is_empty()
            || self.is_marked_directory(dir)
            || self.list_all().iter().any(|(path, _)| dir.contains(path))
    }

    /// `file_find_first`: the names in `dir` matching `pattern`, directories first when asked for,
    /// each group in the order JavaScript's `localeCompare` gives (as before, so a game enumerates
    /// the same order it always did).
    pub fn find_files(&self, dir: &VPath, pattern: &str, include_dirs: bool) -> Vec<String> {
        let matcher = wildcard(pattern);
        let mut files = Vec::new();
        let mut dirs: IndexMap<String, String> = IndexMap::new();
        for (path, _) in self.list_all() {
            if !dir.contains(&path) {
                continue
            }
            if path.segments.len() == dir.segments.len() + 1 {
                let name = path.base_name();
                if matcher.test(name) {
                    files.push(name.to_owned());
                }
            } else {
                let name = &path.segments[dir.segments.len()];
                dirs.entry(name.to_lowercase()).or_insert_with(|| name.clone());
            }
        }
        for marked in self.marked_directories() {
            if marked.segments.len() == dir.segments.len() + 1 && dir.contains(&marked) {
                let name = marked.base_name().to_owned();
                dirs.entry(name.to_lowercase()).or_insert(name);
            }
        }
        let mut dirs: Vec<String> = if include_dirs { dirs.into_values().collect() } else { Vec::new() };
        let options = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&options, &"sensitivity".into(), &"base".into());
        let locales = js_sys::Array::new();
        let by_name =
            |a: &String, b: &String| js_sys::JsString::from(a.as_str()).locale_compare(b, &locales, &options).cmp(&0);
        dirs.sort_by(by_name);
        files.sort_by(by_name);
        dirs.extend(files);
        dirs
    }
}

/// `file_find_first`'s wildcard (`*`, `?`), as the case-insensitive regular expression it always was.
fn wildcard(pattern: &str) -> js_sys::RegExp {
    let mut source = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => source.push_str("[^/]*"),
            '?' => source.push_str("[^/]"),
            _ => {
                if ".+^${}()|[]\\".contains(ch) {
                    source.push('\\');
                }
                source.push(ch);
            },
        }
    }
    source.push('$');
    js_sys::RegExp::new(&source, "i")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static LOG: RefCell<Vec<String>> = RefCell::new(Vec::new());
    }

    fn mirror(key: &str, value: Option<&str>) {
        LOG.with(|log| log.borrow_mut().push(format!("{key}={}", value.unwrap_or("-"))));
    }

    fn saved() {
        LOG.with(|log| log.borrow_mut().push("save".into()));
    }

    fn log() -> Vec<String> {
        LOG.with(|log| log.take())
    }

    /// Store keys the TypeScript implementation minted for these strings (game id `game-1`).
    #[test]
    fn keys_match_the_typescript() {
        let cases: &[(&str, &str)] = &[
            ("save1", "game:save1"),
            ("Data\\save1", "game:Data/save1"),
            ("\\save1", "game:save1"),
            ("/save1", "game:save1"),
            ("D:\\x\\save1", "game:x/save1"),
            ("C:\\probe\\p_drive.txt", "game:probe/p_drive.txt"),
            ("c:", "game:"),
            ("Z:foo", "game:foo"),
            ("/game", "game:"),
            ("/game/", "game:"),
            ("/GAME/Save/Save1", "game:Save/Save1"),
            ("/game\\p_wd.txt", "game:p_wd.txt"),
            ("/gamex/a", "game:gamex/a"),
            ("/appdata", "appdata:"),
            ("/appdata/X/y", "appdata:X/y"),
            ("/temp/a", "temp:a"),
            ("/TEMP", "temp:"),
            ("..\\p_up.txt", "game:p_up.txt"),
            ("a/../../b", "game:b"),
            ("./a/./b//c\\\\d", "game:a/b/c/d"),
            ("PCase\\P_CASE.TXT", "game:PCase/P_CASE.TXT"),
            ("pcase\\p_case.txt", "game:pcase/p_case.txt"),
            ("C:\\Users\\player\\AppData\\Local\\Temp\\gm_ttt_1\\p_tmp.txt", "temp:gm_ttt_1/p_tmp.txt"),
            ("C:\\Users\\player\\AppData\\Local\\Temp\\gm82 dll cache\\SDL2.dll", "temp:gm82%20dll%20cache/SDL2.dll"),
            ("C:\\Users\\player\\AppData\\Roaming\\probe\\p_tmp3.txt", "appdata:probe/p_tmp3.txt"),
            ("C:\\Users\\player\\AppData\\Local\\probe\\p_env_local.txt", "appdata:probe/p_env_local.txt"),
            (
                "C:\\Users\\player\\AppData\\Roaming\\renex engine global\\settings.cfg",
                "appdata:renex%20engine%20global/settings.cfg",
            ),
            ("C:\\Documents and Settings\\player\\Local Settings\\Temp\\x", "temp:x"),
            ("C:\\Documents and Settings\\player\\Local Settings\\Application Data\\X\\a", "appdata:X/a"),
            ("C:\\Documents and Settings\\player\\Application Data\\Y\\b", "appdata:Y/b"),
            ("C:\\game\\Save\\save1", "game:Save/save1"),
            ("C:\\gamesave1", "game:gamesave1"),
            ("C:\\game", "game:"),
            ("c:/GAME/x", "game:x"),
            // `Temp` with no separator after it is not a temp marker, only an appdata one.
            ("C:\\Users\\player\\AppData\\Local\\Temp", "appdata:Temp"),
            ("C:\\Users\\player\\AppData\\Local\\Temp\\", "temp:"),
            ("C:\\Users\\player\\AppData\\Local", "game:Users/player/AppData/Local"),
            ("C:\\Users\\player\\AppData\\Local\\", "appdata:"),
            ("C:\\Users\\player\\AppData\\Roaming\\..\\..\\x", "appdata:x"),
            ("x\\AppData\\Local\\Temp\\..\\..\\y", "temp:y"),
            ("save/renex² engine.cfg", "game:save/renex%C2%B2%20engine.cfg"),
            ("セーブ\\データ1.sav", "game:%E3%82%BB%E3%83%BC%E3%83%96/%E3%83%87%E3%83%BC%E3%82%BF1.sav"),
            ("a:b%c#d?e f+g", "game:b%25c%23d%3Fe%20f%2Bg"),
            ("名前/İstanbul\\AppData\\Local\\z", "appdata:"),
            ("İ\\appdata\\local\\temp\\q", "temp:"),
            ("\u{212A}ey\\AppData\\Roaming\\k", "appdata:k"),
            ("ΣΑΒΕ\\ΣΑΒΕΣ.txt", "game:%CE%A3%CE%91%CE%92%CE%95/%CE%A3%CE%91%CE%92%CE%95%CE%A3.txt"),
            ("😀/AppData/Roaming/😀.sav", "appdata:%F0%9F%98%80.sav"),
            ("", "game:"),
            ("/", "game:"),
            ("\\\\server\\share\\f", "game:server/share/f"),
            ("C:\\Users\\player\\AppData\\LocalLow\\x", "game:Users/player/AppData/LocalLow/x"),
            ("C:\\Users\\player\\AppData\\Local Settings\\Temp\\y", "temp:y"),
            ("~!*'()-_.", "game:~!*'()-_."),
            ("tab\there", "game:tab%09here"),
        ];
        for (raw, want) in cases {
            assert_eq!(store_key(FILE, "game-1", &parse_path(raw)), format!("gems:file:game-1:{want}"), "{raw}");
        }
    }

    #[test]
    fn keys_round_trip() {
        let path = parse_path("a:b%c#d?e f+g/セーブ:x");
        let key = store_key(GONE, "g", &path);
        assert_eq!(parse_key(&key), Some((GONE, "g", path)));
        assert_eq!(parse_key("gems:meta:g"), None);
        assert_eq!(parse_key("gems:file:g:game:%E0"), None);
    }

    /// What the host is told, and in what order, for each kind of change.
    #[test]
    fn mirrors_every_change() {
        let seed = vec![("gems:file:g:game:old".to_owned(), "b2xk".to_owned())];
        let mut vfs = Vfs::new("g".into(), seed, mirror, saved);
        assert_eq!(vfs.read(&parse_path("old")), Some(b"old".to_vec()));
        assert_eq!(vfs.read(&parse_path("OLD")), None);

        vfs.write(&parse_path("Save\\a"), b"hi".to_vec());
        assert_eq!(log(), ["gems:gone:g:game:Save/a=-", "gems:file:g:game:Save/a=aGk=", "save"]);
        vfs.write(&parse_path("/temp/x"), b"t".to_vec());
        vfs.create_dir(&parse_path("/temp/d"));
        assert_eq!(log(), Vec::<String>::new());
        vfs.create_dir(&parse_path("Dir"));
        assert_eq!(log(), ["gems:dir:g:game:Dir=1"]);

        assert!(vfs.remove(&parse_path("old")));
        assert_eq!(log(), ["gems:file:g:game:old=-", "save"]);
        assert!(!vfs.remove(&parse_path("missing")));
        assert_eq!(log(), ["gems:file:g:game:missing=-", "save"]);

        let listed: Vec<String> = vfs.store_list().iter().map(|(p, n)| format!("{}\t{n}", p.format())).collect();
        assert_eq!(listed, ["/game/Save/a\t3", "/temp/x\t1"]);
        assert!(vfs.dir_exists(&parse_path("save")) && vfs.dir_exists(&parse_path("Dir")));
        assert!(vfs.dir_exists(&parse_path("/temp/d")) && !vfs.dir_exists(&parse_path("nope")));
        // A directory mark is found by its exact case, as it always was.
        assert!(!vfs.dir_exists(&parse_path("dir")));

        vfs.drop_key("gems:file:g:game:Save/a");
        assert_eq!(log(), Vec::<String>::new());
        assert!(!vfs.exists(&parse_path("Save\\a")));
    }
}
