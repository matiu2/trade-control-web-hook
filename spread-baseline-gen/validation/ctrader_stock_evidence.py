"""Validate fixed demo stock evidence without changing its original measurements."""

import copy
import datetime as dt
import hashlib
import json
import math
import os
import tempfile
from pathlib import Path

FROM = "2026-07-24T16:26:00Z"
TO = "2026-10-22T16:26:00Z"
IDENTITY = {
    "profile": "ctrader-demo",
    "environment": "demo",
    "endpoint": "wss://demo.ctraderapi.com:5036",
    "account_id": 49247794,
    "broker": "Spotware",
    "login": 5944473,
}
POLICY = {"max_minutes_per_instrument": 20}
REVIEWS = {"Reviewed", "MaskNotComputable"}


class NoEligible(ValueError):
    def __init__(self, symbols):
        super().__init__(
            "no eligible stock profiles; original failure evidence retained, no bake"
        )
        self.symbols = symbols


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


def parse(raw):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, f"duplicate JSON key {key}")
            result[key] = value
        return result

    def invalid(value):
        raise ValueError(f"non-finite JSON number {value}")

    return json.loads(raw, object_pairs_hook=unique, parse_constant=invalid)


def snapshot(path):
    path = Path(path)
    require(
        path.is_file() and not path.is_symlink(),
        f"expected regular evidence file: {path}",
    )
    raw = path.read_bytes()
    return parse(raw), {
        "path": str(path.resolve()),
        "sha256": hashlib.sha256(raw).hexdigest(),
    }


def encoded(value):
    return (
        json.dumps(value, indent=2, ensure_ascii=False, allow_nan=False) + "\n"
    ).encode()


def sync_parent(path):
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def existing_exact(path, raw):
    require(
        path.is_file() and not path.is_symlink(),
        f"refusing nonregular evidence: {path}",
    )
    with path.open("rb") as stream:
        require(stream.read() == raw, f"refusing to replace existing evidence: {path}")
        os.fsync(stream.fileno())


def write_new(path, raw):
    """Publish only synced complete bytes, atomically, without replacing evidence."""
    path = Path(path)
    if path.exists() or path.is_symlink():
        existing_exact(path, raw)
        sync_parent(path)
        return
    descriptor, temporary = tempfile.mkstemp(
        dir=path.parent, prefix=path.name + ".publish.", suffix=".tmp"
    )
    temporary = Path(temporary)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        try:
            os.link(temporary, path, follow_symlinks=False)
        except FileExistsError:
            existing_exact(path, raw)
        sync_parent(path)
    finally:
        temporary.unlink(missing_ok=True)
        sync_parent(path)


def preparation_marker(evidence):
    return encoded(
        {
            "schema_version": 1,
            **{
                key: evidence[key]
                for key in ["source_report", "source_audit", "catalogue"]
            },
        }
    )


def preparation_started(config):
    paths = [
        config.preparation,
        config.ready,
        config.ready.with_suffix(".quarantine.json"),
        config.provenance,
        config.provenance.with_suffix(".sha256"),
    ]
    return any(
        path.exists()
        or path.is_symlink()
        or any(path.parent.glob(path.name + ".publish.*.tmp"))
        for path in paths
    )


def instant(value):
    value = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    require(value.utcoffset() == dt.timedelta(0), "evidence timestamps must be UTC")
    return value


def whole_minute(value):
    value = instant(value)
    require(value.second == value.microsecond == 0, "expected whole M1 boundary")
    return value


def finite(value):
    return type(value) in (int, float) and math.isfinite(value)


def catalogue(workspace):
    value, descriptor = snapshot(workspace / "broker-ctrader/src/catalogue/stocks.json")
    require(
        {
            key: value["source_" + field]
            for key, field in [
                ("profile", "profile"),
                ("account_id", "account_id"),
                ("environment", "environment"),
                ("broker", "broker_name"),
                ("login", "trader_login"),
                ("endpoint", "endpoint"),
            ]
        }
        == IDENTITY,
        "stock catalogue identity differs from the pinned demo",
    )
    rows = value["records"]
    require(
        len(rows) == 350
        and len({r["name"] for r in rows}) == 350
        and len({r["id"] for r in rows}) == 350
        and all(
            r["enabled"] is True and type(r["id"]) is int and r["id"] > 0 for r in rows
        ),
        "expected exactly 350 unique enabled native stock instruments",
    )
    return {r["name"]: r for r in rows}, descriptor


