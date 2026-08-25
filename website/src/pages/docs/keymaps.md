---
layout: ../../layouts/Docs.astro
title: Keymaps
description: Every binding in neosh, including the ones that ship, is an ordinary entry in one table, bound to a command name. Setting the same key replaces what was there.
---

## The basics

```ts
await neosh.opt.set("mapleader", ",");            // set it before you use it
await neosh.keymap.set("chat", "<leader>pa", "project.open");
await neosh.keymap.set("chat", "<leader>pp", "sidebar.focus");
await neosh.keymap.del("chat", "<C-o>");          // drop a default you do not want
```

Keys bind to command names, never to callbacks. That is what makes every binding listable with `^Z`, runnable by name from `^K`, and replaceable by another plugin or by you.

`<leader>` is substituted when the binding is made, not when the key is pressed, the same as Neovim and for the same reason: a binding that silently moved when you changed `mapleader` halfway down your config would be impossible to reason about. Nothing ships bound to `<leader>`, so it is entirely yours.

A sequence you start and do not finish is typed literally after `timeoutlen` milliseconds, so `,pa` does not cost you the comma key:

```toml
[options]
mapleader = ","
timeoutlen = 500
```

## Notation

Neovim's: `<C-l>`, `<S-Tab>`, `<CR>`, `<Esc>`, `<A-Up>`, `<F1>`, `<D-k>`, `<leader>x`, multi-key sequences like `gd`. Modes are `chat` (the composer and everything around it) and `normal` (reading the transcript).

## Scopes

Resolution is window, then buffer, then buffer kind, then global, first match winning. Binding against a kind is how you put a key inside a panel you did not open:

```ts
// `x` archives a conversation by default; this replaces it, in the sidebar only.
await neosh.keymap.set("chat", "x", "acme.mine", {
  scope: { kind: "buf_kind", name: "neosh.sidebar" },
  desc: "Do it my way",
});
```

The panel's own default loses to yours, because a default that overwrites a choice is not a default. The tier order for keys, commands and highlight groups everywhere is: bundled plugin, then installed plugin, then your `init.ts`, later always winning.

The host's own surfaces all have kinds you can bind against: `neosh.sidebar`, `neosh.transcript`, `neosh.composer`, `neosh.question`, `neosh.picker`, `neosh.confirm`, `neosh.prompt`.

## Why the defaults are what they are

A default binding is a key every terminal sends: Ctrl with a letter, `⇧⇥`, `⏎`, `⌫`, `Esc`. Three families are ruled out on purpose:

- **Function keys.** On a Mac the top row is brightness and volume until a setting says otherwise, so `F1` is a key a new Mac cannot press.
- **Option and Alt.** macOS treats Option as a compose key: `⌥p` is `π` unless the terminal was told otherwise.
- **Ctrl with punctuation.** In a plain terminal `Ctrl+/` and `Ctrl+7` are the same byte.

Arrows, `PgUp`, `Home` and `End` are bound wherever they mean something and are never the only way to do anything. If you have the ruled-out keys, use them; the dropped defaults are one line each:

```ts
await neosh.keymap.set("chat", "<F1>", "help.keys");
await neosh.keymap.set("chat", "<A-Up>", "model.upgrade");
await neosh.keymap.set("chat", "<A-Down>", "model.downgrade");
await neosh.keymap.set("chat", "<A-v>", "chat.image.drop");
```

## Enhanced keys

On startup neosh asks the terminal for the Kitty keyboard protocol, supported by kitty, Ghostty, WezTerm, foot, rio, Alacritty, iTerm2 and Windows Terminal, and quietly does without on terminals that lack it. It buys three things: `⌘` bindings like `<D-k>` arrive at all, Ctrl with punctuation becomes distinguishable, and `Esc` is unambiguous so nothing waits to find out whether a sequence is coming.

`NEOSH_NO_ENHANCED_KEYS=1` turns the request off. An environment variable rather than an option, because the terminal has to be in the right mode before the first keystroke, which is before there is any config.

## Picker keys

Every picker, prompt and completion list reads the `ui.keys.*` settings, listed in [Options](/docs/options). A widget claims them window-scoped while it is open, so they outrank your global bindings and are released with the window.

## Raw keys, for widget authors

Keys bind to command names, so a widget that needs raw input claims what nothing else wanted:

```ts
const win = await neosh.float.open(buf, { focusable: true, closeOnBlur: true });
await neosh.focus.push(win);
const release = await neosh.keymap.capture(win, "my.key");
```

While the window is focused, keys no binding claimed are sent to `my.key` with a `KeyContext`. Bindings still win, so a capture cannot take away the key that quits. The capture is released when the window closes or the plugin unloads.

## Seeing what is bound

`^Z` lists every binding and `^K` every command, both read from the live registry, so a plugin loaded five minutes ago is in them without anything having been written down. Every picker's foot strip is written from the bindings themselves, so rebinding `ui.keys.*` changes what the strip says rather than making it a lie.
