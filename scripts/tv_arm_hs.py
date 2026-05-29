#!/usr/bin/env python3
"""
Arm a reversal setup (H&S / inverse) from the current TradingView chart.

Reads chart annotations via tv-mcp, writes a `trade.yaml` spec, hands it to
`trade-control build-trade --from-file` to mint a trade_id and emit 5 signed
alert YAMLs + a manifest, then (with `--create-alerts`) posts each alert to
TradingView, anchored to the original chart drawing.

Drawing label vocabulary (case-insensitive):
  horizontal_line:
    "too-high"           → short trade; invalidation veto level
    "too-low"            → long trade;  invalidation veto level
  trend_line:
    "neckline"           → break level (prep `break-and-close`). Legacy
                            alias: "break-and-close".
    "retest"             → retest level (prep)
  fib_retracement:       → TP. Furthest visible level in the profit
                            direction is the TP price.
  vertical_line:
    "trade-expiry"       → trade_expiry / not_after. Legacy: "trade-expired".

The signing + trade_id minting + manifest emission are delegated to
`trade-control build-trade --from-file`. This script only deals with the
chart (reading drawings, posting alerts back).

Flags: see `--help`.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

TV_MCP_ROOT = Path("/home/matiu/Downloads/tradingview-mcp-jackson")
TV_MCP_CLI = TV_MCP_ROOT / "src" / "cli" / "index.js"

TRADE_CONTROL_FALLBACK = Path(
    "/home/matiu/projects/trading-libraries/trade-control-web-hook"
    "/target/release/trade-control"
)
DEFAULT_KEY_FILE = Path.home() / ".config" / "trade-control" / "key.hex"

ARM_OUT_ROOT = Path("/tmp/trade-control-arm")

# Default account when none is supplied via env / CLI. Matches the operator's
# working OANDA worker index.
DEFAULT_ACCOUNT_BY_BROKER = {
    "oanda": "ms-oanda-1",
    "tradenation": "ms-tn-1",
}

# Exchange → broker tag. Anything not listed falls back to oanda.
BROKER_BY_EXCHANGE = {
    "TRADENATION": "tradenation",
    "OANDA": "oanda",
}

TRADE_EXPIRY_LABELS = {"trade-expiry", "trade-expired"}
RETEST_LABELS = {"retest", "neckline-retest", "retrace"}
BREAK_LABELS = {"neckline", "break-and-close"}
# Blackout / pause window markers. The two label aliases are
# interchangeable — operators can write whichever reads better on the
# chart. Detected as vertical lines and paired chronologically into
# (start, end) windows; each pair becomes one `pause`/`resume` alert
# pair via `trade-control build-pause`.
BLACKOUT_START_LABELS = {"blackout-start", "pause"}
BLACKOUT_END_LABELS = {"blackout-end", "resume"}
# News window markers. Like blackouts but independent: news windows
# don't pause entries — they ENABLE a separate close-on-reversal alert
# that flattens an open trade only while a known news event is in
# play. Operator draws two vertical lines labelled `news-start` and
# `news-end`; the script pairs them chronologically and shells out
# to `trade-control build-news` for each pair.
NEWS_START_LABELS = {"news-start"}
NEWS_END_LABELS = {"news-end"}


def tv(*args: str) -> dict:
    result = subprocess.run(
        ["node", str(TV_MCP_CLI), *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def list_drawings() -> list[dict]:
    return tv("draw", "list")["shapes"]


def get_drawing(entity_id: str) -> dict:
    return tv("draw", "get", entity_id)


def get_state() -> dict:
    return tv("state")


# ---------------------------------------------------------------------------
# Drawing classification

@dataclass
class Roles:
    invalidation: Optional[dict] = None
    invalidation_label: Optional[str] = None  # "too-high" or "too-low"
    break_and_close: Optional[dict] = None
    retest: Optional[dict] = None
    tp_fib: Optional[dict] = None
    trade_expiry: Optional[dict] = None
    # Blackout windows. Each pair is (start_drawing, end_drawing),
    # chronologically ordered. May be empty (no news on the chart).
    # Populated by `classify()` from `blackout-start` / `blackout-end`
    # vertical lines (and their `pause` / `resume` aliases). An odd
    # count is a hard error — `classify()` raises before the bundle is
    # emitted so a misdrawn chart can't silently arm half a blackout.
    blackout_pairs: list[tuple[dict, dict]] = field(default_factory=list)
    # News windows. Same shape as `blackout_pairs` but a separate
    # namespace: news windows ENABLE the `06-close-on-reversal` alert
    # rather than pausing entries. Populated from `news-start` /
    # `news-end` vertical lines.
    news_pairs: list[tuple[dict, dict]] = field(default_factory=list)


def label_of(d: dict) -> str:
    return (d.get("properties", {}).get("text") or "").strip()


def latest_time(d: dict) -> int:
    return max(p["time"] for p in d["points"])


def classify(drawings: list[dict]) -> Roles:
    """Group drawings by role. When multiple drawings claim the same role,
    keep the latest one (older drawings are stale leftovers from prior setups).

    Blackout lines (`blackout-start` / `blackout-end`, or their `pause` /
    `resume` aliases) are collected as a list rather than singled out —
    multiple news events per trade are valid and each becomes its own
    `pause`/`resume` pair. An odd count is a hard error: a misdrawn
    chart shouldn't be allowed to arm half a blackout."""
    by_role: dict[str, list[tuple[dict, str]]] = {
        "invalidation": [],
        "break_and_close": [],
        "retest": [],
        "tp_fib": [],
        "trade_expiry": [],
    }
    blackout_starts: list[dict] = []
    blackout_ends: list[dict] = []
    news_starts: list[dict] = []
    news_ends: list[dict] = []

    for stub in drawings:
        d = get_drawing(stub["id"])
        kind = stub["name"]
        lbl = label_of(d).lower()

        if kind == "horizontal_line" and lbl in {"too-high", "too-low"}:
            by_role["invalidation"].append((d, lbl))
        elif kind == "trend_line" and lbl in BREAK_LABELS:
            by_role["break_and_close"].append((d, lbl))
        elif kind == "trend_line" and lbl in RETEST_LABELS:
            by_role["retest"].append((d, lbl))
        elif kind == "fib_retracement":
            by_role["tp_fib"].append((d, ""))
        elif kind == "vertical_line" and lbl in TRADE_EXPIRY_LABELS:
            by_role["trade_expiry"].append((d, lbl))
        elif kind == "vertical_line" and lbl in BLACKOUT_START_LABELS:
            blackout_starts.append(d)
        elif kind == "vertical_line" and lbl in BLACKOUT_END_LABELS:
            blackout_ends.append(d)
        elif kind == "vertical_line" and lbl in NEWS_START_LABELS:
            news_starts.append(d)
        elif kind == "vertical_line" and lbl in NEWS_END_LABELS:
            news_ends.append(d)

    roles = Roles()
    for name in ("invalidation", "break_and_close", "retest", "tp_fib", "trade_expiry"):
        cands = by_role[name]
        if not cands:
            continue
        if len(cands) > 1:
            print(f"# Note: {len(cands)} '{name}' drawings; picking the latest",
                  file=sys.stderr)
        chosen, lbl = max(cands, key=lambda pair: latest_time(pair[0]))
        if name == "invalidation":
            roles.invalidation = chosen
            roles.invalidation_label = lbl
        elif name == "break_and_close":
            roles.break_and_close = chosen
        elif name == "retest":
            roles.retest = chosen
        elif name == "tp_fib":
            roles.tp_fib = chosen
        elif name == "trade_expiry":
            roles.trade_expiry = chosen

    roles.blackout_pairs = pair_vertical_lines(
        blackout_starts, blackout_ends, kind="blackout"
    )
    roles.news_pairs = pair_vertical_lines(news_starts, news_ends, kind="news")
    return roles


