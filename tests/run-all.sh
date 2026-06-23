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

echo "============================================"
echo "  All tests passed!"
echo "============================================"
