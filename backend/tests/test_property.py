"""Property-based & fuzz tests (Hypothesis)."""

import contextlib
import math
from datetime import datetime, timezone

from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st

from app.fhir import builders as b
from app.fhir.validation import validate_observation
from app.services.anomaly import threshold_flags
from app.services.simulator import DEMO_PATIENTS, VitalsSimulator
from app.services.stream import FRAME, Sample, decode, encode

KNOWN = {"P001"}
temps = st.floats(25.0, 45.0, allow_nan=False)
sys_ = st.floats(40.0, 300.0, allow_nan=False)
dia = st.floats(20.0, 200.0, allow_nan=False)


@given(temps)
def test_any_plausible_temperature_yields_valid_fhir(t):
    o = b.body_temperature("o", "P001", "D", datetime.now(timezone.utc), t)
    assert validate_observation(o, KNOWN) == []


@given(sys_, dia)
def test_bp_valid_iff_systolic_gt_diastolic(s, d):
    o = b.blood_pressure("o", "P001", "D", datetime.now(timezone.utc), s, d)
    issues = validate_observation(o, KNOWN)
    # builder rounds to 2dp, so compare rounded values
    assert (issues == []) == (round(s, 2) > round(d, 2))


@given(
    st.integers(0, 2),
    st.integers(0, 2**32 - 1),
    st.floats(0, 4e12),
    temps,
    sys_,
    dia,
    st.integers(0, 31),
)
def test_binary_frame_roundtrip(idx, seq, ts, t, s, d, flags):
    ids = ["P001", "P002", "P003"]
    sample = Sample(ids[idx], idx, seq, ts, t, s, d, flags)
    out = decode(encode(sample), ids)
    assert len(encode(sample)) == FRAME.size == 28
    assert (out.patient_id, out.seq, out.ts_ms, out.alerts) == (ids[idx], seq, ts, flags)
    for a, b_ in ((out.temp_c, t), (out.systolic, s), (out.diastolic, d)):
        assert math.isclose(a, b_, rel_tol=1e-6)  # float32 precision


@given(st.binary(max_size=64))
def test_decoder_never_crashes_on_garbage(buf):
    with contextlib.suppress(ValueError):
        decode(buf, ["P001"])


json_values = st.recursive(
    st.none() | st.booleans() | st.integers() | st.floats(allow_nan=False) | st.text(max_size=20),
    lambda children: (
        st.lists(children, max_size=4) | st.dictionaries(st.text(max_size=12), children, max_size=4)
    ),
    max_leaves=25,
)


@settings(max_examples=300, suppress_health_check=[HealthCheck.too_slow])
@given(
    st.dictionaries(
        st.sampled_from(
            [
                "resourceType",
                "status",
                "code",
                "subject",
                "valueQuantity",
                "component",
                "category",
                "effectiveDateTime",
                "id",
                "meta",
                "foo",
            ]
        ),
        json_values,
        max_size=8,
    )
)
def test_validator_fuzz_never_raises(data):
    data.setdefault("resourceType", "Observation")
    issues = validate_observation(data, KNOWN)
    assert isinstance(issues, list)
    for i in issues:
        assert {"severity", "code", "diagnostics"} <= set(i)


@settings(max_examples=50, deadline=None)
@given(st.integers(0, 10_000))
def test_simulator_always_emits_valid_observations(seed):
    import random

    sim = VitalsSimulator(DEMO_PATIENTS[0], 1.0, random.Random(seed))
    ts = datetime.now(timezone.utc)
    for _ in range(20):
        _, t, s, d = sim.step()
        assert validate_observation(b.body_temperature("o", "P001", "D", ts, t), KNOWN) == []
        assert validate_observation(b.blood_pressure("o", "P001", "D", ts, s, d), KNOWN) == []


@given(temps, sys_, dia)
def test_threshold_flags_consistent_with_interpretation(t, s, d):
    f = threshold_flags(t, s, d)
    assert bool(f & 1) == (b.interpret_temp(t) == "H")
    assert bool(f & 2) == (b.interpret_temp(t) == "L")
