/* global d3 */
import { auth, fhirGet, login, VitalsStream } from "./api.js";
import { TimeSeriesChart } from "./chart.js";
import { alertLabels, bundleToPoints, decodeFrame } from "./protocol.js";

const $ = (id) => document.getElementById(id);
const MAX_POINTS = 3600;
const fmt1 = d3.format(".1f");
const fmt0 = d3.format(".0f");

const state = {
  patients: [], allowed: [], current: null, windowMin: 5, paused: false,
  buffers: new Map(), loaded: new Set(), lastFlags: new Map(), alertCount: 0,
  showTable: false,
};
let stream = null;

// ------------------------------------------------------------------- charts
const charts = {
  temp: new TimeSeriesChart($("chart-temp"), {
    title: "Body temperature", unit: "°C", fmt: fmt1, code: "8310-5",
    series: [{ key: "temp", label: "Temperature", cls: "s1" }],
    minDomain: [36, 37.6], band: [35, 38],
    thresholds: [{ value: 38, label: "38.0 fever" }, { value: 35, label: "35.0 hypothermia" }],
    alertTest: (p) => p.temp != null && (p.temp >= 38 || p.temp < 35),
    onClick: inspect,
  }),
  bp: new TimeSeriesChart($("chart-bp"), {
    title: "Blood pressure", unit: "mmHg", fmt: fmt0, code: "85354-9",
    series: [{ key: "sys", label: "Systolic", cls: "s1" },
             { key: "dia", label: "Diastolic", cls: "s2" }],
    minDomain: [70, 145], band: null,
    thresholds: [{ value: 140, label: "140" }, { value: 90, label: "90" }],
    alertTest: (p) => (p.sys ?? 0) >= 140 || (p.dia ?? 0) >= 90 ||
      (p.sys != null && p.sys < 90) || (p.dia != null && p.dia < 60),
    onClick: inspect,
  }),
};

// legend with series toggling (identity never colour-alone: label + swatch)
for (const s of charts.bp.o.series) {
  const b = document.createElement("button");
  b.setAttribute("aria-pressed", "true");
  const sw = document.createElement("span");
  sw.className = "swatch";
  sw.style.background = `var(--series-${s.cls.slice(1)})`;
  b.append(sw, document.createTextNode(s.label));
  b.onclick = () => b.setAttribute("aria-pressed", String(charts.bp.toggle(s.key)));
  $("legend-bp").append(b);
}

// ------------------------------------------------------------------- views
function showLogin(msg = "") {
  stream?.close();
  auth.clear();
  $("dash-view").hidden = true;
  $("login-view").hidden = false;
  $("logout").hidden = true;
  $("who").textContent = "";
  $("login-error").textContent = msg;
  setConn("off");
}
auth.onExpired(() => showLogin("Your session expired. Please sign in again."));

$("login-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const f = new FormData(ev.target);
  $("login-error").textContent = "";
  try {
    const res = await login(f.get("username"), f.get("password"));
    ev.target.reset();
    $("who").textContent = `${f.get("username")} · ${res.roles.join(", ")}`;
    $("logout").hidden = false;
    $("login-view").hidden = true;
    $("dash-view").hidden = false;
    startStream();
  } catch (e) {
    $("login-error").textContent = e.message;
  }
});
$("logout").onclick = () => showLogin();

function setConn(status, delay) {
  const el = $("conn");
  const map = {
    live: ["Live", "pill-on"], connecting: ["Connecting…", "pill-warn"],
    reconnecting: [`Reconnecting in ${Math.round((delay || 0) / 1000)}s`, "pill-warn"],
    off: ["Disconnected", "pill-off"],
  };
  const [text, cls] = map[status];
  el.textContent = text;
  el.className = `pill ${cls}`;
}

// ------------------------------------------------------------------- stream
function startStream() {
  stream = new VitalsStream({
    onStatus: setConn,
    onHello: (msg) => {
      state.patients = msg.patients;
      state.allowed = msg.allowed;
      renderPatientTabs();
      if (!state.current || !state.allowed.includes(state.current)) selectPatient(state.allowed[0]);
    },
    onFrame: (buf) => {
      let s;
      try { s = decodeFrame(buf, state.patients); } catch { return; }
      const arr = state.buffers.get(s.patient) || [];
      arr.push(s);
      if (arr.length > MAX_POINTS) arr.splice(0, arr.length - MAX_POINTS);
      state.buffers.set(s.patient, arr);
      trackAlerts(s);
      if (s.patient === state.current && !state.paused) redraw();
    },
  });
  stream.connect();
}

let lastBytes = 0;
setInterval(() => {
  if (!stream) return;
  $("k-rate").textContent = String(stream.bytes - lastBytes);
  lastBytes = stream.bytes;
}, 1000);

function renderPatientTabs() {
  const el = $("patients");
  el.replaceChildren();
  for (const pid of state.allowed) {
    const b = document.createElement("button");
    b.textContent = pid;
    b.setAttribute("role", "tab");
    b.onclick = () => selectPatient(pid);
    el.append(b);
  }
  markTabs();
}
function markTabs() {
  for (const b of $("patients").children) {
    const on = b.textContent === state.current;
    b.classList.toggle("on", on);
    b.setAttribute("aria-selected", String(on));
  }
}

