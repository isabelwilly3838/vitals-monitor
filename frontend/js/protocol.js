// Binary frame decoder + FHIR helpers. Mirrors backend/app/services/stream.py.
export const FRAME_BYTES = 28;
export const ALERTS = [
  { bit: 1, label: "Fever (≥ 38.0 °C)" },
  { bit: 2, label: "Hypothermia (< 35.0 °C)" },
  { bit: 4, label: "Hypertension (≥ 140/90)" },
  { bit: 8, label: "Hypotension (< 90/60)" },
  { bit: 16, label: "Rapid BP change" },
];

export function decodeFrame(buf, patientIds) {
  if (buf.byteLength !== FRAME_BYTES) throw new Error("bad frame length");
  const v = new DataView(buf);
  if (v.getUint8(0) !== 1) throw new Error("unsupported frame version");
  const idx = v.getUint8(1);
  const nanToNull = (x) => (Number.isNaN(x) ? null : Math.round(x * 100) / 100);
  return {
    patient: patientIds[idx],
    seq: v.getUint32(2, true),
    ts: v.getFloat64(6, true),
    temp: nanToNull(v.getFloat32(14, true)),
    sys: nanToNull(v.getFloat32(18, true)),
    dia: nanToNull(v.getFloat32(22, true)),
    alerts: v.getUint8(26),
  };
}

export function alertLabels(flags) {
  return ALERTS.filter((a) => flags & a.bit).map((a) => a.label);
}

const LOINC = "http://loinc.org";
const code = (cc) => (cc?.coding || []).find((c) => c.system === LOINC)?.code;

/** Merge a FHIR searchset Bundle of temp + BP Observations into time-indexed points. */
export function bundleToPoints(bundle) {
  const byTs = new Map();
  for (const { resource: o } of bundle.entry || []) {
    if (o.resourceType !== "Observation") continue;
    const ts = Date.parse(o.effectiveDateTime);
    const p = byTs.get(ts) || { ts, temp: null, sys: null, dia: null, alerts: 0 };
    const c = code(o.code);
    if (c === "8310-5") p.temp = o.valueQuantity?.value ?? null;
    if (c === "85354-9") {
      for (const comp of o.component || []) {
        if (code(comp.code) === "8480-6") p.sys = comp.valueQuantity?.value ?? null;
        if (code(comp.code) === "8462-4") p.dia = comp.valueQuantity?.value ?? null;
      }
    }
    if (p.temp != null && p.temp >= 38) p.alerts |= 1;
    if (p.temp != null && p.temp < 35) p.alerts |= 2;
    if ((p.sys ?? 0) >= 140 || (p.dia ?? 0) >= 90) p.alerts |= 4;
    if ((p.sys != null && p.sys < 90) || (p.dia != null && p.dia < 60)) p.alerts |= 8;
    byTs.set(ts, p);
  }
  return [...byTs.values()].sort((a, b) => a.ts - b.ts);
}
