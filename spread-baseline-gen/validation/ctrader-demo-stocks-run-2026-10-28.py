#!/usr/bin/env python3
"""Read-only historical collection, followed by the strict additive finisher.

Operational record for this fixed demo sampling window. Broker orders and
deployment are not part of this job. All measurements use the Rust cache path.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import subprocess
import time


WORKSPACE = Path(__file__).resolve().parents[2]
ARTIFACTS = Path(__file__).resolve().parent
PARSER = argparse.ArgumentParser(description=__doc__)
PARSER.add_argument("binaries", type=Path)
PARSER.add_argument("--verify-baked", action="store_true",
                    help="Recover after baking: verify all identical rows and rerun gates")
OPTIONS = PARSER.parse_args()
BINARIES = OPTIONS.binaries.resolve()
REPORT = ARTIFACTS / "ctrader-demo-stocks-90d-report.json"
STATUS = ARTIFACTS / "ctrader-demo-stocks-job.json"
FINISH = ARTIFACTS / "ctrader-demo-stocks-finish.json"
FROM = "2026-07-24T16:26:00Z"
TO = "2026-10-22T16:26:00Z"
POLICY = ["--quarantine-crossed-minutes", "--max-quarantined-minutes", "20"]


def record(state, **details):
    data = {
        "state": state,
        "updated_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "account": "ctrader-demo",
        "requested_count": 350,
        "report": str(REPORT),
        "finish_status": str(FINISH),
        "binaries": str(BINARIES),
        "verify_baked": OPTIONS.verify_baked,
        **details,
    }
    temporary = STATUS.with_suffix(".json.tmp")
    temporary.write_text(json.dumps(data, indent=2) + "\n")
    os.replace(temporary, STATUS)
    print(json.dumps(data), flush=True)


def run():
    attempts = () if OPTIONS.verify_baked else range(1, 4)
    for attempt in attempts:
        selection = (
            ["--resume", str(REPORT)]
            if REPORT.exists()
            else ["--stocks", "--from", FROM, "--to", TO]
        )
        record("collecting", attempt=attempt)
        result = subprocess.run(
            [str(BINARIES / "generate-ctrader"), "--account", "ctrader-demo",
             "--out", str(REPORT), *POLICY, *selection],
            cwd=WORKSPACE, check=False,
        )
        if result.returncode == 0:
            break
        record("collection-attempt-failed", attempt=attempt,
               exit_code=result.returncode)
        if attempt == 3:
            raise RuntimeError("Collection remains incomplete; report retains failures")
        time.sleep(30)

    report = json.loads(REPORT.read_text())
    symbols = report["requested_symbols"]
    if not (report["complete"] and not report["failures"]
            and len(symbols) == len(set(symbols)) == len(report["instruments"]) == 350
            and report["identity"]["profile"] == "ctrader-demo"
            and report["identity"]["account_id"] == 49247794
            and report["identity"]["environment"] == "demo"
            and report["from_utc"] == FROM and report["to_utc"] == TO
            and report["quarantine_policy"]["max_minutes_per_instrument"] == 20):
        raise RuntimeError("Completed report differs from the pinned demo stock scope")

    record("verifying-and-baking", completed_count=350)
    selection = [argument for symbol in symbols for argument in ["--only", symbol]]
    recovery = ["--verify-baked"] if OPTIONS.verify_baked else []
    subprocess.run(
        [str(BINARIES / "finish-ctrader-nonstocks"), "--account", "ctrader-demo",
         "--stocks", "--workspace", str(WORKSPACE), "--report", str(REPORT),
         "--status", str(FINISH), "--from", FROM, "--to", TO,
         *POLICY, *selection, *recovery],
        cwd=WORKSPACE, check=True,
    )
    record("complete", completed_count=350,
           detail="All stock profiles baked and release gates passed; no deployment")


try:
    run()
except Exception as error:
    record("failed", error=str(error))
    raise
