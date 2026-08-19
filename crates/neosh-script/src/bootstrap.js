// The plugin runtime's dispatch loop.
//
// Loaded as the main module so it can `import` the virtual `@neosh/api`. Everything else — every
// plugin, every call, every event — flows through the two ops this file uses. Keeping the v8
// surface that small is deliberate: it is the part that is hardest to test and most likely to break
// across deno_core versions.
import { __createContext, __dispatch, __teardown } from "@neosh/api";

const ops = Deno.core.ops;

function describe(e) {
  if (e instanceof Error) return e.stack || `${e.name}: ${e.message}`;
  return String(e);
}

// ---------------------------------------------------------------------------
// Timers
//
// A bare deno_core has none, so without this a plugin cannot debounce, poll, or retry — and
// `setTimeout` being missing is not a thing anyone expects to have to check for. The deadlines live
// in Rust; this side only holds the callbacks and dispatches them when the host says they are due,
// which is why cancellation and shutdown are the host's decision rather than a race.
// ---------------------------------------------------------------------------

const timers = new Map();

function arm(fn, ms, repeat, args) {
  if (typeof fn !== "function") {
    throw new TypeError("setTimeout/setInterval requires a function");
  }
  const id = ops.op_neosh_timer_start(Number(ms) || 0, repeat);
  timers.set(id, { fn, args, repeat });
  return id;
}

function disarm(id) {
  const n = Number(id);
  if (!Number.isFinite(n)) return;
  ops.op_neosh_timer_clear(n);
  timers.delete(n);
}

globalThis.setTimeout = (fn, ms, ...args) => arm(fn, ms, false, args);
globalThis.setInterval = (fn, ms, ...args) => arm(fn, ms, true, args);
globalThis.clearTimeout = disarm;
globalThis.clearInterval = disarm;

function fireTimers(ids) {
  for (const id of ids) {
    const t = timers.get(id);
    if (!t) continue;
    // Delete before calling, so a one-shot that re-arms itself gets a fresh id rather than
    // resurrecting this one.
    if (!t.repeat) timers.delete(id);
    try {
      const r = t.fn(...t.args);
      // An async callback that rejects must not become an unhandled rejection that takes down the
      // dispatch loop.
      if (r && typeof r.then === "function") r.then(undefined, reportTimerError);
    } catch (e) {
      reportTimerError(e);
    }
  }
}

function reportTimerError(e) {
  ops.op_neosh_send({ type: "log", level: "error", message: `timer callback: ${describe(e)}` });
}

async function handle(msg) {
  switch (msg.type) {
    case "load": {
      try {
        // Dynamic import goes through the host's module loader, which transpiles TypeScript and
        // resolves `@neosh/api` to the embedded source.
        const mod = await import(msg.url);
        const ctx = __createContext(msg.plugin, msg.config, msg.version);
        if (typeof mod.activate !== "function") {
          throw new Error(`${msg.url} does not export an \`activate\` function`);
        }
        await mod.activate(ctx);
        ops.op_neosh_send({ type: "loaded", plugin: msg.plugin, error: null });
      } catch (e) {
        // A plugin that throws during activation is reported and skipped; it must not take the
        // editor down with it.
        ops.op_neosh_send({ type: "loaded", plugin: msg.plugin, error: describe(e) });
      }
      break;
    }
    case "plugin":
      await __dispatch(msg.plugin, msg.msg);
      break;
    case "unload":
      __teardown(msg.plugin);
      break;
    case "timer":
      fireTimers(msg.ids);
      break;
  }
}

// Not awaited at the top level: the module must finish evaluating so the host can start feeding it.
(async () => {
  for (;;) {
    const msg = await ops.op_neosh_next();
    // A null message means the host closed the channel.
    if (msg === null || msg === undefined) return;
    // Deliberately *not* awaited. `handle` may run a plugin's `activate`, which awaits a host
    // response — and that response can only arrive through the next `op_neosh_next` call. Awaiting
    // here would block the one loop capable of delivering it, deadlocking every plugin that calls
    // the API during activation. Handlers run concurrently instead, which is also how an
    // out-of-process plugin would behave.
    handle(msg).catch((e) => {
      ops.op_neosh_send({ type: "log", level: "error", message: describe(e) });
    });
  }
})();
