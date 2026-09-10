"""雷达采集的统计口径、只读边界、缓存和网站数据解析测试。"""
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch
import radar_data as radar


class RadarTests(unittest.TestCase):
    """使用临时数据库和模拟HTTP，不触碰真实会话、代理或网络。"""
    def setUp(self):
        """每例独立的临时数据目录。"""
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)

    def tearDown(self):
        """清理本例临时文件，不清理任何用户目录。"""
        self.directory.cleanup()

    def context(self, model, effort):
        """构造没有正文的任务上下文记录。"""
        return json.dumps({"type": "turn_context", "payload": {"model": model, "effort": effort}}).encode() + b"\n"

    def test_latest_context_incremental_and_partial_record(self):
        """只计最新配置，工具/正文不参与；未写完的行必须留到下次读取。"""
        path = self.root / "rollout.jsonl"
        first = self.context("gpt-6-astra", "max")
        second = self.context("gpt-6-astra", "medium")
        path.write_bytes(first + second[:20])
        cache = {}
        self.assertEqual("max", radar.latest_context(path, cache)["effort"])
        with path.open("ab") as file:
            file.write(second[20:])
        self.assertEqual("medium", radar.latest_context(path, cache)["effort"])
        self.assertEqual(path.stat().st_size, cache[str(path)]["offset"])
        self.assertEqual("medium", radar.latest_context(path, cache)["effort"])
        path.write_bytes(self.context("gpt-5.5", "high"))
        self.assertEqual("gpt-5.5", radar.latest_context(path, cache)["model"])

    def test_ten_tasks_not_ten_turns(self):
        """同一任务多个轮次只计最新一个，子代理与自动任务不混入。"""
        db_path = self.root / "state.sqlite"
        with sqlite3.connect(db_path) as db:
            db.execute("CREATE TABLE threads(rollout_path TEXT,source TEXT,thread_source TEXT,recency_at_ms INTEGER,updated_at_ms INTEGER,updated_at INTEGER)")
            for index in range(12):
                path = self.root / (str(index) + ".jsonl")
                path.write_bytes(self.context("gpt-5.5", "high") + self.context("gpt-6-astra", "medium"))
                db.execute("INSERT INTO threads VALUES(?,?,?,?,?,?)", (str(path), "vscode", "user", index, index, 0))
            db.execute("INSERT INTO threads VALUES(?,?,?,?,?,?)", ("not-read", "vscode", "automation", 100, 100, 0))
            db.execute("INSERT INTO threads VALUES(?,?,?,?,?,?)", ("not-read", "subAgent", "subagent", 100, 100, 0))
        result, warnings = radar.task_counts(db_path, self.root / "contexts.json")
        self.assertEqual(10, result["total"])
        self.assertEqual([{"model": "gpt-6-astra", "effort": "medium", "count": 10}], result["combos"])
        self.assertEqual([], warnings)
        with radar.read_db(db_path) as db:
            with self.assertRaises(sqlite3.OperationalError):
                db.execute("DELETE FROM threads")

    def test_rolling_24h_counts_only_successful_proxy_requests(self):
        """24h边界、应用类型、来源和成功状态都必须满足，按实际记录模型分组。"""
        path = self.root / "usage.sqlite"
        now = 200000
        with sqlite3.connect(path) as db:
            db.execute("""CREATE TABLE proxy_request_logs(model TEXT,created_at INTEGER,app_type TEXT,data_source TEXT,
                status_code INTEGER,error_message TEXT,input_tokens INTEGER,output_tokens INTEGER,cache_read_tokens INTEGER,cache_creation_tokens INTEGER)""")
            entries = [("gpt-6-astra", now - 1, "codex", "proxy", 200, None),
                       ("gpt-5.6-luna", now - 86400, "codex", "proxy", 200, None),
                       ("old", now - 86401, "codex", "proxy", 200, None),
                       ("import", now - 1, "codex", "codex_session", 200, None),
                       ("claude", now - 1, "claude", "proxy", 200, None),
                       ("bad", now - 1, "codex", "proxy", 500, "failed"),
                       ("stream-error", now - 1, "codex", "proxy", 200, "failed"),
                       ("future", now + 1, "codex", "proxy", 200, None)]
            db.executemany("INSERT INTO proxy_request_logs VALUES(?,?,?,?,?,?,0,0,0,0)", entries)
        result = radar.usage_counts(path, now)
        self.assertEqual(2, result["total"])
        self.assertEqual(2, result["missing_usage"])
        self.assertEqual(2, sum(sum(model["bins"]) for model in result["models"]))
        self.assertTrue(all(len(model["bins"]) == 96 for model in result["models"]))
        self.assertEqual(5, len(result["labels"]))
        self.assertTrue(all(model["effort"] == "unknown" for model in result["models"]))

    def test_scores_use_coding_only_and_unknown_is_not_zero(self):
        """只取编码原值，不需要视觉数据，缺失费用保持未知。"""
        common = {"model": "gpt-6-astra", "effort": "medium"}
        software = {"schema": 3, "mode": "equal_latest_3", "source_updated_at": "2026-09-08T02:00:00Z",
                    "points": [dict(common, total=3, iq=100, average_price_usd=2, average_minutes=10)]}
        result = radar.coding_scores(software)
        self.assertEqual(100, result["points"][0]["score"])
        self.assertEqual(2, result["points"][0]["price"])
        self.assertEqual(10, result["points"][0]["minutes"])
        self.assertEqual("09-08 10:00", result["updated"])
        software["points"][0]["average_price_usd"] = None
        self.assertIsNone(radar.coding_scores(software)["points"][0]["price"])

    def test_public_cache_avoids_repeated_downloads_and_marks_stale(self):
        """五秒刷新不能强刷公共站点，网络故障保留数据但标明旧缓存。"""
        response = Mock()
        response.headers = {"Cache-Control": "public, max-age=30", "X-Codex-Cache": "HIT"}
        response.read.return_value = b'{"ok":true}'
        manager = Mock()
        manager.__enter__ = Mock(return_value=response)
        manager.__exit__ = Mock(return_value=False)
        with patch.object(radar, "urlopen", return_value=manager) as fetch, patch.object(radar.time, "time", return_value=100):
            radar.public_data(self.root, "sample", radar.SITE)
            radar.public_data(self.root, "sample", radar.SITE)
            self.assertEqual(1, fetch.call_count)
        with patch.object(radar, "urlopen", side_effect=OSError("offline")), patch.object(radar.time, "time", return_value=140):
            body, warning = radar.public_data(self.root, "sample", radar.SITE)
        self.assertEqual('{"ok":true}', body)
        self.assertIn("旧缓存", warning)

    def test_reset_extracts_announcement_only(self):
        """不把网站其他文字或未确认的截止时间当成已完成重置。"""
        parser = radar.ResetParser()
        parser.feed('''<p>ignored</p><section class="site-announcement site-announcement-reset">
          <strong class="site-announcement-headline">用量重置</strong>
          <span class="site-announcement-lead">预计北京时间</span>
          <p class="site-announcement-reset-detail">实际完成仍待确认</p>
          <div data-window-closes-at="2026-09-08T10:00:00+08:00" data-expired-text="等待官方确认"></div>
          <a class="site-announcement-reset-source" href="https://x.com/example">查看</a></section>''')
        result = parser.result()
        self.assertEqual("用量重置", result["headline"])
        self.assertEqual("等待官方确认", result["expired"])
        self.assertIsNotNone(result["deadline"])
        self.assertNotIn("ignored", json.dumps(result))

    def test_radar_dispatch_never_opens_usb(self):
        """雷达复用进程桥接，但不进入串口识别和设备控制路径。"""
        import bridge
        with patch.object(radar, "collect", return_value={"test": True}), patch.object(bridge.display, "find_holocubic_port") as find:
            self.assertEqual({"test": True}, bridge.execute({"action": "radar", "cache_dir": str(self.root)}))
            find.assert_not_called()


if __name__ == "__main__":
    unittest.main()
