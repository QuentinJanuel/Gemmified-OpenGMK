# Gemmified-OpenGMK

This is a fork of OpenGMK that runs GameMaker 8 games in the browser. It compiles the OpenGMK
emulator to WebAssembly and packages it as a plugin for any host that speaks the GEMS plugin
protocol. The upstream project's README follows [below](#opengmk).

## Install

Give a GEMS host this update feed:

```
https://gemmified-opengmk.dappermink.me/latest.json
```

The feed lists only the latest release, and the host uses it to install the engine and keep it
up to date. It can also carry an optional top-level `icon`, an https URL to
`https://gemmified-opengmk.dappermink.me/icon.png`.

A release publishes the plugin as
`https://gemmified-opengmk.dappermink.me/<version>/opengmk-<version>.zip` and makes it the only
version in the feed. Builds of earlier versions are then deleted. The SoundFont that MIDI music
plays through (about 31 MiB) is not in the zip. It is published next to the zip, at
`<version>/GeneralUser-GS.sf2`, and the manifest lists it as an optional asset with that `url`.
If a host doesn't serve it, the game still runs but MIDI music stays silent.

## The plugin

A plugin is a directory with two parts:

- `plugin.json`: the manifest. It holds `id`, `version`, `apiVersion`, `entry`, `formats`,
  `updates` (the feed above), `assets` (the SoundFont) and `capabilities` (the settings the
  engine reads). `scripts/write-manifest.mjs` writes it from `host/plugin.js`, so the two always
  agree.
- `host.js` (built from `host/plugin.js`): the entry module. Its default export has an
  `accepts(files)` method, which picks a game's `.exe`, and a `create(host)` method, which
  returns a session.

The session lives in the `host` crate, which runs in the page. It owns the `AudioContext`, the
`Worker` and the shared buffer that blocking GML dialogs are answered through. The engine
(`gm8emulator`) runs in that worker (`host/worker.js`), and `gm8emulator/src/gems.rs` is its side
of the message protocol between the two. The host page must be cross-origin isolated, because
the dialogs use `SharedArrayBuffer`.

## Build and test

You need Rust 1.77 (from `rust-toolchain.toml`) with the `wasm32-unknown-unknown` and
`wasm32-wasi` targets, the `wasm-bindgen` CLI matching `Cargo.lock` (0.2.114), Node 22, and pnpm.

```sh
pnpm install
pnpm build      # fetches and checks the SoundFont, builds both wasm modules into dist/
pnpm package    # the plugin directory, in plugin/
pnpm lint       # clippy
pnpm test       # sm3 natively, gm8emulator on wasm32-wasi via Node, the release scripts
pnpm format:check   # needs the nightly toolchain pinned in package.json
```

### Releasing

Every pull request is checked by `.github/workflows/pull-request.yml`. A release is started by hand:
Actions, Release, Run workflow, on `downstream`. It takes the version from the live feed and increases
the part you choose (patch by default), or takes an exact version. It builds the plugin as that
version, uploads it, replaces the feed, and creates the `v<version>` tag and GitHub release.
`package.json`'s version is only a placeholder, so there is never a version-bump commit.

The web build always uses `--no-default-features`, which leaves out the TAS tooling. The
desktop build and the TAS framework described below come from upstream. This fork keeps the
code they share with the web build plausibly correct, but neither builds nor tests them. Building
GM8Decompiler, which the upstream README below describes, now happens in gm8-lib.

## Relation to OpenGMK

