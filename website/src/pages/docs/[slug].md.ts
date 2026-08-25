import type { APIRoute, GetStaticPaths } from "astro";

// Every docs page, as the markdown it was written in — /docs/keymaps.md beside
// /docs/keymaps — so "Copy page" and any agent get the source, not the HTML.

const pages = import.meta.glob("./*.md", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

export const getStaticPaths: GetStaticPaths = () =>
  Object.keys(pages).map((path) => ({
    params: { slug: path.replace("./", "").replace(".md", "") },
  }));

export const GET: APIRoute = ({ params }) => {
  const raw = pages[`./${params.slug}.md`];
  if (!raw) return new Response("not found", { status: 404 });
  // The layout line is site plumbing; title and description stay.
  const body = raw.replace(/^layout:.*\n/m, "");
  return new Response(body, {
    headers: { "Content-Type": "text/markdown; charset=utf-8" },
  });
};