def validate_audit(report, audit, native):
    expected = {
        "version": 1,
        "identity": IDENTITY,
        "from_utc": FROM,
        "to_utc": TO,
        "requested_symbols": report["requested_symbols"],
        "policy": POLICY,
    }
    require(
        audit["binding"] == expected, "full audit identity/window/policy/scope differs"
    )
    records = audit["records"]
    keys = [(r["symbol"], r["from_utc"]) for r in records]
    require(
        keys == sorted(set(keys)), "full audit has duplicated or unordered exclusions"
    )
    counts = {}
    for record in records:
        require(
            set(record)
            == {
                "symbol",
                "symbol_id",
                "from_utc",
                "to_utc",
                "observed_at_utc",
                "bid",
                "ask",
                "reason",
            },
            "invalid exclusion schema",
        )
        symbol = record["symbol"]
        require(
            symbol in native and record["symbol_id"] == native[symbol]["id"],
            "exclusion native identity changed",
        )
        left, right = whole_minute(record["from_utc"]), whole_minute(record["to_utc"])
        require(
            instant(FROM) <= left < right <= instant(TO)
            and right - left == dt.timedelta(minutes=1)
            and left <= instant(record["observed_at_utc"]) < right,
            "exclusion outside its whole M1 window",
        )
        require(
            record["reason"] == "crossed-historical-book"
            and finite(record["bid"])
            and finite(record["ask"])
            and record["bid"] > record["ask"] > 0,
            "invalid crossed-book evidence",
        )
        counts[symbol] = counts.get(symbol, 0) + 1
        require(counts[symbol] <= 20, f"audit budget exceeded for {symbol}")


def quality_reason(row):
    profile = row["profile"]
    review, bars = profile["review"], profile["n_bars"]
    require(
        review in REVIEWS | {"InsufficientData"} and type(bars) is int and bars >= 0,
        "invalid statistical profile schema",
    )
    if review not in REVIEWS:
        return f"profile review is {review}"
    if bars < 48:
        return "fewer than 48 retained M1 observations"
    if bars < len(row.get("quarantined_minutes", [])) * 999:
        return "crossed-minute exclusions exceed 0.1%"
    low, median, high = [
        profile[key]
        for key in ("baseline_low_pips", "baseline_median_pips", "baseline_high_pips")
    ]
    widen, forecast = profile["hour_widen_frac"], profile["hour_p90_frac"]
    if not (
        len(widen) == len(forecast) == 24
        and all(finite(v) and v >= 0 for v in [low, median, high, *widen, *forecast])
        and low <= median <= high
    ):
        return "invalid spread values or percentiles"
    mask = profile["elevated_hours"]
    if not (
        type(mask) is int
        and 0 <= mask < 1 << 24
        and row["elevated_local_hours"] == [h for h in range(24) if mask & (1 << h)]
        and all(
            value == (forecast[h] if mask & (1 << h) else 0)
            for h, value in enumerate(widen)
        )
    ):
        return "invalid hour mask/forecast evidence"
    return None


def full_evidence(config):
    """Refuse incomplete scans, identity errors, and mismatched audit provenance."""
    native, native_descriptor = catalogue(config.workspace)
    report, source = snapshot(config.report)
    require(
        report["schema_version"] == 1
        and report["identity"] == IDENTITY
        and report["source"] == "paired-ticks-m1"
        and report["from_utc"] == FROM
        and report["to_utc"] == TO
        and report["quarantine_policy"] == POLICY
        and report.get("excluded", []) == [],
        "full report schema/identity/window/policy differs",
    )
    wanted = report["requested_symbols"]
    require(
        len(wanted) == len(set(wanted)) == 350 and set(wanted) == set(native),
        "full report must request exactly the pinned 350 native instruments",
    )
    rows, failures = report["instruments"], report["failures"]
    names, failed = [r["symbol"] for r in rows], [f["symbol"] for f in failures]
    require(
        len(names) == len(set(names))
        and len(failed) == len(set(failed))
        and not set(names) & set(failed)
        and set(names) | set(failed) == set(wanted),
        "not all 350 native symbols were attempted exactly once; refuse subset derivation",
    )
    require(
        type(report["complete"]) is bool and report["complete"] == (not failures),
        "full report completion marker is inconsistent; resume the original checkpoint",
    )
    require(
        all(isinstance(f["error"], str) and f["error"] for f in failures),
        "invalid retained source failure",
    )
    audit_path = config.report.with_suffix(".quarantine.json")
    audit, audit_descriptor = snapshot(audit_path)
    require(
        report["quarantine_audit"]
        == {**audit_descriptor, "excluded_minutes": len(audit["records"])},
        "full report lost or changed its durable audit",
    )
    validate_audit(report, audit, native)
    eligible, quality = [], []
    for row in rows:
        name = row["symbol"]
        require(
            row["symbol_id"] == native[name]["id"]
            and finite(row["pip_size"])
            and row["pip_size"] == 10.0 ** (-native[name]["pip_position"])
            and row["schedule"] == "utc"
            and row["timezone"] == "UTC",
            f"native metadata changed for {name}",
        )
        first, last = (
            whole_minute(row["first_minute_utc"]),
            whole_minute(row["last_minute_utc"]),
        )
        require(
            instant(FROM) <= first <= last < instant(TO),
            f"invalid observation bounds for {name}",
        )
        require(
            row.get("quarantined_minutes", [])
            == [r for r in audit["records"] if r["symbol"] == name],
            f"profile exclusions do not match the full audit for {name}",
        )
        reason = quality_reason(row)
        if reason:
            quality.append(
                {"symbol": name, "error": reason, "review": row["profile"]["review"]}
            )
        else:
            eligible.append(row)
    selected = {r["symbol"] for r in eligible}
    symbols = [name for name in wanted if name in selected]
    if not symbols:
        raise NoEligible(wanted)
    return {
        "report": report,
        "audit": audit,
        "source_report": source,
        "source_audit": audit_descriptor,
        "catalogue": native_descriptor,
        "rows": eligible,
        "symbols": symbols,
        "failed_symbols": [name for name in wanted if name not in selected],
        "quality_exclusions": quality,
    }


