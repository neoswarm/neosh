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

// ---------------------------------------------------------------------------
// Activation order
//
// A plugin that `requires` another waits for that one's `activate` to settle before its own runs,
// so `import { api } from "plugin:sidebar"` sees a sidebar that has set itself up. The promise is
// recorded synchronously, before the first await, because the host sends the required plugin's
// load first and this loop handles messages in order — so by the time a dependent looks, its
// dependency is here. A dependency that failed fails the dependent, with the name in the error.
// ---------------------------------------------------------------------------

const activations = new Map();
// Each plugin's entry module, kept so `unload` can call its `deactivate` — which the docs had
// promised for as long as the docs existed, and which nothing had ever called.
const modules = new Map();

async function activate(msg) {
  for (const name of msg.requires ?? []) {
    const dep = activations.get(name);
    if (!dep) {
      throw new Error(`requires plugin "${name}", which is not loaded`);
    }
    const error = await dep;
    if (error) {
      throw new Error(`requires plugin "${name}", which failed to activate: ${error}`);
    }
  }
  // Dynamic import goes through the host's module loader, which transpiles TypeScript and
  // resolves `@neosh/api` to the embedded source.
  const mod = await import(msg.url);
  const ctx = __createContext(msg.plugin, msg.config, msg.version);
  if (typeof mod.activate !== "function") {
    throw new Error(`${msg.url} does not export an \`activate\` function`);
  }
  modules.set(msg.plugin, mod);
  await mod.activate(ctx);
}

// `deactivate`, then the teardown that was always there. Bounded: a plugin that never returns
// from its goodbye must not hold a reload — or a shutdown — hostage, so after a moment the
// subscriptions are disposed under it.
async function unload(plugin) {
  activations.delete(plugin);
  const mod = modules.get(plugin);
  modules.delete(plugin);
  if (mod && typeof mod.deactivate === "function") {
    try {
      await Promise.race([
        Promise.resolve(mod.deactivate()),
        new Promise((r) => setTimeout(r, 1000)),
      ]);
    } catch (e) {
      ops.op_neosh_send({ type: "log", level: "warn", message: `${plugin} deactivate: ${describe(e)}` });
    }
  }
  __teardown(plugin);
}

async function handle(msg) {
  switch (msg.type) {
    case "load": {
      // Settles to `null` on success and to the error text on failure; never rejects, so a
      // dependent awaiting it cannot become an unhandled rejection.
      const done = activate(msg).then(
        () => null,
        (e) => describe(e),
      );
      activations.set(msg.plugin, done);
      const error = await done;
      // A plugin that throws during activation is reported and skipped; it must not take the
      // editor down with it.
      ops.op_neosh_send({ type: "loaded", plugin: msg.plugin, error });
      break;
    }
    case "plugin":
      await __dispatch(msg.plugin, msg.msg);
      break;
    case "unload":
      await unload(msg.plugin);
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
