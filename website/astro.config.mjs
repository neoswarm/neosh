import { defineConfig } from "astro/config";

export default defineConfig({
  // Set this to the real domain before deploying.
  // site: "https://neosh.dev",

  // The terminal mockup draws with significant whitespace; compression
  // collapses the space-only spans it is built from.
  compressHTML: false,
});
