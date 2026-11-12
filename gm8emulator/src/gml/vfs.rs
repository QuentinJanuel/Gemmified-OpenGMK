//! The game's filesystem, as GEMS has it (the GEMS spec §13): the bundle, the store and the replica the
//! host persists. The model is in `overlay`; this module is the GML-facing surface over it, plus the
//! §13.5 profile constants and the redirect that keeps saves written before §13.5 where they are.
//!
//! The handle layer in `file.rs` reads a whole file once on open and writes it back once on flush,
//! so these calls happen per file, not per byte.

#[cfg(target_arch = "wasm32")]
mod overlay;

/// Separates the store listing `legacy_redirect` reads, one `path\tsize` per line. Neither
/// character can occur in a Windows path, so no escaping is needed.
#[cfg(any(target_arch = "wasm32", test))]
const FIND_SEPARATOR: char = '\n';

/// The `game` mount's root, as the engine reports it to GML; `overlay::parse_path` reads it back.
///
/// Real, non-empty roots are the point: see the note in `wasm_entry.rs` on why an empty string
/// causes a family of path bugs.
pub const GAME_ROOT: &str = "/game";

/// `temp_directory` as GEMS spec §13.5's `gm8` profile has it: a Windows path below `TEMP`, which
/// `overlay::parse_path` files under the `temp` mount. Games climb out of it to find the user's folders
/// (`filename_dir(filename_dir(filename_dir(temp_directory))) + "\Roaming\"`, gm82core's
/// `directory_appdata`, gm82snd/gm82joy's `%TEMP%\gm82 dll cache`), so it has to be the real shape.
pub const TEMP_DIRECTORY: &str = "C:\\Users\\player\\AppData\\Local\\Temp\\gm_ttt_1";

/// `environment_get_variable`'s answers for the variables §13.5 fixes.
pub fn profile_env(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_uppercase().as_str() {
        "USERPROFILE" => "C:\\Users\\player",
        "APPDATA" => "C:\\Users\\player\\AppData\\Roaming",
        "LOCALAPPDATA" => "C:\\Users\\player\\AppData\\Local",
        "TEMP" | "TMP" => "C:\\Users\\player\\AppData\\Local\\Temp",
        _ => return None,
    })
}

/// Splits on both separators and cleans `.`/`..` as `overlay::parse_path` does.
#[cfg(any(target_arch = "wasm32", test))]
fn clean(raw: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in raw.split(['\\', '/']) {
        match segment {
            "" | "." => (),
            ".." => {
                out.pop();
            },
            s => out.push(s),
        }
    }
    out.join("/")
}

/// Before §13.5 (0.1.4 and earlier) `temp_directory` was `/temp` and `APPDATA`/`LOCALAPPDATA` were
/// empty, so a path the game now builds into `AppData\Roaming\<rest>` or `AppData\Local\<rest>`
/// used to land in the `game` mount instead: at `<rest>` (the environment variables), at
/// `Roaming/<rest>` / `Local/<rest>` (climbing out of `temp_directory`, which is where renex keeps
/// its settings) or at `tempRoaming/<rest>` / `tempLocal/<rest>` (gm82core's `directory_appdata`),
/// and `USERPROFILE + "\AppData\Local\<rest>"` landed in `appdata` at `Local/<rest>`.
///
/// A save that already sits at one of those keeps being used there, for reads and writes alike, as
/// long as the new place is empty: its keys never move, and a fresh save goes where §13.5 says.
/// `store` is `list_store`'s output, only fetched for a path that has an old place.
///
/// shortcut: decided per path, so a new file written beside an old one lands in the new folder, and
/// `file_find_first` on that folder then lists only the new files; merge both listings if a game
/// that enumerates its save folder turns up.
#[cfg(any(target_arch = "wasm32", test))]
fn legacy_redirect(path: &str, store: impl FnOnce() -> String) -> Option<String> {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    // §13.5 rules 1 and 2 come first: mount roots and temp are not appdata.
    if ["/game", "/appdata", "/temp"].iter().any(|r| lower == *r || lower.starts_with(&format!("{r}/")))
        || lower.contains("appdata/local/temp/")
    {
        return None
    }
    let (at, marker, side) = [("appdata/roaming/", "Roaming"), ("appdata/local/", "Local")]
        .iter()
        .filter_map(|(m, side)| lower.find(m).map(|at| (at, m.len(), *side)))
        .min()?;
    let rest = clean(&path[at + marker..]);
    let store = store().to_ascii_lowercase();
    let has = |p: &str| {
        let p = p.to_ascii_lowercase();
        store
            .split(FIND_SEPARATOR)
            .filter_map(|l| l.split('\t').next())
            .any(|f| f == p || f.strip_prefix(p.as_str()).map_or(false, |tail| tail.starts_with('/')))
    };
    if has(&format!("/appdata/{rest}")) {
        return None
    }
    let old = [
        (GAME_ROOT, String::new()),
        (GAME_ROOT, format!("{side}/")),
        (GAME_ROOT, format!("temp{side}/")),
        ("/appdata", format!("{side}/")),
    ];
    old.iter().map(|(root, prefix)| format!("{root}/{}", clean(&format!("{prefix}{rest}")))).find(|old| has(old))
}

