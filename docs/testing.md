# Testing

Two different questions, in order: is neosh itself working, and is the thing *you* wrote working.

---

## Trying it

```sh
cargo run                       # uses an existing `claude` CLI login — no API key
cargo run -- --list-models      # everything reachable right now
cargo run -- paths              # where config is read from, and what exists
cargo run -- init               # write a starter config, then edit it
```

`cargo run` needs a real terminal. Outside one it refuses with a message telling you to use
`--ui-protocol=stdio` — that is not a workaround, it is the same session with a different frontend
(see below).

Inside: type, `Enter` to send, `Esc` to interrupt a turn, `<C-r>` to reload config. **`<C-c>` gets
you out** — it cancels a running turn, then clears a draft, then offers to quit on a second press;
`<C-q>` leaves immediately.
The bottom line shows the mode, the selected model, and those keys — if you see it, neosh is running.
The panel on the left is the `sidebar` plugin; `<C-t>` moves into the thread list, `<C-b>` hides it,
`<C-p>` picks a model, `<C-e>` its reasoning effort, `<C-g>` git status, `<C-d>` a diff, `<C-k>` the
command palette, `<C-z>` every binding there is.

## Checking neosh

```sh
./scripts/check.sh              # everything CI runs — do this before committing
```

Six steps: the workspace test suite, the ts-rs binding drift check, then `tsc` over the plugin API,
the example plugin, the bundled plugins, and a freshly scaffolded config. The `tsc` steps matter
because a Rust type change that silently breaks every plugin's types is exactly the failure this
project is built to avoid — and the bundled-plugin step is what keeps "the sidebar is just a plugin"
from quietly becoming false.

Narrower loops:

```sh
cargo test --workspace                          # ~390 tests, no network, no API key
cargo test -p neosh-core                         # buffer/extmark/keymap/option invariants
cargo test -p neosh --test hello_plugin          # the six DoD items, through the real binary
cargo test -p neosh --test user_config           # config, reload, trust, timers, end to end
cargo test -p neosh --test builtin_plugins       # the sidebar and switchers, through the binary
cargo test -p neosh --test sessions              # conversations, projects, worktrees
cargo test -p neosh --test approvals             # the permission prompt, end to end
cargo test -p neosh-vcs                          # git parsers, plus a real repository
cargo test -p neosh-script                       # module loading, transpile, timer heap
```

Everything runs offline against recorded fixtures. Nothing needs credentials.

## Debugging a session

```sh
NEOSH_LOG=/tmp/neosh.log NEOSH_LOG_LEVEL=debug cargo run
```

Logs never go to stdout — stdout is the UI protocol, and a stray log line would corrupt the stream a
frontend is parsing. `neosh.log.info(...)` from a plugin lands here, attributed:

```
INFO neosh_core::editor: hello from the plugin log plugin=greeter
```

`--clean` starts with no config, no plugins and no trust decisions. When something is broken, this is
the first thing to try: it tells you in one step whether the problem is neosh or your config.

---

## Testing what you wrote

`--ui-protocol=stdio` turns the session into JSON lines on stdout and stdin, and `--mock-script`
replays a recorded model turn instead of calling one. Together they make a plugin testable with no
API key, no network, and no terminal — which is also exactly how neosh tests itself.

### 1. A recorded turn

One JSON-encoded `ProviderEvent` per line; a **blank line separates turns**, so one file can drive a
whole tool loop. This one calls your tool, then replies:

```jsonl
{"type": "message_start", "model": "mock", "usage": {}}
{"type": "block_start", "index": 0, "block": {"kind": "tool_use", "id": "c1", "name": "greet"}}
{"type": "tool_input_delta", "index": 0, "partial_json": "{\"who\":\"world\"}"}
{"type": "block_stop", "index": 0}
{"type": "message_delta", "stop_reason": {"kind": "tool_use"}, "usage": {}}
{"type": "message_stop"}

{"type": "message_start", "model": "mock", "usage": {}}
{"type": "block_start", "index": 0, "block": {"kind": "text"}}
{"type": "text_delta", "index": 0, "text": "done"}
{"type": "block_stop", "index": 0}
{"type": "message_delta", "stop_reason": {"kind": "end_turn"}, "usage": {}}
{"type": "message_stop"}
```

To record a real one instead of hand-writing it, run with `NEOSH_LOG_LEVEL=trace` and lift the
provider events out of the log.

There are two ready-made ones under `crates/neosh/tests/fixtures/`.
`markdown_turn.jsonl` is an answer with a heading, a list and a fenced block in it — enough shape
that "the block under the cursor" is a real question. `wrapping_turn.jsonl` is six paragraphs of
prose long enough to **wrap**, which the first one deliberately is not: a buffer row that draws as
four screen rows is the case every piece of caret and page arithmetic gets wrong, and it is
invisible in a fixture whose lines all fit. Use it for anything about where the cursor is.

