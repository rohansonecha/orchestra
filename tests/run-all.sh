#!/bin/bash
# tests/run-all.sh — Run all orchestra tests.
#
# Run on the test box (via tests/run-on-staging.sh):
#   bash ~/orchestra/tests/run-all.sh
# Do NOT run directly on main-box — it kills all tmux sessions.
#
# Any failure is a regression — either fix the code or fix the test.

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "============================================"
echo "  Orchestra Test Suite"
echo "============================================"
echo ""

echo "--- Basic Functionality ---"
bash "$SCRIPT_DIR/basic.sh"
echo ""

echo "--- Upgrade Safety ---"
bash "$SCRIPT_DIR/upgrade.sh"
echo ""

echo "--- Tree View: Rust unit tests ---"
cd ~/orchestra/tui && source ~/.cargo/env && cargo test --quiet 2>&1 | tail -5
echo ""

echo "--- Tree View: collector Python tests ---"
cd ~/orchestra/tree-view && python3 tests/test_collector.py 2>&1 | tail -5
echo ""

echo "--- Tree View: gen-yaml Python tests ---"
python3 tests/test_gen_yaml.py 2>&1 | tail -5
echo ""

echo "============================================"
echo "  All tests passed!"
echo "============================================"