def derived(evidence, report_path):
    report, audit = copy.deepcopy(evidence["report"]), copy.deepcopy(evidence["audit"])
    symbols = evidence["symbols"]
    audit["binding"]["requested_symbols"] = symbols
    audit["records"] = [r for r in audit["records"] if r["symbol"] in symbols]
    report.update(
        requested_symbols=symbols,
        complete=True,
        failures=[],
        instruments=evidence["rows"],
    )
    raw_audit = encoded(audit)
    report["quarantine_audit"] = {
        "path": str(report_path.with_suffix(".quarantine.json").resolve()),
        "sha256": hashlib.sha256(raw_audit).hexdigest(),
        "excluded_minutes": len(audit["records"]),
    }
    return report, raw_audit


def prepare(config):
    evidence = full_evidence(config)
    write_new(config.preparation, preparation_marker(evidence))
    selected_path = config.report if len(evidence["symbols"]) == 350 else config.ready
    if selected_path != config.report:
        report, raw_audit = derived(evidence, selected_path)
        write_new(selected_path.with_suffix(".quarantine.json"), raw_audit)
        write_new(selected_path, encoded(report))
    _, selected_report = snapshot(selected_path)
    _, selected_audit = snapshot(selected_path.with_suffix(".quarantine.json"))
    manifest = {
        "schema_version": 1,
        "source_report": evidence["source_report"],
        "source_audit": evidence["source_audit"],
        "catalogue": evidence["catalogue"],
        "selected_report": selected_report,
        "selected_audit": selected_audit,
        "selected_symbols": evidence["symbols"],
        "failed_symbols": evidence["failed_symbols"],
        "source_failures": evidence["report"]["failures"],
        "quality_exclusions": evidence["quality_exclusions"],
    }
    raw = encoded(manifest)
    write_new(config.provenance, raw)
    write_new(
        config.provenance.with_suffix(".sha256"),
        (hashlib.sha256(raw).hexdigest() + "\n").encode(),
    )
    return verify(config)


def verify(config):
    manifest, seal = snapshot(config.provenance)
    seal_path = config.provenance.with_suffix(".sha256")
    require(
        seal_path.is_file() and not seal_path.is_symlink(),
        "missing or invalid provenance seal; normal preparation is required",
    )
    require(seal_path.read_text() == seal["sha256"] + "\n", "provenance seal changed")
    evidence = full_evidence(config)
    if config.preparation.exists() or config.preparation.is_symlink():
        require(
            config.preparation.is_file()
            and not config.preparation.is_symlink()
            and config.preparation.read_bytes() == preparation_marker(evidence),
            "frozen preparation source changed",
        )
    for key in ["source_report", "source_audit", "catalogue"]:
        require(manifest[key] == evidence[key], f"sealed {key} evidence changed")
    for key in ["selected_symbols", "failed_symbols", "quality_exclusions"]:
        expected = evidence["symbols"] if key == "selected_symbols" else evidence[key]
        require(manifest[key] == expected, f"sealed {key} differs from original scan")
    require(
        manifest["schema_version"] == 1
        and manifest["source_failures"] == evidence["report"]["failures"],
        "sealed source failures/schema changed",
    )
    selected_path = config.report if len(evidence["symbols"]) == 350 else config.ready
    actual, descriptor = snapshot(selected_path)
    audit, audit_descriptor = snapshot(selected_path.with_suffix(".quarantine.json"))
    require(
        manifest["selected_report"] == descriptor
        and manifest["selected_audit"] == audit_descriptor,
        "sealed selected report/audit changed",
    )
    if selected_path != config.report:
        expected, raw_audit = derived(evidence, selected_path)
        require(
            actual == expected and audit == parse(raw_audit),
            "derived subset differs from original successful measurements",
        )
    return manifest
