/**
 * Mordent anonymous intake endpoint — a single dependency-free Cloudflare Worker.
 *
 *   GET  /   → self-contained HTML form
 *   POST /   → validate a report and hand it to the triage workflow
 *
 * Delivery: `repository_dispatch` with event_type "web-submission" and
 * client_payload {type,title,body,contact}. The triage workflow
 * (.github/workflows/triage.yml) owns everything past that point: it runs the
 * spam pass and files the public issue itself, so nothing unvalidated by the
 * model ever becomes public. The title is sent raw (no "[bug] " prefix) — the
 * workflow adds the marker.
 *
 * Contract (POST): JSON or form-encoded { type, title, body, contact?, website? }
 *   - type    : "bug" | "feature"
 *   - title   : 4..120 chars
 *   - body    : 20..8000 chars, not mostly links
 *   - contact : optional, <= 200 chars (passed through for the workflow to append)
 *   - website : honeypot; if non-empty the submission is silently accepted and dropped
 *
 * Success → { ok: true }.
 * Config: GH_REPO (var), GH_TOKEN (secret), DRY_RUN (var).
 * Rate limiting state lives in the INTAKE_KV namespace (see README.md).
 */

const LIMITS = { perIpHour: 3, perIpDay: 10, globalDay: 20 };
const MIN_TITLE = 4;
const MAX_TITLE = 120;
const MIN_BODY = 20;
const MAX_BODY = 8000;
const MAX_CONTACT = 200;
const MAX_REQUEST_BYTES = 20000;
const MIN_BODY_WORDS = 3;
const TYPES = new Set(["bug", "feature"]);

export default {
  async fetch(request, env) {
    const { pathname } = new URL(request.url);
    if (pathname !== "/" && pathname !== "") {
      return json({ ok: false, error: "not found" }, 404);
    }
    if (request.method === "GET" || request.method === "HEAD") {
      return new Response(PAGE, {
        headers: {
          "content-type": "text/html; charset=utf-8",
          "cache-control": "no-store",
          "x-content-type-options": "nosniff",
          "referrer-policy": "no-referrer",
        },
      });
    }
    if (request.method !== "POST") {
      return json({ ok: false, error: "method not allowed" }, 405, { allow: "GET, POST" });
    }
    return handleSubmit(request, env);
  },
};

async function handleSubmit(request, env) {
  const contentType = (request.headers.get("content-type") || "").toLowerCase();

  let raw;
  try {
    raw = await request.text();
  } catch {
    return json({ ok: false, error: "could not read request body" }, 400);
  }
  if (raw.length > MAX_REQUEST_BYTES) {
    return json({ ok: false, error: "request too large" }, 400);
  }

  let data;
  if (contentType.includes("application/json") || contentType === "") {
    try {
      data = JSON.parse(raw || "{}");
    } catch {
      return json({ ok: false, error: "invalid JSON body" }, 400);
    }
    if (!data || typeof data !== "object" || Array.isArray(data)) {
      return json({ ok: false, error: "invalid JSON body" }, 400);
    }
  } else if (contentType.includes("application/x-www-form-urlencoded")) {
    data = Object.fromEntries(new URLSearchParams(raw));
  } else {
    return json({ ok: false, error: "unsupported content-type (use application/json or form-encoded)" }, 415);
  }

  const str = (key) => (typeof data[key] === "string" ? data[key] : "");

  // Honeypot: accept politely, never create anything. (Cost defense.)
  if (str("website").trim() !== "") {
    console.log("intake: honeypot triggered, dropping submission");
    return json({ ok: true });
  }

  const type = str("type").trim().toLowerCase();
  const title = cleanText(str("title"), { keepNewlines: false }).trim();
  const body = cleanText(str("body"), { keepNewlines: true }).trim();
  const contact = cleanText(str("contact"), { keepNewlines: false }).trim().slice(0, MAX_CONTACT);

  if (!TYPES.has(type)) {
    return json({ ok: false, error: 'type must be "bug" or "feature"' }, 400);
  }
  if (title.length < MIN_TITLE || title.length > MAX_TITLE) {
    return json({ ok: false, error: `title must be ${MIN_TITLE}-${MAX_TITLE} characters` }, 400);
  }
  if (body.length < MIN_BODY || body.length > MAX_BODY) {
    return json({ ok: false, error: `body must be ${MIN_BODY}-${MAX_BODY} characters` }, 400);
  }
  if (wordCount(body) < MIN_BODY_WORDS) {
    return json({ ok: false, error: "body is too short to act on" }, 400);
  }
  if (isMostlyLinks(body)) {
    return json({ ok: false, error: "body looks like mostly links; please describe the issue in your own words" }, 400);
  }

  const repo = (env.GH_REPO || "").trim();
  const dryRun = isTruthy(env.DRY_RUN);
  if (!dryRun && (!repo || !env.GH_TOKEN)) {
    return json({ ok: false, error: "intake not configured yet" }, 503);
  }

  const ip = request.headers.get("CF-Connecting-IP") || "unknown";
  const limited = await checkRateLimits(env, ip);
  if (limited) return limited;

  const payload = { type, title, body, contact };
  const dispatch = { event_type: "web-submission", client_payload: payload };

  if (dryRun) {
    console.log(`intake: DRY_RUN, would dispatch to ${repo || "(unset)"}: ${JSON.stringify(dispatch)}`);
    return json({ ok: true });
  }

  try {
    await dispatchSubmission(env.GH_TOKEN, repo, payload);
    console.log(`intake: dispatched web-submission (${type}, ${title.length}-char title)`);
    return json({ ok: true });
  } catch (err) {
    console.error("intake: GitHub dispatch failed:", err && err.message ? err.message : err);
    return json({ ok: false, error: "could not file the report right now; please try again later" }, 502);
  }
}

