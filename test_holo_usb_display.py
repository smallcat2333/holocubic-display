"""Tests for the offline renderer and USB framing protocol."""

import io
import base64
import json
import unittest
import zlib
from contextlib import redirect_stdout

import holo_usb_display as display


class FakeSerial:
    """Return matching device acknowledgements for host protocol tests."""

    def __init__(self) -> None:
        """Initialize an open in-memory serial endpoint."""
        self.is_open = True
        self.commands = []
        self.packets = []
        self.responses = []
        self.expected_data = b""
        self.target = "frame"

    def write(self, raw: bytes) -> int:
        """Decode one host command and queue its simulated response."""
        payload = json.loads(raw[len(display.PROTOCOL_PREFIX) :])
        if payload["cmd"] == "image_begin":
            self.target = payload["target"]
        self.commands.append(payload)
        self.packets.append(raw)
        response = {
            "id": payload["id"],
            "status": "displayed" if payload["cmd"] == "image_end" else "ok",
            "app": "usb_display",
        }
        if payload["cmd"] == "image_end":
            if self.target != "frame":
                response["status"] = "stored"
            response.update(
                {
                    "bytes": len(self.expected_data),
                    "crc32": f"{zlib.crc32(self.expected_data) & 0xFFFFFFFF:08x}",
                }
            )
        elif payload["cmd"] == "quota":
            response.update(
                {"mode": "quota", "remaining": payload["remaining"],
                 "reset_seconds": payload["reset_seconds"], "demo": payload["demo"]}
            )
        self.responses.append(
            display.PROTOCOL_PREFIX + json.dumps(response).encode("ascii") + b"\n"
        )
        return len(raw)

    def flush(self) -> None:
        """Match the pyserial flush interface without buffering writes."""

    def read_until(self, terminator: bytes) -> bytes:
        """Return the next complete simulated response line."""
        return self.responses.pop(0) if self.responses else b""


class RendererTests(unittest.TestCase):
    """Verify computer-side frame generation."""

    def test_chinese_text_renders_as_baseline_jpeg(self) -> None:
        """Render Chinese through a Windows font into an exact-size JPEG."""
        frame = display.render_text_frame("HoloCubic", "USB直连成功", "2026-09-05")
        payload = display.encode_jpeg(frame)
        self.assertEqual((320, 240), frame.size)
        self.assertEqual("RGB", frame.mode)
        self.assertEqual(b"\xff\xd8", payload[:2])
        self.assertEqual(b"\xff\xd9", payload[-2:])


