# Add your plugin

Built a plugin? Put it here. One pull request, one file.

1. Fork [neoswarm/neosh](https://github.com/neoswarm/neosh)
2. Copy `_template.json` to `your-plugin-name.json` in this folder
3. Fill it in and open a pull request

```json
{
  "name": "your-plugin-name",
  "description": "One short line about what it does",
  "author": "your-github-name",
  "repo": "https://github.com/you/your-plugin",
  "npm": "your-npm-package",
  "tags": ["ui"]
}
```

`npm` is optional: if you also publish the plugin there, name the package and
the card shows and ranks by its monthly installs. Installing stays
`neosh plugin add <repo>` either way — npm is where the numbers come from,
not where the plugin does.

Rules, few and simple:

- The repo must be public and contain the plugin
- The description is one line, keep it under 80 characters
- Tags are free form, use ones that exist where you can
- No paid or closed plugins, this list is for things people can read and run

That is the whole process. Merged means live on the site.

## Where the numbers come from

Nothing on the page is a field somebody has to remember to bump:

- **Installs** (`↓ 12k/mo`) are npm's download count for the last month, for
  the package named in `npm` — stamped at build, refreshed in the browser from
  npm's public downloads API, cached for an hour. They are the loudest part of
  the *popular* sort. No `npm` field, no number; nothing else changes.
- **Stars** are your repo's own star count, stamped in at build time and
  refreshed from the GitHub API in the visitor's browser, cached for an hour.
  A builtin wears the count of neoswarm/neosh itself — that is the repo its
  card links to.
- **Updated** is your repo's last push (`pushed_at`, same request). For the
  builtins — where the host repo's last push says nothing about one plugin in
  it — it is the last commit touching `plugins/builtin/<name>`, read from git
  history at build time.
- **Added** is the commit that landed your JSON file in this folder, also read
  from git at build time. It is what "just landed" sorts by.

Deploys therefore need the full history: a shallow clone builds fine but every
date comes out empty (`fetch-depth: 0` on GitHub Actions).

## Clicks

Optional, and off until someone turns it on. The site is static, so counting
needs a counter: [GoatCounter](https://www.goatcounter.com) — free for small
sites, no cookies, and readable back without a backend.

1. Create a site there and note the code (`<code>.goatcounter.com`)
2. Put the code in `GOATCOUNTER` at the top of `src/components/Plugins.astro`
3. In GoatCounter's settings, allow the public to view the dashboard (the
   "visitor counter" / public data option), so the `counter/*.json` endpoint
   answers

Every card click then counts as a `plugin-<name>` event, and the cards show
the totals with `↗`. With the constant empty, nothing loads and nothing is
counted.

## The picture

Every card wears one, down its left edge. Two ways to bring yours:

- **A screenshot.** Add the file to `website/public/plugins/` in the same pull
  request and name it in your JSON — `"image": "your-plugin-name.png"`. A full
  `https://` URL works too, for an image that lives in your own repo. The card
  crops it from the top left, which is where a terminal keeps its content — a
  shot of the part of the screen your plugin owns reads best.
- **Preview data** — describe your plugin's screen as rows of text (see below)
  and `pnpm shots` renders it to the PNG for you, in the exact default palette;
  it is how every builtin's picture is made. We run it when your PR lands, so
  bringing only the data is fine.

A card with neither wears a dark placeholder, and every third-party card
carries its install line with a copy button regardless — that part is not
yours to write, it is read off `repo`.

## The preview

The described kind, in detail. It is data, not markup:
text is escaped, and colours come from a fixed list of highlight groups.

```json
"preview": {
  "title": "neosh · your-plugin",
  "rows": [
    { "s": [["hd", " A HEADING"]], "r": [["key", "^X"]] },
    { "c": "cursor", "s": [" the row the cursor is on"] },
    [" a row can also be a plain array of segments"],
    { "s": [["prompt", " › "], "typed text", ["caret", ""]] }
  ]
}
```

- A **row** is `{ "c": rowGroup?, "s": segments, "r": segments? }` — `s` is the
  left side, `r` is flushed to the right edge, `c` colours the whole row. A row
  that needs none of that can just be the segment array.
- A **segment** is `"plain text"` or `["group", "text"]`.
- Eight rows fit; anything past the eighth is cut, not scrolled.
- Row groups: `cursor`, `band-add`, `band-del`.
- Segment groups: `hd`, `cmt`, `faint`, `sep`, `acc`, `key`, `fav`, `ok`,
  `warn`, `err`, `unread`, `live-fg`, `live-dim`, `prompt`, `ph`, `ubar`,
  `anthropic`, `openai`, `google`, `orouter`, `v-read`, `v-run`, `v-edit`,
  `k`, `ty`, `fn2`, `co`, `gut`, `gut-add`, `gut-del`, `sign-add`, `sign-del`.
- Two segments animate: `["spin", "⠋"]` is a live braille spinner, and
  `["caret", ""]` is a blinking block caret.
- An unknown group renders as plain text — nothing breaks, it just loses its
  colour. Show what your plugin actually draws; a slogan in a terminal is an
  advert, and the screenshots around yours are real.
