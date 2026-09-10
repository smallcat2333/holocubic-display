"""主页预加载与真实画面回读；轮播本身由设备运行，不逐页重发画面。"""
import base64
import uuid
import zlib
from pathlib import Path
import holo_usb_display as display
import radar_usb
import calendar_data
import calendar_usb


def prepare(request, task_values, task_labels):
    """打开串口前校验选项和全部素材，复用既有文字/图片/中文标签渲染。"""
    entries = request["home_entries"]
    if not isinstance(entries, list) or not 1 <= len(entries) <= 5:
        raise ValueError("至少勾选一个轮播画面")
    seen = set()
    for row in entries:
        if row["mode"] not in ("task", "radar", "text", "image", "calendar") or row["mode"] in seen:
            raise ValueError("轮播模式无效或重复")
        if type(row["seconds"]) is not int or not 1 <= row["seconds"] <= 3600:
            raise ValueError("每个画面的停留时间应为 1～3600 秒")
        seen.add(row["mode"])
    settings = request["settings"]
    prepared = {"entries": entries, "images": {}, "task": None, "radar": None}
    if "task" in seen:
        if "resume_task" in request:
            saved = request["resume_task"]
            prepared["labels"] = bytes(saved["labels"])
            if "%08x" % (zlib.crc32(prepared["labels"]) & 0xffffffff) != saved["labels_crc"]:
                raise ValueError("任务恢复标签 CRC 不匹配")
            prepared["task"] = {"cmd": "task", "seconds": saved["duration"], "bp": saved["initial_bp"],
                                "timed": saved["timed"], "looping": saved["looping"], "accent": saved["accent"],
                                "restore": {"counter": saved["counter"], "paused": saved["paused"]}}
        else:
            title, footer, duration, bp, accent = task_values(settings)
            prepared["labels"] = task_labels(title, footer)
            prepared["task"] = {"cmd": "task", "seconds": duration, "bp": bp, "timed": settings["timed"],
                                "looping": settings["looping"], "accent": accent[0]*65536+accent[1]*256+accent[2]}
    if "text" in seen:
        if not settings["text_body"].strip():
            raise ValueError("文字正文不能为空，请先在文字页面编辑")
        frame = display.render_text_frame(settings["text_title"], settings["text_body"], settings["text_footer"],
                                          accent="#%02x%02x%02x" % tuple(settings["accent"]))
        prepared["images"]["text"] = display.encode_jpeg(frame)
    if "image" in seen:
        if not settings["image_path"]:
            raise ValueError("请先在图片页面选择图片")
        prepared["images"]["image"] = display.encode_jpeg(display.render_image_frame(Path(settings["image_path"])))
    if "radar" in seen:
        prepared["radar"] = request["snapshot"]
        radar_usb.compact(prepared["radar"])
    if "calendar" in seen:
        prepared["calendar"] = request["calendar_snapshot"]
        prepared["calendar_pages"] = calendar_data.render(prepared["calendar"])
    return prepared


def start(device, prepared):
    """只初始化一次被选中的场景，静态素材传完后启动设备本地轮播。"""
    device.request({"cmd": "home_begin"})
    try:
        for kind, data in prepared["images"].items():
            device.upload_jpeg(data, target="home_"+kind)
        calendar = calendar_usb.send(device, prepared["calendar"], prepared["calendar_pages"]) if "calendar" in prepared else None
        if prepared["task"] is not None:
            device.upload_jpeg(prepared["labels"], target="task_labels")
            task = device.request(prepared["task"], timeout=8)
            expected = prepared["task"]
            if task["mode"] != "task" or ("restore" not in expected and task["bp"] != expected["bp"]):
                raise ValueError("沙漏预加载未确认")
            if "restore" in expected and (task["duration"] != expected["seconds"] or task["counter"] != expected["restore"]["counter"] or task["paused"] != expected["restore"]["paused"]):
                raise ValueError("沙漏恢复状态未确认")
        radar = radar_usb.send(device, prepared["radar"]) if prepared["radar"] is not None else None
        session = uuid.uuid4().hex[:8]
        result = device.request({"cmd": "home_apply", "entries": prepared["entries"], "session": session}, timeout=8)
        if result["mode"] != "home" or result["home_id"] != session or result["home_entries"] != prepared["entries"]:
            raise ValueError("设备未确认轮播配置")
        if prepared["task"] is not None:
            result["task"]["labels"] = list(prepared["labels"])
        if radar is not None:
            if result["radar"]["crc32"] != radar["crc32"]:
                raise ValueError("预加载雷达在启动期间已变化")
            result["radar"]["radar_data"] = radar["radar_data"]
        if calendar is not None:
            if result["calendar"]["calendar_signature"] != calendar["calendar_signature"]:
                raise ValueError("日历预加载在启动期间已变化")
            for new, old in zip(result["calendar"]["calendar_pages"], calendar["calendar_pages"]):
                new["data"] = old["data"]
        for kind, data in prepared["images"].items():
            if result["home_images"][kind]["crc32"] != "%08x" % (zlib.crc32(data) & 0xffffffff):
                raise ValueError("轮播图片校验值不匹配")
            result["home_images"][kind]["data"] = list(data)
        return result
    except Exception:
        try:
            device.request({"cmd": "clear"})
        except Exception:
            pass  # 串口已断开时保留原始错误，不以清理失败覆盖根因。
        raise


