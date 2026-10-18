// /api/field-report — HTTP FALLBACK for filing an agent field report when the
// canonical GitHub PR path is unavailable.
//
// ──────────────────────────── WHAT THIS IS ──────────────────────────────
// The CANONICAL way to file a field report is a GitHub pull request:
//     xerj feedback --open-pr      →  user-feedback/16-agent-field-reports/
// That path STAYS primary. This endpoint exists ONLY for the agent persona
// that cannot reach it: no shell, no git, no `gh`, no GitHub identity (e.g. a
// sandboxed Claude Code). It was proposed in
// docs/research/llms-txt-2026-09/ (POST https://xerj.org/api/field-report)
// and is the "GitHub-unavailable fallback", nothing more. Every success
// response points the caller back at the GitHub tree.
//
// ─────────────────────────────── CONTRACT ───────────────────────────────
//   POST https://xerj.org/api/field-report
//   Content-Type: application/json
//   Body (all values are short strings):
//     { "agent": "...", "used_for": "...", "verdict": "...",      // required
//       "version": "...", "os": "...", "indexed": "...",          // optional
//       "notes": "..." }                                          // optional
//   200 { ok:true, stored:true, note:"...GitHub PR is canonical..." }
//
//   Method other than POST/OPTIONS .......... 405
//   Content-Type not application/json ....... 415
//   Body larger than ~16 KB ................. 413
//   Malformed JSON / missing required field . 400
//   Same IP again within 60s ................ 429  (rate limit)
//   Daily safety cap reached ................ 503  (use the GitHub PR path)
//
// ────────────────────────── FREE-TIER / ANTI-SPAM ───────────────────────
// Storage is Workers KV (binding FIELD_REPORTS), never R2 (R2 is frozen by
// owner order). KV free tier = 1000 writes/day, 100k reads/day, 1 GB. Two
// guards keep us inside it and stop spam from causing a bill:
//   1. RATE LIMIT — at most 1 accepted report per IP per minute
//      (key `rl:<ip>`, 60s TTL). A second request inside 60s → 429.
//   2. DAILY CAP — an accepted-report counter `cnt:<YYYY-MM-DD>` (2-day TTL).
//      When it reaches DAILY_CAP we DROP new reports with 503 and tell the
//      caller to use the GitHub PR path. This is the deliberate "drop new
//      reports if going over, to prevent overages by spam" rule.
// Each ACCEPTED report costs exactly 3 KV writes: the rate-limit marker, the
// report itself, and the counter bump. DAILY_CAP is sized so 3×cap stays
// well under the 1000 writes/day limit (see the constant below). Rejected
// requests (405/415/413/400/429/503) perform NO writes — at most two cheap
// reads — so a flood of junk cannot burn the write budget or the wallet.
//
// ─────────────────────────────── PRIVACY ────────────────────────────────
// The raw IP is NEVER stored. We store sha-256(IP + salt) as `ip_hash` so
// reports from the same source can be grouped without retaining the address.
// No secrets are stored; untrusted input is length-capped, control-char
// stripped, and never echoed back as HTML.
//
// ─────────────────────────────── ROUTING ────────────────────────────────
// This file lives at functions/api/field-report.js, so Pages Functions serve
// it at /api/field-report (same file-path→URL convention by which
// functions/lead.js serves /lead). landing/_routes.json MUST list
// "/api/field-report" in its include array or the static asset handler
// shadows it and this function never runs.

// ── Tunables ─────────────────────────────────────────────────────────────
// 300 accepted reports/day × 3 KV writes each (rate-limit marker + report +
// counter) = 900 writes/day, deliberately well under the 1000/day KV free
// tier so spam can never push the account into an overage. The owner's stated
// requirement ("well under the 1000/day KV write limit") binds over the
// example value; bump this only after confirming the KV write quota.
const DAILY_CAP = 300;
const RATE_LIMIT_TTL = 60; // seconds; one accepted report per IP per minute
const COUNTER_TTL = 172800; // 2 days; the daily counter self-expires
const MAX_BODY = 16 * 1024; // 16 KB hard cap on the request body

