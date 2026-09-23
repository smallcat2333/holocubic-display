"""Legacy entry retained so Windows CI's python unittest step stays green after the Rust cutover."""
import unittest


class MigrationSmokeTests(unittest.TestCase):
    def test_python_runtime_removed(self):
        from pathlib import Path

        root = Path(__file__).resolve().parent
        self.assertFalse((root / "holo_usb_display.py").exists())
        self.assertTrue((root / "rs_holocubic" / "src" / "bridge.rs").exists())
        bridge = (root / "rs_holocubic" / "src" / "bridge.rs").read_text(encoding="utf-8")
        self.assertNotIn('Command::new("python")', bridge)


if __name__ == "__main__":
    unittest.main()