def mirror(device, state, request, mirror_labels):
    """CRC变化才回读各场景素材；传输后统一校准时钟，防止读取时跨页导致错配。"""
    if state.get("home_protocol") != 1:
        raise ValueError("设备不支持主页轮播回读")
    transferred = False
    if "task" in state:
        task = state["task"]
        uploaded = bytes(task.pop("labels")) if "labels" in task else None
        transferred |= uploaded is None and task["labels_crc"] != request.get("labels_crc")
        state["task"] = mirror_labels(device, task, request.get("labels_crc"), uploaded)
    if "radar" in state:
        radar = state["radar"]
        transferred |= "radar_data" not in radar and radar["crc32"] != request.get("radar_crc")
        state["radar"] = radar_usb.mirror(device, radar, request.get("radar_crc"))
    if "calendar" in state:
        calendar = state["calendar"]
        transferred |= calendar["calendar_signature"] != request.get("calendar_signature") and not all("data" in p for p in calendar["calendar_pages"])
        state["calendar"] = calendar_usb.mirror(device, calendar, request.get("calendar_signature"))
    for kind, picture in state.get("home_images", {}).items():
        size, crc = picture["size"], picture["crc32"]
        if kind not in ("text", "image") or type(size) is not int or not 1 <= size <= 512*1024:
            raise ValueError("轮播图片元数据无效")
        if "data" in picture or request.get("home_crcs", {}).get(kind) == crc:
            continue
        raw = bytearray()
        while len(raw) < size:
            part = device.request({"cmd": "home_asset_read", "kind": kind, "crc32": crc, "offset": len(raw)})
            chunk = base64.b64decode(part["data"], validate=True)
            if part["crc32"] != crc or part["offset"] != len(raw) or not 1 <= len(chunk) <= min(192, size-len(raw)):
                raise ValueError("轮播图片分块不匹配")
            raw.extend(chunk)
        if "%08x" % (zlib.crc32(raw) & 0xffffffff) != crc:
            raise ValueError("轮播图片 CRC 校验失败")
        picture["data"] = list(raw)
        transferred = True
    if not transferred:
        return state
    latest = device.request({"cmd": "status"})
    if latest["mode"] != "home" or latest["home_id"] != state["home_id"]:
        raise ValueError("回读期间轮播配置已变化，请重新检测连接")
    for key, checksum, payload in (("task", "labels_crc", "labels"), ("radar", "crc32", "radar_data")):
        if key in state:
            if latest[key][checksum] != state[key][checksum]:
                raise ValueError("回读期间轮播内容已变化")
            if payload in state[key]:
                latest[key][payload] = state[key][payload]
    for kind, picture in state.get("home_images", {}).items():
        if latest["home_images"][kind]["crc32"] != picture["crc32"]:
            raise ValueError("回读期间轮播图片已变化")
        if "data" in picture:
            latest["home_images"][kind]["data"] = picture["data"]
    if "calendar" in state:
        if latest["calendar"]["calendar_signature"] != state["calendar"]["calendar_signature"]:
            raise ValueError("回读期间日历已变化")
        for new, old in zip(latest["calendar"]["calendar_pages"], state["calendar"]["calendar_pages"]):
            if "data" in old:
                new["data"] = old["data"]
    return latest
