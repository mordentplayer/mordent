# Mordent intake endpoint (Cloudflare Worker)

An **anonymous** bug-report / feature-request form for people without a GitHub
account. A valid submission becomes a GitHub issue on
[`sliced-paraiba/mordent`](https://github.com/sliced-paraiba/mordent), labelled
`from-web`, where the issue-triage pipeline picks it up automatically.

Everything is one dependency-free Worker script plus the static page it serves.

---

## Deployed instance

| Item | Value |
| --- | --- |
| Cloudflare account | `[redacted]'s Account` (`[redacted]`) |
| workers.dev subdomain | `outline2319` |
| Worker name | `mordent-intake` |
| **Public URL** | **https://mordent-intake.outline2319.workers.dev** |
| KV namespace | `INTAKE_KV` → id `64e15885c1594c368e8367e67a9ae6a7` |
| Secrets configured | **none yet** — see [Enable issue filing](#3-create-the-fine-grained-pat-and-set-gh_token) |

Until `GH_TOKEN` is set, `POST /` returns `503 {"ok":false,"error":"intake not configured yet"}`
and `GET /` serves the form (which shows that error inline on submit). No issue is
ever created in that state.

## Files

| File | Purpose |
| --- | --- |
| `worker.js` | The whole Worker: router, validation, spam/rate limits, GitHub client, and the inline HTML page. No npm dependencies. |
| `wrangler.toml` | Worker name, `GH_REPO` / `GH_LABELS` / `DRY_RUN` vars, `INTAKE_KV` binding. |
| `.gitignore` | Keeps local `.wrangler/` state and `.dev.vars` out of git. |

---

## API

### `GET /`

Returns a small self-contained dark-themed HTML page (no external assets, no
tracking, `noindex`). Fields: report kind (`bug` / `feature`), title, description,
an optional public contact line, and a visually hidden honeypot field. Vanilla JS
`fetch` posts JSON and shows success / error inline.

### `POST /`

Accepts `application/json` or `application/x-www-form-urlencoded`:

| Field | Rules |
| --- | --- |
| `type` | required, `"bug"` or `"feature"` |
| `title` | required, 4–120 chars (control chars stripped, single line) |
| `body` | required, 20–8000 chars, at least 3 words, not mostly links |
| `contact` | optional, ≤ 200 chars; appended to the issue body as `contact (unverified): …` |
| `website` | honeypot — MUST be empty. When filled the request is **silently accepted** (`200 {ok:true}`) and nothing is created. |

Success → `200 {"ok":true}`. The created issue has

- title `[bug] <title>` or `[feature] <title>`,
- body = your text + `\n\n---\nsubmitted anonymously via the web form` (+ the optional contact line),
- labels `["from-web"]`.

Errors (JSON, always shaped `{"ok":false,"error":"…"}`):

| Status | When |
| --- | --- |
| `400` | bad JSON, unsupported content-type (`415`), invalid `type`, title/body out of range, too few words, mostly-links body |
| `405` | method other than GET/POST |
| `404` | any path other than `/` |
| `429` | rate limit hit (`Retry-After` header set) |
| `502` | GitHub rejected the issue request |
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

## Configuration

`wrangler.toml` `[vars]`:

| Var | Meaning |
| --- | --- |
| `GH_REPO` | `sliced-paraiba/mordent` — repository that receives the issues. |
| `GH_LABELS` | Comma-separated labels to apply (default `from-web`); set to `""` to disable labels entirely. |
| `DRY_RUN` | `1` = log the would-be issue and return `200 {ok:true}` **without** calling GitHub. Must be `0` in production. |

Secret:

| Secret | Meaning |
| --- | --- |
| `GH_TOKEN` | Fine-grained PAT with **Issues: read and write** on `sliced-paraiba/mordent`. |

### Label handling

`from-web` is created lazily: if GitHub rejects the issue because the label does
not exist (`422`), the Worker creates the label once
(`POST /repos/{repo}/labels`, colour `1d76db`) and retries; if that still fails it
retries the issue **without labels**, so a missing-label problem can never lose a
report. Failures are logged. (Equivalent manual step:
`gh label create from-web --color 1d76db --description "Submitted via the anonymous web form"`.)

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

### 3. Create the fine-grained PAT and set `GH_TOKEN`

> **Do not reuse the `gh` CLI OAuth token** (`gho_…`) as the long-lived secret.
> It is an OAuth-app token with broad `repo` scope, belongs to the whole CLI
> session, and cannot be scoped to a single repository or revoked independently
> of your CLI login.

1. Open <https://github.com/settings/personal-access-tokens/new> (fine-grained tokens).
2. **Token name**: `mordent-intake-worker`.
3. **Resource owner**: `sliced-paraiba`.
4. **Repository access**: *Only select repositories* → `mordent`.
5. **Permissions** → *Repository permissions* → **Issues: Read and write**
   (`Metadata: Read-only` is added automatically; nothing else is needed).
6. **Expiration**: pick a sensible window (e.g. 90 days) and put a reminder in your
   calendar to rotate the secret.
7. **Generate token** and copy the `github_pat_…` value.
8. Store it as a Worker secret (paste when prompted):

   ```sh
   cd infra/intake
   wrangler secret put GH_TOKEN
   ```

   Secrets are encrypted at rest and not readable again; only Workers can use them.
9. Verify:

   ```sh
   curl -s -H 'content-type: application/json' \
     -d '{"type":"bug","title":"Intake smoke test","body":"filed from the intake smoke test to confirm the token works."}' \
     https://mordent-intake.outline2319.workers.dev/
   # → {"ok":true}   and a new [bug] issue appears on the repo
   ```

Deleting/rotating: revoke the token in GitHub settings, then re-run
`wrangler secret put GH_TOKEN`. With the secret absent the Worker degrades to the
`503` "intake not configured yet" response instead of erroring.

---

## Verification performed

Local (`wrangler dev`, `DRY_RUN=1`, `--var DRY_RUN:0` for the 503 case) and live
against the deployed Worker:

- `GET /` → `200`, self-contained HTML (~6.4 KiB, no external requests); `GET /health` → `404`; `PUT /` → `405`.
- Valid JSON and form-encoded submissions → `200 {"ok":true}`; DRY_RUN log shows the exact would-be issue, e.g.
  `{"title":"[bug] Playback stops after the first track","body":"…\n\n---\nsubmitted anonymously via the web form\ncontact (unverified): tester@example.com","labels":["from-web"]}`.
- Invalid type / short title / >8000-char body / mostly-links body / bad JSON → `400`; unsupported content-type → `415`; oversized request → `400`.
- Honeypot filled → `200 {ok:true}` with log `intake: honeypot triggered, dropping submission` and no KV quota or create.
- 4th valid submission in the hour → `429` with `Retry-After: 3600`; global counter seeded at the 20/day cap → `429` "daily submission limit reached".
- Control characters (`\u0007`, `\u0001`, `\u001b`) stripped from title/body before the issue payload is built.
- Two different source IPs received independent per-IP budgets (same-IP keying confirmed).
- `DRY_RUN=0` with no `GH_TOKEN` → `503 {"ok":false,"error":"intake not configured yet"}`; validation still runs first (`400` for bad input).
- Live deploy: page `200`, valid POST `503` (unconfigured), bad type `400`, and a temporary `DRY_RUN=1` deploy proved the **remote** KV binding writes `rl:*` keys (test counters deleted afterwards; final deployed settings: `DRY_RUN=0`, no `GH_TOKEN`).

Not yet exercised (needs the real token): live GitHub issue creation and the
label-create fallback path.