#[cfg(any(target_arch = "wasm32", test))]
fn resolve(path: &str, store: impl FnOnce() -> String) -> std::borrow::Cow<'_, str> {
    legacy_redirect(path, store).map_or(path.into(), Into::into)
}

#[cfg(test)]
mod tests {
    use super::{TEMP_DIRECTORY, legacy_redirect};

    const STORE: &str = "/game/Roaming/renex engine global/settings.cfg\t12\n\
        /game/save/renex² engine.cfg\t4\n/game/tempRoaming/Dir Game/save\t3\n/game/EnvGame/a.ini\t3\n\
        /appdata/Local/Prof/b\t1";

    fn redirect(path: &str) -> Option<String> {
        legacy_redirect(path, || STORE.to_owned())
    }

    #[test]
    fn old_saves_stay_where_they_are() {
        // renex: `filename_dir^3(temp_directory) + "\Roaming\..."`
        let renex = "C:\\Users\\player\\AppData\\Roaming\\renex engine global\\settings.cfg";
        assert_eq!(redirect(renex).as_deref(), Some("/game/Roaming/renex engine global/settings.cfg"));
        // its folder too, for `directory_exists`, in whatever case the game asks (the overlay
        // compares case-insensitively)
        let dir = "C:\\Users\\player\\AppData\\Roaming\\RENEX ENGINE GLOBAL";
        assert_eq!(redirect(dir).as_deref(), Some("/game/Roaming/RENEX ENGINE GLOBAL"));
        // gm82core `directory_appdata + "Roaming\..."`, then `environment_get_variable("APPDATA")`
        let d = "C:\\Users\\player\\AppData\\Roaming\\Dir Game\\save";
        assert_eq!(redirect(d).as_deref(), Some("/game/tempRoaming/Dir Game/save"));
        let e = "C:\\Users\\player\\AppData\\Local\\EnvGame\\a.ini";
        assert_eq!(redirect(e).as_deref(), Some("/game/EnvGame/a.ini"));
        // `environment_get_variable("USERPROFILE") + "\AppData\Local\..."`
        let p = "C:\\Users\\player\\AppData\\Local\\Prof\\b";
        assert_eq!(redirect(p).as_deref(), Some("/appdata/Local/Prof/b"));
    }