def pair_vertical_lines(
    starts: list[dict], ends: list[dict], kind: str
) -> list[tuple[dict, dict]]:
    """Pair start/end vertical lines chronologically.

    Sort each list by its anchor time (vertical lines have one point);
    pair them positionally. If the counts differ — i.e. an orphan
    start or end — raise immediately. The caller bails out before
    arming any alert (including the H&S bundle), mirroring how a
    misdrawn neckline aborts the whole run.

    Each pair must also have `start.time < end.time` — a reversed
    pair almost certainly means the operator mislabeled a line.

    `kind` is the label used in error messages ("blackout" /
    "news") so the operator knows which drawings to fix.
    """
    if len(starts) != len(ends):
        starts_desc = ", ".join(utc_iso(s["points"][0]["time"]) for s in starts)
        ends_desc = ", ".join(utc_iso(e["points"][0]["time"]) for e in ends)
        raise RuntimeError(
            f"{kind} lines must come in matched start/end pairs; "
            f"found {len(starts)} start(s) [{starts_desc}] and "
            f"{len(ends)} end(s) [{ends_desc}]. "
            "Fix the chart (add the missing line or relabel) and re-run."
        )
    starts_sorted = sorted(starts, key=lambda d: d["points"][0]["time"])
    ends_sorted = sorted(ends, key=lambda d: d["points"][0]["time"])
    pairs: list[tuple[dict, dict]] = []
    for i, (s, e) in enumerate(zip(starts_sorted, ends_sorted)):
        if s["points"][0]["time"] >= e["points"][0]["time"]:
            raise RuntimeError(
                f"{kind} pair #{i + 1} is reversed: "
                f"start={utc_iso(s['points'][0]['time'])} is at or after "
                f"end={utc_iso(e['points'][0]['time'])}. "
                f"Each {kind}-start must precede its {kind}-end."
            )
        pairs.append((s, e))
    return pairs


# ---------------------------------------------------------------------------
# TP fib geometry

def tp_price_from_fib(fib: dict, direction: str) -> float:
    """Return the TP price as the symmetric reflection of the fib's two endpoints.

    Convention: the user draws the fib spanning the head→neckline (or
    shoulder→neckline) move. The TP is one full leg further in the profit
    direction — i.e. price reflected through the neckline.

      neckline = endpoint nearest the candle range (highest for long, lowest for short)
      head     = the other endpoint
      TP       = 2 × neckline − head

    This ignores the fib's visible-level configuration. Earlier versions
    walked the visible levels and picked the furthest one, but that only
    gave the right TP if the user happened to draw the fib so extension
    levels landed on it — fragile across charts. The symmetric reflection
    needs only the two anchor points, which is what the user is already
    placing precisely.
    """
    prices = [p["price"] for p in fib["points"]]
    if direction == "long":
        head = min(prices)
        neckline = max(prices)
    else:  # short
        head = max(prices)
        neckline = min(prices)
    return 2.0 * neckline - head


def pcl_exhausted_price_from_fib(fib: dict, direction: str) -> float:
    """Return the pcl-exhausted price: 80% of the way from the fib's
    midpoint toward the TP.

    Geometry: midpoint = (head + neckline) / 2; TP = 2*neckline − head
    (see tp_price_from_fib). The pcl-exhausted level sits 80% of the
    distance from midpoint to TP — beyond it, the pattern's projected
    move is essentially complete and there's no R:R left for a fresh
    entry. For a short trade this is below the neckline (between
    neckline and TP). Bullish mirrors it above.

    Closed form (short): neckline − 0.7 × (head − neckline)
    Closed form (long):  neckline + 0.7 × (neckline − head)
    """
    prices = [p["price"] for p in fib["points"]]
    if direction == "long":
        head = min(prices)
        neckline = max(prices)
    else:  # short
        head = max(prices)
        neckline = min(prices)
    midpoint = (head + neckline) / 2.0
    tp = 2.0 * neckline - head
    return midpoint + 0.8 * (tp - midpoint)


# ---------------------------------------------------------------------------
# Symbol / broker / instrument formatting

def split_symbol(state: dict) -> tuple[str, str]:
    exchange, _, sym = state["symbol"].partition(":")
    if not sym:
        sym, exchange = exchange, ""
    return exchange, sym


def chart_broker(state: dict) -> str:
    exchange, _ = split_symbol(state)
    return BROKER_BY_EXCHANGE.get(exchange.upper(), "oanda")


def instrument_for(broker: str, raw_sym: str) -> str:
    """tradenation uses 'USD/CAD'; oanda uses 'USD_CAD'."""
    if len(raw_sym) == 6 and raw_sym.isalpha():
        a, b = raw_sym[:3].upper(), raw_sym[3:].upper()
        if broker == "tradenation":
            return f"{a}/{b}"
        return f"{a}_{b}"
    if broker == "tradenation":
        return raw_sym.replace("_", "/")
    return raw_sym.replace("/", "_")


def resolve_tn_instrument(name: str, *, quiet_on_miss: bool = False) -> Optional[str]:
    """Map a guess to a TradeNation catalog name via `trade-control`.

    TradeNation's display names don't match TradingView's: the chart shows
    "XAGUSD" but the broker only knows "Spot Silver". This shells out to
    `trade-control instruments resolve --json` (which wraps the
    tradenation-instrument-cache library — fast after the first call).

    Returns the canonical name on hit, or None on miss. With `quiet_on_miss`
    a clean miss is silent (so callers can try a fallback guess before
    surfacing the "did you mean…" list to the user); infra errors still print.
    """
    try:
        binary = find_trade_control()
    except FileNotFoundError as e:
        print(f"ERROR: cannot validate TradeNation instrument: {e}", file=sys.stderr)
        return None
    result = subprocess.run(
        [binary, "instruments", "resolve", name,
         "--broker", "tradenation", "--json"],
        check=False,
        capture_output=True,
        text=True,
    )
    # exit 0 = match, exit 2 = miss; anything else = infra error.
    if result.returncode not in (0, 2):
        print(
            "ERROR: `trade-control instruments resolve` failed "
            f"(exit {result.returncode}): {result.stderr.strip()}",
            file=sys.stderr,
        )
        return None
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as e:
        print(
            f"ERROR: `trade-control instruments resolve` returned non-JSON "
            f"({e}): {result.stdout[:200]}",
            file=sys.stderr,
        )
        return None
    if payload.get("ok"):
        return payload["name"]
    if quiet_on_miss:
        return None
    cands = payload.get("candidates") or []
    print(f"ERROR: TradeNation has no instrument named {name!r}.", file=sys.stderr)
    if cands:
        print("  did you mean:", file=sys.stderr)
        for c in cands:
            print(f"    - {c['name']}", file=sys.stderr)
    else:
        print("  no close candidates.", file=sys.stderr)
    return None


# ---------------------------------------------------------------------------
# Geometry helpers

def horizontal_price(d: dict) -> float:
    return d["points"][0]["price"]


def line_mean_price(d: dict) -> float:
    return sum(p["price"] for p in d["points"]) / len(d["points"])


def utc_iso(unix_seconds: int) -> str:
    return datetime.fromtimestamp(unix_seconds, tz=timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )


def direction_from_invalidation_label(lbl: str) -> str:
    if lbl == "too-high":
        return "short"
    if lbl == "too-low":
        return "long"
    raise ValueError(f"unknown invalidation label: {lbl}")


# ---------------------------------------------------------------------------
# build-trade integration

def find_trade_control() -> str:
    found = shutil.which("trade-control")
    if found:
        return found
    if TRADE_CONTROL_FALLBACK.exists():
        return str(TRADE_CONTROL_FALLBACK)
    raise FileNotFoundError(
        "trade-control binary not found on PATH or at "
        f"{TRADE_CONTROL_FALLBACK}."
    )