async function selectPatient(pid) {
  state.current = pid;
  markTabs();
  redraw();
  if (state.loaded.has(pid)) return;
  // Backfill history from the FHIR REST API so the window is full immediately.
  try {
    const since = new Date(Date.now() - 15 * 60 * 1000).toISOString();
    const bundle = await fhirGet("/fhir/Observation",
      { patient: pid, date: `ge${since}`, _count: 2000 });
    const hist = bundleToPoints(bundle);
    const live = state.buffers.get(pid) || [];
    const firstLive = live.length ? live[0].ts : Infinity;
    state.buffers.set(pid, [...hist.filter((p) => p.ts < firstLive), ...live]);
    state.loaded.add(pid);
    redraw();
  } catch (e) {
    console.warn("history backfill failed", e);
  }
}

// ------------------------------------------------------------------- render
function redraw() {
  const pts = state.buffers.get(state.current) || [];
  charts.temp.setData(pts);
  charts.bp.setData(pts);
  updateTiles(pts);
  if (state.showTable) renderTable(pts);
}

function updateTiles(pts) {
  const lastT = [...pts].reverse().find((p) => p.temp != null);
  const lastB = [...pts].reverse().find((p) => p.sys != null);
  $("k-temp").textContent = lastT ? fmt1(lastT.temp) : "–";
  $("k-bp").textContent = lastB ? `${fmt0(lastB.sys)}/${fmt0(lastB.dia)}` : "–";
  status($("k-temp-s"), lastT && (lastT.temp >= 38 ? "▲ High (fever)" : lastT.temp < 35 ? "▼ Low" : null));
  status($("k-bp-s"), lastB && (lastB.sys >= 140 || lastB.dia >= 90 ? "▲ High"
    : lastB.sys < 90 || lastB.dia < 60 ? "▼ Low" : null));
}
function status(el, alertText) {
  if (alertText === undefined || alertText === null) {
    el.textContent = alertText === null ? "✓ Normal" : "";
    el.className = `tile-status ${alertText === null ? "status-good" : ""}`;
  } else {
    el.textContent = alertText;
    el.className = "tile-status status-critical";
  }
}

function trackAlerts(s) {
  const prev = state.lastFlags.get(s.patient) || 0;
  state.lastFlags.set(s.patient, s.alerts);
  let raised = s.alerts & ~prev;
  // de-flap: ignore a flag re-raised for the same patient within 30 s
  const seen = (state.recent ||= new Map());
  for (const bit of [1, 2, 4, 8, 16]) {
    if (!(raised & bit)) continue;
    const k = `${s.patient}:${bit}`;
    if (s.ts - (seen.get(k) || 0) < 30000) raised &= ~bit;
    seen.set(k, s.ts);
  }
  if (!raised) return;
  state.alertCount += 1;
  $("k-alerts").textContent = String(state.alertCount);
  $("k-alerts-s").textContent = `Last: ${s.patient}`;
  const ul = $("alerts");
  if (ul.firstElementChild?.classList.contains("muted")) ul.replaceChildren();
  const li = document.createElement("li");
  const icon = document.createElement("span"); icon.className = "icon"; icon.textContent = "▲";
  const t = document.createElement("time"); t.textContent = d3.timeFormat("%H:%M:%S")(new Date(s.ts));
  const txt = document.createElement("span");
  txt.textContent = `${s.patient}: ${alertLabels(raised).join(", ")}`;
  li.append(icon, t, txt);
  ul.prepend(li);
  while (ul.children.length > 50) ul.lastChild.remove();
}

function renderTable(pts) {
  const rows = pts.slice(-30).reverse();
  d3.select("#table tbody").selectAll("tr").data(rows).join("tr")
    .selectAll("td").data((p) => [
      d3.timeFormat("%H:%M:%S")(new Date(p.ts)),
      p.temp == null ? "–" : fmt1(p.temp),
      p.sys == null ? "–" : fmt0(p.sys),
      p.dia == null ? "–" : fmt0(p.dia),
      alertLabels(p.alerts).join(", ") || "–",
    ]).join("td").text((d) => d);
}

// ------------------------------------------------------------------- inspector
async function inspect(point, chartOpts) {
  const pre = $("inspect");
  pre.textContent = "Loading…";
  try {
    const iso = new Date(point.ts).toISOString();
    const bundle = await fhirGet("/fhir/Observation",
      { patient: state.current, code: chartOpts.code, date: `eq${iso}` });
    const res = bundle.entry?.[0]?.resource;
    $("inspect-hint").textContent = res
      ? `GET /fhir/Observation/${res.id} · ${bundle.total} match(es)`
      : "No stored resource for that point (it may still be buffered).";
    pre.textContent = res ? JSON.stringify(res, null, 2) : "";
  } catch (e) {
    pre.textContent = `Error: ${e.message}`;
  }
}

// ------------------------------------------------------------------- controls
for (const b of $("windows").children) {
  b.onclick = () => {
    state.windowMin = Number(b.dataset.min);
    for (const o of $("windows").children) {
      o.classList.toggle("on", o === b);
      o.setAttribute("aria-checked", String(o === b));
    }
    Object.values(charts).forEach((c) => c.setWindow(state.windowMin * 60 * 1000));
  };
}
$("pause").onclick = (ev) => {
  state.paused = !state.paused;
  ev.target.textContent = state.paused ? "Resume" : "Pause";
  ev.target.setAttribute("aria-pressed", String(state.paused));
  if (!state.paused) redraw();
};
$("table-toggle").onclick = (ev) => {
  state.showTable = !state.showTable;
  ev.target.setAttribute("aria-pressed", String(state.showTable));
  $("table-wrap").hidden = !state.showTable;
  if (state.showTable) renderTable(state.buffers.get(state.current) || []);
};
