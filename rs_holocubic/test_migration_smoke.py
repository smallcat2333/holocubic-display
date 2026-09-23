"""Discoverable smoke test for the legacy CI python unittest discover step."""
import unittest
from pathlib import Path


class RustCutoverTests(unittest.TestCase):
    def test_no_runtime_bridge_py(self):
        here = Path(__file__).resolve().parent
        self.assertFalse((here / "bridge.py").exists())
        self.assertTrue((here / "src" / "actions.rs").exists())


if __name__ == "__main__":
    unittest.main()
