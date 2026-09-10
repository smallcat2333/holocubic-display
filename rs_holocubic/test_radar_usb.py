"""USB 雷达数据口径与事务测试；串口使用模拟回执，不触碰硬件。"""
import base64
import json
import unittest
import zlib
from unittest.mock import Mock, patch
import radar_usb
import bridge


def snapshot():
    """包含未知分数、五种代理模型和已过截止时间的最小统计样本。"""
    rows = [{"model": model, "count": 96 * (n+1), "bins": [n+1]*96, "effort": "unknown"}
            for n, model in enumerate(["gpt-6-astra", "gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.5", "extra"])]
    return {"collected_at": 100, "tasks": {"combos": [{"model": "gpt-6-astra", "effort": "max", "count": 10}]},
            "scores": None, "usage": {"total": sum(row["count"] for row in rows), "models": rows},
            "reset": {"deadline": 90}, "errors": []}


class RadarUsbTests(unittest.TestCase):
    """验证小包协议不丢计数、不假造未知数据，失败不能发布为成功。"""
    def test_group_other_preserves_counts_and_unknowns(self):
        """Other 合并保留总量，每小时桶由四个15分钟桶相加。"""
        value = radar_usb.compact(snapshot(), 100)
        self.assertEqual(4, len(value["series"]))
        self.assertEqual(value["total"], sum(row[2] for row in value["series"]))
        self.assertTrue(all(sum(row[3]) == row[2] for row in value["series"]))
        self.assertEqual("Other", value["series"][-1][0])
        self.assertEqual([-1, -1, -1], value["cards"][0][3:])
        self.assertEqual(0, value["reset"])

    def test_unavailable_is_distinct_from_zero(self):
        """采集失败不能被设备显示为零请求或没有任务。"""
        data = snapshot()
        data.update(usage=None, tasks=None, reset=None)
        value = radar_usb.compact(data, 100)
        self.assertEqual(-1, value["total"])
        self.assertEqual(-1, value["reset"])
        self.assertFalse(value["tasks"])

    def test_unknown_effort_is_not_displayed(self):
        """模型存在但强度未知时仅保留模型名，不生成未知强度占位文字。"""
        data = snapshot()
        data["tasks"]["combos"][0]["effort"] = "unknown"
        self.assertEqual("Astra", radar_usb.compact(data, 100)["cards"][0][0])

    def test_readback_and_crc_cache(self):
        """首次回读实际设备数据，重复状态使用 CRC 缓存，不依赖当前采集结果。"""
        payload = radar_usb.compact(snapshot(), 100)
        raw = json.dumps(payload).encode()
        crc = "%08x" % (zlib.crc32(raw) & 0xFFFFFFFF)
        state = {"mode": "radar", "radar_mirror_protocol": 1, "radar_size": len(raw),
                 "crc32": crc, "age": 25, "scene_seconds": 12}
        def request(command):
            """模拟设备只读数据分块及最终时钟回执。"""
            if command["cmd"] == "status":
                return dict(state)
            offset = command["offset"]
            return {"crc32": crc, "offset": offset, "data": base64.b64encode(raw[offset:offset+192]).decode()}
        device = Mock(request=Mock(side_effect=request))
        result = radar_usb.mirror(device, dict(state), None)
        self.assertEqual(payload, result["radar_data"])
        self.assertEqual(12, result["scene_seconds"])
        device.request.reset_mock()
        cached = radar_usb.mirror(device, dict(state), crc)
        device.request.assert_not_called()
        self.assertNotIn("radar_data", cached)
        device.request.side_effect = lambda command: {"crc32": crc, "offset": 1, "data": "e30="}
        with self.assertRaisesRegex(ValueError, "分块不匹配"):
            radar_usb.mirror(device, dict(state), None)

    def test_stale_and_inconsistent_data_rejected(self):
        """过期快照及破坏计数守恒的数据在打开事务前报错。"""
        with self.assertRaises(ValueError):
            radar_usb.compact(snapshot(), 131)
        data = snapshot()
        data["usage"]["total"] += 1
        with self.assertRaises(ValueError):
            radar_usb.compact(data, 100)

    def test_transaction_bytes_and_crc(self):
        """96字节分块重组还原同一份 JSON，结束回执必须匹配 CRC。"""
        commands = []
        def request(command, **kwargs):
            """模拟固件的分块和提交回执。"""
            commands.append(command)
            if command["cmd"] == "radar_chunk":
                return {"next_seq": command["seq"] + 1}
            return {"mode": "radar", "crc32": commands[0]["crc32"]}
        with patch.object(radar_usb.time, "time", return_value=100):
            radar_usb.send(Mock(request=request), snapshot())
        chunks = [base64.b64decode(c["data"]) for c in commands[1:-1]]
        self.assertTrue(all(0 < len(chunk) <= 96 for chunk in chunks))
        raw = b"".join(chunks)
        self.assertEqual(len(raw), commands[0]["size"])
        self.assertEqual("%08x" % (zlib.crc32(raw) & 0xFFFFFFFF), commands[0]["crc32"])
        self.assertEqual(radar_usb.compact(snapshot(), 100), json.loads(raw))

    def test_old_firmware_rejected_before_sending(self):
        """旧设备缺少能力标记时拒绝发送，不破坏沙漏画面。"""
        device = Mock()
        device.wait_until_ready.return_value = {"mode": "task"}
        manager = Mock(__enter__=Mock(return_value=device), __exit__=Mock(return_value=False))
        with patch.object(bridge.display, "find_holocubic_port", return_value="TEST"), \
                patch.object(bridge.display, "HoloCubicUSB", return_value=manager):
            with self.assertRaisesRegex(ValueError, "设备程序需要更新"):
                bridge.execute({"action": "radar_usb", "snapshot": snapshot()})
        device.request.assert_not_called()


if __name__ == "__main__":
    unittest.main()