This repository is a fork of [OpenGMK](https://github.com/OpenGMK/OpenGMK). It carries upstream's
history unchanged. `master` is upstream's, untouched, and `downstream` (the default branch) is
where this fork's work lives. `git diff master downstream` lists everything this fork changes.
Here is a summary:

- `gm8emulator` gains a wasm32 build: WebGL2 rendering, Web Audio with MIDI and tracker music,
  a virtual filesystem, and stand-ins for many GM8 extension DLLs.
- `sm3` is new. It translates Direct3D 9 shader bytecode to GLSL ES 3.00, for GM8.2's `shader_*`
  functions.
- `host` is new. It is the page side of the plugin.
- `gm8exe`, `gml-parser` and `gm8decompiler` now live in
  [gm8-lib](https://github.com/QuentinJanuel/gm8-lib), a GameMaker 8 file library extracted from
  OpenGMK. The emulator depends on it at a pinned commit.

## Licence

This fork is licensed GPL-2.0-only, the same as OpenGMK. See [LICENCE.md](LICENCE.md) and
[NOTICE](NOTICE). The GeneralUser GS SoundFont has its own licence, in
[assets/soundfont/LICENSE.txt](assets/soundfont/LICENSE.txt).

---

# OpenGMK

**OpenGMK** is a modern rewrite of the proprietary GameMaker Classic engines,
providing a full sourceport of the runner, a decompiler, a TASing framework,
and libraries for working with gamedata yourself.

> [!IMPORTANT]
> Please remember that this project is a **work in progress**.
> Your savestates may break in future releases.
>
> Please also be aware that there might never _be_ a stable release, since [nothing ever happens](https://i.kym-cdn.com/entries/icons/original/000/045/689/nothing_e.jpg).

## Downloading

There are pre-built binaries for Windows x86:

- <https://github.com/OpenGMK/OpenGMK/releases>
- <https://github.com/OpenGMK/GM8Decompiler/releases> (only GM8Decompiler but newer)

The release page hasn't been updated in a long time, so it's worth noting to **better build it yourself**.

## Compiling

If you want to compile **GM8Decompiler** or **GM8Emulator**:

1. Install the [Rust toolchain](https://rustup.rs).
2. Open a terminal (On Windows: Press <kbd>Win</kbd>+<kbd>R</kbd>, type "powershell", hit <kbd>Enter</kbd>).
3. Clone the repository: `git clone --recurse-submodules https://github.com/OpenGMK/OpenGMK`.
4. Go into the repository: `cd OpenGMK`.
5. Compile the crates: `cargo b -r` (ignore the warnings).
6. The programs `gm8emulator` and `gm8decompiler` will be located in the `target/release` folder.

Copy-pastable version for very lazy people:

```sh
git clone --recurse-submodules https://github.com/OpenGMK/OpenGMK
cd OpenGMK
cargo build --release
./target/release/gm8emulator
```

---

> [!TIP]
> You can also recursively initialize the submodules after you've already cloned:
>
> ```sh
> git submodule update --init --recursive
> ```

> [!WARNING]
> Only MSVC is currently supported for Windows.
> This means that target tuples ending in `-pc-windows-gnu` may not compile.

### Native DLLs for 64-bit Windows

If you're on Windows 64-bit and would like to play games with GM8Emulator that require 32-bit DLLs
to function such as **GMFMODSimple** or **supersound**, you'll also need to build the WoW64 server.
It requires the additional installation of the `i686-pc-windows-msvc` toolchain with rustup and you will need to build it separately.

```sh
rustup target add i686-pc-windows-msvc
cd gm8emulator-wow64   # inside the OpenGMK repo folder
cargo build --release --target=i686-pc-windows-msvc
```

The compiled WoW64 server program will be located in the `gm8emulator-wow64/target/i686-pc-windows-msvc/release` folder.
The binary should either be manually copied to the same folder as `gm8emulator.exe` to work,
or the `OPENGMK_WOW64_BINARY` environment variable should be set with the path to the binary.

Alternatively, you could build the project as 32-bit on Windows directly, eliminating the need for the WoW64 server.
You should note that this will produce a bit slower program, since 32-bit operating mode is no longer
considered primary in today's processors, and the i686 instruction set is much older. In order to do this:

```sh
# inside the OpenGMK folder
rustup target add i686-pc-windows-msvc
cargo build --release --target=i686-pc-windows-msvc
./target/i686-pc-windows-msvc/release/gm8decompiler
# or
./target/i686-pc-windows-msvc/release/gm8emulator
```

Cross-platform extension emulation is planned for the long-term future.

## Recording and Replaying TASes

To play a game normally, simply pass the only argument to `gm8emulator`:

```sh
gm8emulator path/to/game.exe
```

To start record mode, or continue a previous recording, also pass a project name with `-n`.
A folder for the project will be created in:

- Windows: `projects/`.
- Linux (Near™ Future): `$XDG_DATA_HOME/opengmk/projects/` or `~/.local/share/opengmk/projects/`.

```sh
gm8emulator path/to/game.exe -n project-name
```

While in record mode, a `save#.bin` is generated for each savestate (where `#` is the number of the savestate).
You can export a `save#.gmtas` file, which is for sharing, and has input data only.
If you've lost your `save#.bin`, or need to migrate OpenGMK versions, you can recreate it by simply replaying your `save#.gmtas`:

```sh
gm8emulator path/to/game.exe -l -f path/to/save#.gmtas -o path/to/save#.bin
```

Note that `-l` here means disabling the framelimiter so it goes by faster.

_All command-line steps will be streamlined in a future release._

## Common errors

**Q:** Loading a game throws `failed to load 'filename' - unknown format, could not identify file` or similar.

**A:** OpenGMK is made to support _GameMaker Classic_ games.
It’s possible the game you are trying to load was actually made with _GameMaker: Studio_, which is not supported.

If there is a `data.win`, `game.unx`, `game.ios` or similar file, then the game was
not made in GameMaker Classic and OpenGMK cannot decompile or emulate it.

---

**Q:** Loading a game throws `Error code 0xC0072095` or a mention of `wgl.CreateContextAttribsARB`.

**A:** Apparently, your system does not have the necessary version of OpenGL.
At the moment, OpenGMK unfortunately requires **OpenGL 3.3** with support for GLSL 3.30 shaders - we really hope to _significantly_ soften this over time.
Usually this situation occurs when attempting to run on integrated graphics hardware by Intel from the early 2010s and older.
You can try updating the drivers (sometimes they add support for newer versions of OpenGL).
If you also have a discrete GPU or graphics card, set OpenGMK to use it through the corresponding control panel.

---

**Q:** Loading/playing a game throws `unimplemented kernel function` or `not yet implemented` or similar.

**A:** Your game tried to access functionality that's yet to be implemented.
The full GameMaker Classic standard library is absolutely massive, and there's a good bit left to cover.
Sorry mate.

---

**Q:** Entering record mode throws `invalid u8 while decoding bool` or `expected variant` or similar.

**A:** This means that the `save#.bin` file in your project directory is out of date with OpenGMK.
This is a byproduct of it being actively developed, and is bound to happen.

To fix it, open it in the build of OpenGMK it was created with, export a `save#.gmtas` from it,
and recreate the `save#.bin` in the new build with that as described in the recording section.

## History

**GameMaker** (formerly _Game Maker_) is an engine for creating Windows games, originally
created by [Mark Overmars](https://en.wikipedia.org/wiki/Mark_Overmars) in 1999 and [YoYo Games](https://www.yoyogames.com/) since 2007.
_GameMaker 8_ ("GM8") was the last of the _numbered releases_ of GameMaker, released on 2009-12-22
(surpassing _GameMaker 7_) and succeeded by the vastly more popular _GameMaker: Studio_ in 2011.
The pre-Studio versions are often referred to as **GameMaker Classic**.
Due to the huge behavioral differences, as well as _Studio_'s lack of backward-compatibility,
the classic engines are still very widely used, with thousands of games to their name.

One of GameMaker's original strengths as a game engine was its ability to compile an entire project into a single executable.
No external dependencies or installers, just compile, send the `.exe` file to your friend and they will be able to play your game.
This is achieved by having the target executable act as a phase file for the entire collection of assets required to run the game.
In other words, the executable contains not only the game engine code, but all of the objects, scripts, sprites, room layouts, everything required for the game logic.
This behaviour was made optional in _Studio_, giving the creator a choice between a standalone executable or `.msi` installer.
However, the standalone builds just extract the contents of the installer to a temporary
folder when they're launched, so sending the extracted contents as a `.zip` became much more preferable.

This project was originally started as [GM8Emulator](https://github.com/Adamcake/Legacy-GM8Emulator), a program that
can load _GameMaker Classic_ games, and accurately play the game within.
The goal was to have it mimic the original engine as closely as possible, down to the sub-frame and implementation detail (if observable).
Strictly speaking, "emulator" was not the correct term.
In computing, an emulator is a piece of software on a computer system which emulates the behaviour of a different computer system.
We aren't emulating any computer system, just the engine, unless you consider _Game Maker Language_ its own architecture.
A more accurate term would be a _sourceport_, but it didn't sound as cool at the time.

The project required us to write a decompiler as a starting point to extract the assets.
Since we were already maintaining the most up-to-date fork of [Zach Reedy](https://github.com/DatZach)'s
[gm81decompiler](https://github.com/WastedMeerkat/gm81decompiler) from 2013, we used that as a starting point
to develop a much faster version from scratch out of the new codebase, released as [GM8Decompiler](https://github.com/OpenGMK/GM8Decompiler).
GM8Decompiler was originally a separate repository, but the code is now merged into this unified repository which we named the **OpenGMK Project**.

## Contributing

This project has only been worked on by a few people so far in their little free time.
Contributions are always welcome, although we'd prefer if you got in contact beforehand to discuss details (opening an issue, for example).
All contributions are licensed under the same licence as the project.

## Additional Credits

- [DatZach](https://github.com/DatZach) for creating the original free and open source decompiler.
- [Jabberwock-RU](https://github.com/Jabberwock-RU) for creating the project icon and logos.

## Licence

OpenGMK is free and open source software, provided under the [GNU General Public License v2](LICENCE.md).
SPDX-License-Identifier: `GPL-2.0-only`.

OpenGMK is no way affiliated with GameMaker or YoYo Games.