// Per-field length caps (all values must be short strings). Their sum is far
// below MAX_BODY, so an oversized body is rejected before we ever parse it.
const FIELD_CAPS = {
  agent: 120,
  used_for: 1000,
  verdict: 80,
  version: 80,
  os: 120,
  indexed: 200,
  notes: 4000,
};
const REQUIRED = ['agent', 'used_for', 'verdict'];
const OPTIONAL = ['version', 'os', 'indexed', 'notes'];

const CANONICAL_NOTE =
  'Thanks. The canonical path is a GitHub PR: ' +
  'https://github.com/xerj-org/xerj/tree/main/user-feedback/16-agent-field-reports';

// Permissive CORS: agents call this cross-origin from arbitrary contexts.
const CORS = {
  'access-control-allow-origin': '*',
  'access-control-allow-methods': 'POST, OPTIONS',
  'access-control-allow-headers': 'content-type',
  'access-control-max-age': '86400',
};
const JSON_HEADERS = { 'content-type': 'application/json', 'cache-control': 'no-store', ...CORS };

function json(obj, status = 200) {
  return new Response(JSON.stringify(obj), { status, headers: JSON_HEADERS });
}

// Strip control characters, collapse to a plain string, trim, and length-cap.
// Never returns HTML-significant content unescaped because we never render it;
// this only keeps stored values short and free of control bytes.
function clean(value, cap) {
  return String(value)
    .replace(/[\u0000-\u001f\u007f]/g, ' ')
    .trim()
    .slice(0, cap);
}

