import type { APIRoute } from "astro";
import { flatDocs } from "../data/docs";

// The llms.txt convention: a short, link-dense index an agent can fetch first.
// Every docs page is also served as raw markdown at <path>.md, and the whole
// corpus, canonical repo docs and API source included, is /llms-full.txt.

export const GET: APIRoute = ({ site }) => {
  const abs = (p: string) => new URL(p, site).toString();

  const pages = flatDocs
    .map((d) => {
      const md = d.href === "/docs" ? "/docs/index.md" : `${d.href}.md`;
      return `- [${d.title}](${abs(md)})`;
    })
    .join("\n");

  const body = `# Neoswarm (neosh)

> A terminal workspace for running coding agents, built the way Neovim is built:
> one core in Rust, any model, and every feature shipped as a TypeScript plugin
> on a public API. If you can see it, a plugin can replace it.

Every docs page below is plain markdown. For the complete corpus in one file,
including the canonical repository docs and the full typed plugin API source,
fetch ${abs("/llms-full.txt")}.

## Docs

${pages}

## For agents writing plugins

- [skill.md](${abs("/skill.md")}): the plugin-development skill (SKILL.md format), install to ~/.claude/skills/neosh-plugins/SKILL.md
- [agents.md](${abs("/agents.md")}): the same content without frontmatter, for appending to an AGENTS.md or rules file

## Full corpus

- [llms-full.txt](${abs("/llms-full.txt")}): every docs page, the repository's canonical config and plugin guides, the testing guide, and the complete @neosh/api TypeScript source

## Source

- [Repository](https://github.com/neoswarm/neosh)
- [Plugin API package](https://github.com/neoswarm/neosh/tree/main/plugins/api)
- [Bundled plugins, the worked examples](https://github.com/neoswarm/neosh/tree/main/plugins/builtin)
`;

  return new Response(body, {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
};
