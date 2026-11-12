import { test } from "node:test";
import assert from "node:assert/strict";
import { oldVersionKeys } from "./prune-old-versions.mjs";

test("only other versions' objects are deleted", () => {
  const keys = [
    "latest.json",
    "0.1.0/demo-0.1.0.zip",
    "0.1.0/extra.bin",
    "0.10.0/demo-0.10.0.zip",
    "0.1.1/demo-0.1.1.zip",
    "1.0.0-rc.1/demo.zip",
    "sounds/font.sf2",
    "0.1.10/demo.zip",
  ];
  assert.deepEqual(oldVersionKeys(keys, "0.1.1"), [
    "0.1.0/demo-0.1.0.zip",
    "0.1.0/extra.bin",
    "0.10.0/demo-0.10.0.zip",
    "0.1.10/demo.zip",
  ]);
});

test("an empty or already pruned bucket has nothing to delete", () => {
  assert.deepEqual(oldVersionKeys([], "0.1.1"), []);
  assert.deepEqual(oldVersionKeys(["latest.json", "0.1.1/a.zip"], "0.1.1"), []);
});

test("the root icon.png is never selected", () => {
  assert.deepEqual(oldVersionKeys(["icon.png", "latest.json", "0.1.0/a.zip", "0.1.1/a.zip"], "0.1.1"), [
    "0.1.0/a.zip",
  ]);
});
