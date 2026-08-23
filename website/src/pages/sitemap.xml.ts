import type { APIRoute } from "astro";

const pages = [
  { path: "/", priority: "1.0" },
  { path: "/plugins/", priority: "0.8" },
  { path: "/terms/", priority: "0.3" },
  { path: "/privacy/", priority: "0.3" },
];

export const GET: APIRoute = ({ site }) => {
  const urls = pages
    .map(
      (p) =>
        `  <url><loc>${new URL(p.path, site)}</loc><priority>${p.priority}</priority></url>`,
    )
    .join("\n");
  const body = `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls}\n</urlset>\n`;
  return new Response(body, {
    headers: { "Content-Type": "application/xml; charset=utf-8" },
  });
};