def key_file() -> Path:
    env = os.environ.get("TRADE_CONTROL_KEY_FILE", "").strip()
    if env:
        return Path(env)
    if DEFAULT_KEY_FILE.exists():
        return DEFAULT_KEY_FILE
    raise FileNotFoundError(
        f"No key file. Set TRADE_CONTROL_KEY_FILE or create {DEFAULT_KEY_FILE}."
    )


def write_trade_spec(spec: dict, path: Path) -> None:
    """Emit a `trade.yaml` that `build-trade --from-file` accepts."""
    lines = []
    for k, v in spec.items():
        if isinstance(v, str):
            # Escape backslashes and double-quotes so script sources
            # (e.g. allow_entry) survive YAML double-quoted parsing.
            escaped = v.replace("\\", "\\\\").replace('"', '\\"')
            lines.append(f'{k}: "{escaped}"')
        elif isinstance(v, bool):
            lines.append(f"{k}: {'true' if v else 'false'}")
        elif isinstance(v, float):
            # Avoid scientific notation for tiny pip values.
            lines.append(f"{k}: {v:.10g}")
        else:
            lines.append(f"{k}: {v}")
    path.write_text("\n".join(lines) + "\n")


def run_build_trade(spec_path: Path, out_dir: Path) -> None:
    binary = find_trade_control()
    key = key_file()
    result = subprocess.run(
        [
            binary, "build-trade",
            "--from-file", str(spec_path),
            "--key-file", str(key),
            "--output-dir", str(out_dir),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            "trade-control build-trade failed:\n"
            f"  stderr: {result.stderr.strip()}\n"
            f"  stdout: {result.stdout.strip()}"
        )
    # Surface build-trade's own stdout/stderr so the operator can see the trade_id.
    if result.stdout.strip():
        print(result.stdout.rstrip())
    if result.stderr.strip():
        print(result.stderr.rstrip(), file=sys.stderr)


def write_pause_spec(spec: dict, path: Path) -> None:
    """Emit a `pause.yaml` that `build-pause --from-file` accepts.

    Reuses `write_trade_spec`'s shape but kept separate for clarity —
    the field set is different (no `pattern`, `risk_pct`, etc.) and
    a future schema drift on one side shouldn't silently land on the
    other.
    """
    write_trade_spec(spec, path)


def write_news_spec(spec: dict, path: Path) -> None:
    """Emit a `news.yaml` that `build-news --from-file` accepts.

    Kept distinct from `write_pause_spec` for the same reason its
    pause sibling is distinct from `write_trade_spec` — schema drift
    on one shape shouldn't silently land on another.
    """
    write_trade_spec(spec, path)


def run_build_news(spec_path: Path, out_dir: Path) -> None:
    """Shell out to `trade-control build-news` for one news window.

    Each call emits two signed alerts (`01-news-start-<id>.yaml` and
    `02-news-end-<id>.yaml`) plus a manifest into `out_dir`. Called
    once per news pair, with a distinct `out_dir` per pair so
    multiple windows on one chart don't overwrite each other.
    """
    binary = find_trade_control()
    key = key_file()
    result = subprocess.run(
        [
            binary, "build-news",
            "--from-file", str(spec_path),
            "--key-file", str(key),
            "--output-dir", str(out_dir),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            "trade-control build-news failed:\n"
            f"  stderr: {result.stderr.strip()}\n"
            f"  stdout: {result.stdout.strip()}"
        )
    if result.stdout.strip():
        print(result.stdout.rstrip())
    if result.stderr.strip():
        print(result.stderr.rstrip(), file=sys.stderr)


def run_build_pause(spec_path: Path, out_dir: Path) -> None:
    """Shell out to `trade-control build-pause` for one blackout window.

    Each call emits two signed alerts (`01-pause-<id>.yaml` and
    `02-resume-<id>.yaml`) plus a manifest into `out_dir`. The matching
    pause-detection in `main()` calls this once per blackout pair, with
    a distinct `out_dir` per pair so multiple windows on one chart don't
    overwrite each other.
    """
    binary = find_trade_control()
    key = key_file()
    result = subprocess.run(
        [
            binary, "build-pause",
            "--from-file", str(spec_path),
            "--key-file", str(key),
            "--output-dir", str(out_dir),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            "trade-control build-pause failed:\n"
            f"  stderr: {result.stderr.strip()}\n"
            f"  stdout: {result.stdout.strip()}"
        )
    if result.stdout.strip():
        print(result.stdout.rstrip())
    if result.stderr.strip():
        print(result.stderr.rstrip(), file=sys.stderr)


# ---------------------------------------------------------------------------
# Manifest parsing
#
# build-trade emits a manifest.yaml in flat (non-nested) shape:
#   trade_id: hs-eur-aud-060438cc
#   instrument: EUR_AUD
#   trade_expiry: "..."
#   alerts:
#     - file: 01-veto-too-high.yaml
#       purpose: ...
#       action: Veto
#       name: too-high
#       level: ClosePositions
#       not_after: "..."
#     - file: 02-veto-trade-expiry.yaml
#       ...
#     - file: 03-prep-break-and-close.yaml
#       step: break-and-close
#       ...
#     - file: 04-prep-retest.yaml
#       step: retest
#       ...
#     - file: 05-enter.yaml
#       action: Enter
#       ...

def parse_manifest(text: str) -> dict:
    """Tiny YAML reader tailored to the manifest's known shape. Avoids a
    PyYAML dep and is robust against the manifest's lack of nesting beyond
    one list."""
    out: dict = {"alerts": []}
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i].rstrip()
        if not line or line.startswith("#"):
            i += 1
            continue
        if line.startswith("alerts:"):
            i += 1
            while i < len(lines):
                ln = lines[i].rstrip()
                if ln.startswith("  - file:"):
                    entry: dict = {}
                    entry["file"] = ln.split(":", 1)[1].strip()
                    i += 1
                    while i < len(lines) and lines[i].startswith("    "):
                        key, _, val = lines[i].strip().partition(":")
                        entry[key.strip()] = val.strip().strip('"')
                        i += 1
                    out["alerts"].append(entry)
                else:
                    break
            continue
        key, _, val = line.partition(":")
        out[key.strip()] = val.strip().strip('"')
        i += 1
    return out


# ---------------------------------------------------------------------------
# Alert → drawing mapping
#
# Manifest entries map onto drawings by basename prefix, since build-trade
# uses fixed basenames per role:
#   01-veto-too-high.yaml  / 01-veto-too-low.yaml → invalidation horizontal
#   02-veto-trade-expiry.yaml                    → trade-expiry vertical
#   03-prep-break-and-close.yaml                 → neckline trendline
#   04-prep-retest.yaml                          → retest trendline
#   05-enter.yaml                                → Pine alertcondition

def build_alert_spec(
    manifest_entry: dict,
    direction: str,
    roles: Roles,
    blackout_pair: Optional[tuple[dict, dict]] = None,
    news_pair: Optional[tuple[dict, dict]] = None,
) -> Optional[dict]:
    """Translate one manifest entry into the alert-payload shape the
    create_alerts JS expects. Returns None if the entry isn't postable
    (e.g. unrecognised role).

    `blackout_pair` is `(start_drawing, end_drawing)` from
    `roles.blackout_pairs` when this entry is one of a pause-bundle's
    alerts (`01-pause-*` / `02-resume-*`). Each pause-bundle manifest
    is processed independently — the caller pairs each manifest with
    the right `blackout_pair` before calling here.

    `news_pair` plays the same role for news-bundle alerts
    (`01-news-start-*` / `02-news-end-*`). The
    `06-close-on-reversal` alert from the main trade bundle is
    independently Pine-bound (no drawing) — it doesn't need
    `news_pair`.
    """
    fname: str = manifest_entry["file"]
    base = fname.removesuffix(".yaml")
    tv_name = base.split("-", 1)[1] if "-" in base else base  # strip "NN-"

    if base.startswith("01-veto-"):
        # Two 01-veto entries land here for every trade:
        #   - the invalidation veto (drawing-bound to the user-drawn
        #     horizontal line above/below the shoulder),
        #   - the pcl-exhausted veto (value-bound to a price computed
        #     from the fib retracement: 80% of the way from the fib's
        #     midpoint toward TP).
        # The invalidation veto's name matches the trade direction's
        # natural label (too-high for shorts, too-low for longs); the
        # other 01-veto entry is the pcl-exhausted one.
        invalidation_name = "too-high" if direction == "short" else "too-low"
        if tv_name == invalidation_name:
            if roles.invalidation is None:
                return None
            cross_dir = "cross_up" if direction == "short" else "cross_down"
            return {
                "kind": "drawing",
                "drawing_id": roles.invalidation["entity_id"],
                "tool": "LineToolHorzLine",
                "condition_type": cross_dir,
                "frequency": "on_first_fire",
                "auto_deactivate": False,
                "tv_name": tv_name,
            }
        # The opposite name is the pcl-exhausted veto. Value-bound to
        # a price computed from the fib (no drawing on the chart).
        if roles.tp_fib is None:
            return None
        price = pcl_exhausted_price_from_fib(roles.tp_fib, direction)
        # Bidirectional cross — the price level is between neckline
        # and TP, and we want the alert to fire whichever way price
        # gets there first. Matches the TV "price crossing value"
        # alert payload (no direction qualifier on cross).
        return {
            "kind": "price_value",
            "value": price,
            "condition_type": "cross",
            "frequency": "on_first_fire",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    if base == "02-veto-trade-expiry":
        if roles.trade_expiry is None:
            return None
        return {
            "kind": "drawing",
            "drawing_id": roles.trade_expiry["entity_id"],
            "tool": "LineToolVertLine",
            "condition_type": "cross",
            "frequency": "on_first_fire",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    if base == "03-prep-break-and-close":
        if roles.break_and_close is None:
            return None
        cross_dir = "cross_down" if direction == "short" else "cross_up"
        return {
            "kind": "drawing",
            "drawing_id": roles.break_and_close["entity_id"],
            "tool": "LineToolTrendLine",
            "condition_type": cross_dir,
            "frequency": "on_bar_close",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    if base == "04-prep-retest":
        if roles.retest is None:
            return None
        cross_dir = "cross_up" if direction == "short" else "cross_down"
        return {
            "kind": "drawing",
            "drawing_id": roles.retest["entity_id"],
            "tool": "LineToolTrendLine",
            "condition_type": cross_dir,
            "frequency": "on_first_fire",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    if base.startswith("01-pause-"):
        # Pause-bundle: armed by the blackout-start vertical line.
        # `blackout_pair` carries (start, end) — pause fires on the
        # start anchor. The build-pause CLI mints the trailing slug
        # (`01-pause-<blackout_id>`) so two windows on one chart get
        # distinct files.
        if blackout_pair is None:
            return None
        start, _end = blackout_pair
        return {
            "kind": "drawing",
            "drawing_id": start["entity_id"],
            "tool": "LineToolVertLine",
            "condition_type": "cross",
            "frequency": "on_first_fire",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    if base.startswith("02-resume-"):
        # Resume half of the pause-bundle; fires on the blackout-end
        # vertical line and clears the matching `pause:<trade_id>:<id>`
        # KV entry in the worker.
        if blackout_pair is None:
            return None
        _start, end = blackout_pair
        return {
            "kind": "drawing",
            "drawing_id": end["entity_id"],
            "tool": "LineToolVertLine",
            "condition_type": "cross",
            "frequency": "on_first_fire",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    if base.startswith("01-news-start-"):
        # News-bundle: armed by the news-start vertical line. Worker
        # writes the `news:<trade_id>:<news_id>` KV entry, which the
        # separate `06-close-on-reversal` alert checks at fire time.
        if news_pair is None:
            return None
        start, _end = news_pair
        return {
            "kind": "drawing",
            "drawing_id": start["entity_id"],
            "tool": "LineToolVertLine",
            "condition_type": "cross",
            "frequency": "on_first_fire",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    if base.startswith("02-news-end-"):
        if news_pair is None:
            return None
        _start, end = news_pair
        return {
            "kind": "drawing",
            "drawing_id": end["entity_id"],
            "tool": "LineToolVertLine",
            "condition_type": "cross",
            "frequency": "on_first_fire",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    if base == "05-enter":
        # candle-signals v2 (2026-05-26+) has 10 plot()s
        # (plot_0..plot_9 — the 8 signal_* latches plus recent_high /
        # recent_low for SL anchoring) followed by 2 alertcondition()s
        # in source order:
        #   plot_10 = "Long Pattern"  (alertcondition line)
        #   plot_11 = "Short Pattern"
        # TV indexes plot+alertcondition into a single namespace.
        # If the chart still has the OLD v2 (8 plots), this will fire
        # the wrong alert ID — re-add the indicator after a Pine update.
        plot_id = "plot_11" if direction == "short" else "plot_10"
        return {
            "kind": "pine_alertcondition",
            "indicator_name": "Candle Signals",
            "alert_cond_id": plot_id,
            "frequency": "on_bar_close",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    if base == "06-close-on-reversal":
        # Same Pine study as `05-enter` but the OPPOSITE direction:
        # if the trade is long we want to fire on a "Short Pattern"
        # (a confirming bearish reversal candle); if short, on a
        # "Long Pattern". The intent's `require_news_window: true`
        # gates the worker's close — outside any active news window
        # the alert lands but the worker rejects it.
        plot_id = "plot_10" if direction == "short" else "plot_11"
        return {
            "kind": "pine_alertcondition",
            "indicator_name": "Candle Signals",
            "alert_cond_id": plot_id,
            "frequency": "on_bar_close",
            "auto_deactivate": False,
            "tv_name": tv_name,
        }

    return None


# ---------------------------------------------------------------------------
# TV alert creation (inside-page fetch via tv-mcp)

TV_MCP_NODE_TEMPLATE = """
import {{ readFileSync }} from 'node:fs';
import {{ evaluate, evaluateAsync }} from '{tv_mcp_root}/src/connection.js';

const payloadsJson = readFileSync('{payloads_path}', 'utf8');
const payloads = JSON.parse(payloadsJson);

const ctx = await evaluate(`
  (function() {{
    var api = window.TradingViewApi._activeChartWidgetWV.value();
    var ms = api._chartWidget.model().mainSeries();
    var info = ms.symbolInfo();
    return {{
      pro_name: info.pro_name,
      currency: info.currency_code,
      resolution: api.resolution(),
      layout_id: (window.location.pathname.match(/\\\\/chart\\\\/([^\\\\/]+)\\\\//) || [])[1] || null,
    }};
  }})()
`);

const results = [];
for (const item of payloads) {{
  const expiration = new Date(Date.now() + 30 * 24 * 60 * 60 * 1000).toISOString();
  let condition;
  if (item.kind === 'pine_alertcondition') {{
    const studyInfo = await evaluate(`
      (function() {{
        var chart = window.TradingViewApi._activeChartWidgetWV.value();
        var name = ${{JSON.stringify(item.indicator_name)}};
        var sources = chart._chartWidget.model().dataSources();
        var titles = [];
        for (var i = 0; i < sources.length; i++) {{
          var s = sources[i];
          var t = null;
          try {{ t = s.title && s.title(); }} catch(e) {{}}
          var isStudy = false;
          try {{ isStudy = !!chart.getStudyById(s.id()); }} catch(e) {{}}
          titles.push({{ title: t == null ? null : String(t), isStudy: isStudy }});
          try {{
            var rawTitle = s.title && s.title();
            if (rawTitle == null) continue;
            var baseTitle = String(rawTitle).replace(/\\\\s*\\\\(.*$/, '');
            if (baseTitle !== name) continue;
            var id = s.id();
            var study = chart.getStudyById(id);
            if (!study) continue;
            var arr = study.getInputValues();
            var inputs = {{}};
            var pineId, pineVersion, pineFeatures;
            for (var j = 0; j < arr.length; j++) {{
              var k = arr[j].id, v = arr[j].value;
              if (k === 'pineId') pineId = v;
              else if (k === 'pineVersion') pineVersion = v;
              else if (k === 'pineFeatures') pineFeatures = v;
              else if (/^in_\\\\d+$/.test(k)) inputs[k] = v;
            }}
            return {{ id: id, inputs: inputs, pineId: pineId, pineVersion: pineVersion, pineFeatures: pineFeatures }};
          }} catch(e) {{}}
        }}
        return {{ __notFound: true, titles: titles }};
      }})()
    `);
    if (!studyInfo || studyInfo.__notFound) {{
      const titles = (studyInfo && studyInfo.titles) || [];
      const summary = titles.map(function(t) {{
        return (t.isStudy ? '[study] ' : '[other] ') + (t.title === null ? '<no-title>' : JSON.stringify(t.title));
      }}).join(', ');
      results.push({{
        name: item.name,
        error: 'study not found: ' + item.indicator_name + ' | data sources on active chart: [' + summary + ']',
      }});
      continue;
    }}
    const orderedInputs = {{ pineFeatures: studyInfo.pineFeatures }};
    const inNumKeys = Object.keys(studyInfo.inputs)
      .filter(function(k) {{ return /^in_\\d+$/.test(k); }})
      .sort(function(a, b) {{ return parseInt(a.slice(3), 10) - parseInt(b.slice(3), 10); }});
    for (const k of inNumKeys) orderedInputs[k] = studyInfo.inputs[k];
    orderedInputs.__profile = false;
    const studySeries = {{
      type: 'study',
      study: 'Script@tv-scripting-101',
      offsets_by_plot: {{}},
      inputs: orderedInputs,
      pine_id: studyInfo.pineId,
      pine_version: studyInfo.pineVersion,
    }};
    condition = {{
      type: 'alert_cond',
      frequency: item.frequency,
      alert_cond_id: item.alert_cond_id,
      series: [studySeries],
      resolution: ctx.resolution,
    }};
  }} else if (item.kind === 'price_value') {{
    // No drawing lookup — the alert is bound to a numeric price
    // level the script computed (pcl-exhausted at 80% of midpoint→TP).
    // Mirrors the create_alert payload TV's UI sends for "price
    // crossing value" alerts.
    condition = {{
      type: item.condition_type,
      frequency: item.frequency,
      series: [
        {{ type: 'barset' }},
        {{ type: 'value', value: item.value }},
      ],
      resolution: ctx.resolution,
    }};
  }} else {{
  const spec = await evaluate(`
    (function() {{
      var api = window.TradingViewApi._activeChartWidgetWV.value();
      var sh = api.getShapeById(${{JSON.stringify(item.drawing_id)}});
      if (!sh) return null;
      try {{ return sh._source.stateForAlert(); }} catch(e) {{ return {{ err: e.message }}; }}
    }})()
  `);
  if (!spec || spec.err) {{
    results.push({{ name: item.name, error: 'stateForAlert failed: ' + (spec && spec.err || 'shape not found') }});
    continue;
  }}
  let lineEntry;
  if (item.tool === 'LineToolHorzLine') {{
    const price = typeof spec.plots[0] === 'number' ? spec.plots[0] : spec.plots[0].price1;
    const resSec = parseInt(ctx.resolution, 10) * 60;
    const floor = Math.floor(Date.now() / 1000 / resSec) * resSec;
    lineEntry = {{
      type: 'line',
      tool: 'LineToolHorzLine',
      base_time: new Date(floor * 1000).toISOString(),
      offset1: 0,
      price1: price,
      offset2: 1,
      price2: price,
      extend_forward: true,
      extend_backward: true,
      drawing_id: item.drawing_id,
      layout_id: ctx.layout_id,
    }};
  }} else {{
    const plot = spec.plots[0];
    // For trendline alerts, we always need extend_forward:true — the line is
    // drawn over the H&S formation but the prep crossings (break-and-close,
    // retest) happen AFTER the second anchor. With extend_forward:false the
    // server's evaluator only considers the segment between anchors and
    // misses every real crossing. extend_backward stays as drawn (default
    // false) to avoid spurious fires from historical price action.
    var forceExtendForward = item.tool === 'LineToolTrendLine';
    lineEntry = {{
      type: 'line',
      tool: item.tool,
      base_time: new Date(plot.timestamp * 1000).toISOString(),
      offset1: plot.offset1,
      price1: plot.price1,
      offset2: plot.offset2,
      price2: plot.price2,
      extend_forward: forceExtendForward || !!plot.extendForward,
      extend_backward: !!plot.extendBackward,
      drawing_id: item.drawing_id,
      layout_id: ctx.layout_id,
    }};
  }}
    condition = {{
      type: item.condition_type,
      frequency: item.frequency,
      series: [
        {{ type: 'barset' }},
        lineEntry,
      ],
      resolution: ctx.resolution,
    }};
  }}
  const body = {{
    payload: {{
      symbol: '=' + JSON.stringify({{
        'currency-id': ctx.currency,
        session: 'regular',
        symbol: ctx.pro_name,
      }}),
      resolution: ctx.resolution,
      message: item.message,
      sound_file: 'alert/fired',
      sound_duration: 0,
      popup: true,
      expiration: expiration,
      auto_deactivate: item.auto_deactivate !== false,
      email: false,
      sms_over_email: false,
      mobile_push: true,
      web_hook: 'https://trade-control-web-hook.msherborne.workers.dev',
      name: item.tv_name || null,
      conditions: [condition],
      active: true,
      ignore_warnings: true,
    }},
  }};
  const result = await evaluateAsync(`
    fetch('https://pricealerts.tradingview.com/create_alert', {{
      method: 'POST',
      credentials: 'include',
      headers: {{ 'Content-Type': 'text/plain;charset=UTF-8' }},
      body: ${{JSON.stringify(JSON.stringify(body))}},
    }})
    .then(function(r) {{ return r.text().then(function(t) {{ return {{ status: r.status, body: t.slice(0, 2000) }}; }}); }})
    .catch(function(e) {{ return {{ error: e.message }}; }})
  `);
  results.push({{
    name: item.name,
    ...result,
    debug: {{
      tool: item.tool,
      drawing_id: item.drawing_id,
      condition_series_1: condition.series && condition.series[condition.series.length - 1],
    }},
  }});
}}
console.log(JSON.stringify(results, null, 2));
process.exit(0);
"""


def create_alerts(payloads: list[dict], tv_mcp_root: Path) -> list[dict]:
    if not payloads:
        return []
    payloads_path = Path("/tmp/trade-control-arm-payloads.json")
    payloads_path.write_text(json.dumps(payloads))
    script = TV_MCP_NODE_TEMPLATE.format(
        tv_mcp_root=str(tv_mcp_root),
        payloads_path=str(payloads_path),
    )
    script_path = Path("/tmp/trade-control-arm-create.mjs")
    script_path.write_text(script)
    result = subprocess.run(
        ["node", str(script_path)],
        check=False,
        capture_output=True,
        text=True,
        timeout=60,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"create-alerts failed: stderr={result.stderr.strip()} stdout={result.stdout.strip()}"
        )
    if result.stderr.strip():
        print("---- create-alerts node stderr ----")
        print(result.stderr.rstrip())
        print("---- end stderr ----")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError:
        return [{"raw_stdout": result.stdout, "stderr": result.stderr}]


# ---------------------------------------------------------------------------
# Pattern detection from invalidation label

def pattern_for(direction: str) -> str:
    return "hs" if direction == "short" else "ihs"


# ---------------------------------------------------------------------------
# Main

def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Arm a reversal setup from the active TradingView chart.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    p.add_argument(
        "--broker", choices=("oanda", "tradenation"),
        help="Broker to target. Defaults to the chart's exchange "
             "(also TRADE_CONTROL_BROKER env).",
    )
    p.add_argument(
        "--account-id", dest="account_id",
        help="Worker account index (e.g. ms-oanda-1, ms-tn-1). "
             "Defaults per broker; also TRADE_CONTROL_ACCOUNT env.",
    )
    risk = p.add_mutually_exclusive_group()
    risk.add_argument(
        "--risk-pct", dest="risk_pct", type=float,
        help="Risk per trade as a percent of equity. Default 1.0.",
    )
    risk.add_argument(
        "--risk-amount", dest="risk_amount", type=float,
        help="Risk per trade as an absolute home-currency amount (e.g. 5 = 5 AUD). "
             "Lands on intent.risk_amount; takes precedence over risk_pct.",
    )
    g = p.add_mutually_exclusive_group()
    g.add_argument(
        "--create-alerts", action="store_true",
        help="Post the alerts to TradingView.",
    )
    g.add_argument(
        "--dry-run", action="store_true",
        help="Build + sign locally only. No POSTs to TV or the worker.",
    )
    p.add_argument(
        "--broker-dry-run", action="store_true",
        help="Set dry_run on the enter intent so the worker logs the order "
             "but does not send it to the broker. Useful for first-time "
             "live runs of a new sizing path. Compatible with --create-alerts.",
    )
    p.add_argument(
        "--max-retries", dest="max_retries", type=int, default=None,
        help="Opt in to multi-shot entries: if the broker rejects the order "
             "(e.g. spread too wide), the worker will retry on subsequent "
             "enter-alert firings up to this many times. Default (flag absent) "
             "keeps today's single-shot behaviour. Bounded by trade_expiry.",
    )
    p.add_argument(
        "--entry-market", action="store_true",
        help="Use a market order for entry instead of the default pending "
             "stop-entry at the geometry anchor. Useful for confirmed-"
             "candle entries where waiting for a stop level just adds "
             "slippage. SL still anchors to geometry.",
    )
    p.add_argument(
        "--sl-from-recent", action="store_true",
        help="Anchor SL to Pine's recent_high (shorts) / recent_low (longs) "
             "instead of the signal bar's own wick. Spans the indicator's "
             "sl_lookback window of bars *preceding* the signal bar — gives "
             "the trade more breathing room when the signal candle is small. "
             "Requires the v2 indicator from 2026-05-26+; older indicators "
             "silently fall back to the bar extreme (tighter SL).",
    )
    p.add_argument(
        "--entry-filter-script", dest="entry_filter_script", default=None,
        help="Rhai script that gates whether the worker places the entry "
             "order. Lands on the enter intent's `allow_entry` as a "
             "Tunable::Script. Common patterns: 'signal_confirmed' (only "
             "fire on confirmed signals), or "
             "'signal_confirmed || pct(signal_range, tp_distance) >= 10' "
             "(confirmed, or candle large enough to skip waiting). "
             "Validated at sign-time — a bad script blocks the build.",
    )
    p.add_argument(
        "--skip-break-and-close", action="store_true",
        help="Drop the break-and-close prep from the bundle (no alert "
             "emitted and the entry no longer requires it). Use when "
             "the break already happened.",
    )
    p.add_argument(
        "--skip-retest", action="store_true",
        help="Drop the retest prep from the bundle. Does NOT imply "
             "--skip-break-and-close — pass both if you want to skip "
             "both (e.g. stocks, or late setups past the retest).",
    )
    p.add_argument(
        "--require-golden", dest="require_golden", action="store_true",
        help="Require a golden signal candle on entry. Sets "
             "`needs_golden: true` on the trade spec; the worker rejects "
             "the entry unless the incoming shell carries golden=true. "
             "Composes with --entry-filter-script (both must pass).",
    )
    p.add_argument(
        "--close-on-reversal", dest="close_on_reversal", action="store_true",
        help="Emit a 6th alert that closes the trade if an opposing "
             "golden-reversal candle prints during an active news "
             "window. Sets `close_on_news: true` on the trade spec; "
             "build-trade emits `06-close-on-reversal.yaml` (Pine "
             "alertcondition, opposite direction) and the worker only "
             "honours it when `news:<trade_id>:*` KV is populated by "
             "the matching `news-start` alert. Pair with `news-start` "
             "and `news-end` vertical lines on the chart.",
    )
    p.add_argument(
        "--no-instrument-check", dest="no_instrument_check", action="store_true",
        help="Skip the TradeNation catalog lookup that maps the chart "
             "symbol (e.g. XAGUSD) to the broker's canonical name "
             "(e.g. 'Spot Silver'). Use as an escape hatch when the "
             "cache or broker login is unavailable.",
    )
    p.add_argument(
        "--print-completions", action="store_true",
        help="Print a zsh completion script to stdout and exit. "
             "Install with: tv_arm_hs.py --print-completions > "
             "~/.zsh/completions/_tv_arm_hs (and ensure that dir is in fpath).",
    )
    return p.parse_args(argv)


# Single source of truth for the zsh completion script. Kept in lockstep
# with parse_args() above — if you add or rename a flag there, update this.
# Value-taking flags get static value lists where the choice is bounded
# (broker, pattern); free-form everywhere else.
ZSH_COMPLETION = r"""#compdef tv_arm_hs.py tv_arm_hs

# zsh completion for tv_arm_hs.py — generated by --print-completions.
# Regenerate after flag changes: `tv_arm_hs.py --print-completions > _tv_arm_hs`.

_tv_arm_hs() {
    local -a args
    args=(
        '--broker[broker to target]:broker:(oanda tradenation)'
        '--account-id[worker account index, e.g. ms-oanda-1]:account:'
        '(--risk-amount)--risk-pct[risk per trade as %% of equity]:pct:'
        '(--risk-pct)--risk-amount[risk per trade as absolute home-ccy amount]:amount:'
        '(--dry-run)--create-alerts[post the alerts to TradingView]'
        '(--create-alerts)--dry-run[build + sign locally only]'
        '--broker-dry-run[worker logs the order, does not send to broker]'
        '--max-retries[opt into multi-shot entries]:n:'
        '--entry-market[market entry instead of pending stop-entry]'
        '--sl-from-recent[anchor SL to Pine recent_high/recent_low]'
        '--entry-filter-script[Rhai script gating entry placement]:script:'
        '--skip-break-and-close[drop the break-and-close prep]'
        '--skip-retest[drop the retest prep]'
        '--require-golden[require golden candle on entry (needs_golden:true)]'
        '--close-on-reversal[emit a 6th alert that closes the trade on an opposing golden reversal during news]'
        '--no-instrument-check[skip the TN catalog lookup for the chart symbol]'
        '--print-completions[print this zsh completion script and exit]'
        '(- *)'{-h,--help}'[show help and exit]'
    )
    _arguments -s -S $args
}

_tv_arm_hs "$@"
"""


def main(argv: Optional[list[str]] = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])

    if args.print_completions:
        sys.stdout.write(ZSH_COMPLETION)
        return 0

    state = get_state()
    _exchange, raw_sym = split_symbol(state)
    # Broker resolution: --broker > TRADE_CONTROL_BROKER env > chart exchange.
    broker = (
        args.broker
        or os.environ.get("TRADE_CONTROL_BROKER", "").strip().lower()
        or chart_broker(state)
    )
    if broker not in ("oanda", "tradenation"):
        print(f"ERROR: unsupported broker {broker!r}", file=sys.stderr)
        return 1
    instrument = instrument_for(broker, raw_sym)
    if broker == "tradenation" and not args.no_instrument_check:
        # First try the chart symbol ("DE40", "XAGUSD", "EUR/USD"). TN's
        # catalog has symbols for FX/stocks but not indices/commodities, so
        # on miss fall back to the chart's human description ("Germany 40",
        # "Spot Silver") which usually matches TN's display name directly.
        canonical = resolve_tn_instrument(instrument, quiet_on_miss=True)
        if canonical is None:
            description = (tv("info").get("description") or "").strip()
            if description and description != instrument:
                print(
                    f"# {instrument!r} not in TN catalog; retrying with chart "
                    f"description {description!r}",
                    file=sys.stderr,
                )
                canonical = resolve_tn_instrument(description)
            else:
                # No useful fallback — re-run loudly so the user sees the
                # candidate list.
                canonical = resolve_tn_instrument(instrument)
        if canonical is None:
            return 1
        if canonical != instrument:
            print(
                f"# instrument {instrument!r} resolved to canonical "
                f"TN name {canonical!r}",
                file=sys.stderr,
            )
            instrument = canonical

    print(f"# Chart: {state['symbol']} {state['resolution']}")
    print(f"# Broker: {broker}  Instrument: {instrument}")
    print()

    drawings = list_drawings()
    try:
        roles = classify(drawings)
    except RuntimeError as e:
        # Hard-error on malformed blackout pairs: a misdrawn chart
        # shouldn't be allowed to arm half a window. The H&S bundle
        # isn't emitted either — easier for the operator to fix the
        # chart and re-run from scratch than to half-arm + clean up.
        print(f"ERROR: {e}", file=sys.stderr)
        return 1

    # A skipped prep doesn't need a drawing — the bundle won't include
    # that alert and the entry doesn't require it. So drop the
    # corresponding drawing from the required-drawings check.
    missing = []
    if roles.invalidation is None:
        missing.append("horizontal_line labeled 'too-high' or 'too-low'")
    if roles.break_and_close is None and not args.skip_break_and_close:
        missing.append("trend_line labeled 'neckline' (or 'break-and-close')")
    if roles.retest is None and not args.skip_retest:
        missing.append("trend_line labeled 'retest'")
    if roles.tp_fib is None:
        missing.append("fib_retracement (TP)")
    if roles.trade_expiry is None:
        missing.append("vertical_line labeled 'trade-expiry'")
    if missing:
        print("ERROR: missing required drawings:", file=sys.stderr)
        for m in missing:
            print(f"  - {m}", file=sys.stderr)
        return 1

    assert roles.invalidation and roles.tp_fib and roles.trade_expiry
    assert args.skip_break_and_close or roles.break_and_close
    assert args.skip_retest or roles.retest

    inv_label = roles.invalidation_label or ""
    direction = direction_from_invalidation_label(inv_label)
    pattern = pattern_for(direction)
    tp = tp_price_from_fib(roles.tp_fib, direction)
    expiry_unix = roles.trade_expiry["points"][0]["time"]
    expiry_iso = utc_iso(expiry_unix)

    print(f"# Direction: {direction} (from '{inv_label}') → pattern: {pattern}")
    print(f"# TP: {tp:.5f}")
    print(f"# trade_expiry: {expiry_iso}")
    print()

    # Write trade.yaml spec, hand off to build-trade.
    today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    out_dir = ARM_OUT_ROOT / f"{raw_sym}-{today}"
    out_dir.mkdir(parents=True, exist_ok=True)
    spec_path = out_dir / "_input-spec.yaml"

    account = (
        args.account_id
        or os.environ.get("TRADE_CONTROL_ACCOUNT")
        or DEFAULT_ACCOUNT_BY_BROKER[broker]
    )
    risk_pct = args.risk_pct if args.risk_pct is not None else 1.0
    # Each --skip-* flag is independent: --skip-retest only skips the
    # retest prep, --skip-break-and-close only skips the break-and-close
    # prep. Pass both to skip both (e.g. stocks, or setups arriving past
    # both stages).
    skip_preps: list[str] = []
    if args.skip_break_and_close:
        skip_preps.append("break-and-close")
    if args.skip_retest:
        skip_preps.append("retest")
    spec = {
        "pattern": pattern,
        "instrument": instrument,
        "account": account,
        "broker": broker,
        "trade_expiry": expiry_iso,
        "risk_pct": risk_pct,
        "tp_price": round(tp, 5),
    }
    if args.risk_amount is not None:
        spec["risk_amount"] = args.risk_amount
    if args.broker_dry_run:
        spec["dry_run"] = True
    if args.max_retries is not None:
        spec["max_retries"] = args.max_retries
    if args.entry_filter_script is not None:
        spec["allow_entry"] = args.entry_filter_script
    if args.require_golden:
        spec["needs_golden"] = True
    if args.close_on_reversal:
        spec["close_on_news"] = True
    if args.entry_market:
        spec["entry_mode"] = "market"
    if args.sl_from_recent:
        # pattern is "hs" → short → recent_high; "ihs" → long → recent_low.
        # Rust side validates direction compatibility too.
        spec["sl_anchor"] = "recent_high" if pattern == "hs" else "recent_low"
    if skip_preps:
        spec["skip_preps"] = skip_preps
    write_trade_spec(spec, spec_path)
    print(f"# Spec written to {spec_path}:")
    print(spec_path.read_text().rstrip())
    print()

    print(f"# Running: trade-control build-trade --from-file {spec_path} --output-dir {out_dir}")
    try:
        run_build_trade(spec_path, out_dir)
    except (FileNotFoundError, RuntimeError) as e:
        print(f"ERROR: build-trade failed: {e}", file=sys.stderr)
        return 2

    manifest_path = out_dir / "manifest.yaml"
    if not manifest_path.exists():
        print(f"ERROR: no manifest at {manifest_path}", file=sys.stderr)
        return 3
    manifest = parse_manifest(manifest_path.read_text())
    trade_id = manifest.get("trade_id", "")
    print(f"# trade_id: {trade_id}")
    print(f"# {len(manifest['alerts'])} alerts in manifest")
    print()

    # Build a pause/resume bundle per blackout pair on the chart. Each
    # bundle goes in its own sub-dir under the H&S out_dir so two
    # windows on one chart don't collide on basenames.
    # `pause_bundles` is a list of (blackout_pair, pause_manifest,
    # pause_out_dir) tuples that the create_alerts loop below stacks
    # onto the H&S alerts.
    pause_bundles: list[tuple[tuple[dict, dict], dict, Path]] = []
    if roles.blackout_pairs and trade_id:
        print(f"# {len(roles.blackout_pairs)} blackout pair(s) on chart "
              "— emitting pause/resume bundles")
        for i, pair in enumerate(roles.blackout_pairs, start=1):
            start_drawing, end_drawing = pair
            start_iso = utc_iso(start_drawing["points"][0]["time"])
            end_iso = utc_iso(end_drawing["points"][0]["time"])
            print(f"#   pair {i}: {start_iso} → {end_iso}")
            pause_dir = out_dir / f"pause-{i}"
            pause_dir.mkdir(parents=True, exist_ok=True)
            pause_spec_path = pause_dir / "_input-pause.yaml"
            pause_spec = {
                "trade_id": trade_id,
                "instrument": instrument,
                "account": account,
                "broker": broker,
                "start_time": start_iso,
                "end_time": end_iso,
                "reason": f"news:{instrument}-{start_iso}",
            }
            write_pause_spec(pause_spec, pause_spec_path)
            try:
                run_build_pause(pause_spec_path, pause_dir)
            except (FileNotFoundError, RuntimeError) as e:
                print(f"ERROR: build-pause failed for pair {i}: {e}", file=sys.stderr)
                return 2
            pause_manifest_path = pause_dir / "manifest.yaml"
            if not pause_manifest_path.exists():
                print(f"ERROR: no manifest at {pause_manifest_path}", file=sys.stderr)
                return 3
            pause_manifest = parse_manifest(pause_manifest_path.read_text())
            pause_bundles.append((pair, pause_manifest, pause_dir))
        print()
    elif roles.blackout_pairs and not trade_id:
        # build-trade emitted a manifest but without a trade_id — this
        # is a defensive guard; the build-trade pipeline always stamps
        # one today. Don't silently arm pauses with no trade key.
        print("ERROR: have blackout pairs but H&S manifest has no trade_id; "
              "refusing to arm pause bundle", file=sys.stderr)
        return 3

    # News-window bundles. Parallel to pause_bundles but in their own
    # KV namespace and tied to the close-on-reversal alert rather than
    # entry gating. Operator opts in by drawing `news-start` /
    # `news-end` vertical lines; the matching `06-close-on-reversal`
    # alert in the trade bundle (when --close-on-reversal is set) is
    # what acts on the window.
    news_bundles: list[tuple[tuple[dict, dict], dict, Path]] = []
    if roles.news_pairs and trade_id:
        print(f"# {len(roles.news_pairs)} news pair(s) on chart "
              "— emitting news-start/news-end bundles")
        for i, pair in enumerate(roles.news_pairs, start=1):
            start_drawing, end_drawing = pair
            start_iso = utc_iso(start_drawing["points"][0]["time"])
            end_iso = utc_iso(end_drawing["points"][0]["time"])
            print(f"#   pair {i}: {start_iso} → {end_iso}")
            news_dir = out_dir / f"news-{i}"
            news_dir.mkdir(parents=True, exist_ok=True)
            news_spec_path = news_dir / "_input-news.yaml"
            news_spec = {
                "trade_id": trade_id,
                "instrument": instrument,
                "account": account,
                "broker": broker,
                "start_time": start_iso,
                "end_time": end_iso,
                "reason": f"news:{instrument}-{start_iso}",
            }
            write_news_spec(news_spec, news_spec_path)
            try:
                run_build_news(news_spec_path, news_dir)
            except (FileNotFoundError, RuntimeError) as e:
                print(f"ERROR: build-news failed for pair {i}: {e}", file=sys.stderr)
                return 2
            news_manifest_path = news_dir / "manifest.yaml"
            if not news_manifest_path.exists():
                print(f"ERROR: no manifest at {news_manifest_path}", file=sys.stderr)
                return 3
            news_manifest = parse_manifest(news_manifest_path.read_text())
            news_bundles.append((pair, news_manifest, news_dir))
        if not args.close_on_reversal:
            print("# WARN: news-* lines drawn but --close-on-reversal not set; "
                  "the news windows will arm in the worker but nothing fires on "
                  "them.", file=sys.stderr)
        print()
    elif roles.news_pairs and not trade_id:
        print("ERROR: have news pairs but H&S manifest has no trade_id; "
              "refusing to arm news bundle", file=sys.stderr)
        return 3

    if not args.create_alerts:
        if args.dry_run:
            print("# --dry-run: skipping alert creation and webhook POSTs.")
        else:
            print("# (default is dry-run; pass --create-alerts to push to TV)")
        return 0

    print("─" * 72)
    print("## Creating TV alerts")
    if skip_preps:
        print(f"# skipped preps (not in bundle): {', '.join(skip_preps)}")
    if args.max_retries is not None:
        print(f"# max_retries: {args.max_retries}")
    if args.entry_filter_script is not None:
        print(f"# allow_entry script: {args.entry_filter_script}")
    if args.entry_market:
        print("# entry_mode: market")
    print()

    payloads = []
    for entry in manifest["alerts"]:
        fname = entry["file"]
        spec_dict = build_alert_spec(entry, direction, roles)
        if spec_dict is None:
            print(f"# skipping {fname} (no drawing mapping)")
            continue
        signed_path = out_dir / fname
        if not signed_path.exists():
            print(f"# skipping {fname} (file missing)")
            continue
        # Stamp the TV alert title with `<trade_id>-<role>` so sorting by
        # name groups all 5 together and survives pruning sessions.
        role_slug = spec_dict["tv_name"]
        spec_dict["tv_name"] = f"{trade_id}-{role_slug}" if trade_id else role_slug
        spec_dict["name"] = entry["file"]
        spec_dict["message"] = signed_path.read_text()
        payloads.append(spec_dict)

    # Stack pause-bundle alerts onto the H&S payloads. Each bundle was
    # built with its own blackout_pair, so we pass that pair through
    # to `build_alert_spec`. The signed YAML lives in the bundle's own
    # sub-dir; tv_name is stamped `<trade_id>-pause-<id>` /
    # `<trade_id>-resume-<id>` for chronological grouping with the
    # parent trade's alerts.
    for pair, pause_manifest, pause_dir in pause_bundles:
        for entry in pause_manifest.get("alerts", []):
            fname = entry["file"]
            spec_dict = build_alert_spec(
                entry, direction, roles, blackout_pair=pair
            )
            if spec_dict is None:
                print(f"# skipping {fname} (no drawing mapping)")
                continue
            signed_path = pause_dir / fname
            if not signed_path.exists():
                print(f"# skipping {fname} (file missing)")
                continue
            role_slug = spec_dict["tv_name"]
            spec_dict["tv_name"] = (
                f"{trade_id}-{role_slug}" if trade_id else role_slug
            )
            spec_dict["name"] = entry["file"]
            spec_dict["message"] = signed_path.read_text()
            payloads.append(spec_dict)

    # Same flow for news-bundles. The mapping in `build_alert_spec`
    # dispatches on basename prefix (`01-news-start-` / `02-news-end-`)
    # and uses `news_pair` to anchor the vertical-line drawings.
    for pair, news_manifest, news_dir in news_bundles:
        for entry in news_manifest.get("alerts", []):
            fname = entry["file"]
            spec_dict = build_alert_spec(
                entry, direction, roles, news_pair=pair
            )
            if spec_dict is None:
                print(f"# skipping {fname} (no drawing mapping)")
                continue
            signed_path = news_dir / fname
            if not signed_path.exists():
                print(f"# skipping {fname} (file missing)")
                continue
            role_slug = spec_dict["tv_name"]
            spec_dict["tv_name"] = (
                f"{trade_id}-{role_slug}" if trade_id else role_slug
            )
            spec_dict["name"] = entry["file"]
            spec_dict["message"] = signed_path.read_text()
            payloads.append(spec_dict)

    if not payloads:
        print("# nothing to post.")
        return 0

    try:
        results = create_alerts(payloads, TV_MCP_ROOT)
    except (RuntimeError, subprocess.TimeoutExpired) as e:
        print(f"create_alerts FAILED: {e}")
        return 4
    for r in results:
        status = r.get("status") or r.get("error") or "?"
        body = r.get("body", "")
        print(f"  {r.get('name')}: status={status}  body={body[:200]}")
        dbg = r.get("debug")
        if dbg:
            print(f"    tool={dbg.get('tool')}  drawing_id={dbg.get('drawing_id')}")
            print(f"    series[1]={json.dumps(dbg.get('condition_series_1'))[:400]}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
