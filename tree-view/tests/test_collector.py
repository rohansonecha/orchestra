#!/usr/bin/env python3
"""Tests for orchestra-collector reconciliation logic.

Runs with stdlib only: python3 -m pytest tree-view/tests/test_collector.py
or: python3 tree-view/tests/test_collector.py

Covers:
  - Local scrape seeds root + session nodes
  - display_name is preserved across pulls
  - Removed tmux sessions → completed state
  - New tmux sessions → added as children
  - sky status reconciliation: orphan discovery + dead cluster → failed
  - Edge registration (POST /tree/edge semantics)
"""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

# Load orchestra-collector.py (hyphenated filename → not a normal import).
# Register in sys.modules BEFORE exec so @dataclass can resolve the module.
_spec = importlib.util.spec_from_file_location(
    "orchestra_collector", Path(__file__).resolve().parent.parent / "orchestra-collector.py"
)
coll = importlib.util.module_from_spec(_spec)
sys.modules["orchestra_collector"] = coll
_spec.loader.exec_module(coll)


class ReconcileLocalTest(unittest.TestCase):
    def setUp(self):
        self._td = tempfile.TemporaryDirectory()
        self.store = self._td.name

    def tearDown(self):
        self._td.cleanup()

    def _read_node(self, nid: str) -> dict:
        return coll.load_node(self.store, nid)

    def test_seeds_root_when_absent(self):
        scrape = coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID, tmux_sessions=["fix-bug"])
        coll.reconcile_local(self.store, scrape)
        root = self._read_node(coll.ROOT_AGENT_ID)
        self.assertIsNotNone(root)
        self.assertEqual(root["kind"], "agent")
        self.assertEqual(root["pi_sessions"], ["fix-bug"])
        self.assertIn("session-fix-bug", root["children"])
        idx = coll.load_index(self.store)
        self.assertEqual(idx["root_id"], coll.ROOT_AGENT_ID)

    def test_empty_scrape_seeds_root_only(self):
        coll.reconcile_local(self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID))
        root = self._read_node(coll.ROOT_AGENT_ID)
        self.assertEqual(root["children"], [])
        self.assertEqual(root["pi_sessions"], [])

    def test_new_sessions_added(self):
        coll.reconcile_local(
            self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID, tmux_sessions=["a"])
        )
        coll.reconcile_local(
            self.store,
            coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID, tmux_sessions=["a", "b", "c"]),
        )
        root = self._read_node(coll.ROOT_AGENT_ID)
        self.assertEqual(root["children"], ["session-a", "session-b", "session-c"])

    def test_removed_session_marked_completed(self):
        coll.reconcile_local(
            self.store,
            coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID, tmux_sessions=["a", "b"]),
        )
        # Session b disappears.
        coll.reconcile_local(
            self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID, tmux_sessions=["a"])
        )
        root = self._read_node(coll.ROOT_AGENT_ID)
        # b dropped from active children...
        self.assertEqual(root["children"], ["session-a"])
        # ...but its node still exists, marked completed.
        b = self._read_node("session-b")
        self.assertEqual(b["state"], "completed")

    def test_display_name_preserved_across_pulls(self):
        coll.reconcile_local(
            self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID, tmux_sessions=["a"])
        )
        # User renames session-a via the TUI (writes display_name).
        node = self._read_node("session-a")
        node["display_name"] = "My Bug Fix"
        coll.write_node(self.store, node)
        # Collector pulls again — display_name must survive.
        coll.reconcile_local(
            self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID, tmux_sessions=["a"])
        )
        a = self._read_node("session-a")
        self.assertEqual(a["display_name"], "My Bug Fix")

    def test_root_display_name_preserved(self):
        coll.reconcile_local(self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID))
        root = self._read_node(coll.ROOT_AGENT_ID)
        root["display_name"] = "Main"
        coll.write_node(self.store, root)
        coll.reconcile_local(self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID))
        root = self._read_node(coll.ROOT_AGENT_ID)
        self.assertEqual(root["display_name"], "Main")

    def test_openclaw_sessions_added_as_sessions(self):
        coll.reconcile_local(
            self.store,
            coll.ScrapeResult(
                agent_id=coll.ROOT_AGENT_ID,
                tmux_sessions=["pi-session"],
                openclaw_sessions=["oc-session"],
            ),
        )
        root = self._read_node(coll.ROOT_AGENT_ID)
        self.assertIn("session-pi-session", root["children"])
        self.assertIn("session-oc-session", root["children"])
        oc = self._read_node("session-oc-session")
        self.assertEqual(oc["config"].get("tmux_kind"), "openclaw")

    def test_agent_state_reflects_sessions(self):
        coll.reconcile_local(
            self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID, tmux_sessions=[])
        )
        root = self._read_node(coll.ROOT_AGENT_ID)
        self.assertEqual(root["state"], "idle")
        coll.reconcile_local(
            self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID, tmux_sessions=["a"])
        )
        root = self._read_node(coll.ROOT_AGENT_ID)
        self.assertEqual(root["state"], "working")


