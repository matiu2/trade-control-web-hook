#!/usr/bin/env bash
# Live integration test for prep/veto/clear-prep/clear-veto against the
# deployed Worker. Each test posts a real encrypted control envelope and
# parses the YAML response from `status` to verify state.
#
# Requirements:
#   - encrypt-payload binary built (`cargo build --release -p
#     trade-control-cli --bin encrypt-payload`)
#   - Key at $KEY_FILE (default: ~/.config/trade-control/key.hex)
#   - $TRADE_CONTROL_ENDPOINT set, or pass --endpoint <url> to this script
#
# Usage:
#   ./test-live-preps-vetos.sh
#   ./test-live-preps-vetos.sh --endpoint https://my-worker.workers.dev
#
# These tests use a synthetic instrument name (`TEST_INSTRUMENT_PREPS`) so
# they don't collide with any real trading state. The script cleans up
# after itself.

set -euo pipefail

KEY_FILE="${KEY_FILE:-$HOME/.config/trade-control/key.hex}"
BIN="${BIN:-./target/release/encrypt-payload}"
ENDPOINT="${TRADE_CONTROL_ENDPOINT:-}"
INSTRUMENT="TEST_INSTRUMENT_PREPS"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --endpoint) ENDPOINT="$2"; shift 2 ;;
        --key-file) KEY_FILE="$2"; shift 2 ;;
        --bin) BIN="$2"; shift 2 ;;
        --instrument) INSTRUMENT="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$ENDPOINT" ]]; then
    echo "set TRADE_CONTROL_ENDPOINT or pass --endpoint" >&2
    exit 2
fi
if [[ ! -x "$BIN" ]]; then
    echo "$BIN not found or not executable" >&2
    echo "build it with: cargo build --features cli --release --bin encrypt-payload" >&2
    exit 2
fi

PASS=0
FAIL=0
FAILURES=()

# Run encrypt-payload with the common flags. Output goes to stdout.
run() {
    "$BIN" "$@" --key-file "$KEY_FILE" --endpoint "$ENDPOINT"
}

# Pull the current status snapshot as YAML.
fetch_status() {
    run status
}

# `assert_contains <haystack> <pattern> <test name>`
assert_contains() {
    local haystack="$1" pattern="$2" name="$3"
    if echo "$haystack" | grep -qF "$pattern"; then
        echo "  PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $name"
        echo "    expected to find: $pattern"
        echo "    in:"
        echo "$haystack" | sed 's/^/      /'
        FAIL=$((FAIL + 1))
        FAILURES+=("$name")
    fi
}

# `assert_not_contains <haystack> <pattern> <test name>`
assert_not_contains() {
    local haystack="$1" pattern="$2" name="$3"
    if ! echo "$haystack" | grep -qF "$pattern"; then
        echo "  PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $name (unexpectedly found $pattern)"
        FAIL=$((FAIL + 1))
        FAILURES+=("$name")
    fi
}

# Always clean up our synthetic state, even if a test fails.
cleanup() {
    echo
    echo "cleanup: removing test preps and vetos on $INSTRUMENT"
    run clear-prep "$INSTRUMENT" step-one  >/dev/null 2>&1 || true
    run clear-prep "$INSTRUMENT" step-two  >/dev/null 2>&1 || true
    run clear-prep "$INSTRUMENT" refreshed >/dev/null 2>&1 || true
    run clear-veto "$INSTRUMENT" v-one     >/dev/null 2>&1 || true
    run clear-veto "$INSTRUMENT" v-two     >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Make sure we start from a known-clean state.
cleanup

echo
echo "=== Test 1: prep set is visible in status ==="
run prep "$INSTRUMENT" step-one --ttl-hours 1 >/dev/null
snap=$(fetch_status)
assert_contains "$snap" "step: step-one" "prep step-one appears in status"
assert_contains "$snap" "instrument: $INSTRUMENT" "prep instrument matches"

echo
echo "=== Test 2: a second prep on a different step shows alongside the first ==="
run prep "$INSTRUMENT" step-two --ttl-hours 1 >/dev/null
snap=$(fetch_status)
assert_contains "$snap" "step: step-one" "step-one still present"
assert_contains "$snap" "step: step-two" "step-two now present"

echo
echo "=== Test 3: veto set is visible in status ==="
run veto "$INSTRUMENT" v-one --ttl-hours 1 >/dev/null
snap=$(fetch_status)
assert_contains "$snap" "name: v-one" "veto v-one appears in status"

echo
echo "=== Test 4: a second veto sits alongside the first ==="
run veto "$INSTRUMENT" v-two --ttl-hours 1 >/dev/null
snap=$(fetch_status)
assert_contains "$snap" "name: v-one" "v-one still present"
assert_contains "$snap" "name: v-two" "v-two now present"

echo
echo "=== Test 5: clear-prep removes the prep ==="
run clear-prep "$INSTRUMENT" step-one >/dev/null
snap=$(fetch_status)
assert_not_contains "$snap" "step: step-one" "step-one no longer in status"
assert_contains "$snap" "step: step-two" "step-two unaffected by clearing step-one"

echo
echo "=== Test 6: clear-veto removes the veto ==="
run clear-veto "$INSTRUMENT" v-one >/dev/null
snap=$(fetch_status)
assert_not_contains "$snap" "name: v-one" "v-one no longer in status"
assert_contains "$snap" "name: v-two" "v-two unaffected by clearing v-one"

echo
echo "=== Test 7: re-firing a prep refreshes its set_at timestamp ==="
run prep "$INSTRUMENT" refreshed --ttl-hours 1 >/dev/null
first_snap=$(fetch_status)
first_set_at=$(echo "$first_snap" | awk '/step: refreshed/{flag=1} flag && /set_at:/{print; exit}')
sleep 2
run prep "$INSTRUMENT" refreshed --ttl-hours 1 >/dev/null
second_snap=$(fetch_status)
second_set_at=$(echo "$second_snap" | awk '/step: refreshed/{flag=1} flag && /set_at:/{print; exit}')
if [[ -n "$first_set_at" && -n "$second_set_at" && "$first_set_at" != "$second_set_at" ]]; then
    echo "  PASS: refresh updates set_at ($first_set_at -> $second_set_at)"
    PASS=$((PASS + 1))
else
    echo "  FAIL: refresh did not update set_at"
    echo "    first:  $first_set_at"
    echo "    second: $second_set_at"
    FAIL=$((FAIL + 1))
    FAILURES+=("prep refresh updates set_at")
fi

echo
echo "=== Test 8: status snapshot records the outcome of recent control actions ==="
snap=$(fetch_status)
assert_contains "$snap" "action: prep" "seen index records prep actions"
assert_contains "$snap" "outcome: 'prep-set: refreshed" "outcome string carries the prep step + ttl"

echo
echo "=== Test 9: clearing a flag that isn't set returns noop, recorded in status ==="
run clear-prep "$INSTRUMENT" ghost-step >/dev/null
snap=$(fetch_status)
assert_contains "$snap" "outcome: 'prep-cleared: ghost-step (noop)'" "noop clear-prep records (noop) outcome"

echo
echo "=== Summary ==="
echo "passed: $PASS"
echo "failed: $FAIL"
if (( FAIL > 0 )); then
    echo
    echo "failed tests:"
    for f in "${FAILURES[@]}"; do
        echo "  - $f"
    done
    exit 1
fi
