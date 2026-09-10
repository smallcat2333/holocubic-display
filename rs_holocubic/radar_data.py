"""AI 雷达只读采集：任务元数据、24h 代理请求和公共网站数据，绝不上传本地记录。"""
import json
import gzip
import math
import re
import sqlite3
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from html.parser import HTMLParser
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen

SITE = "https://codex-reset-radar.pages.dev"
CHINA = timezone(timedelta(hours=8))


def read_db(path):
    """只读打开正在使用的 SQLite，包含 WAL 中的新数据，不执行迁移或写入。"""
    db = sqlite3.connect(Path(path).as_uri() + "?mode=ro", uri=True, timeout=2)
    db.row_factory = sqlite3.Row
    db.execute("PRAGMA query_only=ON")
    return db


def load_cache(path):
    """派生缓存缺失或损坏时重新采集，不能把损坏缓存当作有效数据。"""
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, ValueError):
        return {}


def save_cache(path, value):
    """原子更新本应用的派生缓存，不修改 Codex 或 CC Switch 数据源。"""
    pending = path.with_suffix(".tmp")
    pending.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")
    pending.replace(path)


def context_line(line):
    """只解析 turn_context 的模型和强度，忽略对话、工具和推理正文。"""
    if b'"type":"turn_context"' not in line[:180] and b'"type": "turn_context"' not in line[:180]:
        return None
    item = json.loads(line)
    if item["type"] != "turn_context":
        return None
    payload = item["payload"]
    model, effort = payload["model"], payload["effort"]
    if not isinstance(model, str) or not model or (effort is not None and not isinstance(effort, str)):
        raise ValueError("模型/强度字段无效")
    return {"model": model, "effort": effort or "unknown"}


def latest_context(path, cached):
    """首次倒序查找最新配置，之后只读新增完整行；文件替换或截断时重建。"""
    path = Path(path)
    stat = path.stat()
    previous = cached.get(str(path), {})
    with path.open("rb") as file:
        if previous.get("inode") == stat.st_ino and previous.get("offset", stat.st_size + 1) <= stat.st_size:
            offset = previous["offset"]
            file.seek(offset)
            block = file.read()
            complete = block.rfind(b"\n") + 1
            latest = previous["context"]
            for line in block[:complete].splitlines():
                result = context_line(line)
                if result is not None:
                    latest = result
            offset += complete
        else:
            position = stat.st_size
            tail = b""
            latest = None
            offset = stat.st_size
            first = True
            while position > 0 and latest is None:
                length = min(position, 1024 * 1024)
                position -= length
                file.seek(position)
                block = file.read(length) + tail
                lines = block.split(b"\n")
                if first:
                    offset = position + block.rfind(b"\n") + 1
                    lines.pop()  # 跳过可能尚未写完的最后一行。
                    first = False
                tail = lines.pop(0) if position else b""
                for line in reversed(lines):
                    latest = context_line(line)
                    if latest is not None:
                        break
    if latest is None:
        raise ValueError("任务尚无模型记录")
    cached[str(path)] = {"inode": stat.st_ino, "offset": offset, "context": latest}
    return latest


def task_counts(state_path, cache_path):
    """最近十个用户主任务各取最新一轮；排除子代理和自动任务，不计工具调用。"""
    cached = load_cache(cache_path)
    warnings, selected, paths = [], [], []
    with read_db(state_path) as db:
        rows = db.execute("""SELECT rollout_path FROM threads
            WHERE source IN ('cli','vscode','exec','appServer')
              AND (thread_source IS NULL OR thread_source='user')
            ORDER BY COALESCE(recency_at_ms,updated_at_ms,updated_at*1000) DESC LIMIT 30""").fetchall()
    for row in rows:
        try:
            selected.append(latest_context(row["rollout_path"], cached))
            paths.append(row["rollout_path"])
        except (OSError, ValueError, KeyError) as error:
            warnings.append("一条任务元数据未读取：" + type(error).__name__)
        if len(selected) == 10:
            break
    save_cache(cache_path, {path: cached[path] for path in paths})
    counts = Counter((row["model"], row["effort"]) for row in selected)
    return {"total": len(selected), "combos": [{"model": model, "effort": effort, "count": count}
            for (model, effort), count in sorted(counts.items(), key=lambda row: (-row[1], row[0]))]}, warnings


