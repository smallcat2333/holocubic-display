"""Validate the GUI bridge inputs, static labels, and USB task contract offline."""
import io
import base64
import unittest
import zlib
from unittest.mock import patch, MagicMock
from PIL import Image, ImageChops
import bridge


def settings():
    """Return one bounded Chinese task matching the GUI JSON schema."""
    return {"task_name": "良率数据核对", "footer": "先核对口径，再核对数据", "hours": 0,
            "minutes": 25, "seconds": 0, "remaining": 72.35, "timed": True, "looping": False, "accent": [53, 231, 255]}


def device_state(raw):
    """Describe one device-owned label image independently of GUI draft contents."""
    return {"mode": "task", "bp": 7235, "looping": False, "mirror_protocol": 1, "labels_size": len(raw),
            "labels_crc": "%08x" % (zlib.crc32(raw) & 0xFFFFFFFF)}


class BridgeTests(unittest.TestCase):
    """Cover the field conversion and reuse of static label transfer."""

    def test_basis_points(self):
        """Round to an exact hundredth and validate seconds without floating timestamps."""
        values = bridge.task_values(settings())
        self.assertEqual(7235, values[3])
        self.assertEqual(1500, values[2])
        for value in (-1, 100.01, float("nan")):
            draft = settings()
            draft["remaining"] = value
            with self.assertRaises(ValueError):
                bridge.task_values(draft)

    def test_chinese_static_labels(self):
        """Keep 320x240 labels sparse and JPEG encoded for the verified decoder."""
        raw = bridge.task_labels("良率数据核对", "先核对口径，再核对数据")
        with Image.open(io.BytesIO(raw)) as frame:
            self.assertEqual((320, 240), frame.size)
            self.assertEqual((0, 0, 0), frame.getpixel((0, 0)))
            self.assertEqual((0, 0, 0), frame.getpixel((160, 120)))
        self.assertLess(len(raw), 12000)

    def test_task_contract(self):
        """Upload labels only once and send integer task parameters over the same USB client."""
        device = MagicMock()
        raw = bridge.task_labels("良率数据核对", "先核对口径，再核对数据")
        device.wait_until_ready.return_value = {"task_protocol": 1, "mirror_protocol": 1, "loop_protocol": 1}
        device.request.return_value = device_state(raw)
        with patch.object(bridge.display, "find_holocubic_port", return_value="TEST"), patch.object(bridge.display, "HoloCubicUSB") as client:
            client.return_value.__enter__.return_value = device
            result = bridge.execute({"action": "task", "settings": settings()})
        self.assertEqual("TEST", result["port"])
        self.assertEqual("task_labels", device.upload_jpeg.call_args[1]["target"])
        self.assertEqual(7235, device.request.call_args[0][0]["bp"])
        self.assertNotIn("task_name", device.request.call_args[0][0])
        self.assertEqual(list(raw), result["labels"])
        self.assertFalse(device.request.call_args[0][0]["looping"])

    def test_loop_flag_is_sent_and_unsupported_device_refused(self):
        """Looping is an explicit boolean and cannot be silently ignored by old devices."""
        draft = settings()
        draft["looping"] = True
        raw = bridge.task_labels(draft["task_name"], draft["footer"])
        device = MagicMock()
        device.wait_until_ready.return_value = {"task_protocol": 1, "mirror_protocol": 1, "loop_protocol": 1}
        device.request.return_value = dict(device_state(raw), looping=True)
        with patch.object(bridge.display, "find_holocubic_port", return_value="TEST"), patch.object(bridge.display, "HoloCubicUSB") as client:
            client.return_value.__enter__.return_value = device
            bridge.execute({"action": "task", "settings": draft})
            self.assertTrue(device.request.call_args[0][0]["looping"])
            device.wait_until_ready.return_value.pop("loop_protocol")
            device.upload_jpeg.reset_mock()
            with self.assertRaisesRegex(ValueError, "设备程序需要更新"):
                bridge.execute({"action": "task", "settings": draft})
            device.upload_jpeg.assert_not_called()
        draft["looping"] = "true"
        with self.assertRaisesRegex(ValueError, "looping"):
            bridge.task_values(draft)

    def test_bottom_title_and_top_note_bounds(self):
        """Keep the longest Chinese title below the separator and notes inside the header."""
        background = Image.new("RGB", (320, 240), (0, 0, 0))
        with patch.object(bridge.display, "encode_jpeg", side_effect=lambda frame: frame):
            title = bridge.task_labels("良率数据核对" * 4, "")
            note = bridge.task_labels("", "核对数据口径" * 6)
        title_box = ImageChops.difference(title, background).getbbox()
        self.assertGreaterEqual(title_box[0], 16)
        self.assertLessEqual(title_box[2], 304)
        self.assertGreaterEqual(title_box[1], 218)
        self.assertLess(title_box[3], 240)
        self.assertAlmostEqual((title_box[0] + title_box[2]) / 2, 160, delta=2)
        note_box = ImageChops.difference(note, background).getbbox()
        self.assertGreaterEqual(note_box[0], 16)
        self.assertLessEqual(note_box[2], 231)
        self.assertGreaterEqual(note_box[1], 8)
        self.assertLessEqual(note_box[3], 40)

    def test_unsupported_firmware_does_not_upload(self):
        """Old devices get an actionable update error before any image is sent."""
        device = MagicMock()
        device.wait_until_ready.return_value = {}
        with patch.object(bridge.display, "find_holocubic_port", return_value="TEST"), patch.object(bridge.display, "HoloCubicUSB") as client:
            client.return_value.__enter__.return_value = device
            with self.assertRaisesRegex(ValueError, "设备程序需要更新"):
                bridge.execute({"action": "task", "settings": settings()})
        device.upload_jpeg.assert_not_called()

    def test_read_device_labels_and_cache_crc(self):
        """Reconnect reads actual bytes once and cached status polls transfer no image."""
        raw = b"device-jpeg" * 45
        state = device_state(raw)
        device = MagicMock()
        replies = [{"crc32": state["labels_crc"], "offset": offset,
                    "data": base64.b64encode(raw[offset:offset + 192]).decode("ascii")}
                   for offset in range(0, len(raw), 192)]
        device.request.side_effect = replies + [dict(state)]
        result = bridge.mirror_labels(device, dict(state), None)
        self.assertEqual(list(raw), result["labels"])
        self.assertEqual({"cmd": "status"}, device.request.call_args[0][0])
        device.request.reset_mock()
        result = bridge.mirror_labels(device, dict(state), state["labels_crc"])
        self.assertNotIn("labels", result)
        device.request.assert_not_called()

    def test_invalid_readback_does_not_become_preview(self):
        """Corrupt data and swapped scenes fail instead of fabricating a draft preview."""
        raw = b"device-jpeg"
        state = device_state(raw)
        device = MagicMock()
        device.request.return_value = {"crc32": "badcrc00", "offset": 0, "data": base64.b64encode(raw).decode("ascii")}
        with self.assertRaisesRegex(ValueError, "分块不匹配"):
            bridge.mirror_labels(device, dict(state), None)
        with self.assertRaisesRegex(ValueError, "CRC32"):
            bridge.mirror_labels(device, dict(state), None, b"wrong")


if __name__ == "__main__":
    unittest.main()
