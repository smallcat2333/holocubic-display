"""日历分页原子上传与设备实际画面回读；不写日历清单数据库。"""
import base64
import zlib
import calendar_data


def scene_state(state):
    """主页中读取保留的日历场景，单独显示时直接使用根状态。"""
    return state["calendar"] if state["mode"] == "home" else state


def send(device, snapshot, pages, standalone=False):
    """上传到非活动槽，全部页面校验完再一次切换；中途失败保留旧画面。"""
    prepared = device.request({"cmd": "calendar_begin", "pages": len(pages), "signature": snapshot["signature"],
                               "date": snapshot["date"], "total": snapshot["total"], "done": snapshot["done"]})
    bank = prepared["bank"]
    if bank not in ("a", "b"):
        raise ValueError("设备返回了无效日历槽")
    for index, data in enumerate(pages, 1):
        device.upload_jpeg(data, target="calendar_%s_%02d" % (bank, index))
    result = device.request({"cmd": "calendar_commit", "signature": snapshot["signature"], "standalone": standalone}, timeout=8)
    scene = scene_state(result)
    if scene["mode"] != "calendar" or scene["calendar_signature"] != snapshot["signature"] or len(scene["calendar_pages"]) != len(pages):
        raise ValueError("设备未确认日历内容")
    for meta, raw in zip(scene["calendar_pages"], pages):
        if meta["size"] != len(raw) or meta["crc32"] != "%08x" % (zlib.crc32(raw) & 0xffffffff):
            raise ValueError("日历页面 CRC 回执不匹配")
        meta["data"] = list(raw)
    return result


def mirror(device, state, known_signature):
    """仅内容变更时回读已发布的页面，读取结束后重新校准设备页码相位。"""
    signature = state["calendar_signature"]
    if not 1 <= len(state["calendar_pages"]) <= calendar_data.MAX_PAGES:
        raise ValueError("设备日历页数无效")
    if signature == known_signature or all("data" in page for page in state["calendar_pages"]):
        return state
    for index, page in enumerate(state["calendar_pages"], 1):
        size = page["size"]
        if type(size) is not int or not 1 <= size <= 512*1024:
            raise ValueError("设备日历页面大小无效")
        raw = bytearray()
        while len(raw) < size:
            part = device.request({"cmd": "calendar_read", "signature": signature, "page": index, "offset": len(raw)})
            chunk = base64.b64decode(part["data"], validate=True)
            if part["page"] != index or part["offset"] != len(raw) or part["crc32"] != page["crc32"] or not 1 <= len(chunk) <= min(192, size-len(raw)):
                raise ValueError("日历回读分块不匹配")
            raw.extend(chunk)
        if "%08x" % (zlib.crc32(raw) & 0xffffffff) != page["crc32"]:
            raise ValueError("日历回读 CRC 不匹配")
        page["data"] = list(raw)
    final = scene_state(device.request({"cmd": "status"}))
    if final["mode"] != "calendar" or final["calendar_signature"] != signature:
        raise ValueError("回读期间日历已变化，请重新检测连接")
    for latest, old in zip(final["calendar_pages"], state["calendar_pages"]):
        if latest["crc32"] != old["crc32"]:
            raise ValueError("回读期间日历页已变化")
        latest["data"] = old["data"]
    return final
