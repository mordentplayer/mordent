# Mordent intake endpoint (Cloudflare Worker)

An **anonymous** bug-report / feature-request form for people without a GitHub
account. A valid submission is handed to the triage workflow
(`.github/workflows/triage.yml`) via a `repository_dispatch` event; that workflow
classifies it and files the public issue on
[`mordentplayer/mordent`](https://github.com/mordentplayer/mordent) with the
`from-web` label. The Worker itself never creates issues, so nothing reaches the
repository without passing the classification step.

Everything is one dependency-free Worker script plus the static page it serves.

The endpoint is deliberately **machine-friendly**: a plain `curl` (or any agent /
script) POSTing JSON is a first-class client, exactly like the HTML form. There is
no browser challenge, CAPTCHA or attestation widget by design. Abuse control is the
rate limiting and body validation described below, plus the triage pipeline's own
classification pass, which is the gate before anything becomes public.

---

## Deployed instance

| Item | Value |
| --- | --- |
| Cloudflare account | a dedicated Cloudflare account (Workers + Workers KV) — account identifiers are intentionally not published here |
| workers.dev subdomain | `outline2319` |
| Worker name | `mordent-intake` |
| **Public URL (canonical)** | **https://mordent.app/feedback** — a Worker route on the `mordent.app` zone (`pattern = "mordent.app/feedback*"`, see `wrangler.toml`); the apex root itself is the `mordent` static-assets Worker for `../../site/` |
| Public URL (also live) | **https://mordent-intake.outline2319.workers.dev** — the workers.dev endpoint, kept working (`workers_dev = true`) |
| KV namespace | `INTAKE_KV` → id `64e15885c1594c368e8367e67a9ae6a7` |
| Secrets configured | `GH_TOKEN` — the org-scoped fine-grained PAT of the `mordent-bot` account (Issues: read and write + Contents: read and write), installed 2026-10-11; the value is never printed or committed (while installing it is held in a mode-`0600` file outside the repo — see *Token storage*) |
| Delivery channel | `repository_dispatch`, event_type `web-submission`, `client_payload {type,title,body,contact}` — see [Delivery channel](#delivery-channel) |

If the `GH_TOKEN` secret is ever removed (rotation window), `POST /` degrades to
`503 {"ok":false,"error":"intake not configured yet"}` and `GET /` still serves the
form (which shows that error inline on submit). No issue is ever half-created in
that state — that is the verified behaviour of the *unconfigured* worker, not its
current state.

## Files

| File | Purpose |
| --- | --- |
| `worker.js` | The whole Worker: router, validation, spam/rate limits, dispatch client, and the inline HTML page. No npm dependencies. |
| `wrangler.toml` | Worker name, `GH_REPO` / `DRY_RUN` vars, `INTAKE_KV` binding, and the `mordent.app/feedback*` route. |
| `.gitignore` | Keeps local `.wrangler/` state and `.dev.vars` out of git. |

---

## API

### `GET /` (and `/feedback`, `/feedback/`)

Returns a small self-contained dark-themed HTML page (no external assets, no
tracking, `noindex`). Fields: report kind (`bug` / `feature`), title, description,
an optional public contact line, and a visually hidden honeypot field. Vanilla JS
`fetch` posts JSON back to the path the page was served from, so the form works
identically on the workers.dev root and on `mordent.app/feedback`. Any other path
is a `404 {"ok":false,"error":"not found"}`.

### `POST /` (and `/feedback`, `/feedback/`)

Accepts `application/json` or `application/x-www-form-urlencoded`:

| Field | Rules |
| --- | --- |
| `type` | required, `"bug"` or `"feature"` |
| `title` | required, 4–120 chars (control chars stripped, single line) |
| `body` | required, 20–8000 chars, at least 3 words, not mostly links |
| `contact` | optional, ≤ 200 chars; passed through to the workflow, which appends it to the issue as `contact (unverified): …` |
| `website` | honeypot — MUST be empty. When filled the request is **silently accepted** (`200 {ok:true}`) and nothing is dispatched. |

Success → `200 {"ok":true}` once the dispatch event has been queued. The issue is
then filed by the workflow, so its final shape is:

- title = the submitted title, prefixed with `[bug] ` / `[feature] ` by the
  workflow unless the title already starts with a `[` marker,
- body = the model's classification and analysis, with the original report in a
  collapsed `Original report (verbatim)` block plus the optional contact line,
- label `from-web`.

Errors (JSON, always shaped `{"ok":false,"error":"…"}`):

| Status | When |
| --- | --- |
| `400` | bad JSON, unsupported content-type (`415`), invalid `type`, title/body out of range, too few words, mostly-links body |
| `405` | method other than GET/POST |
| `404` | any path other than `/` |
| `429` | rate limit hit (`Retry-After` header set) |
| `502` | GitHub rejected the dispatch request (nothing was queued; the submission is *not* silently lost) |
| `503` | `DRY_RUN` off and no `GH_TOKEN` secret |

---

## Spam and cost defences

Every filed issue costs a paid triage-model run, so the endpoint is deliberately
stingy:

| Defence | Behaviour |
| --- | --- |
| Per-IP hourly | 3 valid submissions/hour → `429` |
| Per-IP daily | 10 valid submissions/day → `429` |
| Global daily cap | 20 valid submissions/day across everyone → `429 try again later` |
| Honeypot | non-empty `website` → `200 {ok:true}`, nothing created, no quota burned |
| Body checks | ≥ 20 chars, ≥ 3 words, and not mostly URLs |

Counters live in `INTAKE_KV` under `rl:h:<YYYY-MM-DDTHH>:<hash>`,
`rl:d:<YYYY-MM-DD>:<hash>` and `rl:g:<YYYY-MM-DD>`, where
`<hash> = sha256("<CF-Connecting-IP>|<day>")` — the raw IP is never stored.
Only *validated* submissions consume quota, so malformed traffic and honeypot
hits cost nothing. KV is eventually consistent, so a burst of concurrent
requests from one client can overshoot a limit slightly; that is acceptable here.

There is intentionally **no CAPTCHA or browser attestation** (see the intro): the
endpoint must stay usable by `curl` and by agents, so the per-client and global
budgets above bound the worst case, and the triage pipeline's classification is
the final gate that decides what becomes a public issue — spam never surfaces.

## Configuration

`wrangler.toml` `[vars]`:

| Var | Meaning |
| --- | --- |
| `GH_REPO` | `mordentplayer/mordent` — repository the dispatch is sent to. |
| `DRY_RUN` | `1` = log the would-be dispatch and return `200 {ok:true}` **without** calling GitHub. Must be `0` in production. |

Secret:

| Secret | Meaning |
| --- | --- |
| `GH_TOKEN` | Fine-grained PAT for `mordentplayer/mordent` with **Issues: read and write** *and* **Contents: read and write**. Issues write is no longer used by the Worker itself (the workflow files the issue with its own `GITHUB_TOKEN`); Contents write is what authorises `repository_dispatch` — GitHub requires it and answers `403 ... x-accepted-github-permissions: contents=write` without it. |

### Delivery channel

The Worker validates, rate-limits and then fires one `repository_dispatch`:

```json
POST https://api.github.com/repos/mordentplayer/mordent/dispatches
{ "event_type": "web-submission",
  "client_payload": { "type": "bug", "title": "…", "body": "…", "contact": "…" } }
```

The title is sent **raw** — the workflow adds the `[bug] ` / `[feature] ` marker
(see the POST contract above). `.github/workflows/triage.yml` picks up the
`web-submission` action in its `web` job, runs a cheap spam classification first
(spam is dropped with a redacted note on the `triage-reports` branch and *no* issue
is created) and only then investigates and files the issue with
`GITHUB_TOKEN`. The Worker therefore never creates public content, and the
repository's `issues: opened` triage job does not re-run on these issues (bot
author guard).

Trade-off, deliberately accepted: a dispatch is fire-and-forget — GitHub answers
`204` as soon as the event is queued, so the Worker reports `{ok:true}` before the
workflow has run, and a *later* workflow failure is invisible to the submitter
(it shows up in the Actions run). In exchange, nothing unclassified can ever
become public. A non-`204` dispatch response is reported honestly as `502` rather
than falling back to creating an unclassified public issue.

### Label handling

The Worker no longer touches labels. The workflow creates `from-web` only if it is
missing (`gh label create … || true`, colour `ededed`, description "Submitted via
the anonymous web form") and never overwrites an existing label's colour or
description, then files the issue with `--label from-web`.

---

## Deployment / operations

```sh
cd infra/intake

# one-time (already done): create the KV namespace and paste its id into wrangler.toml
wrangler kv namespace create INTAKE_KV

# deploy (reads wrangler.toml, so DRY_RUN=0)
wrangler deploy

# local development with a throwaway local KV and no GitHub calls
wrangler dev --var DRY_RUN:1

# tail production logs (DRY_RUN would-be issues, honeypot hits, GitHub failures)
wrangler tail mordent-intake
```

Local `wrangler dev` uses its own state directory (`.wrangler/state`, git-ignored)
and never touches the production KV namespace.

### The `mordent.app/feedback` route

`mordent.app` (apex) is the `mordent` **static-assets Worker** (Cloudflare's
Pages-in-Workers flow; config in `../../site/wrangler.jsonc`, attached as a
`custom_domain` route). This Worker holds a single additional **zone route**:

```toml
routes = [
  { pattern = "mordent.app/feedback*", zone_name = "mordent.app" },
]
```

The zone route wins over the static-assets custom domain for the prefix it matches,
so the two Workers coexist on one hostname: `/feedback*` → this form, everything
else → the landing page. Verified live (see below). `workers_dev = true` keeps the
workers.dev URL as a second entry point, and the form posts back to whatever path
served it (`location.pathname`), so both entry points work unchanged. `wrangler
deploy` creates the route; no separate step. Rate limits and the honeypot are
unchanged (`CF-Connecting-IP` is the real visitor IP on both entry points).

### 3. The fine-grained PAT (`GH_TOKEN`) — now `mordent-bot`'s org token

**Status: installed 2026-10-11** as the Worker secret `GH_TOKEN`. This is now the
**org-scoped fine-grained PAT of the `mordent-bot` account**: resource owner
`mordentplayer` (the organisation — a personal-account token cannot carry
permissions on an org-owned repository), repository access limited to
`mordentplayer/mordent`, with **Issues: Read and write** and **Contents: Read and
write**. Verified: `GET /user` → `mordent-bot`, `GET /repos/mordentplayer/mordent`
→ `200` (`admin: true` as the bot is an org owner), dispatch probe → `204`, a
dangling-blob write → `201`, and an intentionally invalid issue creation → `422`
(**not** `403`, proving Issues write without creating anything). The dispatched
workflow run's actor is therefore `mordent-bot`.

Token storage (why a local file is involved at all):

| File (mode `0600`, outside the repo) | Purpose |
| --- | --- |
| `~/intake-token` | the value piped into `wrangler secret put GH_TOKEN`; keep absent when a swap is not in progress |
| `~/mordent-bot-token` | the canonical saved copy of the bot PAT |

Both are `chmod 600` and are never committed; the secret cannot be read back from
Workers after upload. The earlier `mordent-intake-worker` PAT (resource owner: a
personal account) was used only until the org move and is no longer installed.

> **Do not reuse the `gh` CLI OAuth token** (`gho_…`) as the long-lived secret.
> It is an OAuth-app token with broad `repo` scope, belongs to the whole CLI
> session, and cannot be scoped to a single repository or revoked independently
> of your CLI login. Likewise, an org owner's admin rights do not help: a
> fine-grained PAT is gated by its own permission set and its resource owner.

1. Open <https://github.com/settings/personal-access-tokens/new> (fine-grained tokens).
2. **Token name**: e.g. `mordent-bot-intake-worker`.
3. **Resource owner**: **`mordentplayer`** (the organisation, not a user).
4. **Repository access**: *Only select repositories* → `mordent`.
5. **Permissions** → *Repository permissions* → **Issues: Read and write** and
   **Contents: Read and write** (`Metadata: Read-only` is added automatically;
   nothing else is needed). The organisation must permit fine-grained PATs
   (Org settings → Personal access tokens).
6. **Expiration**: pick a sensible window (e.g. 90 days) and put a reminder in your
   calendar to rotate the secret.
7. **Generate token** and copy the `github_pat_…` value.
8. Store it as a Worker secret (never paste it into a shell history line; pipe it
   from the 0600 file):

   ```sh
   cd infra/intake
   wrangler secret put GH_TOKEN < ~/intake-token
   ```

   Secrets are encrypted at rest and not readable again; only Workers can use them.
9. Verify the dispatch capability (a `204` means the workflow will receive the
   event; the probe event type matches no trigger, so nothing runs):

   ```sh
   curl -s -o /dev/null -w '%{http_code}\n' -X POST \
     -H "Authorization: Bearer $GH_TOKEN" \
     -d '{"event_type":"intake-capability-probe"}' \
     https://api.github.com/repos/mordentplayer/mordent/dispatches
   # → 204
   ```

Deleting/rotating: revoke the token in GitHub settings, then re-run
`wrangler secret put GH_TOKEN`. With the secret absent the Worker degrades to the
`503` "intake not configured yet" response instead of erroring.

---

## Verification performed

### Move to `mordent.app/feedback` (2026-10-11)

- `GET`/`HEAD https://mordent.app/feedback` → `200 text/html; charset=utf-8` with the self-contained form (`cache-control: no-store`); `GET /feedback/` → `200`; `POST /feedback` with a filled honeypot → `200 {"ok":true}` and nothing dispatched (no issue filed). `GET /feedback/xyz` → `404 {"ok":false,"error":"not found"}` (the Worker's own JSON 404).
- The workers.dev entry point still works: `GET /` → `200` form, honeypot `POST /` → `200 {"ok":true}`.
- Zone-route precedence over the `mordent` static-assets custom domain, verified on the apex: `https://mordent.app/` → `200` landing page (canonical `https://mordent.app/`, feedback links to `https://mordent.app/feedback`), `https://mordent.app/nonexistent` → `404` rendering `404.html`. Right after the deploy some edge locations still served the previous state (the assets Worker's 404) until route propagation settled; a re-check was consistently correct.
- Pre-move verification of the routing logic stays valid: `wrangler dev` was not re-run for the prefix change — the live checks above cover the two accepted paths (`/`, `/feedback`) and the rejections.

Live behaviour before the move:

- `GET /` → `200`, self-contained HTML (~6.4 KiB, no external requests); `GET /health` → `404`; `PUT /` → `405`.
- Valid JSON and form-encoded submissions → `200 {"ok":true}`; the DRY_RUN log shows the exact would-be event, e.g.
  `{"event_type":"web-submission","client_payload":{"type":"bug","title":"Queue pane stays empty after restart","body":"…","contact":"tester@example.com"}}` (title raw, `contact` `""` when the field is omitted).
- Invalid type / short title / >8000-char body / mostly-links body / bad JSON → `400`; unsupported content-type → `415`; oversized request → `400`.
- Honeypot filled → `200 {ok:true}` with log `intake: honeypot triggered, dropping submission` and no KV quota or dispatch.
- 4th valid submission in the hour → `429` with `Retry-After: 3600`; global counter seeded at the 20/day cap → `429` "daily submission limit reached".
- Control characters (`\u0007`, `\u0001`, `\u001b`) stripped from title/body before the dispatch payload is built.
- Two different source IPs received independent per-IP budgets (same-IP keying confirmed).
- `DRY_RUN=0` with no `GH_TOKEN` → `503 {"ok":false,"error":"intake not configured yet"}`; validation still runs first (`400` for bad input). With a deliberately invalid token the dispatch call fails and the endpoint answers `502` and logs the upstream status — i.e. a rejected dispatch is never reported as success.
- Live deploy: page `200`, bad type `400`, and a temporary `DRY_RUN=1` deploy proved the **remote** KV binding writes `rl:*` keys (test counters deleted afterwards).

End to end against the live Worker and the public repo:

- PAT checks: `GET /user`/`GET /repos/mordentplayer/mordent` → `200`; invalid issue creation → `422`; `POST /dispatches` → `403` before the Contents permission and `204` after it.
- Org/bot token swap (the current configuration): the secret is now the **`mordent-bot` org-scoped PAT**. Verified on `mordentplayer/mordent`: `GET /user` → `mordent-bot`, repo read → `200` (`admin: true`), dispatch probe → `204`, dangling-blob write → `201`, invalid issue create → `422`. The earlier personal-account PAT (resource owner: a personal account) could not dispatch against the org repo at all (`403` with `x-accepted-github-permissions: contents=write`) — a personal resource owner does not carry write permissions on an org-owned repository, which is the whole reason for the org token.
- Org-path dispatch smoke: a synthetic JSON `curl` submission → `200 {"ok":true}`; the Worker created **no** issue itself; the `repository_dispatch` run `38113659767` shows `actor = triggering_actor = mordent-bot`; its `web` job succeeded and filed issue #7 with the `from-web` label, the `[bug] ` prefix added by the workflow, and the analysis plus collapsed verbatim report. Because the workflow still uses its own `GITHUB_TOKEN` (wstriage's `MORDENT_BOT_TOKEN` switch is not on main yet), #7 was authored by `github-actions[bot]`, not by `mordent-bot`.
- Cleanup: #7 was **deleted** (not closed) with the repository admin login via the GraphQL `deleteIssue` mutation — issue count back to `0` — while `triage-reports/issue-7.md` (`2660` bytes, sha `20be414f`) remains on the `triage-reports` branch.

<!-- previous run, kept for history -->
- Dispatch smoke (pre-org-move): a synthetic JSON `curl` submission → `200 {"ok":true}`; the Worker created **no** issue itself, the `repository_dispatch` run [38111262415](https://github.com/mordentplayer/mordent/actions/runs/38111262415) ran the `web` job to success, and issue [#6](https://github.com/mordentplayer/mordent/issues/6) was filed by `github-actions[bot]` with the `from-web` label, the classification/analysis body and the original report in a collapsed `Original report (verbatim)` block plus the `contact (unverified):` line. (The spam pass ran as part of that job and cleared it as not spam.)
- Earlier, the direct-issue era was verified the same way: issue #3 → `Issue triage` run `38110549687` → report `triage-reports/issue-3.md`; that path has since been removed.
- Honeypot re-checked on the live endpoint: `200 {ok:true}` and no issue created.
- Confirmed the endpoint ships no attestation/CAPTCHA integration: the Cloudflare account's challenge-widget list is empty and there is no such verification code or secret in this directory — a browser challenge was considered and deliberately rejected to keep the endpoint scriptable.

Not exercised here: a spam-verdict dispatch — the `web` job's spam branch is the
triage pipeline's own path (it has since produced `triage-reports/spam-*.md`), and
deliberately sending spam through the public endpoint was left alone.