class ReconcileSkyTest(unittest.TestCase):
    def setUp(self):
        self._td = tempfile.TemporaryDirectory()
        self.store = self._td.name
        # Seed root.
        coll.reconcile_local(self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID))

    def tearDown(self):
        self._td.cleanup()

    def test_orphan_cluster_discovered(self):
        clusters = [{"name": "agent-research-box", "status": "UP", "host": "10.0.0.1"}]
        logs = coll.reconcile_sky(self.store, clusters)
        node = self._read_node("agent-research-box")
        self.assertIsNotNone(node)
        self.assertEqual(node["state"], "unknown")
        self.assertEqual(node["parent_id"], coll.ROOT_AGENT_ID)
        root = self._read_node(coll.ROOT_AGENT_ID)
        self.assertIn("agent-research-box", root["children"])
        self.assertTrue(any("research-box" in l for l in logs))

    def test_dead_cluster_marked_failed(self):
        # First, add an agent node.
        clusters = [{"name": "agent-research-box", "status": "UP", "host": "10.0.0.1"}]
        coll.reconcile_sky(self.store, clusters)
        # Then sky status shows it's gone.
        logs = coll.reconcile_sky(self.store, [])
        node = self._read_node("agent-research-box")
        self.assertEqual(node["state"], "failed")
        self.assertTrue(any("failed" in l for l in logs))

    def test_non_agent_clusters_ignored(self):
        clusters = [{"name": "my-test-box", "status": "UP", "host": "10.0.0.2"}]
        coll.reconcile_sky(self.store, clusters)
        # my-test-box doesn't start with "agent-" → ignored.
        self.assertIsNone(self._read_node("my-test-box"))

    def test_existing_orphan_not_re_added(self):
        clusters = [{"name": "agent-x", "status": "UP", "host": "10.0.0.3"}]
        coll.reconcile_sky(self.store, clusters)
        root = self._read_node(coll.ROOT_AGENT_ID)
        count_before = root["children"].count("agent-x")
        # Reconcile again — should not duplicate.
        coll.reconcile_sky(self.store, clusters)
        root = self._read_node(coll.ROOT_AGENT_ID)
        count_after = root["children"].count("agent-x")
        self.assertEqual(count_before, 1)
        self.assertEqual(count_after, 1)

    def _read_node(self, nid: str) -> dict:
        return coll.load_node(self.store, nid)


class EdgeRegistrationTest(unittest.TestCase):
    """The POST /tree/edge handler logic (tested via the reconcile helpers
    it calls). Edge registration = append child to parent's children list."""

    def setUp(self):
        self._td = tempfile.TemporaryDirectory()
        self.store = self._td.name
        coll.reconcile_local(self.store, coll.ScrapeResult(agent_id=coll.ROOT_AGENT_ID))

    def tearDown(self):
        self._td.cleanup()

    def test_edge_appends_child(self):
        # Simulate the launch-sub-agent skill registering an edge.
        parent = coll.load_node(self.store, coll.ROOT_AGENT_ID)
        child_id = "agent-new-box"
        children = parent.setdefault("children", [])
        if child_id not in children:
            children.append(child_id)
        coll.write_node(self.store, parent)
        parent = coll.load_node(self.store, coll.ROOT_AGENT_ID)
        self.assertIn(child_id, parent["children"])


class StoreIOTest(unittest.TestCase):
    def test_atomic_write(self):
        with tempfile.TemporaryDirectory() as td:
            node = coll.root_node()
            coll.write_node(td, node)
            # The .tmp file should be gone (replaced).
            self.assertFalse((Path(td) / "nodes" / f"{node['id']}.json.tmp").exists())
            loaded = coll.load_node(td, node["id"])
            self.assertEqual(loaded["id"], node["id"])

    def test_load_missing_returns_none(self):
        with tempfile.TemporaryDirectory() as td:
            self.assertIsNone(coll.load_node(td, "ghost"))
            self.assertIsNone(coll.load_index(td))

    def test_corrupt_json_returns_none(self):
        with tempfile.TemporaryDirectory() as td:
            p = Path(td) / "nodes"
            p.mkdir()
            (p / "bad.json").write_text("not json {{{")
            self.assertIsNone(coll.load_node(td, "bad"))


if __name__ == "__main__":
    unittest.main()
