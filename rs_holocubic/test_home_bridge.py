"""主页轮播的输入、单次预加载、只读缓存及失败边界测试。"""
import base64
import unittest
import zlib
from unittest.mock import Mock, patch
import bridge
import home_bridge
from test_bridge import settings


class HomeBridgeTests(unittest.TestCase):
    """使用模拟设备，避免测试影响用户正在运行的画面。"""
    def test_checkpoint_keeps_original_duration_and_labels(self):
        """动效升级从设备快照恢复，不用未发送草稿缩短任务周期或替换标签。"""
        raw = bridge.task_labels("原任务", "原说明")
        saved = {"labels": list(raw), "labels_crc": "%08x" % (zlib.crc32(raw) & 0xffffffff),
                 "duration": 3600, "initial_bp": 10000, "counter": 984, "paused": True,
                 "timed": True, "looping": True, "accent": 3532799}
        result = home_bridge.prepare({"home_entries": [{"mode": "task", "seconds": 15}],
                                      "settings": settings(), "resume_task": saved}, bridge.task_values, bridge.task_labels)
        self.assertEqual(3600, result["task"]["seconds"])
        self.assertEqual({"counter": 984, "paused": True}, result["task"]["restore"])
        self.assertEqual(raw, result["labels"])
    def test_playlist_validation_before_usb(self):
        """空列表、重复页面、非法时间和缺失图片在连接设备前报错。"""
        for entries in ([], [{"mode": "task", "seconds": 0}],
                        [{"mode": "task", "seconds": 1}]*2,
                        [{"mode": "image", "seconds": 10}]):
            draft = dict(settings(), image_path="")
            with patch.object(bridge.display, "find_holocubic_port") as find:
                with self.assertRaises(ValueError):
                    bridge.execute({"action": "home", "settings": draft, "home_entries": entries})
                find.assert_not_called()

    def test_task_is_prepared_once_and_duration_is_independent(self):
        """每页2秒不改变25分钟任务时长，任务只初始化一次，不在Python中轮播。"""
        entries = [{"mode": "task", "seconds": 2}]
        prepared = home_bridge.prepare({"home_entries": entries, "settings": settings()}, bridge.task_values, bridge.task_labels)
        self.assertEqual(1500, prepared["task"]["seconds"])
        commands = []
        def request(command, **kwargs):
            """模拟任务预加载和轮播启动回执。"""
            commands.append(command)
            if command["cmd"] == "task":
                return {"mode": "task", "bp": 7235}
            if command["cmd"] == "home_apply":
                return {"mode": "home", "home_id": command["session"], "home_entries": entries, "task": {}}
            return {"status": "ok"}
        device = Mock(request=Mock(side_effect=request))
        result = home_bridge.start(device, prepared)
        self.assertEqual(1, sum(c["cmd"] == "task" for c in commands))
        device.upload_jpeg.assert_called_once_with(prepared["labels"], target="task_labels")
        self.assertEqual(list(prepared["labels"]), result["task"]["labels"])

    def test_failed_preparation_is_cleared(self):
        """设备拒绝启动时退出预加载状态，不声称已经开始轮播。"""
        prepared = {"entries": [{"mode": "image", "seconds": 1}], "images": {}, "task": None, "radar": None}
        device = Mock()
        device.request.side_effect = [{"status": "ok"}, RuntimeError("rejected"), {"status": "ok"}]
        with self.assertRaisesRegex(RuntimeError, "rejected"):
            home_bridge.start(device, prepared)
        self.assertEqual({"cmd": "clear"}, device.request.call_args[0][0])

    def test_picture_readback_and_cache(self):
        """重新连接回读实际素材，命中缓存则不查询；不从电脑草稿构造图片。"""
        raw = b"actual device picture"*20
        crc = "%08x" % (zlib.crc32(raw) & 0xffffffff)
        def state():
            """每次提供独立元数据，模拟设备时钟在读取后重新采样。"""
            return {"mode": "home", "home_protocol": 1, "home_id": "12345678",
                    "home_images": {"image": {"crc32": crc, "size": len(raw)}}}
        def request(command):
            """设备按当前画面身份读取有界分块。"""
            if command["cmd"] == "status":
                return state()
            offset = command["offset"]
            return {"offset": offset, "crc32": crc, "data": base64.b64encode(raw[offset:offset+192]).decode()}
        device = Mock(request=Mock(side_effect=request))
        result = home_bridge.mirror(device, state(), {}, Mock())
        self.assertEqual(list(raw), result["home_images"]["image"]["data"])
        device.request.reset_mock()
        result = home_bridge.mirror(device, state(), {"home_crcs": {"image": crc}}, Mock())
        self.assertNotIn("data", result["home_images"]["image"])
        device.request.assert_not_called()


if __name__ == "__main__":
    unittest.main()
