---
layout: ../../layouts/Docs.astro
title: Keys
description: Every default binding that ships. All of them are ordinary entries in one table — set the same key in your init.ts and yours replaces ours.
---

**Nothing here is fixed.** Every key below is an ordinary binding to a *command name*, in the same table your `init.ts` writes to. Setting the same key replaces it; `neosh.keymap.del` removes it; a capability whose key you delete still runs from `^K` by name. `^Z` lists the live registry, which is the truth — this page is what ships. See [keymaps](/docs/keymaps) for how to change any of it.

Every default is a key your terminal already sends: Ctrl with a letter, `⇧⇥`, `⏎`, `⌫`, `Esc`. Nothing needs `fn` or Option-as-Alt. Arrows, `PgUp`/`PgDn`, `Home` and `End` are bound wherever they mean something and are **never the only way** to do anything.

## Chat

| Key | Does |
| --- | --- |
| `⏎` | Send. While a turn is running, **steer** it: the message is taken in at the next gap |
| `⇧⏎` | Newline, so a pasted snippet stays one message |
| `Esc` | Interrupt the turn in this conversation. The conversation survives it |
| `^Y` | Take the last thing you queued back into the composer, to change it or drop it |
| `^V` | Attach the image on the clipboard — the picture, or the one it only names |
| `⌫` | On an empty composer, take the last attached image back off |
| `^P` | Pick a model. Mid-turn too — the running agent is told |
| `^E` | Every option this model has, on one panel, applied as you move |
| `⇧⇥` | Permission mode. Belongs to this conversation and is saved with it |
| `^T` | Projects and conversations |
| `^J` | The computers in this workspace |
| `^F` | The archive |
| `^N` | New conversation. In a repository it asks where |
| `^O` | Add a project |
| `^B` | Toggle the sidebar |
| `^K` | Command palette |
| `^W` | Windows, panes and tabs — hold it to see what follows. See below |
| `/` | Complete a command by name — neosh's, and whatever the agent accepts |
| `^L` | What the plan has left, and where the week went |
| `^G` | Git status |
| `^D` | Show what changed |
| `^S` | Read the transcript |
| `⌥Y` | Copy this conversation's directory |
| `^A` `^X` | Select everything, cut the selection |
| `^C` | Copy the selection, or clear the draft, or (twice) quit |
| `PgUp` `PgDn` `^End` | Scroll, and back to the newest message |
| `^R` | Reload configuration |
| `^Q` | Close this terminal. Whatever is running keeps running |
| `^Z` | Every binding, live |

Dragging an image onto the terminal pastes its path, and a pasted path to an image is attached rather than typed out.

**Composer editing** is a text field: `←`/`→` by character and `^←`/`^→` by word, `Home`/`End` and `^Home`/`^End` for the ends, shift with any of them to select, `^⌫` and `^U` to delete a word or back to the start of the line. `^W` was the word-delete and is now the window prefix — a key cannot both delete a word and wait to see whether a window verb follows it, because the waiting is what makes every word-delete feel late. `ui.keys.delete_word` and `ui.keys.window_prefix` swap them back if you would rather.

`model.upgrade` and `model.downgrade` ship with **no key** — they had `⌥↑`/`⌥↓`, which is not a key every terminal sends. `^K` runs both by name, and `init.ts` can bind them.

## Windows, panes and tabs — `^W`

The one prefix that ships. It earns it: twenty-four verbs, and no Ctrl-with-a-letter left to give them — so most would otherwise arrive with no key at all.

**Hold `^W` and wait.** A panel lists everything that follows it, read out of your own keymap rather than out of this page — so a rebinding shows *your* letter, and a plugin that adds a verb under the prefix appears in it with nobody writing glue. `ui.keys.hint_delay` is how long the pause is (300 ms; `0` shows it at once), and `ui.keys.window_prefix` moves the prefix.

