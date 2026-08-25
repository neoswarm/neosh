import type { APIRoute } from "astro";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { flatDocs } from "../data/docs";

// The whole corpus in one file, regenerated on every build from the same
// sources the site itself is built from, so it cannot drift from what ships.

const websitePages = import.meta.glob("./docs/*.md", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

// This file lives at website/src/pages/, so the repository root is three up.
const repo = (p: string) =>
  readFileSync(fileURLToPath(new URL(`../../../${p}`, import.meta.url)), "utf-8");

const stripFrontmatter = (md: string) =>
  md.replace(/^---\n[\s\S]*?\n---\n/, "");

const titleOf = (md: string) =>
  /^title:\s*(.+)$/m.exec(md)?.[1]?.trim() ?? "Untitled";

export const GET: APIRoute = ({ site }) => {
  const abs = (p: string) => new URL(p, site).toString();

  const sections: string[] = [];

  sections.push(`# Neoswarm (neosh): full documentation

> A terminal workspace for running coding agents, built the way Neovim is
> built: one core in Rust, any model, and every feature shipped as a
> TypeScript plugin on a public API. This file is generated at build time
> from the site's docs, the repository's canonical guides, and the plugin
> API source. Repository: https://github.com/neoswarm/neosh

Contents:
1. Website docs (${flatDocs.length} pages)
2. Canonical repository guides: docs/config.md, docs/plugins.md, docs/testing.md
3. The @neosh/api TypeScript surface: index.ts and ui.ts, verbatim
`);

  // 1. Website docs, in sidebar order.
  for (const d of flatDocs) {
    const slug = d.href === "/docs" ? "index" : d.href.replace("/docs/", "");
    const raw = websitePages[`./docs/${slug}.md`];
    if (!raw) continue;
    sections.push(
      `\n\n====================\n# ${titleOf(raw)}\nURL: ${abs(d.href)}\n====================\n\n${stripFrontmatter(raw)}`,
    );
  }

  // 2. The canonical guides the docs above are distilled from.
  for (const [path, label] of [
    ["docs/config.md", "Canonical guide: configuring neosh (docs/config.md)"],
    ["docs/plugins.md", "Canonical guide: writing a plugin (docs/plugins.md)"],
    ["docs/testing.md", "Canonical guide: testing a plugin (docs/testing.md)"],
  ] as const) {
    sections.push(`\n\n====================\n# ${label}\n====================\n\n${repo(path)}`);
  }

  // 3. The API itself. The single most useful thing for an agent writing a
  // plugin: every namespace, signature and doc comment, exactly as shipped.
  for (const [path, label] of [
    ["plugins/api/src/index.ts", "The @neosh/api surface (plugins/api/src/index.ts)"],
    ["plugins/api/src/ui.ts", "The @neosh/api/ui widgets (plugins/api/src/ui.ts)"],
  ] as const) {
    sections.push(
      `\n\n====================\n# ${label}\n====================\n\n\`\`\`ts\n${repo(path)}\n\`\`\``,
    );
  }

  return new Response(sections.join("\n"), {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
};
