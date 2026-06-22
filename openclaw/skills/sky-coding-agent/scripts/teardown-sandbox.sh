#!/bin/bash
# teardown-sandbox.sh — Tear down a work-agent sandbox.
#
# Usage: teardown-sandbox.sh <task-id>

set -euo pipefail

TASK_ID="$1"
STATE_DIR="/tmp/orchestra-sandboxes"
CLUSTER_FILE="$STATE_DIR/${TASK_ID}.cluster"

if [ ! -f "$CLUSTER_FILE" ]; then
    echo "No sandbox for task '$TASK_ID'. Nothing to tear down."
    exit 0
fi

CLUSTER_NAME=$(cat "$CLUSTER_FILE")

sky down "$CLUSTER_NAME" -y 2>&1

rm -f "$CLUSTER_FILE" "$STATE_DIR/${TASK_ID}.session"
echo "Sandbox torn down: $CLUSTER_NAME"
