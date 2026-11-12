import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildFeed } from "./publish-feed.mjs";

const sha = (c) => c.repeat(64);
const release = (version, c = "a") => ({
  id: "demo",
  version,
  url: `https://example.test/${version}/demo-${version}.zip`,
  sha256: sha(c),
});

test("a release is the whole feed", () => {
  assert.deepEqual(buildFeed(release("0.1.0")), {
    id: "demo",
    latest: "0.1.0",
    versions: { "0.1.0": { url: "https://example.test/0.1.0/demo-0.1.0.zip", sha256: sha("a") } },
  });
});

test("a later release replaces the previous one, it never accumulates", () => {
  const feed = buildFeed(release("0.2.0", "b"));
  assert.equal(feed.latest, "0.2.0");
  assert.deepEqual(Object.keys(feed.versions), ["0.2.0"]);
});

test("a malformed release is refused", () => {
  assert.throws(() => buildFeed({ ...release("0.1.0"), sha256: sha("A") }));
  assert.throws(() => buildFeed({ ...release("0.1.0"), url: "http://example.test/x.zip" }));
});

const icon = "https://example.test/icon.png";

test("an icon is added at the top level", () => {
  assert.equal(buildFeed({ ...release("0.1.0"), icon }).icon, icon);
});

test("without an icon the feed has no icon key", () => {
  assert.equal("icon" in buildFeed(release("0.1.0")), false);
});

test("a non-https icon is refused", () => {
  assert.throws(() => buildFeed({ ...release("0.1.0"), icon: "http://example.test/icon.png" }));
});

const dir = mkdtempSync(join(tmpdir(), "feed-test-"));
writeFileSync(join(dir, "plugin.json"), JSON.stringify({ id: "demo", version: "0.1.0" }));
execFileSync("zip", ["-q", "demo.zip", "plugin.json"], { cwd: dir });
const cli = (...extra) =>
  spawnSync("node", ["scripts/publish-feed.mjs", "b", "https://example.test", join(dir, "demo.zip"), "--dry-run", ...extra], {
    encoding: "utf8",
  });

test("--dry-run prints the feed with the icon", () => {
  const r = cli("--icon", icon);
  assert.equal(r.status, 0);
  assert.equal(JSON.parse(r.stdout).icon, icon);
  assert.equal("icon" in JSON.parse(cli().stdout), false);
});

test("--icon with a bad or missing url exits with an error", () => {
  assert.notEqual(cli("--icon", "http://x.test/i.png").status, 0);
  assert.notEqual(cli("--icon").status, 0);
});
