#!/usr/bin/env python3
"""Read-only full stock scan and strict additive promotion with immutable provenance.

No broker orders or deployment. Full failures remain in the original report;
fully verified eligible rows use a separate complete report and matching audit.
"""

import argparse
from pathlib import Path

from ctrader_stock_batch import Config, run


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument(
        "--verify-baked",
        action="store_true",
        help="Verify sealed evidence and baked rows, then rerun gates without collection",
    )
    options = parser.parse_args()
    run(
        Config(
            workspace=Path(__file__).resolve().parents[2],
            artifacts=Path(__file__).resolve().parent,
            binaries=options.binaries.resolve(),
            verify_baked=options.verify_baked,
        )
    )


if __name__ == "__main__":
    main()