def usage_counts(db_path, now):
    """滚动24h内成功 Codex 代理记录，按15分钟分桶；排除会话导入及失败记录。"""
    start = now - 86400
    with read_db(db_path) as db:
        rows = db.execute("""SELECT model, CAST((created_at-?)/900 AS INTEGER) AS bucket,
                COUNT(*) AS n, SUM(input_tokens+output_tokens+cache_read_tokens+cache_creation_tokens=0) AS missing
            FROM proxy_request_logs WHERE app_type='codex' AND data_source='proxy'
              AND status_code>=200 AND status_code<300 AND (error_message IS NULL OR error_message='')
              AND created_at>=? AND created_at<? GROUP BY model,bucket""", (start, start, now)).fetchall()
    models, missing = {}, 0
    for row in rows:
        if not row["model"] or not 0 <= row["bucket"] < 96:
            raise ValueError("代理模型或时间字段无效")
        record = models.setdefault(row["model"], {"model": row["model"], "effort": "unknown", "count": 0, "bins": [0] * 96})
        record["count"] += row["n"]
        record["bins"][row["bucket"]] += row["n"]
        missing += row["missing"]
    values = sorted(models.values(), key=lambda row: (-row["count"], row["model"]))
    return {"total": sum(row["count"] for row in values), "models": values, "missing_usage": missing,
            "start": start, "end": now,
            "labels": [datetime.fromtimestamp(start + n * 3600, CHINA).strftime("%H:%M") for n in (0, 6, 12, 18, 24)]}


def public_data(cache_dir, name, url):
    """只 GET 固定公共地址，遵守缓存并使用条件请求；失败时明确标记旧缓存。"""
    path = cache_dir / (name + ".json")
    cached = load_cache(path)
    now = time.time()
    if cached.get("expires", 0) > now:
        return cached["body"], cached.get("warning", "")
    headers = {"User-Agent": "HoloCubic-Radar/0.1", "Accept-Encoding": "gzip"}
    if cached.get("etag"):
        headers["If-None-Match"] = cached["etag"]
    if cached.get("modified"):
        headers["If-Modified-Since"] = cached["modified"]
    try:
        try:
            with urlopen(Request(url, headers=headers), timeout=8) as response:
                if response.headers.get("Content-Encoding", "").lower() == "gzip":
                    with gzip.GzipFile(fileobj=response) as compressed:
                        raw = compressed.read(8 * 1024 * 1024 + 1)
                else:
                    raw = response.read(8 * 1024 * 1024 + 1)
                if len(raw) > 8 * 1024 * 1024:
                    raise ValueError("网站响应超过8MB")
                body = raw.decode("utf-8-sig")
                metadata = response.headers
        except HTTPError as error:
            if error.code != 304 or "body" not in cached:
                raise
            body, metadata = cached["body"], error.headers
        match = re.search(r"(?:^|,)\s*max-age=(\d+)", metadata.get("Cache-Control", ""))
        ttl = max(5, min(int(match.group(1)) if match else 30, 300))
        edge = metadata.get("X-Codex-Cache", "")
        warning = "网站返回缓存数据（" + edge + "）" if edge.startswith("STALE") or edge == "ERROR" else ""
        if warning:
            ttl = 5  # 站点正在后台更新时，下个界面周期再检查，不发强制刷新请求。
        record = {"body": body, "expires": now + ttl, "etag": metadata.get("ETag", ""),
                  "modified": metadata.get("Last-Modified", ""), "warning": warning}
        save_cache(path, record)
        return body, warning
    except Exception as error:
        if "body" not in cached:
            raise
        return cached["body"], "网站暂不可用，显示旧缓存：" + type(error).__name__


def finite(value):
    """公共数据中的缺失值保持未知，禁止转成零分或零费用。"""
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        return None
    return float(value)


def coding_scores(software):
    """仅使用网站软件编码实测值，不混入空间能力，也不按强度人为调整排名。"""
    if software["schema"] != 3 or software["mode"] != "equal_latest_3":
        raise ValueError("雷达评分接口结构已变化")
    values = []
    for left in software["points"]:
        total, score = finite(left["total"]), finite(left["iq"])
        if total is None or total <= 0 or score is None or score < 0:
            continue
        item = {"model": left["model"], "effort": left["effort"]}
        for source, target in (("iq", "score"), ("average_price_usd", "price"), ("average_minutes", "minutes")):
            item[target] = finite(left.get(source))
        values.append(item)
    if not values:
        raise ValueError("暂无有效的编码评分")
    updated = datetime.fromisoformat(software["source_updated_at"].replace("Z", "+00:00"))
    return {"points": values, "updated": updated.astimezone(CHINA).strftime("%m-%d %H:%M")}


