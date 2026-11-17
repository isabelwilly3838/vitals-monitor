// REST + WebSocket client. The token lives in memory only (never localStorage),
// which limits what an XSS payload could steal.
let token = null;
let onUnauthorized = () => {};

export const auth = {
  get token() { return token; },
  set(t) { token = t; },
  clear() { token = null; },
  onExpired(fn) { onUnauthorized = fn; },
};

export async function login(username, password) {
  const body = new URLSearchParams({ username, password });
  const r = await fetch("/auth/token", { method: "POST", body });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j?.issue?.[0]?.diagnostics || `Login failed (${r.status})`);
  token = j.access_token;
  return j;
}

export async function fhirGet(path, params = {}) {
  const url = new URL(path, location.origin);
  for (const [k, v] of Object.entries(params)) {
    (Array.isArray(v) ? v : [v]).forEach((x) => url.searchParams.append(k, x));
  }
  const r = await fetch(url, {
    headers: { Authorization: `Bearer ${token}`, Accept: "application/fhir+json" },
  });
  if (r.status === 401) { onUnauthorized(); throw new Error("Session expired"); }
  const j = await r.json();
  if (!r.ok) throw new Error(j?.issue?.[0]?.diagnostics || `HTTP ${r.status}`);
  return j;
}

/** Auto-reconnecting vitals stream with exponential backoff + jitter. */
export class VitalsStream {
  constructor({ onHello, onFrame, onStatus }) {
    Object.assign(this, { onHello, onFrame, onStatus });
    this.backoff = 500;
    this.closed = false;
    this.bytes = 0;
  }

  connect() {
    this.closed = false;
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = (this.ws = new WebSocket(`${proto}://${location.host}/ws/vitals`));
    ws.binaryType = "arraybuffer";
    this.onStatus("connecting");
    ws.onopen = () => ws.send(JSON.stringify({ token, encoding: "binary" }));
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        const msg = JSON.parse(ev.data);
        if (msg.type === "hello") {
          this.backoff = 500;
          this.onStatus("live");
          this.onHello(msg);
        }
        return;
      }
      this.bytes += ev.data.byteLength;
      this.onFrame(ev.data);
    };
    ws.onclose = (ev) => {
      if (this.closed) return this.onStatus("off");
      if (ev.code === 4401) { this.onStatus("off"); return onUnauthorized(); }
      const delay = Math.min(15000, this.backoff) * (0.75 + Math.random() * 0.5);
      this.backoff *= 2;
      this.onStatus("reconnecting", delay);
      this.timer = setTimeout(() => this.connect(), delay);
    };
  }

  close() {
    this.closed = true;
    clearTimeout(this.timer);
    this.ws?.close();
  }
}