async function sha256hex(input) {
  const data = new TextEncoder().encode(input);
  const digest = await crypto.subtle.digest('SHA-256', data);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

// Single entry point so we control every status code (including the 405) and
// attach CORS headers uniformly.
export async function onRequest(context) {
  const { request } = context;
  const method = request.method.toUpperCase();

  if (method === 'OPTIONS') {
    // CORS preflight — no body, just the allow headers.
    return new Response(null, { status: 204, headers: CORS });
  }
  if (method !== 'POST') {
    // Method guard: POST only.
    return json({ ok: false, error: 'method not allowed; use POST' }, 405);
  }
  return handlePost(context);
}

async function handlePost({ request, env }) {
  // Content-Type guard.
  const ct = (request.headers.get('content-type') || '').toLowerCase();
  if (!ct.includes('application/json')) {
    return json({ ok: false, error: 'content-type must be application/json' }, 415);
  }

  // Body size cap — reject early on the declared length when present.
  const declared = parseInt(request.headers.get('content-length') || '0', 10);
  if (Number.isFinite(declared) && declared > MAX_BODY) {
    return json({ ok: false, error: 'body too large' }, 413);
  }

  // Read the raw body and enforce the cap again on the actual bytes (a lying
  // or absent content-length cannot sneak past).
  let raw;
  try {
    raw = await request.text();
  } catch (_) {
    return json({ ok: false, error: 'could not read body' }, 400);
  }
  if (raw.length > MAX_BODY) {
    return json({ ok: false, error: 'body too large' }, 413);
  }

  // Parse JSON.
  let body;
  try {
    body = JSON.parse(raw);
  } catch (_) {
    return json({ ok: false, error: 'invalid JSON body' }, 400);
  }
  if (body === null || typeof body !== 'object' || Array.isArray(body)) {
    return json({ ok: false, error: 'body must be a JSON object' }, 400);
  }

  // Validate + sanitise fields. Every field must be a short string; anything
  // that is not a string is rejected rather than coerced.
  const report = {};
  for (const key of REQUIRED) {
    const v = body[key];
    if (typeof v !== 'string' || clean(v, FIELD_CAPS[key]).length === 0) {
      return json({ ok: false, error: `missing or invalid required field: ${key}` }, 400);
    }
    report[key] = clean(v, FIELD_CAPS[key]);
  }
  for (const key of OPTIONAL) {
    if (body[key] === undefined || body[key] === null) {
      report[key] = '';
      continue;
    }
    if (typeof body[key] !== 'string') {
      return json({ ok: false, error: `field must be a string: ${key}` }, 400);
    }
    report[key] = clean(body[key], FIELD_CAPS[key]);
  }

  // Without the KV binding we cannot rate-limit, count, or store. Degrade like
  // functions/lead.js does when its bucket is absent: accept-but-do-not-store,
  // no writes, and still hand back the canonical GitHub pointer. This is the
  // state before the owner runs the one-time `wrangler kv namespace create`.
  if (!env.FIELD_REPORTS) {
    console.log('[field-report] no FIELD_REPORTS KV binding; not stored');
    return json({ ok: true, stored: false, note: CANONICAL_NOTE });
  }
  const kv = env.FIELD_REPORTS;

  const ip = request.headers.get('CF-Connecting-IP') || '';

  // ── Guard 1: rate limit (1 accepted report per IP per minute) ──────────
  // A read; no write unless the request is ultimately accepted below.
  const rlKey = `rl:${ip}`;
  try {
    if (await kv.get(rlKey)) {
      return json({ ok: false, error: 'rate limited: one report per minute per source' }, 429);
    }
  } catch (_) {
    // Read failure: fail closed on the rate limiter is unfriendly; fail open
    // but the daily cap below still bounds total writes.
  }

  // ── Guard 2: daily safety cap (anti-overage) ───────────────────────────
  // Approximate counter (KV has no atomic increment; the 1-per-minute rate
  // limit makes races negligible, and the cap sits far below the write quota).
  const today = new Date().toISOString().slice(0, 10); // YYYY-MM-DD
  const cntKey = `cnt:${today}`;
  let count = 0;
  try {
    count = parseInt((await kv.get(cntKey)) || '0', 10) || 0;
  } catch (_) {
    count = 0;
  }
  if (count >= DAILY_CAP) {
    // Over the safety cap: DROP the report to prevent KV-write overages from
    // spam, and tell the caller to use the canonical GitHub PR path instead.
    return json(
      {
        ok: false,
        error: 'daily capacity reached; please file via the GitHub PR path',
        note: CANONICAL_NOTE,
      },
      503
    );
  }

  // ── Accept: 3 KV writes (rate-limit marker, report, counter bump) ──────
  const now = new Date();
  const iso = now.toISOString();
  const rand = (crypto.randomUUID ? crypto.randomUUID() : `${Math.random()}`).slice(0, 8);
  const key = `report:${today}T${now.getTime()}-${rand}`;

  const stored = {
    received_at: iso,
    ip_hash: ip ? await sha256hex(ip + (env.FIELD_REPORT_SALT || 'xerj-field-report-v1')) : '',
    agent: report.agent,
    used_for: report.used_for,
    verdict: report.verdict,
    version: report.version,
    os: report.os,
    indexed: report.indexed,
    notes: report.notes,
    ua: clean(request.headers.get('user-agent') || '', 300),
  };

  try {
    // Reserve the per-IP minute slot first so a burst of concurrent valid
    // requests from one IP cannot all slip through.
    if (ip) await kv.put(rlKey, '1', { expirationTtl: RATE_LIMIT_TTL });
    await kv.put(key, JSON.stringify(stored));
    await kv.put(cntKey, String(count + 1), { expirationTtl: COUNTER_TTL });
  } catch (e) {
    console.log('[field-report] KV write failed:', String(e));
    return json({ ok: false, error: 'storage unavailable; please file via the GitHub PR path', note: CANONICAL_NOTE }, 503);
  }

  return json({ ok: true, stored: true, note: CANONICAL_NOTE });
}
