//! The dashboard's single, dependency-free HTML page: vanilla JS,
//! `fetch("/api/state")` for the snapshot, `EventSource("/events")` for
//! live updates (each event also triggers a state re-fetch, so a lagged
//! SSE subscription self-heals). No external assets -- the daemon must
//! render this page with no network access beyond itself.
//!
//! **Do not replace the re-fetch with a client-side reducer.** Applying
//! events incrementally in the browser looks like the obvious
//! optimisation, and it would require mirroring the server's projection
//! semantics here exactly and forever -- two reducers that drift. It would
//! also break the self-healing property above: a viewer that missed an
//! event would stay wrong instead of correcting on the next one. If the
//! per-event re-fetch ever becomes a cost worth addressing, debounce it;
//! do not reduce here. See the parent module's own note.

pub const PAGE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>crew dashboard</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0; background: #101216; color: #d7dae0; font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; }
  header { padding: 14px 20px; border-bottom: 1px solid #23262d; display: flex; gap: 12px; align-items: center; }
  /* BRAND.md §06 horizontal lockup: mark left, wordmark right, gap of one
     cell. One cell is 5 of the mark's 32 grid units, so 5px at 32px. §04's
     clear space (also one cell) is covered by the header's own padding. */
  header .lockup { display: flex; align-items: center; gap: 5px; }
  /* §04: 32px is the floor when the mark sits beside the wordmark. */
  header .lockup svg { display: block; width: 32px; height: 32px; }
  /* §05: lowercase, medium weight, -0.03em tracking. Inter is named first
     but never fetched -- this page loads no external asset, so it falls
     through to the platform's own UI sans. */
  header h1 { font-size: 20px; margin: 0; color: #fff; font-family: Inter, ui-sans-serif, system-ui, -apple-system, sans-serif; font-weight: 500; letter-spacing: -0.03em; text-transform: lowercase; }
  header .live { font-size: 12px; color: #6a7382; }
  header .live.on { color: #57d38c; }
  main { padding: 20px; display: grid; gap: 24px; max-width: 1100px; }
  h2 { font-size: 13px; text-transform: uppercase; letter-spacing: .08em; color: #8a93a4; margin: 0 0 10px; }
  .cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(240px, 1fr)); gap: 10px; }
  .card { background: #171a20; border: 1px solid #23262d; border-radius: 8px; padding: 10px 12px; }
  .card .title { color: #fff; margin-bottom: 4px; overflow: hidden; text-overflow: ellipsis; }
  .card .meta { font-size: 12px; color: #8a93a4; }
  /* The per-runtime glyph is BRAND.md's own vocabulary rather than a
     third-party logo: one cell, one colour, one runtime (§02). Square
     corners and a flat fill, because §08 forbids rounding the cells,
     gradients on them, and setting the mark in a pill or tile. */
  .card .runtime { display: flex; align-items: center; gap: 6px; margin-bottom: 4px; font-size: 12px; }
  .cell { width: 10px; height: 10px; flex: none; }
  td .cell { display: inline-block; vertical-align: -1px; margin-right: 6px; }
  /* The task cell absorbs the table's spare width and clips with an
     ellipsis. The server already bounds the string; how much of it fits is
     the viewport's business, which only the browser knows. The full first
     line stays available as the cell's tooltip. */
  td.task { max-width: 0; width: 40%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  td.task:empty::after { content: "—"; color: #5a6270; }
  table { width: 100%; border-collapse: collapse; }
  th, td { text-align: left; padding: 6px 10px; border-bottom: 1px solid #1c1f26; font-size: 13px; }
  /* Every cell stays on one line so rows keep a uniform height; the task
     cell is the only one allowed to absorb slack, and it clips rather than
     wraps. Without this the wide task column squeezes `worker` and `cost`
     into two-line cells and the table goes ragged. */
  td { white-space: nowrap; }
  th { color: #8a93a4; font-weight: normal; }
  .state { padding: 1px 8px; border-radius: 10px; font-size: 12px; background: #23262d; }
  .state.working { background: #1d3a5f; color: #7fb7ff; }
  .state.succeeded { background: #14351f; color: #57d38c; }
  .state.failed, .state.lost { background: #3d1a1e; color: #ff8f8f; }
  .state.queued { background: #33301a; color: #e6d37a; }
  #feed { background: #0c0e12; border: 1px solid #23262d; border-radius: 8px; padding: 10px 12px; max-height: 280px; overflow-y: auto; font-size: 12px; }
  #feed div { color: #8a93a4; padding: 1px 0; }
  .empty { color: #5a6270; font-size: 13px; }
</style>
</head>
<body>
<header>
  <span class="lockup">
    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" width="32" height="32" role="img" aria-label="Crew">
      <title>Crew</title>
      <defs>
        <linearGradient id="crewBar" gradientUnits="userSpaceOnUse" x1="0.5" y1="0" x2="31.5" y2="0">
          <stop offset="0" stop-color="#f74fcc"></stop>
          <stop offset="1" stop-color="#9362f4"></stop>
        </linearGradient>
      </defs>
      <rect x="0.5" y="0.5" width="31" height="5" fill="url(#crewBar)"></rect>
      <rect x="7" y="7" width="5" height="5" fill="#d97757"></rect>
      <rect x="7" y="13.5" width="5" height="5" fill="#d9a441"></rect>
      <rect x="7" y="20" width="5" height="5" fill="#524b8e"></rect>
      <rect x="20" y="7" width="5" height="5" fill="#fdfcfc"></rect>
      <rect x="20" y="13.5" width="5" height="5" fill="#58a6ff"></rect>
      <rect x="20" y="20" width="5" height="5" fill="#10a37f"></rect>
      <rect x="20" y="26.5" width="5" height="5" fill="#28bae7"></rect>
    </svg>
    <h1>crew</h1>
  </span>
  <span id="live" class="live">connecting…</span>
</header>
<main>
  <section>
    <h2>Runs</h2>
    <table>
      <thead><tr><th>run</th><th>state</th><th>worker</th><th>task</th><th>cost</th><th>started</th><th>completed</th></tr></thead>
      <tbody id="runs"><tr><td colspan="7" class="empty">loading…</td></tr></tbody>
    </table>
  </section>
  <section>
    <h2>Workers</h2>
    <div id="workers" class="cards"><span class="empty">loading…</span></div>
  </section>
  <section>
    <h2>Escalations</h2>
    <div id="escalations" class="empty">none pending</div>
  </section>
  <section>
    <h2>Event feed</h2>
    <div id="feed"></div>
  </section>
</main>
<script>
  // The TAIL of the id, not the head. Every id here is a UUIDv7, whose
  // leading bits are a millisecond timestamp -- so ids minted in the same
  // burst share a prefix, and crew mints them in bursts. Six runs fanned
  // out together all rendered as the same eight characters until this took
  // the random tail instead.
  const short = id => (id || "").replace(/-/g, "").slice(-8);
  // Timestamps are RFC3339 with nanoseconds, which is unreadable in a
  // table and drowns the columns that carry meaning. Show the time; keep
  // the full value in the cell's tooltip, where precision belongs.
  const clock = ts => {
    if (!ts) return "–";
    const match = /T(\d{2}:\d{2}:\d{2})/.exec(ts);
    return match ? match[1] : ts;
  };
  const esc = text => { const div = document.createElement("div"); div.textContent = text ?? ""; return div.innerHTML; };

  // BRAND.md §02 -- one cell, one colour, one runtime, each taken from
  // that tool's own brand. A name absent from this table renders NEUTRAL:
  // an unknown runtime never gets a guessed brand colour. Dark ground
  // only (§03), which this page is.
  //
  // `hermes` and `opencode` have §02 colours but no adapter here yet; they
  // are listed so adding one needs no change to this table.
  //
  // `ompRpc` and `omp` are BOTH correct and neither is redundant: `ompRpc`
  // is `AdapterKind::wire_name`, which is what a worker row actually
  // carries and therefore the key that fires here, while `omp` is the
  // *config-file* key (`RESERVED_ADAPTER_CONFIG_KEYS`, config/crew.rs).
  // Same runtime, two spellings by design -- do not delete one as a
  // duplicate.
  const BRAND = {
    claude: "#d97757",
    hermes: "#d9a441",
    opencode: "#fdfcfc",
    copilot: "#58a6ff",
    codex: "#10a37f",
    ompRpc: "#28bae7",
    omp: "#28bae7",
  };
  const NEUTRAL = "#8a93a4";
  const brand = adapter => BRAND[adapter] || NEUTRAL;
  // `adapter · model` is the worker label until workers are nameable; the
  // id stays reachable as the cell's tooltip. A run whose worker row is
  // missing has no adapter at all, so it shows the id in the neutral
  // colour rather than a runtime it cannot prove.
  const runtimeLabel = row => (row.adapter ? `${row.adapter} · ${row.model ?? "?"}` : null);

  // Render exactly what the vendor reported, and nothing else. claude
  // reports tokens and a cost; codex reports tokens and never a price;
  // copilot reports neither under ACP v1. A dollar figure is NEVER derived
  // from tokens at an assumed rate -- that would be a number nobody
  // reported, shown in the one place a reader trusts a number. Absence is
  // an em-dash, never $0.00: zero is a reported figure and this is not one.
  const money = usd => `$${usd.toFixed(2)}`;
  const tokens = n => (n >= 1000 ? `${(n / 1000).toFixed(1)}k tok` : `${n} tok`);
  const spendOf = usage => {
    if (!usage) return "—";
    if (typeof usage.costUsd === "number") return money(usage.costUsd);
    const total = (usage.inputTokens || 0) + (usage.outputTokens || 0);
    return total > 0 ? tokens(total) : "—";
  };
  // The tooltip carries the tokens behind a price, and names the reason a
  // cell is empty -- "nothing reported" is a fact worth stating, since a
  // blank cell otherwise reads as a bug.
  const costTitle = usage => {
    if (!usage) return "this run's vendor reported no usage";
    const parts = [`${usage.inputTokens || 0} in / ${usage.outputTokens || 0} out`];
    if (typeof usage.costUsd !== "number") parts.push("no cost reported by this vendor");
    return parts.join(" · ");
  };
  // A worker's total says what it covers whenever coverage is partial. A
  // bare sum over runs where only some vendors reported understates the
  // real spend, and the reader cannot see that from the figure alone. A
  // worker whose every run reported shows the clean number, because there
  // the figure really is the whole story.
  const workerSpend = spend => {
    if (!spend || !spend.runsTotal) return "—";
    const priced = typeof spend.costUsd === "number";
    const totalTokens = (spend.inputTokens || 0) + (spend.outputTokens || 0);
    const figure = priced ? money(spend.costUsd) : (totalTokens > 0 ? tokens(totalTokens) : null);
    if (figure === null) return "—";
    const covered = priced ? spend.runsReportingCost : spend.runsReportingTokens;
    return covered < spend.runsTotal
      ? `${figure} (${covered} of ${spend.runsTotal} runs reported)`
      : figure;
  };

  async function refresh() {
    const response = await fetch("/api/state");
    if (!response.ok) return;
    const state = await response.json();

    const runs = state.runs || [];
    document.getElementById("runs").innerHTML = runs.length
      ? runs.map(run => {
          const label = runtimeLabel(run);
          const colour = brand(run.adapter);
          return `<tr>
          <td title="${esc(run.runId)}">${esc(short(run.runId))}</td>
          <td><span class="state ${esc(run.state)}">${esc(run.state)}</span></td>
          <td title="${esc(run.workerId)}" style="color:${colour}"><span class="cell" style="background:${colour}"></span>${esc(label || short(run.workerId))}</td>
          <td class="task" title="${esc(run.taskSummary || "")}">${esc(run.taskSummary || "—")}</td>
          <td class="spend" title="${esc(costTitle(run.usage))}">${esc(spendOf(run.usage))}</td>
          <td title="${esc(run.startedAt || "")}">${esc(clock(run.startedAt))}</td>
          <td title="${esc(run.completedAt || "")}">${esc(clock(run.completedAt))}</td>
        </tr>`;
        }).join("")
      : `<tr><td colspan="7" class="empty">no runs yet</td></tr>`;

    const workers = state.workers || [];
    document.getElementById("workers").innerHTML = workers.length
      ? workers.map(worker => {
          const adapter = worker.profileRef?.adapter;
          const colour = brand(adapter);
          return `<div class="card">
          <div class="title" title="${esc(worker.workerId)}">${esc(short(worker.workerId))}</div>
          <div class="runtime" style="color:${colour}">
            <span class="cell" style="background:${colour}"></span>
            <span>${esc(runtimeLabel(worker.profileRef || {}) || "unknown runtime")}</span>
          </div>
          <div class="meta">spend ${esc(workerSpend(worker.spend))}</div>
          <div class="meta" title="${esc(worker.createdAt)}">created ${esc(clock(worker.createdAt))}</div>
        </div>`;
        }).join("")
      : `<span class="empty">no workers yet</span>`;

    const escalations = state.pendingEscalations || [];
    document.getElementById("escalations").innerHTML = escalations.length
      ? escalations.map(item => `<div class="card">${esc(item.kind)} · run ${esc(short(item.runId))}: ${esc(item.question)}</div>`).join("")
      : `<span class="empty">none pending</span>`;
  }

  function follow() {
    const live = document.getElementById("live");
    const feed = document.getElementById("feed");
    const source = new EventSource("/events");
    // "reconnecting" is only honest while a reconnect can plausibly
    // succeed. The daemon idle-exits when nothing is connected over IPC and
    // no runs are live, and an open viewer deliberately does NOT count as
    // activity -- a passive page must not pin a daemon alive. So after a
    // few failed retries, say the daemon is gone instead of implying a
    // transient blip the browser will fix. EventSource keeps retrying
    // either way; only the label changes.
    let failures = 0;
    source.onopen = () => {
      failures = 0;
      live.textContent = "live";
      live.classList.add("on");
      live.title = "receiving events from the daemon";
    };
    source.onerror = () => {
      failures += 1;
      live.classList.remove("on");
      if (failures < 3) {
        live.textContent = "reconnecting…";
        live.title = "the event stream dropped; retrying";
      } else {
        live.textContent = "daemon not running";
        live.title =
          "the daemon is not reachable. It exits when idle, and an open dashboard does not keep it alive. " +
          "Start work in the repository (or run crewd serve) and reload.";
      }
    };
    source.onmessage = message => {
      let label = "event";
      try {
        const envelope = JSON.parse(message.data);
        const kind = envelope.event?.type || envelope.event?.kind || Object.keys(envelope.event || {})[0] || "event";
        label = `#${envelope.sequence} ${kind}` + (envelope.runId ? ` · run ${short(envelope.runId)}` : "");
      } catch { /* render the placeholder label */ }
      const row = document.createElement("div");
      row.textContent = `${new Date().toLocaleTimeString()} ${label}`;
      feed.prepend(row);
      while (feed.childElementCount > 200) feed.lastElementChild.remove();
      refresh();
    };
  }

  refresh();
  follow();
</script>
</body>
</html>
"##;