/* ------------------------------------------------------------------ limits */

/**
 * Per-IP hour/day window plus a global daily cap. Returns a 429 Response when
 * over budget, otherwise null (and bumps the counters).
 *
 * KV is not atomic: concurrent submissions from the same client can race and
 * exceed the limit by a small amount. That is acceptable here.
 */
async function checkRateLimits(env, ip) {
  const kv = env.INTAKE_KV;
  if (!kv) {
    console.warn("intake: INTAKE_KV not bound, rate limiting disabled");
    return null;
  }
  const now = new Date();
  const day = now.toISOString().slice(0, 10);
  const hour = now.toISOString().slice(0, 13);
  const ipHash = await sha256Hex(`${ip}|${day}`);
  const hourKey = `rl:h:${hour}:${ipHash}`;
  const dayKey = `rl:d:${day}:${ipHash}`;
  const globalKey = `rl:g:${day}`;
  const ttl = secondsUntilUtcMidnight(now);

  const [hourCount, dayCount, globalCount] = await Promise.all([
    kv.get(hourKey),
    kv.get(dayKey),
    kv.get(globalKey),
  ]).then((values) => values.map((v) => Number(v) || 0));

  const retryAfter = String(ttl);
  if (globalCount >= LIMITS.globalDay) {
    return json({ ok: false, error: "daily submission limit reached, try again later" }, 429, {
      "retry-after": retryAfter,
    });
  }
  if (dayCount >= LIMITS.perIpDay) {
    return json({ ok: false, error: "daily submission limit reached for this connection, try again later" }, 429, {
      "retry-after": retryAfter,
    });
  }
  if (hourCount >= LIMITS.perIpHour) {
    return json({ ok: false, error: "too many submissions, try again in a little while" }, 429, {
      "retry-after": "3600",
    });
  }

  await Promise.all([
    kv.put(hourKey, String(hourCount + 1), { expirationTtl: 3600 }),
    kv.put(dayKey, String(dayCount + 1), { expirationTtl: ttl }),
    kv.put(globalKey, String(globalCount + 1), { expirationTtl: ttl }),
  ]);
  return null;
}

/* --------------------------------------------------------------- github */

/**
 * Hand the submission to the triage workflow via `repository_dispatch`
 * (event_type `web-submission`). The workflow owns everything after this
 * point: it runs the spam pass and files the public issue, so an unclassified
 * report never becomes public. A non-204 response means the submission was
 * *not* queued — the caller must say so instead of claiming success.
 */
