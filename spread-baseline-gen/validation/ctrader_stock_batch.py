"""Collect a full stock scan, then strictly promote immutable eligible evidence."""

import datetime
import fcntl
import json
import os
import subprocess
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path

import ctrader_stock_evidence as evidence


@dataclass(frozen=True)
class Config:
    workspace: Path
    artifacts: Path
    binaries: Path
    verify_baked: bool = False
    retry_delay: float = 30
    run_id: str = field(default_factory=lambda: uuid.uuid4().hex)

    @property
    def report(self):
        return self.artifacts / "ctrader-demo-stocks-90d-report.json"

    @property
    def ready(self):
        return self.artifacts / "ctrader-demo-stocks-ready-90d-report.json"

    @property
    def provenance(self):
        return self.artifacts / "ctrader-demo-stocks-ready-provenance.json"

    @property
    def preparation(self):
        return self.artifacts / "ctrader-demo-stocks-ready-preparation.json"

    @property
    def status(self):
        return self.artifacts / "ctrader-demo-stocks-job.json"

    @property
    def finish(self):
        suffix = "-recovery-" + self.run_id if self.verify_baked else ""
        return self.artifacts / f"ctrader-demo-stocks-finish{suffix}.json"


POLICY = ["--quarantine-crossed-minutes", "--max-quarantined-minutes", "20"]


def record(config, state, **details):
    value = dict(
        state=state,
        updated_at_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        account="ctrader-demo",
        requested_count=350,
        report=str(config.report),
        finish_status=str(config.finish),
        provenance=str(config.provenance),
        binaries=str(config.binaries),
        verify_baked=config.verify_baked,
        **details,
    )
    temporary = config.status.with_suffix("." + config.run_id + ".tmp")
    with temporary.open("wb") as stream:
        os.fchmod(stream.fileno(), 0o600)
        stream.write(evidence.encoded(value))
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, config.status)
    print(json.dumps(value), flush=True)


def collect(config):
    for attempt in range(1, 4):
        selection = (
            ["--resume", str(config.report)]
            if config.report.exists()
            else ["--stocks", "--from", evidence.FROM, "--to", evidence.TO]
        )
        record(config, "collecting", attempt=attempt)
        result = subprocess.run(
            [
                str(config.binaries / "generate-ctrader"),
                "--account",
                "ctrader-demo",
                "--out",
                str(config.report),
                *POLICY,
                *selection,
            ],
            cwd=config.workspace,
            check=False,
        )
        if result.returncode == 0:
            return
        record(
            config,
            "collection-attempt-failed",
            attempt=attempt,
            exit_code=result.returncode,
        )
        if attempt < 3:
            time.sleep(config.retry_delay)
    # A nonzero generator exit may still leave a fully attempted scan. The evidence
    # validator distinguishes retained per-symbol failures from an interrupted scan.


def summary(manifest):
    return {
        "ready_count": len(manifest["selected_symbols"]),
        "failed_count": len(manifest["failed_symbols"]),
        "ready_symbols": manifest["selected_symbols"],
        "failed_symbols": manifest["failed_symbols"],
        "selected_report": manifest["selected_report"]["path"],
    }


def finish(config, manifest):
    selection = [
        argument
        for symbol in manifest["selected_symbols"]
        for argument in ["--only", symbol]
    ]
    recovery = ["--verify-baked"] if config.verify_baked else []
    return subprocess.run(
        [
            str(config.binaries / "finish-ctrader-nonstocks"),
            "--account",
            "ctrader-demo",
            "--stocks",
            "--workspace",
            str(config.workspace),
            "--report",
            manifest["selected_report"]["path"],
            "--status",
            str(config.finish),
            "--from",
            evidence.FROM,
            "--to",
            evidence.TO,
            *POLICY,
            *selection,
            *recovery,
        ],
        cwd=config.workspace,
        check=False,
    )


def run_locked(config):
    details = {}
    try:
        if config.verify_baked and config.status.exists():
            # Preserve the previous failed job and finisher statuses. A recovery gets
            # a separate finisher status file and never reseals measurement evidence.
            evidence.write_new(
                config.artifacts
                / f"ctrader-demo-stocks-job-before-recovery-{config.run_id}.json",
                config.status.read_bytes(),
            )
        if config.verify_baked:
            # Verification-only recovery never creates a missing seal or artifact.
            manifest = evidence.verify(config)
        elif (
            config.provenance.exists()
            and config.provenance.with_suffix(".sha256").exists()
        ):
            manifest = evidence.verify(config)
        else:
            # Once publication starts, the full source is frozen. Even an orphaned
            # publication temp means preparation must resume without recollection.
            if not evidence.preparation_started(config):
                collect(config)
            manifest = evidence.prepare(config)
        details = summary(manifest)
        record(
            config,
            "verifying-and-baking" if not config.verify_baked else "verifying-baked",
            **details,
        )
        result = finish(config, manifest)
        # Complete-checkpoint finisher optimization retains report/audit bytes. A
        # mutation is fatal even if the subprocess reports success.
        evidence.verify(config)
        if result.returncode:
            raise RuntimeError(
                f"strict finisher failed with exit {result.returncode}; retain evidence and use --verify-baked if rows were baked"
            )
        state = "complete-with-failures" if details["failed_count"] else "complete"
        record(
            config,
            state,
            **details,
            detail="Selected profiles passed strict additive bake/release gates; original failed profiles remain uncovered; no deployment",
        )
    except Exception as error:
        if isinstance(error, evidence.NoEligible):
            details = {
                "ready_count": 0,
                "failed_count": len(error.symbols),
                "ready_symbols": [],
                "failed_symbols": error.symbols,
            }
        record(config, "failed", **details, error=str(error))
        raise


def run(config):
    # Never let a recovery race an active collector or overwrite its status.
    with config.status.with_suffix(".json.lock").open("a") as lock:
        os.fchmod(lock.fileno(), 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        run_locked(config)
