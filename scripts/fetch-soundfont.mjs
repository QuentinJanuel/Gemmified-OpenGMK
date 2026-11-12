// Fetches the SoundFont bank MIDI music is rendered through, into assets/soundfont/.
//
// The bank is ~31MiB of sample data that never changes, so it is vendored by reference rather
// than committed: soundfont.json pins the exact bytes and this downloads them. That keeps a
// binary of that size out of the repository's history, where it would be paid for by every
// clone forever, while leaving the build reproducible -- the url is pinned to an upstream
// commit, and the sha256 is checked, so "whatever is on main today" can never slip in.
//
// Idempotent: a correct copy already on disk is left alone, which is what makes it safe to put
// in front of every build. Run directly with `pnpm run soundfont`.
import { createHash } from "node:crypto";
import { mkdir, readFile, rename, stat, unlink, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const dir = path.join(here, "..", "assets", "soundfont");
const manifest = JSON.parse(await readFile(path.join(dir, "soundfont.json"), "utf8"));
const target = path.join(dir, manifest.file);

const sha256 = (buf) => createHash("sha256").update(buf).digest("hex");

/** The hash of what is on disk, or null if there is nothing there to hash. */
async function onDisk() {
  try {
    return sha256(await readFile(target));
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}

const existing = await onDisk();
if (existing === manifest.sha256) {
  console.log(`soundfont: ${manifest.file} already present and verified.`);
  process.exit(0);
}
if (existing !== null) {
  console.warn(`soundfont: ${manifest.file} does not match soundfont.json — refetching.`);
}

console.log(`soundfont: fetching ${manifest.name} v${manifest.version} (${(manifest.bytes / 1048576).toFixed(1)} MiB)…`);
const response = await fetch(manifest.url);
if (!response.ok) {
  throw new Error(`soundfont: ${manifest.url} returned HTTP ${response.status}`);
}
const bytes = Buffer.from(await response.arrayBuffer());

// Both checks, not just the hash: a length mismatch names the likelier failure (a truncated or
// proxied download) in terms someone can act on, rather than as an opaque digest difference.
if (bytes.length !== manifest.bytes) {
  throw new Error(`soundfont: expected ${manifest.bytes} bytes, got ${bytes.length}`);
}
const got = sha256(bytes);
if (got !== manifest.sha256) {
  throw new Error(`soundfont: sha256 mismatch\n  expected ${manifest.sha256}\n  got      ${got}`);
}

// Written aside and renamed so an interrupted run cannot leave a half-file that later looks
// like a corrupt bank rather than an absent one.
await mkdir(dir, { recursive: true });
const tmp = `${target}.partial`;
await writeFile(tmp, bytes);
await rename(tmp, target);
await stat(target);
console.log(`soundfont: wrote ${manifest.file}, sha256 verified.`);
