// OpenGMK's worker: every message from the host realm's half goes to the engine's `onMessage`
// (gm8emulator/src/gems.rs). Messages that arrive while the engine loads wait for it.

const early = [];
self.onmessage = (event) => early.push(event.data);

/** Answers a request the engine cannot: a payload that did not load, or a panic mid-message. */
function fail(data, code, error) {
  if (data?.w !== "req") return;
  const detail = String(error?.message ?? error);
  self.postMessage({ w: "err", id: data.id, error: { engineError: true, code, detail } });
}

let handle;
try {
  const engine = await import("./gm8emulator.js");
  await engine.default();
  handle = (data) => {
    try {
      engine.onMessage(data);
    } catch (error) {
      // A game that panics the engine while loading is reported as the game's fault.
      fail(data, data?.body?.kind === "start" ? "corrupt-game" : "internal", error);
    }
  };
} catch (error) {
  handle = (data) => fail(data, "asset-unavailable", error);
}
self.onmessage = (event) => handle(event.data);
early.splice(0).forEach(handle);
