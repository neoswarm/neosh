---
layout: ../../layouts/Docs.astro
title: Appearance
description: The theme is a set of semantic groups plugins link to rather than colours they pick. Override any group and a theme switch leaves yours alone.
---

## Theme and motion

```toml
[options]
"ui.theme" = "dark"       # or "light", or a contributed theme
"ui.motion" = true        # text that moves while something is happening
"ui.hints" = false        # a shortcut row under the composer
"ui.ascii_only" = false   # for terminals without a decent font
"ui.nerd_font" = false    # brand glyphs for providers, where the font has them
```

Motion is reserved for one thing: something is happening and you cannot see it yet. While a turn is in flight the working line sweeps, because a still screen and a wedged process look identical. `Status.Streaming` sweeps, `Status.Pending` pulses. The frontend animates on its own clock, so it costs about 1% of a core, stays on over SSH, and stops the moment the row scrolls out of sight. `ui.motion = false` removes the movement and keeps the colour; it never makes text disappear.

## Overriding colours

The theme is a set of semantic groups, like `Status.Working`, `Git.Added` and `Meter.Fill`, that plugins link to rather than choosing colours. Override any of them from `init.ts` and a theme switch leaves yours alone:

```ts
await neosh.hl.define("Status.Working", { fg: "#ff00ff", bold: true });
```

Highlight groups have owners: a group is yours until you reset it or your plugin unloads, and then it goes back to the theme's. `neosh.hl.list()` shows every group with its owner, and `neosh.hl.get("Normal")` reads a resolved colour rather than making you guess at one.

## Colouring one window

Neovim's `winhighlight`: a window, or every window of a kind, reads group names through a map, and nothing else on screen changes.

```ts
await neosh.hl.define("Acme.Panel", { bg: { kind: "rgb", r: 26, g: 27, b: 38 } });
await neosh.win.setHighlights({ kind: "neosh.sidebar" }, { Normal: "Acme.Panel" });
```

## Shipping a theme

A theme is a contribution: listed beside `dark` and `light` on `ui.theme`, applied when chosen, and gone with its plugin. Groups it does not name come from `base`.

```ts
await neosh.ext.contribute("ui.theme", "gruvbox", {
  base: "dark",
  groups: {
    Comment: { fg: { kind: "rgb", r: 146, g: 131, b: 116 } },
    "Gruv.Extra": { link: "Normal" },
  },
});
```

## How answers are drawn

Answers render as markdown as they arrive: headings lose their hashes, fenced code is indented under its language, and bold, italic and link markers are removed rather than shown, since a terminal can draw bold and drawing the asterisks as well says the same thing twice. Rendering is incremental: settled blocks are drawn once and never touched again, and only the trailing partial block is redrawn per tick.

Tables are laid out in columns with a rule under the labels; one too wide to line up becomes one block per row instead. There is no syntax highlighting inside fences on purpose: doing it properly is a grammar per language, and doing it with a regex colours the wrong words in exactly the code you are reading closely. A fence gets one colour and its language in the corner.

`chat.markdown = false` shows exactly what the model sent, for when the question is "what did it actually say".

## The status strip

```
 chat  ask ⇧⇥  main  ███░░░░░ 38% of 200k  ↑12k ↓4k          claude-opus-5 ^P  High ^E
```

Left is what the conversation is spending: the permission mode, the branch, how full the context window is, tokens in and out. Right is what it is spending it on. When the strip is too narrow, whole segments are dropped, least important first; nothing is ever truncated, because half a token count is a wrong token count.

Every entry carries the key that changes it, immediately after it. Plugins own the contents:

```ts
await neosh.status.set("model", { text: "opus", keys: "^P", align: "right", priority: 10 });
```

## The hint row

`ui.hints = true` puts a row of shortcuts under the composer. It is not a hard-coded list: every entry is registered by whoever owns the feature, so a disabled plugin takes its shortcut with it and an installed one adds its own.

```ts
await neosh.hint.set("model", { keys: "^P", label: "model", priority: 10 });
```

Entries sort by priority and are dropped from the end when the terminal is too narrow. Reading mode draws its own key row whatever the setting says, because those keys are live, different, and written down nowhere else.
