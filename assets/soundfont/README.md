# GeneralUser GS

The instrument bank MIDI music is rendered through. GM8 games embed `.mid` for background
music fairly often, and MIDI is a score rather than encoded audio — without a bank there is
nothing to play it with. See `gm8emulator/src/game/audio_wasm/midi.rs`.

- **Bank:** GeneralUser GS v2.0.3 by S. Christian Collins, from
  <https://github.com/mrbumpy409/GeneralUser-GS> (upstream: <https://schristiancollins.com>).
- **Licence:** see `LICENSE.txt`. It permits use in software projects and redistribution, and
  asks that you host your own copy rather than hot-linking the author's downloads — which is
  why each release publishes its own copy.

## The `.sf2` is not in this repository

It is ~31MiB of sample data that never changes, so it is vendored *by reference*: `soundfont.json`
pins the exact bytes (an upstream URL at a fixed commit, a sha256 and a length), and
`../../scripts/fetch-soundfont.mjs` downloads and verifies them into this directory, which
`.gitignore` excludes.

`pnpm run build` fetches it first, so nothing needs doing by hand; `pnpm run soundfont` does the
fetch alone. A copy already on disk is verified and left alone, so neither is a download on a
warm checkout.

## How it reaches a browser

`pnpm run build` copies it into `dist/`; `pnpm run package` leaves it out of `plugin/`. A tagged
release uploads it next to the plugin zip, at `<version>/GeneralUser-GS.sf2`, and the manifest
lists it as an optional asset with that `url`. The engine (`gm8emulator/src/gems.rs`) fetches it
from the host's asset base only when `WasmGame.needs_soundfont()` says the game it just parsed
actually contains MIDI, so a game with no MIDI never downloads it.

To move to a different bank: replace all four fields in `soundfont.json` and `LICENSE.txt`.