class ProtocolTests(unittest.TestCase):
    """Verify ordered chunks and end-to-end metadata."""

    def test_home_assets_reuse_ordered_upload(self):
        """Home images use the existing validated upload, stored separately from the visible frame."""
        for target in ("home_text", "home_image"):
            endpoint = FakeSerial()
            endpoint.expected_data = b"home-image"
            client = display.HoloCubicUSB("TEST")
            client.serial = endpoint
            with redirect_stdout(io.StringIO()):
                result = client.upload_jpeg(endpoint.expected_data, target=target)
            self.assertEqual("stored", result["status"])
            self.assertEqual(target, endpoint.commands[0]["target"])

    def test_multipart_reply_survives_partial_serial_reads(self):
        """Large replies are pulled in bounded pieces, including half-lines separated by read timeouts."""
        class MultipartSerial(FakeSerial):
            """Expose one large reply through small independently acknowledged reads."""
            def write(self, raw):
                """Split every reply into two serial reads to reproduce the observed timeout boundary."""
                command = json.loads(raw[len(display.PROTOCOL_PREFIX):])
                self.commands.append(command)
                if command["cmd"] == "response_read":
                    offset = command["offset"]
                    reply = {"id": command["id"], "status": "part", "offset": offset,
                             "data": base64.b64encode(self.full[offset:offset+96]).decode()}
                else:
                    self.full = json.dumps({"id": command["id"], "status": "ok", "mode": "home", "padding": "x"*1800}).encode()
                    reply = {"id": command["id"], "status": "multipart", "size": len(self.full),
                             "crc32": "%08x" % (zlib.crc32(self.full) & 0xffffffff)}
                packet = display.PROTOCOL_PREFIX + json.dumps(reply).encode() + b"\n"
                self.responses.extend((packet[:17], b"", packet[17:]))
                return len(raw)
        client = display.HoloCubicUSB("TEST")
        endpoint = MultipartSerial()
        client.serial = endpoint
        reply = client.request({"cmd": "status"})
        self.assertEqual("home", reply["mode"])
        self.assertEqual(1800, len(reply["padding"]))
        self.assertGreater(len(endpoint.commands), 10)
        self.assertIn("_transport_ms", reply)

    def test_upload_uses_expected_chunks_and_crc(self) -> None:
        """Split bytes into 96-byte packets and retain CRC32 metadata."""
        payload = bytes(range(200))
        endpoint = FakeSerial()
        endpoint.expected_data = payload
        client = display.HoloCubicUSB("TEST")
        client.serial = endpoint

        with redirect_stdout(io.StringIO()):
            response = client.upload_jpeg(payload)

        self.assertEqual(
            ["image_begin", "image_chunk", "image_chunk", "image_chunk", "image_end"],
            [command["cmd"] for command in endpoint.commands],
        )
        self.assertEqual([0, 1, 2], [command["seq"] for command in endpoint.commands[1:4]])
        self.assertEqual(
            f"{zlib.crc32(payload) & 0xFFFFFFFF:08x}", endpoint.commands[0]["crc32"]
        )
        self.assertEqual(200, response["bytes"])
        self.assertEqual(zlib.crc32(payload) & 0xFFFFFFFF, response["crc32"])
        self.assertIsInstance(response["crc32"], int)
        self.assertEqual(8, len(f"{response['crc32']:08X}"))

    def test_upload_rejects_incorrect_completion_crc(self) -> None:
        """Reject a completed upload whose reported checksum belongs to different data."""
        endpoint = FakeSerial()
        endpoint.expected_data = b"wrong"
        client = display.HoloCubicUSB("TEST")
        client.serial = endpoint
        with redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(display.HoloCubicError, "CRC32"):
                client.upload_jpeg(b"right")

    def test_label_upload_is_stored_not_displayed(self) -> None:
        """Keep static label storage distinct from a displayed full-screen frame."""
        endpoint = FakeSerial()
        endpoint.expected_data = b"label-jpeg"
        client = display.HoloCubicUSB("TEST")
        client.serial = endpoint
        with redirect_stdout(io.StringIO()):
            result = client.upload_jpeg(endpoint.expected_data, target="task_labels")
        self.assertEqual("stored", result["status"])
        self.assertEqual("task_labels", endpoint.commands[0]["target"])

    def test_quota_uses_one_small_state_packet(self) -> None:
        """Start animation with one sub-160-byte packet instead of uploading images."""
        endpoint = FakeSerial()
        client = display.HoloCubicUSB("TEST")
        client.serial = endpoint
        response = client.set_quota(72, 3600, demo=True)
        self.assertEqual(["quota"], [item["cmd"] for item in endpoint.commands])
        self.assertLess(len(endpoint.packets[0]), 160)
        self.assertEqual(72, response["remaining"])
        self.assertTrue(response["demo"])

    def test_quota_rejects_invalid_values_before_sending(self) -> None:
        """Reject invalid external values without opening or writing the USB port."""
        client = display.HoloCubicUSB("TEST")
        for remaining, reset_seconds in [(-1, 60), (101, 60), (1.5, 60), (50, -1), (50, 604801)]:
            with self.subTest(remaining=remaining, reset_seconds=reset_seconds):
                with self.assertRaises(ValueError):
                    client.set_quota(remaining, reset_seconds)


if __name__ == "__main__":
    unittest.main()
