/**
 * `@neosh/api` — the entire plugin surface.
 *
 * This file is both the implementation and the types. It is embedded into the host binary and
 * transpiled at load, and it is what plugin authors type-check against, so the two cannot drift
 * apart — there is no second copy to forget to update.
 *
 * Everything here is built on the wire types in `./generated/`, which are emitted from the Rust
 * side by ts-rs and drift-checked in CI. Nothing in this file reaches the host except as an
 * `ApiCall`, which means an out-of-process plugin in another language has exactly this surface.
 */

import type { ApiCall } from "./generated/ApiCall";
import type { ApiError } from "./generated/ApiError";
import type { ApiOk } from "./generated/ApiOk";
import type { ApiResponse } from "./generated/ApiResponse";
import type { BranchInfo } from "./generated/BranchInfo";
import type { BufferId } from "./generated/BufferId";
import type { Capability } from "./generated/Capability";
import type { CommitInfo } from "./generated/CommitInfo";
import type { CommandEntry } from "./generated/CommandEntry";
import type { AccountKind } from "./generated/AccountKind";
import type { Brand } from "./generated/Brand";
import type { CredentialInfo } from "./generated/CredentialInfo";
import type { CredentialSource } from "./generated/CredentialSource";
import type { CursorMotion } from "./generated/CursorMotion";
import type { DiffTarget } from "./generated/DiffTarget";
import type { Dock } from "./generated/Dock";
import type { Gravity } from "./generated/Gravity";
import type { Hint } from "./generated/Hint";
import type { ExtmarkId } from "./generated/ExtmarkId";
import type { FileChange } from "./generated/FileChange";
import type { FileState } from "./generated/FileState";
import type { ExtmarkInfo } from "./generated/ExtmarkInfo";
import type { ExtmarkOpts } from "./generated/ExtmarkOpts";
import type { FloatConfig } from "./generated/FloatConfig";
import type { HighlightDef } from "./generated/HighlightDef";
import type { HighlightSpec } from "./generated/HighlightSpec";
import type { HookName } from "./generated/HookName";
import type { HookOutcome } from "./generated/HookOutcome";
import type { HookPayload } from "./generated/HookPayload";
import type { InstanceConfig } from "./generated/InstanceConfig";
import type { KeyContext } from "./generated/KeyContext";
import type { KeymapEntry } from "./generated/KeymapEntry";
import type { KeymapScope } from "./generated/KeymapScope";
import type { MessageLevel } from "./generated/MessageLevel";
import type { Mode } from "./generated/Mode";
import type { ModelEntry } from "./generated/ModelEntry";
import type { ModelInfo } from "./generated/ModelInfo";
import type { ModelSelection } from "./generated/ModelSelection";
import type { ModelTier } from "./generated/ModelTier";
import type { NamespaceId } from "./generated/NamespaceId";
import type { OptionChoice } from "./generated/OptionChoice";
import type { OptionEntry } from "./generated/OptionEntry";
import type { OptionSelection } from "./generated/OptionSelection";
import type { OptionSpec } from "./generated/OptionSpec";
import type { OptionType } from "./generated/OptionType";
import type { OptionValue } from "./generated/OptionValue";
import type { PermissionDecision } from "./generated/PermissionDecision";
import type { PluginEvent } from "./generated/PluginEvent";
import type { Pricing } from "./generated/Pricing";
import type { ProviderEvent } from "./generated/ProviderEvent";
import type { ProviderOptionDescriptor } from "./generated/ProviderOptionDescriptor";
import type { Message } from "./generated/Message";
import type { Rect } from "./generated/Rect";
import type { RepoInfo } from "./generated/RepoInfo";
import type { RepoStatus } from "./generated/RepoStatus";
import type { SessionId } from "./generated/SessionId";
import type { SessionInfo } from "./generated/SessionInfo";
import type { StatusAlign } from "./generated/StatusAlign";
import type { StatusSegment } from "./generated/StatusSegment";
import type { StopReason } from "./generated/StopReason";
import type { SurfaceCell } from "./generated/SurfaceCell";
import type { TextEdit } from "./generated/TextEdit";
import type { SurfaceId } from "./generated/SurfaceId";
import type { ToolCall } from "./generated/ToolCall";
import type { ToolDef } from "./generated/ToolDef";
import type { ToolResult } from "./generated/ToolResult";
import type { TurnRequest } from "./generated/TurnRequest";
import type { Usage } from "./generated/Usage";
import type { Viewport } from "./generated/Viewport";
import type { WindowId } from "./generated/WindowId";
import type { WindowLayout } from "./generated/WindowLayout";
import type { WorktreeInfo } from "./generated/WorktreeInfo";

export type {
  AccountKind, ApiError, BranchInfo, Brand, BufferId, Capability, CommandEntry, CommitInfo,
  CredentialInfo, CredentialSource, DiffTarget, Dock, CursorMotion, ExtmarkId, ExtmarkInfo, ExtmarkOpts, FileChange, FileState, FloatConfig,
  Gravity, HighlightDef, HighlightSpec, Hint, HookName, HookOutcome, HookPayload, InstanceConfig, KeyContext,
  KeymapEntry, KeymapScope, MessageLevel, Mode, ModelEntry, ModelInfo, ModelSelection, ModelTier, NamespaceId,
  OptionChoice, OptionEntry, OptionSelection, OptionSpec, OptionType, OptionValue,
  Message, PermissionDecision, PluginEvent, Pricing, ProviderEvent, ProviderOptionDescriptor,
  Rect, RepoInfo, RepoStatus, SessionId, SessionInfo, StatusAlign, StatusSegment, StopReason,
  SurfaceCell, SurfaceId, TextEdit, ToolCall, ToolDef, ToolResult, TurnRequest, Usage, Viewport,
  WindowId, WindowLayout,
  WorktreeInfo,
};

// ---------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------

/**
 * UTF-8 byte length of a string.
 *
 * Every column in this API is a **UTF-8 byte offset**, matching Neovim and for the same reason: it
 * keeps display-width math in exactly one place, the frontend. JavaScript's `.length` is UTF-16
 * code units, which agrees with bytes only for ASCII — so a highlight placed with `.length` on a
 * line containing an emoji or any CJK lands in the wrong column, or inside a character.
 */
export function byteLength(s: string): number {
  let n = 0;
  for (const ch of s) {
    const c = ch.codePointAt(0) ?? 0;
    n += c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4;
  }
  return n;
}

/**
 * Byte offset of each code point in `s`, plus one final entry holding the total length.
 *
 * `byteOffsets(s)[i]` is the column to pass for the `i`th code point, so a match found with
 * `Array.from(s)` can be turned into marks without measuring the prefix again per position.
 */