class ResetParser(HTMLParser):
    """只提取网站重置公告区，不执行脚本，也不读取或提交账户相关表单。"""
    def __init__(self):
        """跟踪公告 section 和需要的文本字段。"""
        super().__init__(convert_charrefs=True)
        self.active = False
        self.stack = []
        self.fields = {"headline": [], "lead": [], "detail": []}
        self.deadline = None
        self.source = ""
        self.expired = "等待网站确认重置状态"

    def handle_starttag(self, tag, attrs):
        """识别公告标题、日期和来源链接，忽略其余网页区域。"""
        data = dict(attrs)
        classes = data.get("class", "").split()
        if tag == "section" and "site-announcement-reset" in classes:
            self.active = True
        if not self.active:
            return
        field = next((key for suffix, key in (("headline", "headline"), ("lead", "lead"), ("reset-detail", "detail"))
                      if "site-announcement-" + suffix in classes), None)
        if tag not in ("br", "img", "input", "hr", "meta", "link"):
            self.stack.append((tag, field))
        if "data-window-closes-at" in data:
            self.deadline = int(datetime.fromisoformat(data["data-window-closes-at"].replace("Z", "+00:00")).timestamp())
            self.expired = data.get("data-expired-text", self.expired)
        if tag == "a" and "site-announcement-reset-source" in classes:
            self.source = data.get("href", "")

    def handle_data(self, data):
        """保留公告必要文本，不混入页面其他内容。"""
        if self.active:
            for _, key in self.stack:
                if key is not None:
                    self.fields[key].append(data)
                    break

    def handle_endtag(self, tag):
        """退出当前标签，公告结束后停止采集。"""
        if not self.active:
            return
        if self.stack and self.stack[-1][0] == tag:
            self.stack.pop()
        if tag == "section":
            self.active = False

    def result(self):
        """站点未给出预告时返回未知，不臆造下一次重置时间。"""
        values = {key: " ".join("".join(parts).split()) for key, parts in self.fields.items()}
        values.update(deadline=self.deadline, source=self.source, expired=self.expired)
        if not values["headline"]:
            raise ValueError("网站当前没有可识别的重置公告")
        return values


def collect(cache_dir, profile=None):
    """独立采集各区域；一个来源失败不抹掉其他来源，更不接触 USB。"""
    profile = Path(profile) if profile else Path.home()
    cache_dir = Path(cache_dir)
    cache_dir.mkdir(parents=True, exist_ok=True)
    now = int(time.time())
    result = {"collected_at": now, "collected_label": datetime.fromtimestamp(now, CHINA).strftime("%H:%M:%S"),
              "tasks": None, "usage": None, "scores": None, "reset": None, "errors": []}
    try:
        result["tasks"], warnings = task_counts(profile / ".codex/state_5.sqlite", cache_dir / "contexts.json")
        result["errors"].extend(warnings)
    except Exception as error:
        result["errors"].append("Codex 任务读取失败：" + str(error))
    try:
        result["usage"] = usage_counts(profile / ".cc-switch/cc-switch.db", now)
    except Exception as error:
        result["errors"].append("CC Switch 读取失败：" + str(error))
    with ThreadPoolExecutor(max_workers=2) as pool:
        jobs = {name: pool.submit(public_data, cache_dir, name, SITE + endpoint) for name, endpoint in (
            ("software", "/api/intelligence-efficiency-metrics"), ("reset_page", "/"))}
        bodies = {}
        for name, future in jobs.items():
            try:
                bodies[name], warning = future.result()
                if warning:
                    result["errors"].append(name + "：" + warning)
            except Exception as error:
                result["errors"].append(name + " 读取失败：" + str(error))
    if "software" in bodies:
        try:
            result["scores"] = coding_scores(json.loads(bodies["software"]))
        except Exception as error:
            result["errors"].append("评分解析失败：" + str(error))
    if "reset_page" in bodies:
        try:
            parser = ResetParser()
            parser.feed(bodies["reset_page"])
            result["reset"] = parser.result()
        except Exception as error:
            result["errors"].append("重置公告解析失败：" + str(error))
    return result