| Key | Does |
| --- | --- |
| `v` `s` | Split: the new pane on the right, or below. Focus follows |
| `c` | Split to the right **on a new conversation** — splitting and `^N` in one key |
| `q` | Close this pane. The last one is refused — `^Q` closes the terminal |
| `o` | Close every pane but this one |
| `h` `j` `k` `l` | Go to the pane that way. No wrap-around: the edge is where it stops |
| `w` | The next pane, or the next **tab** when there is only one pane — so it always goes somewhere |
| `<` `>` | Narrower, wider — Vim's, and the same keys the project panel resizes with |
| `-` `+` | Shorter, taller. At an edge the other boundary moves rather than nothing happening |
| `H` `J` `K` `L` | **Move this pane** to that edge — Vim's, and the counterpart to `hjkl` moving *you* |
| `x` | Swap this pane with the next, keeping their places |
| `=` | Give every pane an equal share |
| `t` | A tab. **Asks what goes in it**: a new conversation, a shell, or this conversation again |
| `T` | A tab with a **shell**, skipping the question |
| `S` | Split into a **shell** |
| `X` | Close this tab and everything in it |
| `n` `p` | The next / previous tab, wrapping |
| `1`–`9` | That tab, counting from the left as the bar numbers them |
| `r` | Name this tab. `↵` on an empty name gives it back to being named by what is in it |
| `,` `.` | Move this tab along the bar |
| `?` | The list, now rather than after the pause |

**Every pane is a whole conversation surface** — its own transcript, its own message field, its own draft, scroll, search and reading position. Two chats side by side are two of all of that; typing in one does nothing to the other. A split starts on what you were already reading, which is what `:split` means in Vim and is the only version that needs no dialog; `^T` and `^N` then change what is in the pane you are in.

**A pane can be a shell.** `^WT` for a tab with one, `^WS` to split into one. Every key goes to the program inside — `^C` interrupts *it* — except the window prefix, which is always your way out. It runs `$SHELL -l` in the conversation's directory.

**The strip along the top is always there.** Tabs on the left, numbered as `^W1`…`^W9` counts them, and the keys for wherever you are on the right. The pane with the keyboard is the one whose **edges are lit**.

## Reading the transcript — `^S`

`^S` puts the editor in `Normal`, so chat's bindings stop firing and these get their keys back. Only `^C`, `^Q`, `^R` and `^S` are bound in every mode.

| Key | Does |
| --- | --- |
| `hjkl`, arrows | Move. A count first does it that many times: `5j` |
| `w` `b` `e` `ge` | By word. `W` `B` `E` by WORD — everything that is not a space |
| `0` `^` `$` `g_` | Line start, first non-blank, last character, last non-blank |
| `f` `F` `t` `T` then `;` `,` | To a character on this line, or just before it |
| `%` | The bracket matching the one under the cursor |
| `gg` `G` | Ends of the transcript. With a count it is a row: `12G` |
| `^D` `^U` | Half a screen |
| `^F` `^B`, `PgUp` `PgDn` | A screen |
| `^E` `^Y` | Scroll a line without moving the cursor |
| `H` `M` `L` | The top, middle and bottom row of the window |
| `zz` `zt` `zb` | Put the cursor's line in the middle, at the top, at the bottom |
| `[` `]` | Previous / next **turn** |
| `c` `C` | Next / previous **tool call**. The card you land on opens itself |
| `{` `}` | Previous / next **block** |
| `⇥` `za` | *Keep* the tool card under the cursor open |
| `/` `?` then `n` `N` | Search, and step through the matches |
| `*` `#` | Search for the word under the cursor |
| `v` `V` | Start a selection; `V` by whole lines |
| `o` | While selecting: the other end of what you have |
| `gv` | The selection you last gave up, back where it was |
| `iw` `aw` `ip` `ap` `i"` `i(` … | Text objects, while selecting. `a` takes the delimiters |
| `y` | Copy the selection, and leave |
| `yy` `Y` | Copy the line |
| `y` + a motion | Copy what it covers: `yw`, `y$`, `y5j`, `yG` |
| `yi` + an object | Copy one without selecting it first — `yiw`, `yi"` |
| `yc` | Copy the **code block** the cursor is in, without its indent or language line |
| `ym` | Copy the whole **turn** — the question and everything it produced |
| `yp` | Copy this conversation's **directory** |
| `ya` | Copy the entire transcript |
| `i` `a` `o` `⏎` | Back to the composer |
| `^S` | And back out, the way you came in |
| `Esc` | One thing per press: the selection, then the search highlight, then the mode |

