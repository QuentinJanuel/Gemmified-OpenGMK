import assert from "node:assert/strict";
import test from "node:test";
import plugin from "./plugin.js";

const index = (...paths) => ({ files: paths.map((path) => ({ path, size: 1 })) });

test("takes a GameMaker 8 game by its .exe", async () => {
  // I Wanna See The Moon, and Verve's GM8.2 engine: an .exe and loose data, no Studio data.
  const moon = index("I Wanna See The Moon ver1.2.exe", "Read Me.txt");
  const verve = index("Verve GM82 Engine.exe", "data/music/megaman.ogg", "data/sounds/boss_hit.wav");
  assert.deepEqual(await plugin.accepts(moon), {
    confidence: "probable",
    entry: "I Wanna See The Moon ver1.2.exe",
  });
  assert.equal((await plugin.accepts(verve))?.entry, "Verve GM82 Engine.exe");
});

test("declines a GameMaker Studio game, whose .exe is the Studio runner", async () => {
  // I wanna follow it (GMS1): the runner .exe beside its data.win.
  const followIt = index("options.ini", "D3DX9_43.dll", "data.win", "I wanna follow it.exe");
  assert.equal(await plugin.accepts(followIt), null);
  assert.equal(await plugin.accepts(index("game/Game.exe", "game/DATA.WIN")), null);
  assert.equal(await plugin.accepts(index("runner", "assets/game.unx")), null);
});

test("declines a game with no executable", async () => {
  assert.equal(await plugin.accepts(index("readme.txt")), null);
});
