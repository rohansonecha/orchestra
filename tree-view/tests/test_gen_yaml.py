#!/usr/bin/env python3
"""Tests for gen-agent-yaml.py template rendering.

Runs with stdlib only: python3 tree-view/tests/test_gen_yaml.py
"""

from __future__ import annotations

import importlib.util
import json
import sys
import unittest
from pathlib import Path

# Load gen-agent-yaml.py (hyphenated filename → not a normal import).
_spec = importlib.util.spec_from_file_location(
    "gen_agent_yaml", Path(__file__).resolve().parent.parent / "gen-agent-yaml.py"
)
gen = importlib.util.module_from_spec(_spec)
sys.modules["gen_agent_yaml"] = gen
_spec.loader.exec_module(gen)


class RenderTest(unittest.TestCase):
    def test_substitutes_vars(self):
        out = gen.render("Hello {{NAME}}!", {"NAME": "research-box"})
        self.assertEqual(out, "Hello research-box!")

    def test_missing_var_becomes_empty(self):
        out = gen.render("[{{MISSING}}]", {})
        self.assertEqual(out, "[]")

    def test_conditional_block_included_when_truthy(self):
        tpl = "before {{#GPU}}gpu={{GPU}}{{/GPU}} after"
        out = gen.render(tpl, {"GPU": 1})
        self.assertEqual(out, "before gpu=1 after")

    def test_conditional_block_omitted_when_falsy(self):
        tpl = "before {{#GPU}}gpu={{GPU}}{{/GPU}} after"
        out = gen.render(tpl, {"GPU": 0})
        self.assertEqual(out, "before  after")

    def test_conditional_block_omitted_when_absent(self):
        tpl = "before {{#GPU}}gpu{{/GPU}} after"
        out = gen.render(tpl, {})
        self.assertEqual(out, "before  after")

    def test_multiple_vars(self):
        tpl = "{{A}}-{{B}}-{{C}}"
        out = gen.render(tpl, {"A": "1", "B": "2", "C": "3"})
        self.assertEqual(out, "1-2-3")


class BuildVarsTest(unittest.TestCase):
    def test_defaults_applied(self):
        v = gen.build_vars("research-box", {}, "http://parent:8080/", "http://collector:7777")
        self.assertEqual(v["AGENT_NAME"], "research-box")
        self.assertEqual(v["AGENT_ID"], "agent-research-box")
        self.assertEqual(v["MODEL"], "")  # empty = pi default model
        self.assertEqual(v["CPUS"], 4)
        self.assertEqual(v["GPU"], 0)
        self.assertEqual(v["BRIDGE_PORT"], 8080)
        self.assertTrue(v["AGENT_TOKEN"].startswith("oc-"))

    def test_config_overrides_defaults(self):
        config = {"model": "other-model", "resources": {"cpus": 8, "gpu": 2}}
        v = gen.build_vars("x", config, "", "")
        self.assertEqual(v["MODEL"], "other-model")
        self.assertEqual(v["CPUS"], 8)
        self.assertEqual(v["GPU"], 2)
        # Unspecified resource fields keep defaults.
        self.assertEqual(v["MEMORY"], 16)

    def test_token_reused_when_provided(self):
        v1 = gen.build_vars("x", {}, "", "")
        v2 = gen.build_vars("x", {}, "", "", token=v1["AGENT_TOKEN"])
        self.assertEqual(v1["AGENT_TOKEN"], v2["AGENT_TOKEN"])

    def test_token_generated_when_not_provided(self):
        v1 = gen.build_vars("x", {}, "", "")
        v2 = gen.build_vars("x", {}, "", "")
        self.assertNotEqual(v1["AGENT_TOKEN"], v2["AGENT_TOKEN"])


class TemplateRenderTest(unittest.TestCase):
    """Render the actual agent-template.yaml end-to-end."""

    def test_renders_without_leftover_placeholders(self):
        template = gen.TEMPLATE_PATH.read_text()
        v = gen.build_vars("research-box", {}, "http://parent:8080/", "http://collector:7777")
        out = gen.render(template, v)
        # No {{VAR}} placeholders should remain.
        self.assertNotRegex(out, r"\{\{[A-Z_]+\}\}")
        # No leftover conditional blocks.
        self.assertNotIn("{{#", out)
        self.assertNotIn("{{/", out)
        # Key substitutions present.
        self.assertIn("agent-research-box", out)
        self.assertIn("ORCHESTRA_MODEL", out)
        self.assertIn(v["AGENT_TOKEN"], out)

    def test_gpu_line_present_when_gpu_nonzero(self):
        template = gen.TEMPLATE_PATH.read_text()
        v = gen.build_vars("gpu-box", {"resources": {"gpu": 4}}, "", "")
        out = gen.render(template, v)
        self.assertIn("accelerators: 4", out)

    def test_gpu_line_absent_when_gpu_zero(self):
        template = gen.TEMPLATE_PATH.read_text()
        v = gen.build_vars("cpu-box", {"resources": {"gpu": 0}}, "", "")
        out = gen.render(template, v)
        self.assertNotIn("accelerators:", out)


if __name__ == "__main__":
    unittest.main()
