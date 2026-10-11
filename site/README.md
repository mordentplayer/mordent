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
