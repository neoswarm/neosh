/**
 * The Phase 0 definition of done, as one plugin.
 *
 * Everything here goes through `@neosh/api`. There is no privileged path, no host patch, and no
 * core change — this file is loaded from disk at startup exactly as a third-party plugin would be.
 *
 * It also writes a running report into its own buffer. That is what the end-to-end test asserts
 * on: the test reads the UI event stream and checks for the same lines a user would see on screen,
 * rather than reaching into internals.
 */
import type {
  BufferId,
  HookOutcome,
  HookPayload,
  NamespaceId,
  PluginContext,
  ToolResult,
} from "@neosh/api";

export async function activate(ctx: PluginContext): Promise<void> {
  const { neosh } = ctx;

  // A group that links to a built-in, so it looks right under a theme this plugin never saw.
  await neosh.hl.define("Hello.Note", { link: "Diagnostic.Info" });

  const report = await neosh.buf.create({ name: "[hello]" });
  const ns = await neosh.ns.create("hello");
  const lines: string[] = [];
  const say = async (line: string) => {
    lines.push(line);
    await neosh.buf.setLines(report, 0, -1, lines);
  };

  await say("hello-plugin: activated");

  // ---- 1. a command, bound to a key ------------------------------------
  //
  // Keys bind to command *names*, never to callbacks, so every binding is listable and the user
  // can remap it.
  const cmd = await neosh.cmd.register(
    "hello.open",
    async () => {
      await openFloat();
    },
    { desc: "Open the hello float" },
  );
  ctx.subscriptions.push(cmd);
  await neosh.keymap.set("normal", "<C-h>", "hello.open", { desc: "hello: open float" });
  await say("1. command hello.open registered and bound to <C-h>");

  // ---- 2. a float with content ------------------------------------------
  let floatWin: number | null = null;
  async function openFloat(): Promise<BufferId> {
    const buf = await neosh.buf.create({ name: "[hello-float]" });
    await neosh.buf.setLines(buf, 0, -1, [
      "hello from a TypeScript plugin",
      "no core changes were required",
    ]);
    floatWin = await neosh.float.open(buf, {
      anchor: { kind: "screen" },
      width: { kind: "auto" },
      height: { kind: "auto" },
      border: "rounded",
      borderHl: "Hello.Note",
      title: "hello",
    });
    await say("2. float opened");
    return buf;
  }
  const floatBuf = await openFloat();

  // ---- 3. an extmark that survives an append above it -------------------
  //
  // The interesting case is not attaching the mark; it is that inserting a line *above* it must
  // not move it off its own text. Marks are stored on their line in the core, so this holds
  // structurally rather than by a fix-up pass.
  const markId = await neosh.ns.mark(ns, floatBuf, 1, 0, {
    virtText: [{ text: "  ← still attached", hlGroup: "Hello.Note" }],
    virtTextPos: "eol",
  });
  const before = await neosh.ns.getMark(ns, floatBuf, markId);
  await neosh.buf.setLines(floatBuf, 0, 0, ["(a line inserted above)"]);
  const after = await neosh.ns.getMark(ns, floatBuf, markId);
  const survived = after !== null && after.row === (before?.row ?? -1) + 1;
  await say(
    `3. extmark ${survived ? "survived" : "LOST"} the insert above ` +
      `(row ${before?.row} -> ${after?.row})`,
  );

  // ---- 4. a tool the agent can call --------------------------------------
  const tool = await neosh.tool.register(
    {
      name: "hello_greet",
      description: "Greet someone by name.",
      inputSchema: {
        type: "object",
        properties: { who: { type: "string", description: "Who to greet." } },
        required: ["who"],
        additionalProperties: false,
      },
    },
    async (input): Promise<ToolResult> => {
      const who = (input as { who?: string }).who ?? "world";
      await say(`4. tool hello_greet ran for ${who}`);
      return { content: `Hello, ${who}!`, is_error: false };
    },
  );
  ctx.subscriptions.push(tool);
  await say("4. tool hello_greet registered");

  // ---- 5. a blocking pre-hook that can veto ------------------------------
  //
  // Registered `blocking`, so it is awaited and its answer is honoured. An observer could watch
  // the same calls but could not stop one.
  const hook = await neosh.hook.register(
    "tool_pre",
    async (payload: HookPayload): Promise<HookOutcome> => {
      if (payload.hook !== "tool_pre") return { action: "continue" };
      if (payload.call.name === "read_file") {
        const path = (payload.call.input as { path?: string }).path ?? "";
        if (path.includes(".env") || path.includes("secret")) {
          await say(`5. vetoed read_file ${path}`);
          return { action: "veto", reason: `hello-plugin refuses to read ${path}` };
        }
      }
      return { action: "continue" };
    },
    { blocking: true, timeoutMs: 2000 },
  );
  ctx.subscriptions.push(hook);
  await say("5. blocking tool_pre hook registered");

  // ---- 6. streaming tokens into a buffer, incrementally -------------------
  //
  // `appendText` is the streaming fast path: it rewrites one line rather than resending the
  // document, so cost per token does not grow with transcript length.
  const stream = await neosh.buf.create({ name: "[hello-stream]" });
  let tokenCount = 0;
  ctx.subscriptions.push(
    neosh.agent.onTurnStart(() => {
      tokenCount = 0;
      void neosh.buf.setLines(stream, 0, -1, [""]);
    }),
  );
  ctx.subscriptions.push(
    neosh.agent.onToken((e) => {
      tokenCount += 1;
      void neosh.buf.appendText(stream, e.text);
    }),
  );
  ctx.subscriptions.push(
    neosh.agent.onTurnEnd(() => {
      void say(`6. streamed ${tokenCount} chunks into [hello-stream]`);
    }),
  );
  await say("6. streaming listener attached");

  await say("hello-plugin: ready");
  neosh.log.info("hello-plugin activated");
  void floatWin;
}
