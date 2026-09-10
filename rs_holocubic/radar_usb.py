"""将已有雷达统计压缩成 USB 原生场景数据；不传图片，不写 SD，不重新采集。"""
import base64
import json
import math
import time
import zlib

MODELS = {"gpt-6-astra": ("Astra", 0xFF7E1D), "gpt-5.6-sol": ("Sol", 0xF2C20E),
          "gpt-5.6-terra": ("Terra", 0x589FFF), "gpt-5.6-luna": ("Luna", 0xB6C4D7),
          "gpt-5.5": ("5.5", 0x24D7EA)}


def bounded(value, maximum=10000000):
    """外部计数限制在设备精确表示的整数范围，异常不能截断成正常数据。"""
    if type(value) is not int or not 0 <= value <= maximum:
        raise ValueError("雷达计数超出设备范围")
    return value


def ascii_label(value, length):
    """设备内置字体仅支持 ASCII；未知模型保留可显示部分并限制宽度。"""
    return str(value).encode("ascii", "replace").decode()[:length]


def compact(snapshot, now=None):
    """十张编码卡轮播，右侧前三模型加 Other，96桶合并24小时桶且保持总量。"""
    now = int(time.time()) if now is None else now
    age = now - snapshot["collected_at"]
    if not 0 <= age <= 30:
        raise ValueError("雷达数据已过期，请等待新采集结果")
    cards = []
    scores = {(p["model"], p["effort"]): p for p in (snapshot["scores"] or {"points": []})["points"]}
    for combo in (snapshot["tasks"] or {"combos": []})["combos"]:
        name, color = MODELS.get(combo["model"], (ascii_label(combo["model"], 10), 0xB186EF))
        score = scores.get((combo["model"], combo["effort"]), {})
        values = []
        for key, scale in (("score", 100), ("price", 100), ("minutes", 10)):
            value = score.get(key)
            if value is None:
                values.append(-1)
            elif isinstance(value, bool) or not math.isfinite(value) or value < 0:
                raise ValueError("编码评分字段无效")
            else:
                values.append(bounded(round(value * scale)))
        caption = name if combo["effort"] == "unknown" else name + " " + combo["effort"]
        cards.append([ascii_label(caption, 20), bounded(combo["count"], 10), color] + values)
    if len(cards) > 10:
        raise ValueError("任务卡片超过十种组合")
    usage = snapshot["usage"]
    series = []
    total = -1 if usage is None else bounded(usage["total"])
    if usage is not None:
        rows = usage["models"]
        for index, row in enumerate(rows):
            if len(row["bins"]) != 96 or sum(row["bins"]) != row["count"]:
                raise ValueError("请求趋势与计数不一致")
            bins = [bounded(sum(row["bins"][n:n + 4])) for n in range(0, 96, 4)]
            if index < 3:
                name, color = MODELS.get(row["model"], (ascii_label(row["model"], 8), 0xB186EF))
                series.append([name, color, bounded(row["count"]), bins])
            elif index == 3:
                series.append(["Other", 0xB186EF, bounded(row["count"]), bins])
            else:
                series[3][2] = bounded(series[3][2] + row["count"])
                series[3][3] = [bounded(a + b) for a, b in zip(series[3][3], bins)]
        if sum(row[2] for row in series) != total:
            raise ValueError("模型计数与总请求数不一致")
    reset = snapshot["reset"]
    deadline = None if reset is None else reset["deadline"]
    reset_seconds = -1 if deadline is None else max(0, deadline - now)
    if reset_seconds > 10000000:
        raise ValueError("重置预告时间超出设备范围")
    return {"cards": cards, "series": series, "total": total, "reset": reset_seconds,
            "age": age, "warn": bool(snapshot["errors"]), "tasks": snapshot["tasks"] is not None}


def send(device, snapshot, standalone=False):
    """96字节有序 RAM 分块及 CRC 完整校验后一次提交，失败不发布半帧。"""
    payload = compact(snapshot)
    data = json.dumps(payload, separators=(",", ":"), ensure_ascii=True).encode("ascii")
    if len(data) > 8192:
        raise ValueError("雷达数据超过设备 8KB 上限")
    crc = "%08x" % (zlib.crc32(data) & 0xFFFFFFFF)
    device.request({"cmd": "radar_begin", "size": len(data), "crc32": crc})
    for sequence, offset in enumerate(range(0, len(data), 96)):
        reply = device.request({"cmd": "radar_chunk", "seq": sequence,
                                "data": base64.b64encode(data[offset:offset + 96]).decode("ascii")})
        if reply["next_seq"] != sequence + 1:
            raise ValueError("雷达数据分块回执不匹配")
    result = device.request({"cmd": "radar_end", "standalone": standalone}, timeout=8)
    scene = result["radar"] if result["mode"] == "home" else result
    if scene["mode"] != "radar" or scene["crc32"] != crc:
        raise ValueError("设备未确认雷达画面")
    scene["radar_data"] = payload
    return result


def mirror(device, state, known_crc):
    """仅 CRC 变化时回读设备当前 RAM 数据；回执时钟驱动桌面独立轮播和倒计时。"""
    if state.get("radar_mirror_protocol") != 1:
        raise ValueError("设备程序需要更新：请上传支持雷达预览的 main.lua 和 radar_scene.lua")
    crc, size = state["crc32"], state["radar_size"]
    if not isinstance(crc, str) or len(crc) != 8 or type(size) is not int or not 1 <= size <= 8192:
        raise ValueError("设备雷达元数据无效")
    if "radar_data" in state or crc == known_crc:
        return state
    raw = bytearray()
    while len(raw) < size:
        part = device.request({"cmd": "radar_read", "crc32": crc, "offset": len(raw)})
        chunk = base64.b64decode(part["data"], validate=True)
        if part["crc32"] != crc or part["offset"] != len(raw) or not 1 <= len(chunk) <= min(192, size-len(raw)):
            raise ValueError("设备雷达分块不匹配")
        raw.extend(chunk)
    if "%08x" % (zlib.crc32(raw) & 0xFFFFFFFF) != crc:
        raise ValueError("设备雷达 CRC 校验失败")
    result = device.request({"cmd": "status"})
    if result["mode"] == "home":
        result = result["radar"]
    if result["mode"] != "radar" or result["crc32"] != crc:
        raise ValueError("回读期间设备画面已变化，请重新检测连接")
    result["radar_data"] = json.loads(raw)
    return result
