// xerj-metrics — records GitHub traffic (clones/views) and website /get
// installs into R2 once a day, and serves a small charts dashboard.
//
// Why: GitHub's traffic API only returns a rolling 14 days, so the numbers are
// lost if nobody snapshots them. The cron below merges the current 14-day
// window into metrics/history.json on every run (upsert by date), so history
// accumulates indefinitely. Installs are counted from the installs/<date>/
// objects the installer already writes. No PII is stored or read.
//
// Routes:
//   GET /            -> the dashboard (HTML + Chart.js)
//   GET /data.json   -> the accumulated history as JSON (CORS open)
//   GET /refresh?key=REFRESH_KEY -> run a snapshot now (for seeding/manual runs)
//
// Secrets (wrangler secret put ...): GITHUB_TOKEN (a PAT with traffic read),
// REFRESH_KEY (any string, gates /refresh).

const OWNER = "xerj-org";
const REPO = "xerj";
const KEY = "metrics/history.json";

async function ghTraffic(env, kind) {
  const r = await fetch(`https://api.github.com/repos/${OWNER}/${REPO}/traffic/${kind}`, {
    headers: {
      Authorization: `Bearer ${env.GITHUB_TOKEN}`,
      "User-Agent": "xerj-metrics",
      Accept: "application/vnd.github+json",
    },
  });
  if (!r.ok) throw new Error(`github ${kind} ${r.status}`);
  return r.json();
}

async function countInstalls(env, date) {
  let count = 0, cursor;
  do {
    const list = await env.INSTALLS.list({ prefix: `installs/${date}/`, cursor, limit: 1000 });
    count += list.objects.length;
    cursor = list.truncated ? list.cursor : undefined;
  } while (cursor);
  return count;
}

async function loadHistory(env) {
  const obj = await env.INSTALLS.get(KEY);
  return obj ? await obj.json() : { days: {} };
}

async function snapshot(env) {
  const hist = await loadHistory(env);
  hist.days = hist.days || {};
  const touch = (day) => (hist.days[day] = hist.days[day] || { date: day });

  const clones = await ghTraffic(env, "clones");
  for (const d of clones.clones || []) {
    const day = d.timestamp.slice(0, 10);
    const e = touch(day);
    e.clones = d.count;
    e.cloneUniques = d.uniques;
  }
  const views = await ghTraffic(env, "views");
  for (const d of views.views || []) {
    const day = d.timestamp.slice(0, 10);
    const e = touch(day);
    e.views = d.count;
    e.viewUniques = d.uniques;
  }

  // Count installs for every day in the traffic window plus today. Past days
  // are immutable, but recounting is cheap and self-heals a missed run.
  const days = new Set();
  for (const d of clones.clones || []) days.add(d.timestamp.slice(0, 10));
  for (const d of views.views || []) days.add(d.timestamp.slice(0, 10));
  days.add(new Date().toISOString().slice(0, 10));
  for (const day of days) {
    touch(day).installs = await countInstalls(env, day);
  }

  hist.updated = new Date().toISOString();
  await env.INSTALLS.put(KEY, JSON.stringify(hist), {
    httpMetadata: { contentType: "application/json" },
  });
  return hist;
}

