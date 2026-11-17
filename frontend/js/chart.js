/* global d3 */
// Reusable real-time time-series chart (D3 v7).
// Features: sliding time window, normal-range band, threshold lines, alert marks,
// crosshair + tooltip, click-to-inspect, legend toggling, direct end labels.

const tooltip = () => document.getElementById("tooltip");

export class TimeSeriesChart {
  /**
   * @param {HTMLElement} el
   * @param {object} opts
   *   series: [{key, label, cls}]         cls: "s1" | "s2" (CSS colour token)
   *   minDomain: [lo, hi]                 y-domain always includes this range
   *   band: [lo, hi] | null               normal-range shading
   *   thresholds: [{value, label}]
   *   alertTest: (point) => boolean       marks a point in the critical colour
   *   fmt: (v) => string, unit: string
   *   onClick: (point) => void
   */
  constructor(el, opts) {
    this.el = el;
    this.o = opts;
    this.points = [];
    this.windowMs = 5 * 60 * 1000;
    this.hidden = new Set();
    this.margin = { top: 10, right: 64, bottom: 24, left: 40 };

    this.svg = d3.select(el).append("svg").attr("role", "img")
      .attr("aria-label", `${opts.title} chart`);
    this.gGrid = this.svg.append("g").attr("class", "gridline");
    this.gBand = this.svg.append("rect").attr("class", "band");
    this.gThr = this.svg.append("g");
    this.gX = this.svg.append("g").attr("class", "axis");
    this.gY = this.svg.append("g").attr("class", "axis");
    this.gLines = this.svg.append("g");
    this.gAlerts = this.svg.append("g");
    this.gLabels = this.svg.append("g");
    this.cross = this.svg.append("line").attr("class", "crosshair").style("display", "none");
    this.gHover = this.svg.append("g").style("display", "none");
    this.overlay = this.svg.append("rect").attr("class", "overlay");

    this.paths = Object.fromEntries(opts.series.map((s) =>
      [s.key, this.gLines.append("path").attr("class", `line ${s.cls}`)]));
    this.hoverDots = Object.fromEntries(opts.series.map((s) =>
      [s.key, this.gHover.append("circle").attr("r", 4.5).attr("class", `dot dot-${s.cls}`)]));

    this.overlay
      .on("pointermove", (ev) => this._hover(ev))
      .on("pointerleave", () => this._unhover())
      .on("click", (ev) => { const p = this._nearest(ev); if (p) opts.onClick?.(p, this.o); });

    new ResizeObserver(() => this.render()).observe(el);
  }

  setData(points) { this.points = points; this.render(); }
  setWindow(ms) { this.windowMs = ms; this.render(); }
  toggle(key) {
    this.hidden.has(key) ? this.hidden.delete(key) : this.hidden.add(key);
    this.render();
    return !this.hidden.has(key);
  }

  render() {
    const { width, height } = this.el.getBoundingClientRect();
    if (!width) return;
    const m = this.margin;
    const iw = width - m.left - m.right;
    const ih = height - m.top - m.bottom;
    this.svg.attr("width", width).attr("height", height);

    const now = this.points.length ? this.points[this.points.length - 1].ts : Date.now();
    const t0 = now - this.windowMs;
    const vis = this.points.filter((p) => p.ts >= t0);
    this.vis = vis;
    const series = this.o.series.filter((s) => !this.hidden.has(s.key));

    const x = (this.x = d3.scaleTime().domain([t0, now]).range([m.left, m.left + iw]));
    const vals = vis.flatMap((p) => series.map((s) => p[s.key])).filter((v) => v != null);
    const [lo, hi] = d3.extent([...vals, ...this.o.minDomain]);
    const pad = (hi - lo) * 0.08;
    const y = (this.y = d3.scaleLinear().domain([lo - pad, hi + pad]).nice()
      .range([m.top + ih, m.top]));

    const ticksX = Math.max(2, Math.floor(iw / 110));
    this.gX.attr("transform", `translate(0,${m.top + ih})`)
      .call(d3.axisBottom(x).ticks(ticksX).tickSizeOuter(0).tickFormat(d3.timeFormat("%H:%M:%S")));
    this.gY.attr("transform", `translate(${m.left},0)`)
      .call(d3.axisLeft(y).ticks(5).tickSize(0).tickPadding(6))
      .call((g) => g.select(".domain").remove());
    this.gGrid.attr("transform", `translate(${m.left},0)`)
      .call(d3.axisLeft(y).ticks(5).tickSize(-iw).tickFormat(""));

    if (this.o.band) {
      const [b0, b1] = this.o.band;
      this.gBand.attr("x", m.left).attr("width", iw)
        .attr("y", y(Math.min(b1, y.domain()[1]))).attr("height",
          Math.max(0, y(Math.max(b0, y.domain()[0])) - y(Math.min(b1, y.domain()[1]))));
    }

    const thr = this.gThr.selectAll("g").data(this.o.thresholds.filter((t) =>
      t.value >= y.domain()[0] && t.value <= y.domain()[1]));
    const thrE = thr.enter().append("g");
    thrE.append("line").attr("class", "threshold");
    thrE.append("text").attr("class", "threshold-label").attr("text-anchor", "end").attr("dy", -3);
    thr.exit().remove();
    thr.merge(thrE).select("line").attr("x1", m.left).attr("x2", m.left + iw)
      .attr("y1", (d) => y(d.value)).attr("y2", (d) => y(d.value));
    thr.merge(thrE).select("text").attr("x", m.left + iw).attr("y", (d) => y(d.value))
      .text((d) => d.label);

    for (const s of this.o.series) {
      const line = d3.line().defined((p) => p[s.key] != null)
        .x((p) => x(p.ts)).y((p) => y(p[s.key])).curve(d3.curveMonotoneX);
      this.paths[s.key].attr("d", this.hidden.has(s.key) ? null : line(vis));
    }

    // alert marks: critical-coloured dots on the first visible series
    const key = series[0]?.key;
    const alerts = key ? vis.filter((p) => p[key] != null && this.o.alertTest(p)) : [];
    this.gAlerts.selectAll("circle").data(alerts, (d) => d.ts)
      .join("circle").attr("class", "alert-mark").attr("r", 2.5)
      .attr("cx", (d) => x(d.ts)).attr("cy", (d) => y(d[key]));

    // direct end labels (multi-series only; single series is named by the title)
    const last = vis[vis.length - 1];
    const labels = this.o.series.length > 1 && last
      ? series.filter((s) => last[s.key] != null).map((s) => ({ s, v: last[s.key] })) : [];
    const ly = labels.map((l) => y(l.v));
    if (ly.length === 2 && Math.abs(ly[0] - ly[1]) < 14) {
      const mid = (ly[0] + ly[1]) / 2;
      const up = ly[0] < ly[1] ? 0 : 1;
      ly[up] = mid - 7; ly[1 - up] = mid + 7;
    }
    this.gLabels.selectAll("text").data(labels, (d) => d.s.key).join("text")
      .attr("class", "end-label").attr("x", m.left + iw + 6).attr("y", (_, i) => ly[i])
      .attr("dy", "0.35em").text((d) => `${d.s.label} ${this.o.fmt(d.v)}`);

    this.overlay.attr("x", m.left).attr("y", m.top).attr("width", iw).attr("height", ih);
    if (this.hoverTs != null) this._drawHover(this.hoverTs);
  }

