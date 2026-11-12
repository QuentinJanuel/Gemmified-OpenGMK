// Publishes a released zip as the update feed, <base>/latest.json, at the bucket's root. Run by the
// release step right after the zip itself is uploaded:
//
//   node scripts/publish-feed.mjs <bucket> <base url> <zip> [--icon <https url>] [--dry-run]
//
// The id and version come from the zip's own plugin.json, so the feed can only list a zip under the
// id and version that zip declares. A release REPLACES the feed: it lists only that version, and
// scripts/prune-old-versions.mjs then deletes the older builds. --dry-run prints the feed and
// uploads nothing. --icon adds an optional top-level "icon" to
// the feed; without it the feed is unchanged.
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { basename, join } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";

/** The feed for one release: only that version. */
export function buildFeed({ id, version, url, sha256, icon }) {
  const feed = { id, latest: version, versions: { [version]: { url, sha256 } } };
  return validateFeed(icon === undefined ? feed : { ...feed, icon }, id);
}

export function validateFeed(feed, id) {
  if (typeof feed.id !== "string" || feed.id !== id) throw new Error(`feed id is not ${id}`);
  if (typeof feed.versions !== "object" || feed.versions === null)
    throw new Error("feed has no versions");
  if (!Object.hasOwn(feed.versions, feed.latest))
    throw new Error(`feed latest ${feed.latest} is not one of its versions`);
  if (feed.icon !== undefined && !/^https:\/\/\S+$/.test(feed.icon))
    throw new Error("feed icon is not https");
  for (const [version, { url, sha256 }] of Object.entries(feed.versions)) {
    if (!/^https:\/\/\S+$/.test(url)) throw new Error(`feed ${version}: url is not https`);
    if (!/^[0-9a-f]{64}$/.test(sha256)) throw new Error(`feed ${version}: bad sha256`);
  }
  return feed;
}

async function main() {
  const args = process.argv.slice(2);
  const dryRun = args.includes("--dry-run");
  const at = args.indexOf("--icon");
  const icon = at === -1 ? undefined : (args.splice(at, 2)[1] ?? "");
  const [bucket, base, zip] = args.filter((a) => a !== "--dry-run");
  if (!zip)
    throw new Error("usage: publish-feed.mjs <bucket> <base url> <zip> [--icon <https url>] [--dry-run]");

  const manifest = JSON.parse(execFileSync("unzip", ["-p", zip, "plugin.json"], { encoding: "utf8" }));
  const sha256 = createHash("sha256").update(readFileSync(zip)).digest("hex");
  const url = `${base}/${manifest.version}/${basename(zip)}`;

  const feed = buildFeed({ id: manifest.id, version: manifest.version, url, sha256, icon });
  const json = `${JSON.stringify(feed, null, 2)}\n`;
  if (dryRun) {
    process.stdout.write(json);
    return;
  }
  const file = join(mkdtempSync(join(tmpdir(), "feed-")), "latest.json");
  writeFileSync(file, json);
  execFileSync(
    "pnpm",
    ["dlx", "wrangler@4.120.0", "r2", "object", "put", `${bucket}/latest.json`, "--file", file,
      "--content-type", "application/json", "--cache-control", "public, max-age=300", "--remote"],
    { stdio: "inherit" },
  );
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) await main();
