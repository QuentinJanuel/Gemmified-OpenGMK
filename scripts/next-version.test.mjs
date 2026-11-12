import assert from "node:assert/strict";
import test from "node:test";
import { nextVersion } from "./next-version.mjs";

test("bumps one part and resets the ones after it", () => {
  assert.equal(nextVersion("0.2.1", "patch"), "0.2.2");
  assert.equal(nextVersion("0.2.9", "minor"), "0.3.0");
  assert.equal(nextVersion("0.9.9", "major"), "1.0.0");
});

test("an exact version must be newer", () => {
  assert.equal(nextVersion("0.2.1", "patch", "1.0.0"), "1.0.0");
  assert.throws(() => nextVersion("0.2.1", "patch", "0.2.1"), /not newer/);
  assert.throws(() => nextVersion("0.2.1", "patch", "0.1.9"), /not newer/);
});

test("refuses what is not a version or a bump", () => {
  assert.throws(() => nextVersion("0.2", "patch"), /not a version/);
  assert.throws(() => nextVersion("0.2.1", "huge"), /bump must be/);
  assert.throws(() => nextVersion("0.2.1", "patch", "v1.0.0"), /not a version/);
});
