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
from dataclasses import dataclass
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


def label_of(d: dict) -> str:
    return (d.get("properties", {}).get("text") or "").strip()


def latest_time(d: dict) -> int:
    return max(p["time"] for p in d["points"])


def classify(drawings: list[dict]) -> Roles:
    """Group drawings by role. When multiple drawings claim the same role,
    keep the latest one (older drawings are stale leftovers from prior setups)."""
    by_role: dict[str, list[tuple[dict, str]]] = {
        "invalidation": [],
        "break_and_close": [],
        "retest": [],
        "tp_fib": [],
        "trade_expiry": [],
    }

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
    return roles


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
) -> Optional[dict]:
    """Translate one manifest entry into the alert-payload shape the
    create_alerts JS expects. Returns None if the entry isn't postable
    (e.g. unrecognised role)."""
    fname: str = manifest_entry["file"]
    base = fname.removesuffix(".yaml")
    tv_name = base.split("-", 1)[1] if "-" in base else base  # strip "NN-"

    if base.startswith("01-veto-"):
        if roles.invalidation is None:
            return None
        # too-high (short) → cross_up; too-low (long) → cross_down.
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
    return p.parse_args(argv)


def main(argv: Optional[list[str]] = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])

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

    print(f"# Chart: {state['symbol']} {state['resolution']}")
    print(f"# Broker: {broker}  Instrument: {instrument}")
    print()

    drawings = list_drawings()
    roles = classify(drawings)

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
