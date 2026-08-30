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
  header { padding: 14px 20px; border-bottom: 1px solid #23262d; display: flex; gap: 12px; align-items: baseline; }
  header h1 { font-size: 16px; margin: 0; color: #fff; }
  header .live { font-size: 12px; color: #6a7382; }
  header .live.on { color: #57d38c; }
  main { padding: 20px; display: grid; gap: 24px; max-width: 1100px; }
  h2 { font-size: 13px; text-transform: uppercase; letter-spacing: .08em; color: #8a93a4; margin: 0 0 10px; }
  .cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(240px, 1fr)); gap: 10px; }
  .card { background: #171a20; border: 1px solid #23262d; border-radius: 8px; padding: 10px 12px; }
  .card .title { color: #fff; margin-bottom: 4px; overflow: hidden; text-overflow: ellipsis; }
  .card .meta { font-size: 12px; color: #8a93a4; }
  table { width: 100%; border-collapse: collapse; }
  th, td { text-align: left; padding: 6px 10px; border-bottom: 1px solid #1c1f26; font-size: 13px; }
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
  <h1>crew</h1>
  <span id="live" class="live">connecting…</span>
</header>
<main>
  <section>
    <h2>Runs</h2>
    <table>
      <thead><tr><th>run</th><th>state</th><th>worker</th><th>started</th><th>completed</th></tr></thead>
      <tbody id="runs"><tr><td colspan="5" class="empty">loading…</td></tr></tbody>
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
  const short = id => (id || "").slice(0, 8);
  const esc = text => { const div = document.createElement("div"); div.textContent = text ?? ""; return div.innerHTML; };

  async function refresh() {
    const response = await fetch("/api/state");
    if (!response.ok) return;
    const state = await response.json();

    const runs = state.runs || [];
    document.getElementById("runs").innerHTML = runs.length
      ? runs.map(run => `<tr>
          <td title="${esc(run.runId)}">${esc(short(run.runId))}</td>
          <td><span class="state ${esc(run.state)}">${esc(run.state)}</span></td>
          <td title="${esc(run.workerId)}">${esc(short(run.workerId))}</td>
          <td>${esc(run.startedAt || "–")}</td>
          <td>${esc(run.completedAt || "–")}</td>
        </tr>`).join("")
      : `<tr><td colspan="5" class="empty">no runs yet</td></tr>`;

    const workers = state.workers || [];
    document.getElementById("workers").innerHTML = workers.length
      ? workers.map(worker => `<div class="card">
          <div class="title" title="${esc(worker.workerId)}">${esc(short(worker.workerId))}</div>
          <div class="meta">${esc(worker.profileRef?.adapter)} · ${esc(worker.profileRef?.model)}</div>
          <div class="meta">created ${esc(worker.createdAt)}</div>
        </div>`).join("")
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
