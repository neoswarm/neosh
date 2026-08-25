import type { APIRoute } from "astro";
import body from "../data/skill-body.md?raw";

// The neosh plugin-development skill, in the SKILL.md format Claude Code and
// compatible agents load from a skills directory. One file, installed with:
//
//   mkdir -p ~/.claude/skills/neosh-plugins &&
//     curl -fsSL https://neoswarm.dev/skill.md -o ~/.claude/skills/neosh-plugins/SKILL.md
//
// /agents.md is the same content without frontmatter, for AGENTS.md and rules files.

const frontmatter = `---
name: neosh-plugins
description: Write, test and install plugins for neosh, the terminal agent workspace. Use when the user asks to build, change or debug a neosh plugin, extend the neosh workspace (panels, keys, commands, themes, tools, providers), or configure neosh's init.ts.
---

`;

export const GET: APIRoute = () =>
  new Response(frontmatter + body, {
    headers: { "Content-Type": "text/markdown; charset=utf-8" },
  });