### 2. Run it

```sh
neosh --ui-protocol=stdio --clean \
      --plugin-dir ./plugins \
      --mock-script ./turn.jsonl --model mock/mock
```

`--clean` is important: without it your own config loads too, and a test that passes because of
something in your `init.ts` is not a test.

### 3. Drive it

**Wait for output, never for a duration.** This is the mistake to avoid — driving input before your
plugin has finished activating produces `no such tool`, intermittently, and only on a loaded machine
or in CI. Have your plugin announce itself (`neosh.notify("myplugin ready")`) and wait for that.

A harness small enough to copy:

```python
import json, subprocess, threading, queue, time

proc = subprocess.Popen(
    ["neosh", "--ui-protocol=stdio", "--clean",
     "--plugin-dir", "./plugins", "--mock-script", "./turn.jsonl", "--model", "mock/mock"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

events, q = [], queue.Queue()
threading.Thread(target=lambda: [q.put(l) for l in proc.stdout], daemon=True).start()

def send(o): proc.stdin.write(json.dumps(o) + "\n"); proc.stdin.flush()

def texts():
    out = []
    for e in events:
        if e.get("type") == "buffer_lines":
            out += [x["text"] for x in e["lines"] if x["text"]]
        elif e.get("type") == "message":
            out.append(e["text"])
    return out

def wait_for(needle, budget=15):
    end = time.time() + budget
    while time.time() < end:
        if any(needle in t for t in texts()): return
        try: events.append(json.loads(q.get(timeout=end - time.time())))
        except Exception: break
    raise AssertionError(f"never saw {needle!r}; saw {texts()}")

def key(c): send({"type": "key", "key": {"code": {"kind": "char", "c": c}, "mods": {}}})

send({"type": "ready", "width": 80, "height": 24})
wait_for("greeter ready")                 # your plugin said so — not a sleep

for c in "hi": key(c)
send({"type": "key", "key": {"code": {"kind": "enter"}, "mods": {}}})

wait_for("turn finished")
assert not any("no such tool" in t for t in texts())
```

Reading a blocking stream on a thread rather than inline is not incidental: a blocking read ignores
your timeout entirely, so a stalled host hangs the test forever instead of failing with a transcript.
(neosh's own test harness had this bug.)

### 4. Assert on what a user would see

The stdio stream is the same `UiEvent` sequence the terminal renders, so assert on rendered text
rather than reaching into internals. The events you will want:

| event | |
|---|---|
| `buffer_lines` | the only buffer mutation. `lines[].text` plus `lines[].marks` — text and extmarks together, so they cannot desync |
| `message` | transient notifications (`neosh.notify`) |
| `window_opened` / `window_closed` | includes `layout.kind == "float"` |
| `flush` | end of a coalesced frame |

Input you can send: `ready`, `key`, `paste`, `viewport_changed`, `resize`, `command`. A key is
`{"type":"key","key":{"code":{"kind":"char","c":"x"},"mods":{"ctrl":true}}}`; other `kind`s include
`enter`, `esc`, `backspace`, `up`, `down`, `f` (with `n`).

`{"type":"command","name":"git.branch.new"}` runs a command by name. Prefer it over synthesising a
keystroke: a command with no default binding has no key to synthesise, and one that does breaks the
moment a user rebinds it. This is the same event a menu or a palette entry in a real frontend sends.

### Type checking

```sh
cd ~/.config/neosh && npx tsc --noEmit     # your config
cd my-plugin && npx tsc --noEmit           # your plugin
```

The types come from the binary itself (`neosh init` writes them, and startup refreshes them when
they drift), so this checks against the neosh you are actually running rather than a published
package that may be a version behind.

TypeScript is **transpiled, not type-checked** at load — types are erased and a type error becomes a
runtime surprise. Run `tsc` yourself; it is the only thing that catches them.

### Reload instead of restart

While iterating, `<C-r>` re-reads config, reloads every plugin, and resets settings you deleted back
to their defaults. A config that stops parsing leaves the running one alone and tells you why, so a
typo costs you nothing.

---

## Known sharp edges

- **A plugin that throws during `activate` is reported and skipped.** Check the log, or the message
  line — it will not stop neosh from starting.
- **`--clean` and `--config-dir <dir>`** are how you isolate a test from your real config. Every XDG
  root is also overridable: `NEOSH_CONFIG_DIR`, `NEOSH_DATA_DIR`, `NEOSH_STATE_DIR`,
  `NEOSH_CACHE_DIR`.
- **Project config in `.neosh/` needs `neosh trust`** before its code runs. In a test, either avoid
  it or point `NEOSH_STATE_DIR` at a scratch directory and run `neosh trust` first.
- **The runtime has no `fetch` or filesystem access.** If your plugin needs either, it goes through
  the neosh API, and that is deliberate — see [config.md](config.md#what-the-runtime-gives-you).
