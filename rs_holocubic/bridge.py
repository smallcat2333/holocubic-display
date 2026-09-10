"""egui JSON bridge: reuse verified USB and Pillow rendering; stdout is one JSON reply."""
import contextlib
import base64
import io
import json
import math
import sys
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import holo_usb_display as display
from PIL import Image, ImageDraw


def task_values(settings):
    """Validate external GUI values and convert percentage to exact basis points."""
    title, footer = settings["task_name"].strip(), settings["footer"]
    if not 1 <= len(title) <= 24 or any(ch in title for ch in "\r\n"):
        raise ValueError("任务名称需要 1～24 个字符且不能换行")
    if len(footer) > 36 or any(ch in footer for ch in "\r\n"):
        raise ValueError("顶部说明最多 36 个字符且不能换行")
    hours, minutes, seconds = settings["hours"], settings["minutes"], settings["seconds"]
    if any(type(v) is not int for v in (hours, minutes, seconds)) or not (0 <= hours <= 168 and 0 <= minutes < 60 and 0 <= seconds < 60):
        raise ValueError("无效的时间字段")
    duration = hours * 3600 + minutes * 60 + seconds
    remaining = settings["remaining"]
    if not 1 <= duration <= 604800 or type(remaining) not in (float, int) or not math.isfinite(remaining) or not 0 <= remaining <= 100:
        raise ValueError("时间或百分比超出范围")
    if type(settings["timed"]) is not bool:
        raise ValueError("timed 必须是布尔值")
    if type(settings["looping"]) is not bool:
        raise ValueError("looping 必须是布尔值")
    accent = settings["accent"]
    if len(accent) != 3 or any(type(v) is not int or not 0 <= v <= 255 for v in accent):
        raise ValueError("无效的 RGB 颜色")
    return title, footer, duration, math.floor(remaining * 100 + 0.5), accent


def task_labels(title, footer):
    """Render a centered bottom title and up to two top note lines; animation stays local."""
    frame = Image.new("RGB", display.SCREEN_SIZE, (0, 0, 0))
    draw = ImageDraw.Draw(frame)
    for size in range(13, 9, -1):
        note_font = display.load_font(size)
        note_lines = display.wrap_text(draw, footer, note_font, 215)
        if len(note_lines) <= 2:
            break
    for index, line in enumerate(note_lines):
        draw.text((16, 8 + index * 16), line, font=note_font, fill=(130, 185, 196), anchor="lt")
    font = display.fit_single_line_font(draw, title, 288, 18, 8, bold=True)
    width = display.text_width(draw, title, font)
    draw.text(((320 - width) / 2, 218), title, font=font, fill=(225, 248, 255), anchor="lt")
    return display.encode_jpeg(frame)


def mirror_labels(device, state, known_crc, uploaded=None):
    """Read the active label bytes only on CRC changes; never rebuild them from the draft."""
    if state["mode"] != "task":
        return state
    if state.get("mirror_protocol") != 1:
        raise ValueError("设备程序需要更新：请上传新版 main.lua 和 quota_scene.lua 以回读实际画面")
    expected_crc, size = state["labels_crc"], state["labels_size"]
    if not isinstance(expected_crc, str) or len(expected_crc) != 8 or not 1 <= size <= 512 * 1024:
        raise ValueError("设备标签元数据无效")
    if uploaded is not None:
        data = uploaded
    elif expected_crc == known_crc:
        return state
    else:
        data = bytearray()
        while len(data) < size:
            part = device.request({"cmd": "task_labels_read", "crc32": expected_crc, "offset": len(data)})
            chunk = base64.b64decode(part["data"], validate=True)
            if part["crc32"] != expected_crc or part["offset"] != len(data) or not 1 <= len(chunk) <= min(192, size - len(data)):
                raise ValueError("设备标签分块不匹配")
            data.extend(chunk)
        # File transfer takes time: sample again so the visual values are current.
        state = device.request({"cmd": "status"})
        if state["mode"] == "home":
            state = state["task"]
        if state["mode"] != "task" or state["labels_crc"] != expected_crc:
            raise ValueError("回读期间设备任务已变化，请重新检测连接")
    if len(data) != size or "%08x" % (zlib.crc32(data) & 0xFFFFFFFF) != expected_crc:
        raise ValueError("设备标签 CRC32 校验失败")
    state["labels"] = list(data)
    return state