  _nearest(ev) {
    if (!this.vis?.length) return null;
    const [mx] = d3.pointer(ev, this.svg.node());
    const t = this.x.invert(mx).getTime();
    const i = d3.bisector((p) => p.ts).center(this.vis, t);
    return this.vis[i];
  }

  _hover(ev) {
    const p = this._nearest(ev);
    if (!p) return;
    this.hoverTs = p.ts;
    this._drawHover(p.ts);
    this._tooltip(p, ev);
  }

  _drawHover(ts) {
    const p = this.vis.find((d) => d.ts === ts);
    if (!p) return this._unhover();
    const cx = this.x(p.ts);
    this.cross.style("display", null).attr("x1", cx).attr("x2", cx)
      .attr("y1", this.y.range()[1]).attr("y2", this.y.range()[0]);
    this.gHover.style("display", null);
    for (const s of this.o.series) {
      const v = p[s.key];
      this.hoverDots[s.key].style("display", v == null || this.hidden.has(s.key) ? "none" : null)
        .attr("cx", cx).attr("cy", v == null ? 0 : this.y(v));
    }
  }

  _tooltip(p, ev) {
    const tt = tooltip();
    tt.replaceChildren(); // build with DOM nodes / textContent only (no innerHTML -> no XSS)
    const add = (cls, text) => {
      const d = document.createElement("div"); d.className = cls; d.textContent = text;
      tt.append(d); return d;
    };
    add("t-time", d3.timeFormat("%H:%M:%S.%L")(new Date(p.ts)));
    for (const s of this.o.series) {
      if (this.hidden.has(s.key)) continue;
      const row = add("t-row", "");
      const k = document.createElement("span"); k.className = "t-key";
      const sw = document.createElement("span"); sw.className = "swatch";
      sw.style.background = `var(--series-${s.cls.slice(1)})`;
      sw.style.width = "10px"; sw.style.height = "2px"; sw.style.display = "inline-block";
      k.append(sw, document.createTextNode(s.label));
      const v = document.createElement("strong");
      v.textContent = p[s.key] == null ? "–" : `${this.o.fmt(p[s.key])} ${this.o.unit}`;
      row.append(k, v);
    }
    if (this.o.alertTest(p)) add("t-row status-critical", "▲ Outside normal range");
    add("t-hint", "Click to inspect FHIR resource");
    tt.hidden = false;
    const r = tt.getBoundingClientRect();
    let left = ev.clientX + 14;
    if (left + r.width > window.innerWidth - 8) left = ev.clientX - r.width - 14;
    tt.style.left = `${Math.max(8, left)}px`;
    tt.style.top = `${Math.max(8, ev.clientY - r.height - 10)}px`;
  }

  _unhover() {
    this.hoverTs = null;
    this.cross.style("display", "none");
    this.gHover.style("display", "none");
    tooltip().hidden = true;
  }
}
