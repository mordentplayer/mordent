# `site/` — the mordent.app landing page

The public single-page site for [Mordent](https://github.com/mordentplayer/mordent),
a native, fast Apple Music client. This directory **is** the site: `index.html` is
served as-is, with no build step.

## Files

| File | Purpose |
| --- | --- |
| `index.html` | The whole site: inline CSS, inline SVG icon, no frameworks, no external requests, no web fonts, no trackers. |
| `404.html` | Matching "page not found" page, served (with a 404 status) for unknown paths. |
| `.gitignore` | Keeps local review screenshots (`preview-*.png`) and wrangler's `.wrangler/` scratch out of the repo. |
| `wrangler.jsonc` | Deployment config: Worker name `mordent`, this directory as static assets, `404.html` for unknown paths, apex custom domain. |
| `.assetsignore` | Keeps repo-only files (`README.md`, `wrangler.jsonc`, `.wrangler/`, …) out of the deployed asset set. |
| `og.png` | 1200×630 social card (dark brand panel, app-icon mark + outlined wordmark + tagline), referenced by the `og:*` / `twitter:*` tags. |

Nothing is fetched at load time. The only network call is a small optional
`fetch()` of the GitHub releases API, which upgrades the per-OS buttons
(AppImage / macOS `.app` / Windows zip) to direct asset URLs — with JavaScript
off, blocked, or the API failing, every button still points at the releases
page and the site works unchanged.

## Hosting — Cloudflare Workers static assets, custom domain `mordent.app`

The site is deployed as a Workers static-assets project (Cloudflare's current
Pages-in-Workers flow) from `site/wrangler.jsonc`:

```sh
cd site && npx wrangler deploy
```

```jsonc
{
  "name": "mordent",
  "assets": { "directory": ".", "not_found_handling": "404-page" },
  "routes": [{ "pattern": "mordent.app", "custom_domain": true }]
}
```

The `mordent.app` custom domain is attached to that Worker, so the site is served
at the root: **https://mordent.app/**. `not_found_handling: "404-page"` serves
`404.html` with a `404` status for unknown paths — there is no `.nojekyll`
equivalent to maintain.

The feedback form lives on the same hostname as a route of a second Worker:
**https://mordent.app/feedback** (`mordent-intake` in `../infra/intake/`, which
also keeps its `workers.dev` URL). `index.html` links only that canonical URL.
Route precedence, verified live: everything under `/feedback*` goes to the intake
Worker, everything else on the apex to this site.

## Preview locally

```sh
xdg-open index.html                       # file:// — everything works
# or serve it, to preview 404 handling too:
python3 -m http.server 8080               # then open http://localhost:8080/
```

The optional releases-API call works from `file://` and `http(s)://` alike; if it
is skipped, the fallback links are used.

## Social card (`og.png`)

1200×630 — the size Discord, Slack, Facebook and iMessage use. X's
`summary_large_image` shows 2:1 and centre-crops to 1200×600, so the card keeps
its content inside the middle ~460 px band: the 15 px shaved off the top and
bottom by X never touches ink.

It is generated from the committed brand assets (the `mordent-lockup.svg` panel
mark and outlined wordmark, verbatim) laid out in a 1200×630 HTML page with the
site's palette, then screenshotted at device scale 1:

```sh
npx -y playwright@1.64.0 screenshot --viewport-size="1200,630" file:///tmp/og-card.html og.png
```

Regenerate only if the brand assets or the tagline change; the PNG is committed
so no build step is needed to deploy.