def execute(request):
    """Execute one bounded GUI operation; never expose tokens or touch firmware flash."""
    action = request["action"]
    if action == "radar":
        import radar_data
        return radar_data.collect(request["cache_dir"])
    if action == "calendar_data":
        import calendar_data
        return calendar_data.collect()
    if action == "ports":
        return {"ports": display.available_ports()}
    if action == "browse":
        import tkinter
        from tkinter import filedialog
        root = tkinter.Tk()
        root.withdraw()
        try:
            root.attributes("-topmost", True)
            path = filedialog.askopenfilename(title="选择显示图片", filetypes=[("图片", "*.png *.jpg *.jpeg *.bmp *.webp"), ("所有文件", "*.*")])
        finally:
            root.destroy()
        return {"path": path}
    settings = request.get("settings")
    values = task_values(settings) if action == "task" else None
    prepared = None
    calendar_pages = None
    if action in ("calendar_update", "calendar_only"):
        import calendar_data
        calendar_pages = calendar_data.render(request["calendar_snapshot"])
    if action == "home":
        import home_bridge
        prepared = home_bridge.prepare(request, task_values, task_labels)
    port = display.find_holocubic_port(request.get("port") or None)
    with display.HoloCubicUSB(port) as device:
        state = device.wait_until_ready()
        uploaded = None
        if action == "status":
            result = state
        elif action in ("calendar_update", "calendar_only"):
            if state.get("calendar_protocol") != 1:
                raise ValueError("设备程序需要更新：请上传支持日历的 Lua 程序")
            import calendar_usb
            result = calendar_usb.send(device, request["calendar_snapshot"], calendar_pages, standalone=action=="calendar_only")
        elif action == "home":
            if state.get("home_protocol") != 1:
                raise ValueError("设备程序需要更新：请上传主页轮播 Lua 程序")
            result = home_bridge.start(device, prepared)
        elif action == "home_stop":
            result = device.request({"cmd": "home_stop"})
        elif action in ("radar_usb", "radar_only"):
            if state.get("radar_protocol") != 1 or state.get("radar_mirror_protocol") != 1:
                raise ValueError("设备程序需要更新：请上传新版 main.lua 和 radar_scene.lua")
            import radar_usb
            result = radar_usb.send(device, request["snapshot"], standalone=action=="radar_only")
        elif action == "task":
            if state.get("task_protocol") != 1 or state.get("mirror_protocol") != 1 or state.get("loop_protocol") != 1:
                raise ValueError("设备程序需要更新：请通过热点上传 main.lua 和 quota_scene.lua")
            title, footer, duration, bp, accent = values
            uploaded = task_labels(title, footer)
            device.upload_jpeg(uploaded, target="task_labels")
            result = device.request({"cmd": "task", "seconds": duration, "bp": bp, "timed": settings["timed"], "looping": settings["looping"], "accent": accent[0] * 65536 + accent[1] * 256 + accent[2]}, timeout=8)
            if result["mode"] != "task" or result["bp"] != bp or result["looping"] != settings["looping"]:
                raise ValueError("设备没有正确应用任务参数")
        elif action in ("pause", "resume", "reset"):
            result = device.request({"cmd": "task_control", "action": action})
        elif action == "clear":
            result = device.request({"cmd": "clear"})
            result["mode"] = "image"
        elif action == "text":
            if not settings["text_body"].strip():
                raise ValueError("正文不能为空")
            accent = "#%02x%02x%02x" % tuple(settings["accent"])
            frame = display.render_text_frame(settings["text_title"], settings["text_body"], settings["text_footer"], accent=accent)
            result = device.upload_jpeg(display.encode_jpeg(frame))
            result["mode"] = "image"
        elif action == "image":
            frame = display.render_image_frame(Path(settings["image_path"]))
            result = device.upload_jpeg(display.encode_jpeg(frame))
            result["mode"] = "image"
        else:
            raise ValueError("不支持的操作: " + action)
        if result["mode"] == "home":
            import home_bridge
            result = home_bridge.mirror(device, result, request, mirror_labels)
        else:
            result = mirror_labels(device, result, request.get("labels_crc"), uploaded)
        if result["mode"] == "radar":
            import radar_usb
            result = radar_usb.mirror(device, result, request.get("radar_crc"))
        if result["mode"] == "calendar":
            import calendar_usb
            result = calendar_usb.mirror(device, result, request.get("calendar_signature"))
    result["port"] = port
    return result


def main():
    """Read a UTF-8 JSON request from stdin and emit exactly one structured result."""
    try:
        request = json.load(sys.stdin)
        with contextlib.redirect_stdout(io.StringIO()):
            result = execute(request)
        print(json.dumps({"ok": True, "result": result}, ensure_ascii=False))
        return 0
    except Exception as error:
        print(json.dumps({"ok": False, "error": str(error)}, ensure_ascii=False))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
