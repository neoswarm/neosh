---
layout: ../../layouts/Docs.astro
title: Docs for agents
description: Give your coding agent one file and it knows how to build neosh plugins. Install the skill, or paste one prompt.
---

## The fastest path

Using Claude Code? Install the neosh skill once and you are done. The agent loads it automatically whenever a task is about neosh:

```sh
mkdir -p ~/.claude/skills/neosh-plugins && curl -fsSL https://neoswarm.dev/skill.md -o ~/.claude/skills/neosh-plugins/SKILL.md
```

Then just ask for what you want:

```
Write me a neosh plugin that shows my open PRs in the sidebar.
```

That is the whole setup. The skill tells the agent where the full API lives, what the sandbox does and does not allow, how to type-check against your exact binary, and how to verify its work in a running workspace.

## Other agents

For Cursor, Codex, or anything that reads an `AGENTS.md`, append the same content (it is the skill without the frontmatter) to your project's file:

```sh
curl -fsSL https://neoswarm.dev/agents.md >> AGENTS.md
```

For a project-scoped skill instead of a global one, put it in the repository:

```sh
mkdir -p .claude/skills/neosh-plugins && curl -fsSL https://neoswarm.dev/skill.md -o .claude/skills/neosh-plugins/SKILL.md
```

## No setup at all

One prompt works without installing anything:

```
Read https://neoswarm.dev/llms-full.txt, then write a neosh plugin that
<what you want>. Put it in ~/.config/neosh/plugins/<name>/ with a
plugin.toml and a main.ts, declare only the permissions it needs, and
type-check it with `cd ~/.config/neosh && npx tsc --noEmit`.
```

## What the agent gets

Every URL is regenerated from the shipped code on each site build, so none of them can drift:

| URL | What it is |
| --- | --- |
| [/skill.md](/skill.md) | The plugin-development skill: constraints, workflow, API map, conventions |
| [/agents.md](/agents.md) | The same content, ready to append to an AGENTS.md or rules file |
| [/llms-full.txt](/llms-full.txt) | The whole corpus: every docs page, the canonical repo guides, and the complete `@neosh/api` TypeScript source |
| [/llms.txt](/llms.txt) | The index, in the llms.txt convention |
| `/docs/<page>.md` | Any single docs page as markdown, also behind the **Copy page** button above |

Two more things on the user's machine make the loop tight: `neosh init` writes the exact API types for the installed binary to `~/.config/neosh/types/`, and a plugin dropped in `~/.config/neosh/plugins/` is live on every save with `^R` to reload.

## neosh as the harness

The other direction also holds: neosh is itself the best place to run the agent that writes the plugin. Open a conversation in the plugin's directory, and the agent's file tools resolve there, `^R` reloads its work, and `^Z` shows whether its binding actually registered. An agent driving its own host is the fastest feedback loop there is.