export function byteOffsets(s: string): number[] {
  const out: number[] = [];
  let n = 0;
  for (const ch of s) {
    out.push(n);
    const c = ch.codePointAt(0) ?? 0;
    n += c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4;
  }
  out.push(n);
  return out;
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/** The ops the host installs. Everything else is built on these. */
interface CoreOps {
  op_neosh_send(msg: unknown): void;
  op_neosh_next(): Promise<unknown>;
  op_neosh_width(text: string): number;
  op_neosh_clip(text: string, columns: number): string;
}
declare const Deno: { core: { ops: CoreOps } };

/**
 * How many terminal columns a string occupies.
 *
 * Use this for any layout with a column in it. JavaScript offers `String.length` (UTF-16 units) and
 * `Array.from(s).length` (code points), and both are wrong for the text a model produces: `"日本"`
 * is 2 code points and **4** columns, `"👋🏽"` is 2 code points and **2**, `"é"` may be 2 code points
 * and **1**. Padding with either draws a ragged rule the first time a CJK model name appears.
 *
 * Synchronous — it is an op, not a host call, so calling it per row costs nothing. It uses the same
 * measurement the renderer does, so a plugin and the frontend agree by construction.
 */
export function width(text: string): number {
  return Deno.core.ops.op_neosh_width(text);
}

/** Truncate to at most `columns` columns, cutting on a grapheme boundary rather than mid-character. */
export function clipToWidth(text: string, columns: number): string {
  return Deno.core.ops.op_neosh_clip(text, Math.max(0, Math.floor(columns)));
}

/** Pad on the right to exactly `columns`, clipping if it is already wider. */
export function padToWidth(text: string, columns: number): string {
  const clipped = clipToWidth(text, columns);
  return clipped + " ".repeat(Math.max(0, columns - width(clipped)));
}

/** Thrown when the host refuses a call. Carries the structured reason. */
export class NeoshError extends Error {
  constructor(readonly cause_: ApiError) {
    super(describeError(cause_));
    this.name = "NeoshError";
  }
}

function describeError(e: ApiError): string {
  switch (e.kind) {
    case "not_found": return `not found: ${e.what}`;
    case "invalid_argument": return `invalid argument: ${e.message}`;
    case "denied": return `denied: ${e.reason}`;
    case "not_permitted": return `not permitted: ${e.capability}`;
    case "busy": return `busy: ${e.message}`;
    case "internal": return `internal error: ${e.message}`;
  }
}

let seq = 0;
const pending = new Map<string, { resolve: (v: ApiOk) => void; reject: (e: unknown) => void }>();

function send(msg: unknown): void {
  Deno.core.ops.op_neosh_send(msg);
}

/**
 * Issue a call that wants an answer.
 *
 * Calls from one plugin are applied by the host in the order they were issued, so a caller that
 * fires several mutations without awaiting cannot race itself.
 */
function call(plugin: string, c: ApiCall): Promise<ApiOk> {
  const id = `${plugin}#${++seq}`;
  return new Promise<ApiOk>((resolve, reject) => {
    pending.set(id, { resolve, reject });
    send({ type: "plugin", plugin, msg: { type: "call", id, call: c } });
  });
}

/** Fire-and-forget. Used on the streaming path so appending a token is not round-trip bound. */
function notify(plugin: string, c: ApiCall): void {
  send({ type: "plugin", plugin, msg: { type: "notify", call: c } });
}

function settle(id: string, response: ApiResponse): void {
  const p = pending.get(id);
  if (!p) return;
  pending.delete(id);
  if (response.status === "ok") p.resolve(response.value);
  else p.reject(new NeoshError(response.error));
}

/** Narrow an `ApiOk` to the variant a call is documented to return. */
function expect<K extends ApiOk["ok"]>(v: ApiOk, kind: K): Extract<ApiOk, { ok: K }> {
  if (v.ok !== kind) {
    throw new Error(`host returned ${v.ok} where ${kind} was expected`);
  }
  return v as Extract<ApiOk, { ok: K }>;
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

export interface Disposable {
  dispose(): void;
}

export interface PluginContext {
  readonly neosh: Neosh;
  readonly pluginId: string;
  readonly config: unknown;
  /** Anything pushed here is disposed when the plugin unloads. */
  readonly subscriptions: Disposable[];
}

/** A plugin's entry module exports this. */
export type Activate = (ctx: PluginContext) => void | Promise<void>;

export interface VirtText {
  text: string;
  hlGroup?: string;
}

export interface MarkOptions {
  endCol?: number;
  hlGroup?: string;
  virtText?: VirtText[];
  virtTextPos?: ExtmarkOpts["virt_text_pos"];
  onDelete?: ExtmarkOpts["on_delete"];
  priority?: number;
}

export interface FloatOptions {
  anchor?: FloatConfig["anchor"];
  offset?: { row: number; col: number };
  width?: FloatConfig["width"];
  height?: FloatConfig["height"];
  z?: number;
  border?: FloatConfig["border"];
  borderHl?: string;
  title?: string;
  closeOnBlur?: boolean;
  focusable?: boolean;
}

export interface Neosh {
  readonly version: number;
  readonly buf: BufferApi;
  readonly win: WindowApi;
  readonly float: FloatApi;
  readonly edit: EditApi;
  readonly ns: NamespaceApi;
  readonly hl: HighlightApi;
  readonly ui: RawCellApi;
  readonly cmd: CommandApi;
  readonly keymap: KeymapApi;
  readonly focus: FocusApi;
  readonly agent: AgentApi;
  readonly tool: ToolApi;
  readonly hook: HookApi;
  readonly provider: ProviderApi;
  readonly git: GitApi;
  readonly gen: GenApi;
  readonly session: SessionApi;
  readonly status: StatusApi;
  readonly hint: HintApi;
  readonly opt: OptionApi;
  readonly state: StateApi;
  readonly rtp: RuntimePathApi;
  readonly path: PathApi;
  readonly timer: TimerApi;
  readonly log: Logger;
  /** Transient UI message. Never enters conversation history. */
  notify(message: string, level?: MessageLevel): void;
  /** Ask the host whether a side effect is allowed. */
  permit(capability: Capability): Promise<PermissionDecision>;
}

export interface BufferApi {
  create(opts?: { name?: string; scratch?: boolean }): Promise<BufferId>;
  lineCount(buf: BufferId): Promise<number>;
  /**
   * `end` is exclusive. Negative indices are `len + 1 + i`, as in Neovim's `nvim_buf_set_lines`:
   * **`-1` is one past the last line**, so `getLines(buf, 0, -1)` is the whole buffer. To address
   * the last line itself, use `-2, -1`.
   */
  getLines(buf: BufferId, start: number, end: number): Promise<string[]>;
  /**
   * Range replacement. There is no whole-document write, by design — streaming a response must not
   * resend the document once per token.
   *
   * `setLines(buf, 0, -1, lines)` replaces the buffer; `setLines(buf, -1, -1, lines)` appends.
   */
  setLines(buf: BufferId, start: number, end: number, lines: string[]): Promise<void>;
  /** Append to the final line without resending it. The streaming fast path. */
  appendText(buf: BufferId, text: string): Promise<void>;
  setName(buf: BufferId, name: string): Promise<void>;
  onChange(
    buf: BufferId,
    cb: (e: { buf: BufferId; start: number; oldEnd: number; newEnd: number }) => void,
  ): Promise<Disposable>;
}

export interface WindowApi {
  /**
   * `gravity` is which end short content settles against: `"start"` (the default) pins it to the
   * top, `"end"` to the bottom, which is what makes a transcript read as a conversation rather
   * than as a document that happens to be in a window.
   */
  open(
    buf: BufferId,
    dock: Dock,
    opts?: { size?: number; gravity?: Gravity },
  ): Promise<WindowId>;
  close(win: WindowId): Promise<void>;
  setBuf(win: WindowId, buf: BufferId): Promise<void>;
  /** `col` is a UTF-8 byte offset, not a character or display column. */
  cursor(win: WindowId): Promise<{ row: number; col: number }>;
  setCursor(win: WindowId, row: number, col: number): Promise<void>;
  scrollTo(win: WindowId, topLine: number): Promise<void>;
  /**
   * How big this window actually is, in cells.
   *
   * `null` until the frontend has drawn it once. This is the only way to learn real geometry:
   * everything about display width is resolved by the frontend, so a plugin sizing a meter or
   * deciding what to drop at 60 columns asks rather than computing an answer it cannot compute
   * correctly.
   */
  viewport(win: WindowId): Promise<Viewport | null>;
}

/**
 * Moving and editing text where a window's cursor is.
 *
 * Verbs rather than positions, because grapheme and word boundaries are genuinely hard and nobody
 * should have to get them right twice. A text field in your plugin behaves the same way the
 * composer does because they are asking the same question, not because they each reimplemented it.
 */
export interface EditApi {
  /**
   * Move the cursor. `select` extends a selection from wherever it was anchored — anchoring first
   * if nothing was — which is shift-and-arrow. Without it the selection is dropped.
   */
  move(win: WindowId, motion: CursorMotion, opts?: { select?: boolean }): Promise<void>;
  /** Edit at the cursor. Typing over a selection replaces it, as everywhere else. */
  apply(win: WindowId, edit: TextEdit): Promise<void>;
  /** Anchor a selection where the cursor is, or drop the one there is. */
  select(win: WindowId, on: boolean): Promise<void>;
  /** What is selected. `""` when nothing is. */
  selection(win: WindowId): Promise<string>;
  /**
   * Put text on the system clipboard.
   *
   * A capability rather than something you could do yourself: the runtime has no terminal, and the
   * frontend is the only thing holding the stream this has to travel down. Over SSH it reaches the
   * terminal you are sitting at, which a clipboard library on the remote host would not.
   */
  copy(text: string): Promise<void>;
}

export interface FloatApi {
  open(buf: BufferId, opts?: FloatOptions): Promise<WindowId>;
  configure(win: WindowId, opts?: FloatOptions): Promise<void>;
  close(win: WindowId): Promise<void>;
}

export interface NamespaceApi {
  create(name: string): Promise<NamespaceId>;
  /** `col` is a UTF-8 byte offset. */
  mark(ns: NamespaceId, buf: BufferId, row: number, col: number, opts?: MarkOptions): Promise<ExtmarkId>;
  getMark(ns: NamespaceId, buf: BufferId, id: ExtmarkId): Promise<ExtmarkInfo | null>;
  allMarks(ns: NamespaceId, buf: BufferId): Promise<ExtmarkInfo[]>;
  delMark(ns: NamespaceId, buf: BufferId, id: ExtmarkId): Promise<void>;
  clear(ns: NamespaceId, buf: BufferId, start?: number, end?: number): Promise<void>;
}

export interface HighlightApi {
  /** Declare a semantic group. Prefer `link` so an unknown theme still looks right. */
  define(name: string, def: { link: string } | HighlightSpec): Promise<void>;
}

export interface RawCellApi {
  claim(win: WindowId, rect: Rect): Promise<SurfaceId>;
  put(surface: SurfaceId, cells: SurfaceCell[]): Promise<void>;
  release(surface: SurfaceId): Promise<void>;
}

export interface CommandApi {
  register(name: string, fn: (args: string[], key?: KeyContext) => void | Promise<void>, opts?: { desc?: string }): Promise<Disposable>;
  exec(name: string, args?: string[]): Promise<void>;
  list(): Promise<CommandEntry[]>;
}

export interface KeymapApi {
  /**
   * Bind a key to a *command name*, never to a callback.
   *
   * That indirection is what makes every binding listable and remappable by the user, and it lets
   * the host resolve routing without calling into a plugin.
   */
  set(mode: Mode, lhs: string, command: string, opts?: { scope?: KeymapScope; desc?: string }): Promise<void>;
  del(mode: Mode, lhs: string, scope?: KeymapScope): Promise<void>;
  list(mode?: Mode): Promise<KeymapEntry[]>;
  /**
   * While `win` is focused, receive every key the keymaps did not claim.
   *
   * For widgets that need raw input: a filter box, a text field, a modal list. Bindings still win,
   * so `<C-q>` keeps quitting while your picker is open — you get what nothing else wanted. The
   * command is invoked with a `KeyContext`, so one handler can switch on the key.
   *
   * Dispose to release. A capture is also dropped when the window closes or your plugin unloads,
   * so a crash cannot leave the keyboard pointing at nothing.
   */
  capture(win: WindowId, command: string): Promise<Disposable>;
}

export interface FocusApi {
  push(win: WindowId): Promise<void>;
  pop(): Promise<void>;
  current(): Promise<WindowId | null>;
}

export interface AgentApi {
  send(text: string): Promise<void>;
  cancel(): Promise<void>;
  selection(): Promise<ModelSelection | null>;
  /** Hot-swap the model. Takes effect on the next turn. */
  setSelection(selection: ModelSelection): Promise<void>;
  /**
   * Every reachable model, each paired with the instance that serves it.
   *
   * The pairing matters: a model id is unique per instance, not globally, so a picker that dropped
   * it would have to guess the owner — and guessing wrong sends the conversation to a different
   * endpoint with no visible error.
   *
   * Answers from a session cache. Discovery is a network round trip per configured provider, so
   * pass `refresh` only when you have reason to believe a lineup changed.
   */
  listModels(instance?: string, opts?: { refresh?: boolean }): Promise<ModelEntry[]>;
  listInstances(): Promise<InstanceConfig[]>;
  /**
   * Where each configured provider's key comes from — and never what it is.
   *
   * There is deliberately no call that returns a secret. A plugin can find out that `anthropic` is
   * authenticated from the keychain, and can ask the host to collect a new key; it cannot read one,
   * so it cannot leak one.
   */
  credentials(): Promise<CredentialInfo[]>;
  /**
   * Ask the host to collect an API key for `instance` from the keyboard.
   *
   * The host runs this prompt itself: a plugin one would have to put the key in a buffer, and a
   * buffer is drawn — the value would cross the frontend boundary and land in whatever it logs.
   * What comes back is whether a key was stored, never the key.
   *
   * Rejects when the instance signs in on its own (a CLI login) or needs no key at all, and when it
   * already has one unless you pass `replace`.
   */
  setCredential(instance: string, opts?: { replace?: boolean }): Promise<boolean>;
  /** Drop a stored key from memory and from the keychain. The environment is not ours to clear. */
  forgetCredential(instance: string): Promise<void>;
  onTurnStart(cb: (e: { turn: string }) => void): Disposable;
  /** One streamed chunk of assistant text. Chunks are provider-sized, not characters. */
  onToken(cb: (e: { turn: string; text: string }) => void): Disposable;
  onThinking(cb: (e: { turn: string; text: string }) => void): Disposable;
  onTurnEnd(cb: (e: { turn: string; stopReason: StopReason; usage: Usage }) => void): Disposable;
  /**
   * A tool is about to run.
   *
   * Distinct from the `tool_pre` hook: that one is asked *whether* the call may proceed and can
   * veto it. This is told that it is happening, cannot influence it, and is therefore what a
   * transcript wants.
   */
  onToolStart(cb: (e: { turn: string; call: ToolCall }) => void): Disposable;
  onToolEnd(cb: (e: { turn: string; call: ToolCall; result: ToolResult }) => void): Disposable;
}

export interface ToolApi {
  /** Lands in the same namespace and shape as a built-in or MCP tool. */
  register(
    def: { name: string; description: string; inputSchema: Record<string, unknown> },
    handler: (input: unknown) => ToolResult | Promise<ToolResult>,
  ): Promise<Disposable>;
  list(): Promise<ToolDef[]>;
}

export interface HookApi {
  /**
   * Register a hook.
   *
   * `blocking: false` (the default) is a pure observer whose return value is ignored — that is what
   * stops an audit plugin from wedging the agent loop. A blocking hook is awaited and may veto;
   * **a blocking hook that does not answer in time is treated as a veto**, so a policy plugin fails
   * closed.
   */
  register(
    hook: HookName,
    fn: (payload: HookPayload) => HookOutcome | Promise<HookOutcome>,
    opts?: { blocking?: boolean; timeoutMs?: number },
  ): Promise<Disposable>;
}

export interface ProviderApi {
  /**
   * Register a model provider implemented in this plugin.
   *
   * Supporting another vendor is a plugin, not a core change. Because a stream cannot cross the
   * RPC boundary as a return value, `handler` receives an `emit` callback and pushes events until
   * it emits `message_stop`.
   */
  register(
    driver: string,
    instances: InstanceConfig[],
    handler: (req: TurnRequest, emit: (e: ProviderEvent) => void, signal: { cancelled: boolean }) => void | Promise<void>,
  ): Promise<Disposable>;
}

/**
 * Version control.
 *
 * The plugin runtime has no process access, so `git` is a host capability rather than something a
 * plugin shells out to. One implementation means a sidebar, a branch picker and a commit UI agree
 * about what "dirty" means instead of each parsing porcelain slightly differently.
 *
 * Reads are free. Writes go through the permission layer as `exec` of `git <verb>`, so
 * `permissions.allow_commands = ["git"]` covers them and a policy hook watching exec sees them.
 *
 * Every call rejects with `not_found` when neosh was started outside a repository — check once with
 * `status()` rather than guarding each call.
 */
export interface GitApi {
  status(): Promise<RepoStatus>;
  /** Local branches, most recently committed first. */
  branches(opts?: { includeRemote?: boolean }): Promise<BranchInfo[]>;
  worktrees(): Promise<WorktreeInfo[]>;
  log(limit?: number): Promise<CommitInfo[]>;
  /** The patch. Pass `stat` for `--stat`, which is what a prompt wants. */
  diff(target?: DiffTarget, opts?: { stat?: boolean }): Promise<string>;
  /** What this branch would merge into: `origin/HEAD`, else `main`/`master`. */
  defaultBranch(): Promise<string | null>;
  createBranch(name: string, opts?: { from?: string }): Promise<void>;
  checkout(rev: string): Promise<void>;
  /** Empty `paths` stages everything, like `git add .` from the repository root. */
  stage(paths?: string[]): Promise<void>;
  unstage(paths?: string[]): Promise<void>;
  commit(message: string): Promise<CommitInfo>;
  addWorktree(path: string, branch: string, opts?: { create?: boolean }): Promise<void>;
  removeWorktree(path: string, opts?: { force?: boolean }): Promise<void>;
}

/**
 * One-shot generation: a prompt through a model, outside the conversation.
 *
 * Branch names, commit messages, thread titles and PR descriptions are all this call. It is
 * deliberately *not* `agent.send` — nothing here enters session history, so asking for a commit
 * message does not change what the agent believes it was asked to do.
 *
 * The model is `gen.model` when set, else the conversation's own. Point that option at something
 * cheap; naming a branch does not need a frontier model.
 */
export interface GenApi {
  complete(prompt: string, opts?: { system?: string; selection?: ModelSelection }): Promise<string>;
  /**
   * Same, but parse the answer as JSON.
   *
   * The host tolerates what models actually return — code fences, a "Sure!" preamble — so callers
   * do not each reimplement that. Rejects if there is no JSON in the response at all.
   */
  json<T = unknown>(prompt: string, opts?: { system?: string; selection?: ModelSelection }): Promise<T>;
}

/**
 * Conversations.
 *
 * A workspace is not one conversation: a branch you are on, a review you are half-way through, a
 * question from yesterday. These are the verbs a thread list needs; what it looks like is yours.
 *
 * Conversations are saved to the state directory as you go and restored at startup, so switching
 * away from one is not a way to lose it.
 */
export interface SessionApi {
  /**
   * Most recently active first, with exactly one flagged `is_active`.
   *
   * Archived conversations are left out unless you ask for them. That is what archiving is for, and
   * a list that included them by default would make every caller responsible for remembering.
   */
  list(opts?: { includeArchived?: boolean }): Promise<SessionInfo[]>;
  current(): Promise<SessionInfo>;
  /**
   * Start one. It inherits the model and system prompt you are using — starting a conversation
   * should not silently change what you are talking to.
   *
   * `cwd` opens it against another checkout, which is how a second project becomes visible.
   */
  create(opts?: { cwd?: string; title?: string; activate?: boolean }): Promise<SessionInfo>;
  /** Rejects with `busy` while a turn is streaming: its output would land in the wrong transcript. */
  switch(session: SessionId): Promise<void>;
  /**
   * Close one. Closing the active conversation moves to the most recently used other; closing the
   * last one is an error, because there is always somewhere for the next thing you type.
   */
  close(session: SessionId): Promise<void>;
  /** Pass `null` to clear a title and go back to the first-message label. */
  rename(session: SessionId, title: string | null): Promise<void>;
  /**
   * Put a conversation away, or bring it back.
   *
   * Not `close`: nothing is deleted, every message survives, and `list({ includeArchived: true })`
   * still finds it. Archiving the active conversation moves you to the most recently used other
   * one — or to a fresh empty one if there is no other.
   */
  archive(session: SessionId, archived?: boolean): Promise<void>;
  /** The conversation itself, for a transcript view that renders rather than replays. */
  messages(session?: SessionId): Promise<Message[]>;
  /** Fires whenever the active conversation changes — switched, created, or closed. */
  onChange(cb: (e: { session: SessionId }) => void): Disposable;
}

/**
 * The status line — the composer footer.
 *
 * The host owns the strip: one line, always visible, never scrolled. Plugins own what is in it.
 * That split is what lets the model switcher and the git plugin each put something there without
 * either knowing the other exists.
 *
 * Setting the same key again replaces that segment, so updating a meter every tick does not need a
 * clear first and cannot leave two. Segments are namespaced per plugin, so two plugins choosing
 * `"model"` cannot collide, and unloading a plugin takes its segments with it.
 */
/**
 * The shortcut row under the composer.
 *
 * Whoever owns a feature owns its hint, which is the only arrangement that stays true: the row is
 * built from what is actually registered right now, so a plugin that is switched off takes its
 * shortcut with it rather than leaving a key advertised that no longer does anything.
 *
 * Write the key the way the user would press it — `^P`, `⇧⏎`, `F1` — not the way a keymap spells
 * it. Hints are dropped from the end when the terminal is too narrow, so put the one you would
 * most want seen at the lowest priority.
 */
export interface HintApi {
  set(key: string, hint: { keys: string; label: string; priority?: number }): Promise<void>;
  clear(key: string): Promise<void>;
}

export interface StatusApi {
  set(key: string, segment: { text: string; hl?: string; align?: StatusAlign; priority?: number }): Promise<void>;
  clear(key: string): Promise<void>;
}

export interface OptionApi {
  /**
   * Declare an option this plugin owns.
   *
   * neosh's own settings are declared through this same call at startup, so there is nothing the
   * built-in options can do that yours cannot — including being set from `config.toml` and shown
   * by a settings UI that has never heard of your plugin.
   *
   * Names are dot-separated lowercase. Namespace yours under your plugin id.
   */
  declare(spec: OptionSpec): Promise<Disposable>;
  /** Typed read. Rejects if the option was never declared. */
  get<T = OptionValue>(name: string): Promise<T>;
  /** Full entry, including type, default and owner — or `null` if undeclared. */
  entry(name: string): Promise<OptionEntry | null>;
  /**
   * Set a declared option. Rejects on an unknown name or a value that does not match the declared
   * type, rather than quietly doing nothing.
   */
  set(name: string, value: OptionValue): Promise<void>;
  /** Restore the declared default. */
  reset(name: string): Promise<void>;
  all(): Promise<OptionEntry[]>;
  /** Fires for every option, not just your own: a setting is shared state. */
  onChange(cb: (e: { name: string; value: OptionValue }) => void): Disposable;
}

/**
 * What your plugin remembers between runs.
 *
 * Small facts a panel needs so it is still arranged the way you left it: which projects are pinned,
 * what order you dragged them into, which sections are folded. Deliberately *not* options — an
 * option is configuration the user writes, and a plugin that rewrites someone's config file because
 * they pressed a key is one they stop trusting with the file.
 *
 * Keyed by your plugin id, which the host knows and you cannot forge, so no other plugin can read
 * or clobber what you store. It is plain JSON on disk: **nothing secret belongs here.**
 */
export interface StateApi {
  /**
   * `null` when nothing was stored — indistinguishable from having stored `null`, which no caller
   * has ever needed to tell apart.
   */
  get<T = unknown>(key: string): Promise<T | null>;
  set(key: string, value: unknown): Promise<void>;
  remove(key: string): Promise<void>;
}

export interface RuntimePathApi {
  /**
   * Add a directory to search for plugins.
   *
   * Only effective before discovery runs, which means: from `init.ts`. That is the point — your
   * config is the thing that decides what else loads, exactly as `init.lua` is.
   */
  add(path: string): Promise<void>;
  list(): Promise<string[]>;
}

/**
 * Completing a typed path.
 *
 * The runtime has no filesystem — deliberately — and a path field without completion is a path
 * field you type wrong. Narrow on purpose: directory names, one level, never file contents and
 * never a recursive walk.
 */
export interface PathApi {
  /**
   * Directories whose path begins with `prefix`, each with a trailing `/` so the answer can go
   * straight back into the field and be completed again.
   *
   * `~` expands against the home directory. A prefix with no `/` completes against the active
   * conversation's directory, which is what someone typing `src` means.
   */
  complete(prefix: string): Promise<string[]>;
}

export interface TimerApi {
  /**
   * Run `fn` once, no sooner than `ms` from now.
   *
   * The returned `Disposable` cancels it, and **every timer is cancelled when your plugin
   * unloads** — which the global `setTimeout` cannot promise, because it has no idea who called it.
   * Prefer this one; reach for the global only when porting code that expects it.
   */
  after(ms: number, fn: () => void): Disposable;
  /** Run `fn` repeatedly until disposed. */
  every(ms: number, fn: () => void): Disposable;
  /**
   * Coalesce bursts: calling the returned function repeatedly runs `fn` once, `ms` after the last
   * call. The shape almost every "re-render after the tokens stop" wants.
   */
  debounce<A extends unknown[]>(ms: number, fn: (...args: A) => void): ((...args: A) => void) & Disposable;
}

export interface Logger {
  info(msg: string): void;
  warn(msg: string): void;
  error(msg: string): void;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

interface Registered {
  commands: Map<string, (args: string[], key?: KeyContext) => unknown>;
  tools: Map<string, (input: unknown) => ToolResult | Promise<ToolResult>>;
  hooks: Map<HookName, (p: HookPayload) => HookOutcome | Promise<HookOutcome>>;
  providers: Map<string, (req: TurnRequest, emit: (e: ProviderEvent) => void, signal: { cancelled: boolean }) => unknown>;
  bufferListeners: Map<number, Array<(e: { buf: BufferId; start: number; oldEnd: number; newEnd: number }) => void>>;
  optionListeners: Array<(e: { name: string; value: OptionValue }) => void>;
  sessionListeners: Array<(e: { session: SessionId }) => void>;
  options: Set<string>;
  /// Timer handles this plugin armed, cleared on unload.
  timers: Set<number>;
  agentListeners: { [K in "turnStart" | "token" | "thinking" | "turnEnd" | "toolStart" | "toolEnd"]: Array<(e: never) => void> };
  streams: Map<string, { cancelled: boolean }>;
  subscriptions: Disposable[];
}

const plugins = new Map<string, Registered>();

function reg(plugin: string): Registered {
  let r = plugins.get(plugin);
  if (!r) {
    r = {
      commands: new Map(),
      tools: new Map(),
      hooks: new Map(),
      providers: new Map(),
      bufferListeners: new Map(),
      optionListeners: [],
      sessionListeners: [],
      options: new Set(),
      timers: new Set(),
      agentListeners: { turnStart: [], token: [], thinking: [], turnEnd: [], toolStart: [], toolEnd: [] },
      streams: new Map(),
      subscriptions: [],
    };
    plugins.set(plugin, r);
  }
  return r;
}

function listener<T>(list: Array<(e: T) => void>, cb: (e: T) => void): Disposable {
  list.push(cb as (e: never) => void as (e: T) => void);
  return {
    dispose() {
      const i = list.indexOf(cb);
      if (i >= 0) list.splice(i, 1);
    },
  };
}

function floatConfig(o: FloatOptions = {}): FloatConfig {
  return {
    anchor: o.anchor ?? { kind: "screen" },
    offset: o.offset ?? { row: 0, col: 0 },
    width: o.width ?? { kind: "auto" },
    height: o.height ?? { kind: "auto" },
    z: o.z ?? 100,
    border: o.border ?? "rounded",
    border_hl: o.borderHl ?? null,
    title: o.title ?? null,
    close_on_blur: o.closeOnBlur ?? false,
    focusable: o.focusable ?? true,
  };
}

function markOpts(o: MarkOptions = {}): ExtmarkOpts {
  return {
    end_col: o.endCol ?? null,
    hl_group: o.hlGroup ?? null,
    virt_text: (o.virtText ?? []).map((v) => ({ text: v.text, hl_group: v.hlGroup ?? null })),
    virt_text_pos: o.virtTextPos ?? "eol",
    on_delete: o.onDelete ?? "clamp",
    priority: o.priority ?? 0,
  };
}

/** Build the API object handed to one plugin. Internal; the host calls this. */
export function __createContext(plugin: string, config: unknown, version: number): PluginContext {
  const r = reg(plugin);
  const c = (x: ApiCall) => call(plugin, x);
  const n = (x: ApiCall) => notify(plugin, x);

  const api: Neosh = {
    version,
    notify(message, level) {
      n({ call: "notify", level: level ?? "info", message });
    },
    async permit(capability) {
      return expect(await c({ call: "permission_check", capability }), "permission").decision;
    },
    opt: {
      async declare(spec) {
        await c({ call: "opt_declare", spec });
        r.options.add(spec.name);
        return { dispose: () => r.options.delete(spec.name) };
      },
      async get<T = OptionValue>(name: string): Promise<T> {
        const entry = expect(await c({ call: "opt_get", name }), "option").entry;
        if (!entry) throw new Error(`option ${name} is not declared`);
        return entry.value as T;
      },
      async entry(name) {
        return expect(await c({ call: "opt_get", name }), "option").entry;
      },
      async set(name, value) {
        await c({ call: "opt_set", name, value });
      },
      async reset(name) {
        await c({ call: "opt_reset", name });
      },
      async all() {
        return expect(await c({ call: "opt_all" }), "options").options;
      },
      onChange(cb) {
        return listener(r.optionListeners, cb);
      },
    },
    edit: {
      async move(win, motion, opts) {
        await c({ call: "win_motion", win, motion, select: opts?.select ?? false });
      },
      async apply(win, edit) {
        await c({ call: "win_edit", win, edit });
      },
      async select(win, on) {
        await c({ call: "win_select", win, on });
      },
      async selection(win) {
        return expect(await c({ call: "win_selection", win }), "text").text;
      },
      async copy(text) {
        await c({ call: "clipboard_write", text });
      },
    },
    path: {
      async complete(prefix) {
        return expect(await c({ call: "path_complete", prefix }), "paths").paths;
      },
    },
    state: {
      async get<T = unknown>(key: string): Promise<T | null> {
        const value = expect(await c({ call: "state_get", key }), "json").value;
        return (value ?? null) as T | null;
      },
      async set(key, value) {
        await c({ call: "state_set", key, value });
      },
      async remove(key) {
        await c({ call: "state_delete", key });
      },
    },
    timer: {
      after(ms, fn) {
        const id = setTimeout(() => {
          r.timers.delete(id);
          fn();
        }, ms);
        r.timers.add(id);
        return {
          dispose() {
            clearTimeout(id);
            r.timers.delete(id);
          },
        };
      },
      every(ms, fn) {
        const id = setInterval(fn, ms);
        r.timers.add(id);
        return {
          dispose() {
            clearInterval(id);
            r.timers.delete(id);
          },
        };
      },
      debounce<A extends unknown[]>(ms: number, fn: (...args: A) => void) {
        let pending: number | undefined;
        const run = (...args: A) => {
          if (pending !== undefined) {
            clearTimeout(pending);
            r.timers.delete(pending);
          }
          const id = setTimeout(() => {
            pending = undefined;
            // Prune here too, not only on the re-arm and dispose paths: a debounce that settles is
            // the *normal* outcome, and leaving its handle behind grows the set once per burst for
            // the life of the plugin.
            r.timers.delete(id);
            fn(...args);
          }, ms);
          pending = id;
          r.timers.add(id);
        };
        run.dispose = () => {
          if (pending !== undefined) {
            clearTimeout(pending);
            r.timers.delete(pending);
            pending = undefined;
          }
        };
        return run;
      },
    },
    rtp: {
      async add(path) {
        await c({ call: "rtp_add", path });
      },
      async list() {
        return expect(await c({ call: "rtp_list" }), "paths").paths;
      },
    },
    log: {
      info: (message) => n({ call: "log", level: "info", message }),
      warn: (message) => n({ call: "log", level: "warn", message }),
      error: (message) => n({ call: "log", level: "error", message }),
    },
    buf: {
      async create(opts) {
        return expect(await c({ call: "buf_create", name: opts?.name ?? null, scratch: opts?.scratch ?? false }), "buf").buf;
      },
      async lineCount(buf) {
        return expect(await c({ call: "buf_line_count", buf }), "count").n;
      },
      async getLines(buf, start, end) {
        return expect(await c({ call: "buf_get_lines", buf, start, end }), "lines").lines;
      },
      async setLines(buf, start, end, lines) {
        await c({ call: "buf_set_lines", buf, start, end, lines });
      },
      async appendText(buf, text) {
        await c({ call: "buf_append_text", buf, text });
      },
      async setName(buf, name) {
        await c({ call: "buf_set_name", buf, name });
      },
      async onChange(buf, cb) {
        await c({ call: "buf_attach", buf });
        const list = r.bufferListeners.get(buf) ?? [];
        r.bufferListeners.set(buf, list);
        return listener(list, cb);
      },
    },
    win: {
      async open(buf, dock, opts) {
        const layout: WindowLayout = {
          kind: "docked",
          dock,
          size: opts?.size ?? null,
          gravity: opts?.gravity ?? "start",
        };
        return expect(await c({ call: "win_open", buf, layout }), "win").win;
      },
      async close(win) {
        await c({ call: "win_close", win });
      },
      async setBuf(win, buf) {
        await c({ call: "win_set_buf", win, buf });
      },
      async cursor(win) {
        const v = expect(await c({ call: "win_get_cursor", win }), "cursor");
        return { row: v.row, col: v.col };
      },
      async setCursor(win, row, col) {
        await c({ call: "win_set_cursor", win, row, col });
      },
      async scrollTo(win, topLine) {
        await c({ call: "win_scroll_to", win, top_line: topLine });
      },
      async viewport(win) {
        return expect(await c({ call: "win_get_viewport", win }), "viewport").viewport ?? null;
      },
    },
    float: {
      async open(buf, opts) {
        return expect(await c({ call: "float_open", buf, config: floatConfig(opts) }), "win").win;
      },
      async configure(win, opts) {
        await c({ call: "float_configure", win, config: floatConfig(opts) });
      },
      async close(win) {
        await c({ call: "win_close", win });
      },
    },
    ns: {
      async create(name) {
        return expect(await c({ call: "ns_create", name }), "ns").ns;
      },
      async mark(ns, buf, row, col, opts) {
        return expect(await c({ call: "mark_set", ns, buf, row, col, opts: markOpts(opts) }), "mark").id;
      },
      async getMark(ns, buf, id) {
        return expect(await c({ call: "mark_get", ns, buf, id }), "mark_info").info;
      },
      async allMarks(ns, buf) {
        return expect(await c({ call: "mark_all", ns, buf }), "marks").marks;
      },
      async delMark(ns, buf, id) {
        await c({ call: "mark_del", ns, buf, id });
      },
      async clear(ns, buf, start, end) {
        await c({ call: "mark_clear", ns, buf, start: start ?? null, end: end ?? null });
      },
    },
    hl: {
      async define(name, def) {
        const d: HighlightDef =
          "link" in def ? { kind: "link", to: def.link } : { kind: "spec", spec: def };
        await c({ call: "hl_define", name, def: d });
      },
    },
    ui: {
      async claim(win, rect) {
        return expect(await c({ call: "surface_claim", win, rect }), "surface").surface;
      },
      async put(surface, cells) {
        await c({ call: "surface_put", surface, cells });
      },
      async release(surface) {
        await c({ call: "surface_release", surface });
      },
    },
    cmd: {
      async register(name, fn, opts) {
        await c({ call: "cmd_register", name, desc: opts?.desc ?? null });
        r.commands.set(name, fn);
        return {
          dispose: () => {
            r.commands.delete(name);
            n({ call: "cmd_unregister", name });
          },
        };
      },
      async exec(name, args) {
        await c({ call: "cmd_exec", name, args: args ?? [] });
      },
      async list() {
        return expect(await c({ call: "cmd_list" }), "commands").commands;
      },
    },
    keymap: {
      async set(mode, lhs, command, opts) {
        await c({ call: "keymap_set", mode, lhs, command, scope: opts?.scope ?? null, desc: opts?.desc ?? null });
      },
      async del(mode, lhs, scope) {
        await c({ call: "keymap_del", mode, lhs, scope: scope ?? null });
      },
      async list(mode) {
        return expect(await c({ call: "keymap_list", mode: mode ?? null }), "keymaps").keymaps;
      },
      async capture(win, command) {
        await c({ call: "keymap_capture", win, command });
        return { dispose: () => n({ call: "keymap_release", win }) };
      },
    },
    focus: {
      async push(win) {
        await c({ call: "focus_push", win });
      },
      async pop() {
        await c({ call: "focus_pop" });
      },
      async current() {
        return expect(await c({ call: "focus_current" }), "focused_win").win;
      },
    },
    agent: {
      async send(text) {
        await c({ call: "agent_send", text });
      },
      async cancel() {
        await c({ call: "agent_cancel" });
      },
      async selection() {
        return expect(await c({ call: "agent_get_selection" }), "selection").selection;
      },
      async setSelection(selection) {
        await c({ call: "agent_set_selection", selection });
      },
      async listModels(instance, opts) {
        const v = await c({
          call: "agent_list_models",
          instance: instance ?? null,
          refresh: opts?.refresh ?? false,
        });
        return expect(v, "models").models;
      },
      async listInstances() {
        return expect(await c({ call: "agent_list_instances" }), "instances").instances;
      },
      async credentials() {
        return expect(await c({ call: "provider_credentials" }), "credentials").credentials;
      },
      async setCredential(instance, opts) {
        // Does not settle until the prompt closes — the answer is what the user did.
        const v = await c({
          call: "provider_set_credential",
          instance,
          replace: opts?.replace ?? false,
        });
        return expect(v, "bool").value;
      },
      async forgetCredential(instance) {
        await c({ call: "provider_forget_credential", instance });
      },
      onTurnStart: (cb) => listener(r.agentListeners.turnStart as Array<(e: { turn: string }) => void>, cb),
      onToken: (cb) => listener(r.agentListeners.token as Array<(e: { turn: string; text: string }) => void>, cb),
      onThinking: (cb) => listener(r.agentListeners.thinking as Array<(e: { turn: string; text: string }) => void>, cb),
      onTurnEnd: (cb) =>
        listener(r.agentListeners.turnEnd as Array<(e: { turn: string; stopReason: StopReason; usage: Usage }) => void>, cb),
      onToolStart: (cb) =>
        listener(r.agentListeners.toolStart as Array<(e: { turn: string; call: ToolCall }) => void>, cb),
      onToolEnd: (cb) =>
        listener(r.agentListeners.toolEnd as Array<(e: { turn: string; call: ToolCall; result: ToolResult }) => void>, cb),
    },
    tool: {
      async register(def, handler) {
        await c({
          call: "tool_register",
          def: {
            name: def.name,
            description: def.description,
            input_schema: def.inputSchema,
            source: { kind: "builtin" },
          },
        });
        r.tools.set(def.name, handler);
        return {
          dispose: () => {
            r.tools.delete(def.name);
            n({ call: "tool_unregister", name: def.name });
          },
        };
      },
      async list() {
        return expect(await c({ call: "tool_list" }), "tools").tools;
      },
    },
    hook: {
      async register(hook, fn, opts) {
        const blocking = opts?.blocking ?? false;
        await c({ call: "hook_register", hook, blocking, timeout_ms: opts?.timeoutMs ?? null });
        r.hooks.set(hook, fn);
        return {
          dispose: () => {
            r.hooks.delete(hook);
            n({ call: "hook_unregister", hook });
          },
        };
      },
    },
    git: {
      async status() {
        return expect(await c({ call: "git_status" }), "status").status;
      },
      async branches(opts) {
        const v = await c({ call: "git_branches", include_remote: opts?.includeRemote ?? false });
        return expect(v, "branches").branches;
      },
      async worktrees() {
        return expect(await c({ call: "git_worktrees" }), "worktrees").worktrees;
      },
      async log(limit) {
        return expect(await c({ call: "git_log", limit: limit ?? 20 }), "commits").commits;
      },
      async diff(target, opts) {
        const v = await c({
          call: "git_diff",
          target: target ?? { kind: "unstaged" },
          stat: opts?.stat ?? false,
        });
        return expect(v, "text").text;
      },
      async defaultBranch() {
        return expect(await c({ call: "git_default_branch" }), "maybe_text").text ?? null;
      },
      async createBranch(name, opts) {
        await c({ call: "git_create_branch", name, from: opts?.from ?? null });
      },
      async checkout(rev) {
        await c({ call: "git_checkout", rev });
      },
      async stage(paths) {
        await c({ call: "git_stage", paths: paths ?? [] });
      },
      async unstage(paths) {
        await c({ call: "git_unstage", paths: paths ?? [] });
      },
      async commit(message) {
        return expect(await c({ call: "git_commit", message }), "commit").commit;
      },
      async addWorktree(path, branch, opts) {
        await c({ call: "git_add_worktree", path, branch, create: opts?.create ?? false });
      },
      async removeWorktree(path, opts) {
        await c({ call: "git_remove_worktree", path, force: opts?.force ?? false });
      },
    },
    gen: {
      async complete(prompt, opts) {
        const v = await c({
          call: "gen_complete",
          prompt,
          system: opts?.system ?? null,
          json: false,
          selection: opts?.selection ?? null,
        });
        return expect(v, "text").text;
      },
      async json(prompt, opts) {
        const v = await c({
          call: "gen_complete",
          prompt,
          system: opts?.system ?? null,
          json: true,
          selection: opts?.selection ?? null,
        });
        return expect(v, "json").value as never;
      },
    },
    session: {
      async list(opts) {
        const v = await c({
          call: "session_list",
          include_archived: opts?.includeArchived ?? false,
        });
        return expect(v, "sessions").sessions;
      },
      async current() {
        return expect(await c({ call: "session_current" }), "session").session;
      },
      async create(opts) {
        const v = await c({
          call: "session_new",
          cwd: opts?.cwd ?? null,
          title: opts?.title ?? null,
          activate: opts?.activate ?? true,
        });
        return expect(v, "session").session;
      },
      async switch(session) {
        await c({ call: "session_switch", session });
      },
      async close(session) {
        await c({ call: "session_close", session });
      },
      async rename(session, title) {
        await c({ call: "session_rename", session, title });
      },
      async archive(session, archived) {
        await c({ call: "session_archive", session, archived: archived ?? true });
      },
      async messages(session) {
        const v = await c({ call: "session_messages", session: session ?? null });
        return expect(v, "messages").messages;
      },
      onChange: (cb) => listener(r.sessionListeners, cb),
    },
    hint: {
      async set(key, hint) {
        await c({
          call: "hint_set",
          key,
          hint: { keys: hint.keys, label: hint.label, priority: hint.priority ?? 0 },
        });
      },
      async clear(key) {
        await c({ call: "hint_clear", key });
      },
    },
    status: {
      async set(key, segment) {
        await c({
          call: "status_set",
          key,
          segment: {
            text: segment.text,
            hl: segment.hl ?? null,
            align: segment.align ?? "left",
            priority: segment.priority ?? 0,
          },
        });
      },
      async clear(key) {
        await c({ call: "status_clear", key });
      },
    },
    provider: {
      async register(driver, instances, handler) {
        await c({ call: "provider_register_driver", driver, instances });
        r.providers.set(driver, handler);
        return { dispose: () => r.providers.delete(driver) };
      },
    },
  };

  return { neosh: api, pluginId: plugin, config, subscriptions: r.subscriptions };
}

/** Route one host message. Internal; the host's bootstrap calls this. */
export async function __dispatch(plugin: string, msg: Record<string, unknown>): Promise<void> {
  const r = reg(plugin);

  if (msg.type === "response") {
    settle(msg.id as string, msg.response as ApiResponse);
    return;
  }

  if (msg.type === "request") {
    const id = msg.id as string;
    const req = msg.request as Record<string, unknown>;
    const respond = (response: unknown) =>
      send({ type: "plugin", plugin, msg: { type: "response", id, response } });

    try {
      if (req.type === "run_tool") {
        const h = r.tools.get(req.name as string);
        if (!h) {
          respond({ type: "error", message: `plugin ${plugin} has no tool ${req.name}` });
          return;
        }
        respond({ type: "tool", result: await h(req.input) });
      } else if (req.type === "hook") {
        const h = r.hooks.get(req.hook as HookName);
        // A hook the plugin no longer has must not block the action: continue, do not veto.
        const outcome: HookOutcome = h ? await h(req.payload as HookPayload) : { action: "continue" };
        respond({ type: "hook", outcome });
      } else if (req.type === "provider_stream") {
        const streamId = req.stream as string;
        const tr = req.request as TurnRequest;
        const h = r.providers.get(tr.selection.instance);
        const byDriver = h ?? [...r.providers.values()][0];
        if (!byDriver) {
          respond({ type: "error", message: `plugin ${plugin} has no provider` });
          return;
        }
        const signal = { cancelled: false };
        r.streams.set(streamId, signal);
        // Answer immediately: a stream cannot be a return value across this boundary.
        respond({ type: "provider_accepted" });
        void (async () => {
          try {
            await byDriver(tr, (e) => notify(plugin, { call: "provider_emit", stream: streamId, event: e }), signal);
          } finally {
            r.streams.delete(streamId);
          }
        })();
      } else {
        respond({ type: "error", message: `unknown request ${String(req.type)}` });
      }
    } catch (e) {
      respond({ type: "error", message: e instanceof Error ? e.message : String(e) });
    }
    return;
  }

  if (msg.type === "event") {
    const ev = msg.event as PluginEvent;
    switch (ev.type) {
      case "command_invoked": {
        const h = r.commands.get(ev.name);
        if (h) await h(ev.args ?? [], ev.key ?? undefined);
        break;
      }
      case "buffer_changed": {
        for (const cb of r.bufferListeners.get(ev.buf) ?? []) {
          cb({ buf: ev.buf, start: ev.start, oldEnd: ev.old_end, newEnd: ev.new_end });
        }
        break;
      }
      case "turn_started":
        for (const cb of r.agentListeners.turnStart) (cb as (e: unknown) => void)({ turn: ev.turn });
        break;
      case "token":
        for (const cb of r.agentListeners.token) (cb as (e: unknown) => void)({ turn: ev.turn, text: ev.text });
        break;
      case "thinking_token":
        for (const cb of r.agentListeners.thinking) (cb as (e: unknown) => void)({ turn: ev.turn, text: ev.text });
        break;
      case "turn_ended":
        for (const cb of r.agentListeners.turnEnd)
          (cb as (e: unknown) => void)({ turn: ev.turn, stopReason: ev.stop_reason, usage: ev.usage });
        break;
      case "tool_started":
        for (const cb of r.agentListeners.toolStart)
          (cb as (e: unknown) => void)({ turn: ev.turn, call: ev.call });
        break;
      case "tool_finished":
        for (const cb of r.agentListeners.toolEnd)
          (cb as (e: unknown) => void)({ turn: ev.turn, call: ev.call, result: ev.result });
        break;
      case "hook_observed": {
        const h = r.hooks.get(ev.hook);
        if (h) await h(ev.payload);
        break;
      }
      case "provider_cancel": {
        const s = r.streams.get(ev.stream);
        if (s) s.cancelled = true;
        break;
      }
      case "option_changed":
        for (const cb of r.optionListeners) cb({ name: ev.name, value: ev.value });
        break;
      case "session_changed":
        for (const cb of r.sessionListeners) cb({ session: ev.session });
        break;
      case "focus_changed":
      case "shutdown":
        break;
    }
  }
}

/** Dispose everything a plugin registered. Internal. */
export function __teardown(plugin: string): void {
  const r = plugins.get(plugin);
  if (!r) return;
  // Before the disposers: a timer that fires mid-teardown would call into a plugin that is halfway
  // gone.
  for (const id of r.timers) {
    clearTimeout(id);
  }
  r.timers.clear();
  for (const d of r.subscriptions) {
    try {
      d.dispose();
    } catch {
      // A failing disposer must not stop the rest from running.
    }
  }
  plugins.delete(plugin);
}
