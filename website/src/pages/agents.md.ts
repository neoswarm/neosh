import type { APIRoute } from "astro";
import body from "../data/skill-body.md?raw";

// The skill body with a heading instead of frontmatter, so it can be appended
// verbatim to an AGENTS.md, CLAUDE.md, or a rules file:
//
//   curl -fsSL https://neoswarm.dev/agents.md >> AGENTS.md

export const GET: APIRoute = () =>
  new Response(`# Writing neosh plugins\n\n${body}`, {
    headers: { "Content-Type": "text/markdown; charset=utf-8" },
  });
