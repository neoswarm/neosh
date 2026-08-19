/**
 * Globals the neosh runtime installs.
 *
 * These are declared here rather than pulled in from `lib.dom` or `@types/node`, because neither is
 * true: the runtime is a bare deno_core with no DOM, no filesystem and no network. Everything with
 * an effect goes through `@neosh/api`, which is what makes hooks and permissions mean anything.
 *
 * Timer handles are opaque numbers. They are not Node's `Timeout` objects and have no `.unref()`.
 */

/** Run `fn` once, no sooner than `ms` from now. */
declare function setTimeout<A extends unknown[]>(
  fn: (...args: A) => unknown,
  ms?: number,
  ...args: A
): number;

/** Run `fn` repeatedly. The next firing is scheduled when the previous one is dispatched, so a slow
 * callback cannot build up a backlog. */
declare function setInterval<A extends unknown[]>(
  fn: (...args: A) => unknown,
  ms?: number,
  ...args: A
): number;

declare function clearTimeout(id: number | undefined): void;
declare function clearInterval(id: number | undefined): void;

/** Present, and the only console there is: it routes to neosh's log file, not to the terminal —
 * stdout belongs to the UI protocol. Prefer `neosh.log`, which is attributed to your plugin. */
declare const console: {
  log(...args: unknown[]): void;
  info(...args: unknown[]): void;
  warn(...args: unknown[]): void;
  error(...args: unknown[]): void;
  debug(...args: unknown[]): void;
};

declare function queueMicrotask(fn: () => void): void;