There is no `^V`: a blockwise selection is a rectangle rather than a range. Nothing here edits — no `d`, `x` or `p` — because the transcript is an artefact you take pieces out of.

The card you are standing on is open, up to `chat.preview_lines`. Reading the last row keeps reading it: a turn still running writes below you and carries you along. Anywhere else you stay put, and `G` starts following again.

## Answering a question

What an agent gets when it asks *you* something. Every key here is bound against the `neosh.question` buffer kind, so `^Z` lists it and `init.ts` can move it.

| Key | Does |
| --- | --- |
| `↵` | Take the option under the cursor. With something typed, send that instead |
| `1`–`9` | Take that option, while nothing has been typed |
| `⇥` | Tick or untick, on a question that takes more than one |
| `^P` `^N`, `↑` `↓` | Move |
| `⇧⇥` | Back to the previous question |
| type | Your own answer. `⌫` back to the options, `^W` a word, `^U` all of it |
| `Esc` | Dismiss. The agent is told nobody answered, which it can act on |

## The project panel — `^T`

| Key | Does |
| --- | --- |
| `↵` | Open a conversation, fold a project, or add one from the `+` row |
| `j` `k` | Move. With a count, that many rows: `5j` |
| `^D` `^U` | Half a screen; `PgUp`/`PgDn` for a whole one |
| `gg` `G` | The ends of the list, and `5gg` / `5G` for the fifth row |
| `>` `<` `=` | Wider, narrower, back to the default — it is the `sidebar.width` setting |
| `n` | New conversation in the project under the cursor |
| `o` | Add a project |
| `f` | Pin a project to the top |
| `J` `K` | Reorder within a group |
| `r` | Rename a conversation |
| `y` | Copy the row's directory |
| `p` | Pull that repository (a git-plugin contribution) |
| `d` | Remove a worktree from disk. It asks first (a git-plugin contribution) |
| `x` `X` | Archive, delete. On a project heading, `X` takes the project off the list |
| `a` | The archive |
| `⇥` | On the plan rows: how much of it to show |
| `?` | The keys for whatever row you are on |
| `Esc` | Back to the composer |

## The archive — `^F`, or `a` in the project panel

Rows can be ticked, and every verb means what is ticked or — with nothing ticked — the row under the cursor.

| Key | Does |
| --- | --- |
| `↵` | Put this one back and go to it |
| `u` | Put these back, and stay here |
| `X` | Delete these, for good. It asks, and says how many |
| `^X` | Empty what the panel is **currently showing**, so a filter first narrows it |
| `<Space>` `⇥` | Tick this row, and step down |
| `a` | Tick everything showing, or untick it |
| `e` | Copy these conversations to the clipboard, as markdown |
| `y` | Copy this conversation's directory |
| `/` | Filter — on the title, the project and the path |
| `s` `S` | The next ordering along; the next grouping along |
| `j` `k`, `^D` `^U`, `gg` `G` | Move, with counts |
| `?` | The keys |
| `Esc` | One thing per press: the marks, then the filter, then the panel |
| `^F` `q` `^C` | Close it |

## Pickers

| Key | Does |
| --- | --- |
| `↵` | Choose |
| `^N`/`^P`, `^J`/`^K`, `↑`/`↓` | Move |
| type | Filter |
| `⇥` `⇧⇥`, `→` `←` | Between the two panes of the model picker |
| `Esc`, `^C` | Close |

In the model picker, chords — because every bare letter a picker takes is a letter its filter can never contain:

| Key | Does |
| --- | --- |
| `^S` | Sign in to the provider the rail is on |
| `^R` | Ask that endpoint what it serves again |
| `^A` | Add a model by id |
| `^D` | Remove one you added |

These are settings (`ui.keys.next`, `ui.keys.accept`, …), so rebinding them changes what the pickers say they do.

## Rebinding

```ts
await neosh.keymap.set("chat", "<C-l>", "chat.clear");
await neosh.keymap.del("chat", "<C-o>");
```

Keys bind to command names, never to callbacks — which is what makes every binding listable with `^Z`, runnable by name from `^K`, and replaceable by you or by another plugin. [Keymaps](/docs/keymaps) covers modes, scopes, `<leader>` and sequences.
