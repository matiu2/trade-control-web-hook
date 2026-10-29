"""Operational runner integration tests; executable fixtures never contact a broker."""

import copy
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import ctrader_stock_batch as batch
import ctrader_stock_evidence as evidence
from ctrader_stock_batch import Config, run

ROOT = Path(__file__).resolve().parents[2]
REPORT = "ctrader-demo-stocks-90d-report.json"
READY = "ctrader-demo-stocks-ready-90d-report.json"
PROVENANCE = "ctrader-demo-stocks-ready-provenance.json"
STATUS = "ctrader-demo-stocks-job.json"
FINISH = "ctrader-demo-stocks-finish.json"


def read(path):
    return json.loads(path.read_text())


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class StockBatchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "trade-control-web-hook"
        self.root.mkdir()
        self.artifacts = self.root / "validation"
        self.artifacts.mkdir()
        self.binaries = self.root / "bin"
        self.binaries.mkdir()
        self.config = Config(self.root, self.artifacts, self.binaries, retry_delay=0)
        native = read(ROOT / "broker-ctrader/src/catalogue/stocks.json")
        directory = self.root / "broker-ctrader/src/catalogue"
        directory.mkdir(parents=True)
        (directory / "stocks.json").write_text(json.dumps(native))
        self.native = native["records"]
        source = read(
            ROOT
            / "spread-baseline-gen/validation/ctrader-demo-etheur-quota20-90d-report.json"
        )
        template = source["instruments"][0]
        template["profile"].update(
            n_bars=1000,
            review="Reviewed",
            elevated_hours=0,
            hour_widen_frac=[0.0] * 24,
            hour_p90_frac=[0.0001] * 24,
        )
        template["elevated_local_hours"] = []
        template["quarantined_minutes"] = []
        source["requested_symbols"] = [row["name"] for row in self.native]
        source["instruments"] = []
        source["failures"] = [
            {"symbol": "AARONS", "error": "no minute candles for AARONS"}
        ]
        source["complete"] = False
        source["excluded"] = []
        for native_row in self.native:
            if native_row["name"] == "AARONS":
                continue
            row = copy.deepcopy(template)
            row.update(
                symbol=native_row["name"],
                symbol_id=native_row["id"],
                pip_size=10.0 ** (-native_row["pip_position"]),
                schedule="utc",
                timezone="UTC",
                first_minute_utc=source["from_utc"],
                last_minute_utc="2026-10-22T16:25:00Z",
            )
            source["instruments"].append(row)
        records = []
        for symbol in ["AARONS", source["instruments"][0]["symbol"]]:
            native_row = next(row for row in self.native if row["name"] == symbol)
            minute = {
                "symbol": symbol,
                "symbol_id": native_row["id"],
                "from_utc": "2026-07-24T16:27:00Z",
                "to_utc": "2026-07-24T16:28:00Z",
                "observed_at_utc": "2026-07-24T16:27:15Z",
                "bid": 2.0,
                "ask": 1.0,
                "reason": "crossed-historical-book",
            }
            records.append(minute)
            for row in source["instruments"]:
                if row["symbol"] == symbol:
                    row["quarantined_minutes"] = [minute]
        self.source = source
        self.audit = {
            "binding": {
                "version": 1,
                "identity": source["identity"],
                "from_utc": source["from_utc"],
                "to_utc": source["to_utc"],
                "requested_symbols": source["requested_symbols"],
                "policy": source["quarantine_policy"],
            },
            "records": sorted(records, key=lambda r: (r["symbol"], r["from_utc"])),
        }
        self._inputs()
        self._program(
            "generate-ctrader",
            """
args=sys.argv[1:];path=Path(args[args.index("--out")+1]);report=json.loads(Path("input.json").read_text());audit=json.loads(Path("audit-input.json").read_text())
audit_path=path.with_suffix(".quarantine.json");audit_path.write_text(json.dumps(audit,indent=2))
report["quarantine_audit"]={"path":str(audit_path),"sha256":hashlib.sha256(audit_path.read_bytes()).hexdigest(),"excluded_minutes":len(audit["records"])}
path.write_text(json.dumps(report,indent=2));sys.exit(0 if report["complete"] else 1)
""",
        )
        self._program(
            "finish-ctrader-nonstocks",
            """
args=sys.argv[1:];status=Path(args[args.index("--status")+1]);report=json.loads(Path(args[args.index("--report")+1]).read_text())
assert report["complete"] and not report["failures"] and "--allow-partial" not in args and "--stocks" in args
if "--verify-baked" in args: assert Path("baked").exists()
Path("baked").write_text("baked selected rows")
if Path("fail-finish").exists():
    Path("fail-finish").unlink();status.write_text(json.dumps({"state":"failed","error":"gate failure fixture"}));sys.exit(9)
status.write_text(json.dumps({"state":"complete"}))
""",
        )

    def _inputs(self):
        (self.root / "input.json").write_text(json.dumps(self.source))
        (self.root / "audit-input.json").write_text(json.dumps(self.audit))

    def _program(self, name, body):
        path = self.binaries / name
        path.write_text(
            "#!/usr/bin/env python3\nimport sys,json,hashlib\nfrom pathlib import Path\n"
            + f"with Path({str(self.root / 'calls.jsonl')!r}).open('a') as log: log.write(json.dumps([Path(sys.argv[0]).name,sys.argv[1:],str(Path.cwd())])+'\\n')\n"
            + body
        )
        path.chmod(0o700)

    def calls(self, name):
        path = self.root / "calls.jsonl"
        return (
            [
                row[1]
                for row in map(json.loads, path.read_text().splitlines())
                if row[0] == name
            ]
            if path.exists()
            else []
        )

    def test_full_scan_three_attempts_then_only_349_strict_complete_rows_finish(self):
        run(self.config)
        self.assertEqual(len(self.calls("generate-ctrader")), 3)
        self.assertIn("--stocks", self.calls("generate-ctrader")[0])
        self.assertTrue(
            all("--resume" in call for call in self.calls("generate-ctrader")[1:])
        )
        selected = read(self.artifacts / READY)
        full = read(self.artifacts / REPORT)
        self.assertEqual(full["failures"], self.source["failures"])
        self.assertEqual(len(full["requested_symbols"]), 350)
        self.assertEqual(len(selected["requested_symbols"]), 349)
        self.assertNotIn("AARONS", selected["requested_symbols"])
        self.assertTrue(selected["complete"])
        self.assertEqual(selected["failures"], [])
        args = self.calls("finish-ctrader-nonstocks")[0]
        self.assertEqual(args.count("--only"), 349)
        self.assertNotIn("--allow-partial", args)
        provenance = read(self.artifacts / PROVENANCE)
        self.assertEqual(
            provenance["source_report"]["sha256"], sha(self.artifacts / REPORT)
        )
        self.assertEqual(
            provenance["source_audit"]["sha256"],
            sha(self.artifacts / REPORT.replace(".json", ".quarantine.json")),
        )
        self.assertEqual(provenance["source_failures"], self.source["failures"])
        audit = read(Path(selected["quarantine_audit"]["path"]))
        self.assertEqual(
            audit["binding"]["requested_symbols"], selected["requested_symbols"]
        )
        self.assertEqual(len(audit["records"]), 1)
        for row in selected["instruments"]:
            self.assertEqual(
                row.get("quarantined_minutes", []),
                [r for r in audit["records"] if r["symbol"] == row["symbol"]],
            )
        status = read(self.artifacts / STATUS)
        self.assertEqual(status["state"], "complete-with-failures")
        self.assertEqual((status["ready_count"], status["failed_count"]), (349, 1))
        self.assertEqual(status["failed_symbols"], ["AARONS"])

    def test_all_350_successes_use_the_immutable_full_report(self):
        row = copy.deepcopy(self.source["instruments"][0])
        native = next(r for r in self.native if r["name"] == "AARONS")
        row.update(
            symbol="AARONS",
            symbol_id=native["id"],
            pip_size=10.0 ** (-native["pip_position"]),
            quarantined_minutes=[self.audit["records"][0]],
        )
        self.source["instruments"].append(row)
        self.source["failures"] = []
        self.source["complete"] = True
        self._inputs()
        run(self.config)
        self.assertEqual(len(self.calls("generate-ctrader")), 1)
        self.assertFalse((self.artifacts / READY).exists())
        manifest = read(self.artifacts / PROVENANCE)
        self.assertEqual(manifest["selected_report"], manifest["source_report"])
        self.assertEqual(read(self.artifacts / STATUS)["state"], "complete")

    def test_unattempted_or_unknown_symbols_never_derive_or_finish(self):
        for mode in ["unattempted", "unknown"]:
            with self.subTest(mode=mode):
                original = copy.deepcopy(self.source)
                if mode == "unattempted":
                    self.source["instruments"].pop()
                else:
                    self.source["requested_symbols"][-1] = "UNKNOWN"
                self._inputs()
                with self.assertRaises(ValueError):
                    run(self.config)
                self.assertFalse((self.artifacts / PROVENANCE).exists())
                self.assertEqual(self.calls("finish-ctrader-nonstocks"), [])
                self.source = original

    def test_quality_failed_rows_are_explicit_exclusions(self):
        row = self.source["instruments"][-1]
        row["profile"]["review"] = "InsufficientData"
        self._inputs()
        run(self.config)
        provenance = read(self.artifacts / PROVENANCE)
        self.assertEqual(provenance["quality_exclusions"][0]["symbol"], row["symbol"])
        self.assertEqual(read(self.artifacts / STATUS)["failed_count"], 2)

    def test_lost_audit_records_and_invalid_native_identity_are_fatal(self):
        self.audit["records"] = [self.audit["records"][0]]
        self._inputs()
        with self.assertRaises(ValueError):
            run(self.config)
        self.assertEqual(self.calls("finish-ctrader-nonstocks"), [])
        self.source["identity"]["account_id"] = 123
        self._inputs()
        with self.assertRaises(ValueError):
            run(self.config)

    def test_baked_then_gate_failure_recovers_without_rewriting_any_evidence(self):
        (self.root / "fail-finish").touch()
        with self.assertRaises(RuntimeError):
            run(self.config)
        paths = [self.artifacts / name for name in [REPORT, READY, PROVENANCE, FINISH]]
        paths.extend(Path(read(path)["quarantine_audit"]["path"]) for path in paths[:2])
        before = {path: path.read_bytes() for path in paths}
        run(
            Config(
                self.root,
                self.artifacts,
                self.binaries,
                verify_baked=True,
                retry_delay=0,
            )
        )
        self.assertEqual(len(self.calls("generate-ctrader")), 3)
        self.assertIn("--verify-baked", self.calls("finish-ctrader-nonstocks")[-1])
        self.assertTrue(all(path.read_bytes() == data for path, data in before.items()))
        # Tampering with a sealed subset or provenance blocks recovery before finisher.
        selected = self.artifacts / READY
        selected.write_bytes(selected.read_bytes() + b" ")
        count = len(self.calls("finish-ctrader-nonstocks"))
        with self.assertRaises(ValueError):
            run(
                Config(
                    self.root,
                    self.artifacts,
                    self.binaries,
                    verify_baked=True,
                    retry_delay=0,
                )
            )
        self.assertEqual(len(self.calls("finish-ctrader-nonstocks")), count)

    def test_recovery_rejects_changed_provenance_and_source_audit_without_regeneration(
        self,
    ):
        run(self.config)
        calls = len(self.calls("finish-ctrader-nonstocks"))
        provenance = self.artifacts / PROVENANCE
        original = provenance.read_bytes()
        provenance.write_bytes(original + b" ")
        recovery = Config(
            self.root, self.artifacts, self.binaries, verify_baked=True, retry_delay=0
        )
        with self.assertRaises(ValueError):
            run(recovery)
        provenance.write_bytes(original)
        audit = self.artifacts / REPORT.replace(".json", ".quarantine.json")
        audit.write_bytes(audit.read_bytes() + b" ")
        with self.assertRaises(ValueError):
            run(
                Config(
                    self.root,
                    self.artifacts,
                    self.binaries,
                    verify_baked=True,
                    retry_delay=0,
                )
            )
        self.assertEqual(len(self.calls("finish-ctrader-nonstocks")), calls)
        self.assertEqual(len(self.calls("generate-ctrader")), 3)

    def test_atomic_publication_never_exposes_a_file_that_failed_to_sync(self):
        target = self.artifacts / "new-evidence.json"
        with (
            mock.patch.object(
                evidence.os, "fsync", side_effect=OSError("disk sync failed")
            ),
            self.assertRaises(OSError),
        ):
            evidence.write_new(target, b'{"complete":true}')
        self.assertFalse(target.exists())
        evidence.write_new(target, b'{"complete":true}')
        self.assertEqual(target.read_bytes(), b'{"complete":true}')
        with self.assertRaises(ValueError):
            evidence.write_new(target, b'{"different":true}')
        self.assertEqual(target.read_bytes(), b'{"complete":true}')
        raced = self.artifacts / "raced.json"

        def concurrent_publication(_source, destination, **_options):
            Path(destination).write_bytes(b"another publisher")
            raise FileExistsError("another publisher won")

        with (
            mock.patch.object(evidence.os, "link", side_effect=concurrent_publication),
            self.assertRaises(ValueError),
        ):
            evidence.write_new(raced, b"our content")
        self.assertEqual(raced.read_bytes(), b"another publisher")

    def test_partial_publication_temp_resumes_without_recollection(self):
        batch.collect(self.config)
        full = self.artifacts / REPORT
        original = full.read_bytes()
        orphan = self.artifacts / (READY + ".publish.interrupted.tmp")
        orphan.write_bytes(b'{"partial')
        run(self.config)
        self.assertEqual(len(self.calls("generate-ctrader")), 3)
        self.assertEqual(full.read_bytes(), original)
        self.assertEqual(orphan.read_bytes(), b'{"partial')
        self.assertEqual(len(read(self.artifacts / READY)["requested_symbols"]), 349)

    def test_manifest_without_seal_requires_normal_preparation_not_verify_baked(self):
        original_write = evidence.write_new

        def interrupt_seal(path, raw):
            if Path(path) == (self.artifacts / PROVENANCE).with_suffix(".sha256"):
                raise RuntimeError("interrupted before seal publication")
            original_write(path, raw)

        with (
            mock.patch.object(evidence, "write_new", side_effect=interrupt_seal),
            self.assertRaises(RuntimeError),
        ):
            run(self.config)
        files = [self.artifacts / name for name in [REPORT, READY, PROVENANCE]]
        files.extend(Path(read(path)["quarantine_audit"]["path"]) for path in files[:2])
        before = {path: path.read_bytes() for path in files}
        recovery = Config(
            self.root, self.artifacts, self.binaries, verify_baked=True, retry_delay=0
        )
        with self.assertRaises(ValueError):
            run(recovery)
        self.assertFalse((self.artifacts / PROVENANCE).with_suffix(".sha256").exists())
        self.assertEqual(self.calls("finish-ctrader-nonstocks"), [])
        run(self.config)
        self.assertEqual(len(self.calls("generate-ctrader")), 3)
        self.assertTrue(all(path.read_bytes() == raw for path, raw in before.items()))
        self.assertEqual(len(self.calls("finish-ctrader-nonstocks")), 1)

    def test_preparation_marker_keeps_original_source_frozen_after_interruption(self):
        original_write = evidence.write_new

        def interrupt_ready(path, raw):
            if Path(path) == self.artifacts / READY:
                raise RuntimeError("interrupted ready publication")
            original_write(path, raw)

        with (
            mock.patch.object(evidence, "write_new", side_effect=interrupt_ready),
            self.assertRaises(RuntimeError),
        ):
            run(self.config)
        self.assertTrue(self.config.preparation.exists())
        source = self.artifacts / REPORT
        changed = read(source)
        changed["failures"][0]["error"] = "changed after source freeze"
        source.write_text(json.dumps(changed))
        with self.assertRaises(ValueError):
            run(self.config)
        self.assertEqual(len(self.calls("generate-ctrader")), 3)
        self.assertEqual(self.calls("finish-ctrader-nonstocks"), [])

    def test_partial_legacy_final_is_never_replaced_or_triggered_recollection(self):
        batch.collect(self.config)
        partial = self.artifacts / READY
        partial.write_bytes(b'{"partial')
        with self.assertRaises(ValueError):
            run(self.config)
        self.assertEqual(partial.read_bytes(), b'{"partial')
        self.assertEqual(len(self.calls("generate-ctrader")), 3)
        self.assertEqual(self.calls("finish-ctrader-nonstocks"), [])

    def isolated_workspace(self):
        isolated = self.root.parent / "isolated" / "trade-control-web-hook"
        isolated.mkdir(parents=True, exist_ok=True)
        (isolated / "baked").write_text("existing baked selected rows")
        for workspace in [self.root, isolated]:
            table = workspace.parent / "candle-cache/src/spread_profiles/ctrader.rs"
            table.parent.mkdir(parents=True, exist_ok=True)
            table.write_bytes(b"all 221 measured profile rows")
        return isolated

    def test_isolated_recovery_keeps_original_seals_but_routes_finisher_and_cwd(self):
        run(self.config)
        isolated = self.isolated_workspace()
        paths = [self.artifacts / name for name in [REPORT, READY, PROVENANCE]]
        paths.extend(Path(read(path)["quarantine_audit"]["path"]) for path in paths[:2])
        before = {path: path.read_bytes() for path in paths}
        run(
            Config(
                self.root,
                self.artifacts,
                self.binaries,
                verify_baked=True,
                retry_delay=0,
                verification_workspace=isolated,
            )
        )
        calls = self.calls("finish-ctrader-nonstocks")
        self.assertEqual(calls[-1][calls[-1].index("--workspace") + 1], str(isolated))
        self.assertEqual(
            calls[-1][calls[-1].index("--report") + 1], str(self.artifacts / READY)
        )
        recorded = list(
            map(json.loads, (self.root / "calls.jsonl").read_text().splitlines())
        )
        self.assertEqual(recorded[-1][2], str(isolated))
        self.assertEqual(len(self.calls("generate-ctrader")), 3)
        self.assertTrue(all(path.read_bytes() == raw for path, raw in before.items()))
        self.assertEqual(
            read(self.artifacts / STATUS)["verification_workspace"], str(isolated)
        )

    def test_verification_workspace_is_rejected_for_normal_collection_in_config_and_cli(
        self,
    ):
        with self.assertRaises(ValueError):
            Config(
                self.root,
                self.artifacts,
                self.binaries,
                verification_workspace=self.root,
            )
        result = subprocess.run(
            [
                sys.executable,
                str(
                    ROOT
                    / "spread-baseline-gen/validation/ctrader-demo-stocks-run-2026-10-28.py"
                ),
                str(self.binaries),
                "--verification-workspace",
                str(self.root),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("requires --verify-baked", result.stderr)
        self.assertEqual(self.calls("generate-ctrader"), [])

    def test_isolated_recovery_rejects_table_mismatch_before_or_after_finisher(self):
        run(self.config)
        isolated = self.isolated_workspace()
        original_table = (
            self.root.parent / "candle-cache/src/spread_profiles/ctrader.rs"
        )
        snapshot_table = isolated.parent / "candle-cache/src/spread_profiles/ctrader.rs"
        calls = len(self.calls("finish-ctrader-nonstocks"))
        snapshot_table.write_bytes(b"different table")
        with self.assertRaises(ValueError):
            run(
                Config(
                    self.root,
                    self.artifacts,
                    self.binaries,
                    verify_baked=True,
                    retry_delay=0,
                    verification_workspace=isolated,
                )
            )
        self.assertEqual(len(self.calls("finish-ctrader-nonstocks")), calls)
        snapshot_table.write_bytes(original_table.read_bytes())
        original_finish = batch.finish

        def changed_during_verification(config, manifest):
            result = original_finish(config, manifest)
            original_table.write_bytes(b"main table changed during verification")
            return result

        with (
            mock.patch.object(batch, "finish", side_effect=changed_during_verification),
            self.assertRaises(ValueError),
        ):
            run(
                Config(
                    self.root,
                    self.artifacts,
                    self.binaries,
                    verify_baked=True,
                    retry_delay=0,
                    verification_workspace=isolated,
                )
            )
        self.assertEqual(read(self.artifacts / STATUS)["state"], "failed")
        self.assertEqual(len(self.calls("generate-ctrader")), 3)

    def test_zero_eligible_is_failed_and_never_bakes(self):
        for row in self.source["instruments"]:
            row["profile"]["review"] = "InsufficientData"
        self._inputs()
        with self.assertRaises(ValueError):
            run(self.config)
        self.assertEqual(self.calls("finish-ctrader-nonstocks"), [])
        status = read(self.artifacts / STATUS)
        self.assertEqual(status["state"], "failed")
        self.assertEqual((status["ready_count"], status["failed_count"]), (0, 350))


if __name__ == "__main__":
    unittest.main()
