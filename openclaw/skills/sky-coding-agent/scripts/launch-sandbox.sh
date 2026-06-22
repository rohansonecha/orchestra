#!/bin/bash
# launch-sandbox.sh — Launch an ephemeral work-agent sandbox via SkyPilot.
#
# Usage: launch-sandbox.sh <task-id>
#
# Requires env vars: SKY_INFRA, GLM_API_KEY
# Requires private/ files to be present at $ORCHESTRA_HOME/private/

set -euo pipefail

TASK_ID="$1"
STATE_DIR="/tmp/orchestra-sandboxes"
CLUSTER_NAME="work-agent-${TASK_ID}"
ORCHESTRA_HOME="${ORCHESTRA_HOME:-/root/orchestra}"

mkdir -p "$STATE_DIR"

# Check if sandbox already exists
if [ -f "$STATE_DIR/${TASK_ID}.cluster" ]; then
    echo "Sandbox already running: $(cat "$STATE_DIR/${TASK_ID}.cluster")"
    exit 0
fi

# Launch the work-agent sandbox
sky launch \
    --infra "$SKY_INFRA" \
    -c "$CLUSTER_NAME" \
    --env GLM_API_KEY \
    -y \
    "${ORCHESTRA_HOME}/skypilot/work-agent.yaml" 2>&1

# Record the cluster name
echo "$CLUSTER_NAME" > "$STATE_DIR/${TASK_ID}.cluster"
echo "Sandbox launched: $CLUSTER_NAME"
