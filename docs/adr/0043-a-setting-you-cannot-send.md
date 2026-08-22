# 0043 — A setting you cannot send, and a panel that shows all of them

**Status:** Accepted.

## Context

Three complaints, one shape.

**The knob only existed for one vendor.** `ProviderOptionDescriptor` has been the mechanism for
per-model options since ADR 0011, and exactly three catalogues filled it in: Anthropic, `claude-cli`
and `codex-cli`. Every OpenAI-shaped endpoint, Gemini, and every ACP agent advertised nothing — so
`^E` on a GPT-5 answered "has no options to set" about a model whose *first documented parameter* is
`reasoning_effort`. The driver was already sending `reasoning_effort` if a selection carried one.
Nothing ever put one there.

**A field was carried and never read.** `prompt_injected_values` has been on the wire since 0011,
documented as "some vendors gate reasoning depth on magic words in the user message", and nothing
anywhere acted on it. Had a catalogue used it, the value would have gone out as the parameter it is
not: `--effort ultrathink`, which is not a level, and the turn fails.

**Only one knob was ever visible.** The switcher was two nested pickers, and with exactly one
descriptor it skipped the outer one — so `^E` opened a list of effort levels and nothing on screen
said that fast mode, the context window or anything else existed.

## Decision

### A knob you cannot send is still a knob

Some settings are words in the message. `claude` reads `ultrathink` and spends past anything
`--effort` accepts; `ultracode` turns on orchestration that has no flag at all. They are settings by
every test that matters — you choose one, it changes the next turn, it should be visible while it is
on — and they cannot travel as parameters.

So they travel in the message, and the plumbing is generic:

* A select declares `prompt_injected_values`; **the value id is the word**. Nothing between the
  catalogue and the driver holds a table of magic strings.
* A boolean declares `prompt_injected_word`, for the switches that are a yes/no rather than a rung.
  Orchestration is not a point on a depth ladder: you can want a fleet of sub-agents on a shallow
  turn, and asking for one by moving a slider marked "how hard should it think" is a control that
  does two things and says one.
* `neosh_provider::injection` takes the words out of the selection and puts them in the message.

Three properties fall out of doing it in the agent rather than in a driver:

1. **Every driver gets it**, including a provider plugin we have never seen.
2. **The transcript never sees it.** The word goes into the copy of the message this turn sends.
   What you typed is what stays on screen and what the next turn replays — otherwise every turn
   after an ultrathink carries the word again and turning it off does not turn it off. For the same
   reason it is applied on the first round only: a tool result travels as a user message, and the
   word on that one would be a fresh instruction after every tool call.
3. **Nothing invalid reaches the wire.** The injected value is replaced with the ladder's own
   default, so the parameter is one the endpoint accepts. The *ladder's* default, not the driver's:
   dropping the option instead would hand a driver whose default is `medium` the job of choosing,
   and quietly lower the depth of the turn you asked to deepen.

**The words are the harness's, not the model's.** `ultrathink` is on `claude-cli` and deliberately
not on `anthropic`: `api.anthropic.com` serves a model, and a message beginning with that word is a
message beginning with a word. Offering a level that silently does nothing — on the one provider
where the same word does something real — would make "it worked yesterday" a difference of instance
that nothing on screen can explain.

### Discovery where it exists, catalogue where it does not

The same split ADR 0011 made for model *ids*, applied to their knobs.

* **OpenRouter says.** Its `/models` carries `supported_parameters`, which is the only place in the
  OpenAI-compatible family where "does this model reason" has a real answer. Four hundred models
  arrive with the right ladder each and no catalogue entry between them.
* **OpenAI does not**, so its ladder is written down — `minimal` at the bottom, which Anthropic has
  no equivalent for, and a top rung that differs by generation.
* **Gemini changed shape between generations**, so the 3 line gets `thinking_level` and the 2.5 line
  gets a thinking switch. Two knobs rather than one knob and a translation layer nobody can see. 2.5
  Pro gets neither, because its budget cannot be set to zero and a control the endpoint rejects is
  worse than no control.
* **xAI, DeepSeek and Mistral get nothing, and that is the finding.** None of them documents a
  reasoning knob on the models seeded here. An invented one is a 400 on the first turn.

### Everything at once, on one surface

`^E` opens a panel: one row per knob, values along the row, the one in effect lit. `←`/`→` moves
along a row, `↑`/`↓` between rows, and what you are looking at *is* the state.

It applies as you move. Changing a reasoning level is reversible, and ADR 0039's rule cuts both
ways: a control that only takes effect when you press the right key to leave is a control that has
to be taught.

Every key is an ordinary binding on a named command, scoped to the panel's buffer **kind** — ADR
0040, and the sidebar's pattern exactly. `^N` and `^P` are pointedly *not* among them: they are the
two most valuable keys in the program, this panel is on screen for a couple of seconds at a time,
and a binding that shadows one of them for the duration is a key that sometimes does something else.

## How it is drawn

A value's prominence is its position on **its own** ladder — `Option.Step1`…`Step5` — measured over
the levels that can actually be sent, so a spoken level bolted past the top does not compress the
five rungs below it. Weight rather than hue, because the palette keeps its three hues for states and
a reasoning level is a scale, which prominence carries natively.

The exception is the level that is not on the ladder at all. It gets `Option.Beyond`, and
`Animation::Spectrum` — a hue travelling along the run — is added to the palette for it. It is the
one animation that abandons the group's own colour, and it is reserved for the one thing that
deserves it: what it has to say is "this is not one of the normal settings", and no amount of amber
says that. It keeps the base colour's lightness and replaces only the hue, so a light theme gets a
rainbow it can read; without truecolor it rotates the six ANSI hues.

The same group goes on the status segment while a spoken value is in effect, which is the only place
that *can* say so: the word never appears in the transcript, and the panel it was chosen in has been
shut for an hour.

The acknowledgement of a choice is a flash on the **strip**, not on the panel. Reverse video, which
the palette reserves for selection and the one-shot "it moved there" — and it is where the setting
now lives, which is the thing worth teaching. A flash inside the panel would mean holding the window
open after the key that dismissed it, and a swallowed `^P` costs more than a lit row is worth.

## Consequences

* A bundled plugin can be more than one file. `neosh:builtin/<name>` was a cannot-be-a-base URL, so
  `./options.ts` beside it had nothing to resolve against; the prefix is now `neosh:/builtin/` and
  the entry is `…/main.ts`. Bundled plugins were the only plugins in the system that could not be
  split up, which sat badly with the claim that they are ordinary.
* `ModelCapabilities::thinking` is now derived from whether a reasoning knob exists, in one place,
  because three vendors spell that idea three ways.
* Two ranged highlights over the same character do not compose — the renderer takes the
  highest-priority one and stops. A group that means "the value in effect" therefore carries its
  band *and* its colour; a background mark under a foreground mark silently drew one of them.
