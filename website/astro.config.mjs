import { defineConfig } from "astro/config";

export default defineConfig({
  // Canonical URLs, the sitemap and social cards are all built from this.
  site: "https://neoswarm.dev",

  // The terminal mockup draws with significant whitespace; compression
  // collapses the space-only spans it is built from.
  compressHTML: false,
});