const DASHBOARD = `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>XERJ Metrics</title>
<style>
:root{--bg:#f7f5ef;--ink:#17150f;--mute:#6b6658;--line:#d9d4c6;--accent:#b5771f;--blue:#2b6ca3}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);
font-family:ui-sans-serif,system-ui,-apple-system,"Inter",sans-serif;padding:32px 20px 64px}
.wrap{max-width:1000px;margin:0 auto}
h1{font-size:clamp(28px,5vw,44px);font-weight:900;letter-spacing:-.02em;margin:0 0 4px}
.sub{color:var(--mute);font-size:13px;text-transform:uppercase;letter-spacing:.08em;margin:0 0 24px}
.stats{display:flex;flex-wrap:wrap;gap:1px;background:var(--line);border:1px solid var(--line);margin-bottom:28px}
.stat{background:var(--bg);padding:16px 20px;flex:1 1 150px}
.stat b{display:block;font-size:34px;font-weight:800;line-height:1}
.stat span{font-size:11px;text-transform:uppercase;letter-spacing:.08em;color:var(--mute)}
.card{background:#fff;border:1px solid var(--line);padding:18px 18px 8px;margin-bottom:22px}
.card h2{font-size:13px;text-transform:uppercase;letter-spacing:.08em;color:var(--mute);margin:0 0 12px}
canvas{width:100%!important;height:300px!important}
.foot{color:var(--mute);font-size:12px;margin-top:20px}
a{color:var(--accent)}
</style></head><body><div class="wrap">
<h1>XERJ metrics</h1>
<p class="sub" id="updated">loading…</p>
<div class="stats" id="stats"></div>
<div class="card"><h2>GitHub traffic — clones &amp; views per day</h2><canvas id="traffic"></canvas></div>
<div class="card"><h2>Installer runs per day (website /get)</h2><canvas id="installs"></canvas></div>
<p class="foot">Snapshotted daily from the GitHub traffic API (which only keeps 14 days) and the privacy-preserving installer log. No IPs, cookies or identifiers. <a href="data.json">raw data</a>.</p>
</div>
<script src="https://cdnjs.cloudflare.com/ajax/libs/Chart.js/4.4.1/chart.umd.min.js"></script>
<script>
(async function(){
  var h = await fetch('data.json').then(function(r){return r.json()});
  var days = Object.keys(h.days||{}).sort();
  var rows = days.map(function(d){return h.days[d]});
  function col(k){return rows.map(function(r){return r[k]||0})}
  document.getElementById('updated').textContent = 'updated ' + (h.updated||'').slice(0,16).replace('T',' ') + ' UTC · ' + days.length + ' days recorded';
  var totClones = col('clones').reduce(function(a,b){return a+b},0);
  var totViews = col('views').reduce(function(a,b){return a+b},0);
  var totInst = col('installs').reduce(function(a,b){return a+b},0);
  var yday = rows[rows.length-2]||rows[rows.length-1]||{};
  var stats = [['clones (all recorded)',totClones],['views (all recorded)',totViews],['installs (all recorded)',totInst],['installs ('+(yday.date||'').slice(5)+')',yday.installs||0]];
  document.getElementById('stats').innerHTML = stats.map(function(s){return '<div class="stat"><b>'+s[1].toLocaleString()+'</b><span>'+s[0]+'</span></div>'}).join('');
  var grid={color:'#ece8dc'}, tick={color:'#6b6658',font:{size:11}};
  new Chart(document.getElementById('traffic'),{type:'line',data:{labels:days,datasets:[
    {label:'clones',data:col('clones'),borderColor:'#b5771f',backgroundColor:'rgba(181,119,31,.08)',tension:.25,fill:true},
    {label:'views',data:col('views'),borderColor:'#2b6ca3',backgroundColor:'rgba(43,108,163,.06)',tension:.25,fill:true}
  ]},options:{responsive:true,maintainAspectRatio:false,scales:{x:{grid:grid,ticks:tick},y:{grid:grid,ticks:tick,beginAtZero:true}},plugins:{legend:{labels:{color:'#17150f'}}}}});
  new Chart(document.getElementById('installs'),{type:'bar',data:{labels:days,datasets:[
    {label:'installs',data:col('installs'),backgroundColor:'#b5771f'}
  ]},options:{responsive:true,maintainAspectRatio:false,scales:{x:{grid:grid,ticks:tick},y:{grid:grid,ticks:tick,beginAtZero:true}},plugins:{legend:{display:false}}}});
})();
</script></body></html>`;

export default {
  async scheduled(event, env, ctx) {
    ctx.waitUntil(snapshot(env));
  },
  async fetch(req, env) {
    const url = new URL(req.url);
    if (url.pathname === "/data.json") {
      const hist = await loadHistory(env);
      return new Response(JSON.stringify(hist), {
        headers: { "content-type": "application/json", "access-control-allow-origin": "*", "cache-control": "max-age=300" },
      });
    }
    if (url.pathname === "/refresh") {
      if (!env.REFRESH_KEY || url.searchParams.get("key") !== env.REFRESH_KEY) {
        return new Response("forbidden", { status: 403 });
      }
      const hist = await snapshot(env);
      return new Response(JSON.stringify({ ok: true, days: Object.keys(hist.days).length, updated: hist.updated }), {
        headers: { "content-type": "application/json" },
      });
    }
    return new Response(DASHBOARD, { headers: { "content-type": "text/html; charset=utf-8" } });
  },
};