    #[test]
    fn new_and_unrelated_paths_are_untouched() {
        for path in [
            // nothing old there: the §13.5 place
            "C:\\Users\\player\\AppData\\Roaming\\probe\\p_tmp3.txt",
            // the Local side never had a `Roaming/` key
            "C:\\Users\\player\\AppData\\Local\\renex engine global\\settings.cfg",
            // temp, including the gm82 dll cache, never falls back to a persisted key
            "C:\\Users\\player\\AppData\\Local\\Temp\\gm82 dll cache\\SDL2.dll",
            &format!("{TEMP_DIRECTORY}\\gm82\\msgbut.gmspr"),
            "/appdata/registry.ini",
            "/game/save/renex² engine.cfg",
            "save1",
        ] {
            assert_eq!(redirect(path), None, "{path}");
        }
        // once the new place has the file, the old one is no longer consulted
        let store = || "/appdata/EnvGame/a.ini\t1\n/game/EnvGame/a.ini\t1".to_owned();
        assert_eq!(legacy_redirect("C:\\Users\\player\\AppData\\Local\\EnvGame\\a.ini", store), None);
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    pub use super::overlay::parse_path;
    use super::{FIND_SEPARATOR, overlay::Vfs};
    use std::cell::RefCell;

    thread_local! {
        static VFS: RefCell<Option<Vfs>> = const { RefCell::new(None) };
    }

    /// This session's filesystem, over the replica the host seeded. `mirror` hears every change to
    /// a persisted entry and `saved` every real save.
    pub fn start(game: String, seed: Vec<(String, String)>, mirror: fn(&str, Option<&str>), saved: fn()) {
        VFS.with(|vfs| *vfs.borrow_mut() = Some(Vfs::new(game, seed, mirror, saved)));
    }

    fn with<T: Default>(f: impl FnOnce(&mut Vfs) -> T) -> T {
        VFS.with(|vfs| vfs.borrow_mut().as_mut().map_or_else(T::default, f))
    }

    fn resolve(path: &str) -> super::overlay::VPath {
        parse_path(&super::resolve(path, list_store_joined))
    }

    fn list_store_joined() -> String {
        let lines: Vec<String> = list_store().into_iter().map(|(path, size)| format!("{path}\t{size}")).collect();
        lines.join(&FIND_SEPARATOR.to_string())
    }

    pub fn read(path: &str) -> Option<Vec<u8>> {
        let path = resolve(path);
        with(|vfs| vfs.read(&path))
    }

    pub fn write(path: &str, data: &[u8]) {
        let path = resolve(path);
        with(|vfs| vfs.write(&path, data.to_vec()))
    }

    /// A file the host put, at the path it named: no redirect, as before.
    pub fn put(path: &str, data: Vec<u8>) {
        with(|vfs| vfs.write(&parse_path(path), data))
    }

    /// An entry the host deleted, by key; not mirrored back.
    pub fn drop_key(key: &str) {
        with(|vfs| vfs.drop_key(key))
    }

    pub fn exists(path: &str) -> bool {
        let path = resolve(path);
        with(|vfs| vfs.exists(&path))
    }

    /// Returns whether anything was there to delete, which is what GML's `file_delete` reports.
    pub fn remove(path: &str) -> bool {
        let path = resolve(path);
        with(|vfs| vfs.remove(&path))
    }

    pub fn dir_exists(path: &str) -> bool {
        let path = resolve(path);
        with(|vfs| vfs.dir_exists(&path))
    }

    /// `file_find_first`'s glob, e.g. `"/game/data/sounds/*.ogg"`. Returns bare names.
    ///
    /// `include_dirs` is the `fa_directory` attribute: subdirectories are reported alongside files,
    /// which is how a game enumerates its own asset folders before looking inside them.
    pub fn find(pattern: &str, include_dirs: bool) -> Vec<String> {
        // The glob is the last segment; only the folder before it can have an old place.
        let full = match pattern.rfind(['\\', '/']) {
            Some(at) => parse_path(&format!("{}{}", super::resolve(&pattern[..at], list_store_joined), &pattern[at..])),
            None => parse_path(pattern),
        };
        let name = match full.base_name() {
            "" => "*",
            name => name,
        };
        with(|vfs| vfs.find_files(&full.parent(), name, include_dirs))
    }

    /// GML's `directory_create`. Recorded explicitly because directories are otherwise inferred
    /// from the files inside them, which cannot represent an empty one -- a game that verifies its
    /// own `directory_create` with `directory_exists` would be told the folder was never made.
    pub fn create_dir(path: &str) {
        let path = resolve(path);
        with(|vfs| vfs.create_dir(&path))
    }

    /// Adds a read-only file to the bundle: something the *engine* provides on the game's behalf,
    /// rather than something the player uploaded or the game wrote.
    ///
    /// Include Files (`game.rs`) and GM8.2 extension payloads (the sprites, shaders and DLLs an
    /// extension package unpacks into `temp_directory`) come through here; the game's own loose
    /// files through `add_bundle_js`.
    pub fn add_bundle(path: &str, data: &[u8]) {
        add_bundle_js(path, js_sys::Uint8Array::from(data))
    }

    pub fn add_bundle_js(path: &str, data: js_sys::Uint8Array) {
        with(|vfs| vfs.add_bundle(parse_path(path), data))
    }

    /// Every file in the store for this game, as `(path, size)`.
    pub fn list_store() -> Vec<(String, usize)> {
        with(|vfs| vfs.store_list().into_iter().map(|(path, size)| (path.format(), size)).collect())
    }
}

// Native builds have a real filesystem and never consult the browser VFS; `file.rs` keeps its
// `std::fs` paths under the same `cfg`. These exist so the module stays portable rather than every
// call site needing its own `cfg`.
#[cfg(not(target_arch = "wasm32"))]
mod imp {
    pub fn read(_path: &str) -> Option<Vec<u8>> {
        None
    }
    pub fn write(_path: &str, _data: &[u8]) {}
    pub fn exists(_path: &str) -> bool {
        false
    }
    pub fn remove(_path: &str) -> bool {
        false
    }
    pub fn dir_exists(_path: &str) -> bool {
        false
    }
    pub fn find(_pattern: &str, _include_dirs: bool) -> Vec<String> {
        Vec::new()
    }
    pub fn create_dir(_path: &str) {}
    pub fn add_bundle(_path: &str, _data: &[u8]) {}
    pub fn list_store() -> Vec<(String, usize)> {
        Vec::new()
    }
}

pub use imp::{add_bundle, create_dir, dir_exists, exists, find, list_store, read, remove, write};
#[cfg(target_arch = "wasm32")]
pub use imp::{add_bundle_js, drop_key, put, start};