async function dispatchSubmission(token, repo, payload) {
  const res = await fetch(`https://api.github.com/repos/${repo}/dispatches`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${token}`,
      accept: "application/vnd.github+json",
      "x-github-api-version": "2022-11-28",
      "content-type": "application/json",
      "user-agent": "mordent-intake-worker",
    },
    body: JSON.stringify({ event_type: "web-submission", client_payload: payload }),
  });
  if (res.status !== 204) {
    throw new Error(`GitHub dispatch responded ${res.status}: ${await readText(res)}`);
  }
}

async function readText(res) {
  try {
    const text = await res.text();
    return text.slice(0, 300);
  } catch {
    return "(no response body)";
  }
}

/* ----------------------------------------------------------------- utils */

/** Strip control characters; optionally flatten all whitespace. */
function cleanText(value, { keepNewlines }) {
  let text = String(value ?? "");
  text = text.replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g, "");
  if (keepNewlines) {
    text = text.replace(/\r\n?/g, "\n").replace(/\n{3,}/g, "\n\n");
  } else {
    text = text.replace(/\s+/g, " ");
  }
  return text;
}

function wordCount(text) {
  return text.split(/\s+/).filter((w) => /[a-z0-9]/i.test(w)).length;
}

/** True when the body is mostly raw links (link spam / SEO payloads). */
function isMostlyLinks(text) {
  const urls = text.match(/https?:\/\/\S+/gi) || [];
  if (urls.length === 0) return false;
  const linkChars = urls.reduce((n, u) => n + u.length, 0);
  if (linkChars / text.length > 0.5) return true;
  const prose = text.replace(/https?:\/\/\S+/gi, "").replace(/\s+/g, " ").trim();
  return urls.length >= 3 && prose.length < 40;
}

async function sha256Hex(input) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(input));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

function secondsUntilUtcMidnight(now) {
  const midnight = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() + 1);
  return Math.max(60, Math.ceil((midnight - now.getTime()) / 1000));
}

function isTruthy(value) {
  const v = String(value ?? "").trim().toLowerCase();
  return v === "1" || v === "true" || v === "yes" || v === "on";
}

function json(payload, status = 200, extraHeaders = {}) {
  return new Response(JSON.stringify(payload), {
    status,
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
      ...extraHeaders,
    },
  });
}

/* ------------------------------------------------------------------ page */

const PAGE = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>Mordent — report a bug or request a feature</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0; padding: 2.5rem 1.25rem 4rem;
    background: #16171b; color: #e6e7ea;
    font: 16px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  }
  main { max-width: 40rem; margin: 0 auto; }
  h1 { font-size: 1.5rem; margin: 0 0 .35rem; letter-spacing: -.01em; }
  .sub { color: #9ba1ac; margin: 0 0 1.75rem; font-size: .95rem; }
  .sub a { color: #7fb3ff; }
  form { display: grid; gap: 1.1rem; }
  label { display: block; font-size: .85rem; text-transform: uppercase; letter-spacing: .06em; color: #9ba1ac; margin-bottom: .4rem; }
  .types { display: flex; gap: .6rem; }
  .types label {
    flex: 1; text-transform: none; letter-spacing: 0; font-size: .95rem; color: #e6e7ea;
    display: flex; align-items: center; gap: .5rem; padding: .65rem .8rem;
    background: #1e2026; border: 1px solid #2c2f37; border-radius: 8px; cursor: pointer; margin: 0;
  }
  .types input { accent-color: #4c8dff; }
  input[type=text], textarea {
    width: 100%; padding: .7rem .8rem; background: #1e2026; color: #e6e7ea;
    border: 1px solid #2c2f37; border-radius: 8px; font: inherit;
  }
  textarea { min-height: 11rem; resize: vertical; }
  input:focus, textarea:focus { outline: 2px solid #4c8dff; outline-offset: 1px; border-color: #4c8dff; }
  .meta { color: #767c87; font-size: .8rem; margin-top: .35rem; }
  button {
    padding: .75rem 1.1rem; border: 0; border-radius: 8px; cursor: pointer;
    background: #4c8dff; color: #08101f; font: inherit; font-weight: 600;
  }
  button[disabled] { opacity: .55; cursor: default; }
  .hp { position: absolute; left: -9999px; width: 1px; height: 1px; overflow: hidden; }
  #status { margin-top: .25rem; padding: .8rem .9rem; border-radius: 8px; font-size: .95rem; display: none; white-space: pre-wrap; }
  #status.ok { display: block; background: #16301f; border: 1px solid #2c6b43; color: #b6f3cc; }
  #status.err { display: block; background: #33191b; border: 1px solid #7a3238; color: #ffc4c9; }
  footer { margin-top: 2rem; color: #767c87; font-size: .82rem; }
</style>
</head>
<body>
<main>
  <h1>Report a bug or request a feature</h1>
  <p class="sub">No GitHub account needed — this files an issue on
    <a href="https://github.com/sliced-paraiba/mordent" rel="noopener noreferrer">sliced-paraiba/mordent</a>
    for you. Keep it short and concrete: what you did, what you expected, what happened.</p>

  <form id="intake" novalidate>
    <div>
      <label>Kind of report</label>
      <div class="types">
        <label><input type="radio" name="type" value="bug" checked> Bug</label>
        <label><input type="radio" name="type" value="feature"> Feature request</label>
      </div>
    </div>

    <div>
      <label for="title">Title</label>
      <input id="title" name="title" type="text" maxlength="120" minlength="4" required
             placeholder="Playback stops after the first track">
      <div class="meta">4–120 characters.</div>
    </div>

    <div>
      <label for="body">Description</label>
      <textarea id="body" name="body" maxlength="8000" required
        placeholder="Steps to reproduce, expected vs. actual behaviour, app version, OS."></textarea>
      <div class="meta"><span id="count">0</span> / 8000 characters (minimum 20).</div>
    </div>

    <div>
      <label for="contact">Contact (optional)</label>
      <input id="contact" name="contact" type="text" maxlength="200" autocomplete="off"
             placeholder="Email or handle, only if you want a follow-up">
      <div class="meta">Shown publicly on the issue. Leave blank to stay fully anonymous.</div>
    </div>

    <div class="hp" aria-hidden="true">
      <label for="website">Website</label>
      <input id="website" name="website" type="text" tabindex="-1" autocomplete="off">
    </div>

    <div>
      <button type="submit" id="send">Send report</button>
      <div id="status" role="status" aria-live="polite"></div>
    </div>
  </form>

  <footer>Reports are rate limited (3 per hour, 10 per day per connection) to keep the
  automated triage queue healthy. Anonymous submissions are labelled <code>from-web</code>.</footer>
</main>
<script>
  var form = document.getElementById("intake");
  var status = document.getElementById("status");
  var send = document.getElementById("send");
  var body = document.getElementById("body");
  var count = document.getElementById("count");

  body.addEventListener("input", function () { count.textContent = body.value.length; });

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    status.className = "";
    status.textContent = "";
    send.disabled = true;
    send.textContent = "Sending\u2026";

    var payload = {
      type: (form.querySelector('input[name="type"]:checked') || {}).value || "",
      title: document.getElementById("title").value,
      body: body.value,
      contact: document.getElementById("contact").value,
      website: document.getElementById("website").value
    };

    fetch("/", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(payload)
    }).then(function (res) {
      return res.json().catch(function () { return {}; }).then(function (data) {
        return { status: res.status, data: data };
      });
    }).then(function (result) {
      if (result.status === 200 && result.data && result.data.ok) {
        form.reset();
        count.textContent = "0";
        status.className = "ok";
        status.textContent = "Thanks — your report was filed. It will be triaged shortly.";
      } else {
        status.className = "err";
        status.textContent = "Sorry, that did not go through: " +
          ((result.data && result.data.error) || "unexpected server response") + " (" + result.status + ")";
      }
    }).catch(function () {
      status.className = "err";
      status.textContent = "Network error — check your connection and try again.";
    }).finally(function () {
      send.disabled = false;
      send.textContent = "Send report";
    });
  });
</script>
</body>
</html>
`;
