// Writes dist/plugin.json from the plugin entry's own object, so the manifest and the engine cannot
// disagree. Run by `pnpm build`.
import { readFile, writeFile } from "node:fs/promises";
import plugin from "../host/plugin.js";

/** Where tagged builds are published; the SoundFont sits beside each zip rather than in it. */
const PUBLISHED = "https://gemmified-opengmk.dappermink.me";
const SOUNDFONT = "GeneralUser-GS.sf2";

const { version } = JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"));

const manifest = {
  id: plugin.id,
  version,
  apiVersion: plugin.apiVersion,
  entry: "host.js",
  name: "OpenGMK",
  formats: ["gm8"],
  description:
    "Runs GameMaker 8 games in the browser, on a WebAssembly build of OpenGMK, a GameMaker 8 emulator in Rust.",
  author: "Dapper Mink",
  updates: `${PUBLISHED}/latest.json`,
  // Published beside the zip, not in it: ~31 MiB that only a game with MIDI music needs. `url` says
  // where; a host serves it at `assets.base` (GEMS spec §7.3), fetched when the engine asks.
  assets: [
    { path: SOUNDFONT, bytes: 32319396, url: `${PUBLISHED}/${version}/${SOUNDFONT}`, optional: true },
  ],
  capabilities: plugin.capabilities,
};

await writeFile(new URL("../dist/plugin.json", import.meta.url), `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`write-manifest: wrote dist/plugin.json for ${plugin.id}@${version}`);
