"""只读 CalendarTask 当前子日历的当天日格，保留完成标记并生成中文分页。"""
import hashlib
import html
import json
import os
import re
from contextlib import closing
from datetime import date
from pathlib import Path
from PIL import Image, ImageDraw
from radar_data import read_db
import holo_usb_display as display

MAX_PAGES = 1


def decode_text(value):
    """还原 CalendarTask 的竖线实体包装，移除已观察到的字体颜色标签，不执行HTML。"""
    value = re.sub(r"\|(&[A-Za-z0-9#]+;)\|", lambda match: html.unescape(match.group(1)), value)
    value = re.sub(r"</?font\b[^>]*>", "", value, flags=re.IGNORECASE)
    return html.unescape(value).replace("\xa0", " ")


def parse_items(content):
    """每个非空源文本行对应一项；仅行首 [+] 表示完成，保留其余文本与顺序。"""
    items = []
    for line in decode_text(content).splitlines():
        text = line.strip()
        done = text.startswith("[+]")
        if done:
            text = text[3:].lstrip()
        if text:
            items.append({"text": text, "done": done})
    return items


def collect(db_path=None, day=None):
    """按本机日期读取当前子日历；只访问日格及选中日历信息，不读取账号凭据或历史。"""
    path = Path(db_path) if db_path else Path(os.environ["APPDATA"]) / "CalendarTask/Db/calendar.db"
    day = date.today() if day is None else day
    with closing(read_db(path)) as db:
        db.execute("BEGIN")
        settings = {row["st_name"]: row for row in db.execute(
            "SELECT st_name,st_nval,st_sval FROM setting_table WHERE st_name IN ('sys_current_sub_account','sys_sub_account_list','group_id')")}
        account_id = settings["sys_current_sub_account"]["st_nval"]
        if type(account_id) is not int or account_id < 0:
            raise ValueError("请在日历清单中选择一个具体子日历")
        accounts = json.loads(decode_text(settings["sys_sub_account_list"]["st_sval"]))["vdata"]["list"]
        account = next(row["name"] for row in accounts if row["id"] == account_id)
        group = settings["group_id"]["st_sval"]
        rows = db.execute("""SELECT it_content FROM item_table
            WHERE u_id=? AND pj_id=0 AND COALESCE(group_id,'')=? AND it_unique_id=?""",
            (account_id, group, "dkcal_mdays_"+day.strftime("%Y%m%d"))).fetchall()
    if len(rows) > 1:
        raise ValueError("当天日格存在多条记录，需先核对日历数据，未擅自合并")
    items = parse_items(rows[0]["it_content"]) if rows else []
    data = {"date": day.isoformat(), "weekday": "周"+"一二三四五六日"[day.weekday()],
            "account": account, "account_id": account_id, "items": items,
            "total": len(items), "done": sum(item["done"] for item in items), "layout": 2}
    data["signature"] = hashlib.sha256(json.dumps(data, ensure_ascii=False, sort_keys=True).encode("utf-8")).hexdigest()[:16]
    data["db_path"] = str(path)
    return data


def render(snapshot):
    """仅渲染紧凑的第一页，溢出明确提示；完成行保留删除线，不自动翻页。"""
    font = display.load_font(14)
    probe = ImageDraw.Draw(Image.new("RGB", (320, 240)))
    rows, y, truncated = [], 60, False
    for index, item in enumerate(snapshot["items"], 1):
        lines = display.wrap_text(probe, item["text"], font, 270)
        for line_index, line in enumerate(lines):
            if y+18 > 217:
                truncated = True
                break
            rows.append((index, line_index == 0 or not rows, line, item["done"], y))
            y += 18
        if truncated:
            break
        y += 6
    pages = [rows]
    output = []
    for page_index, page in enumerate(pages, 1):
        frame = Image.new("RGB", (320, 240), (0, 0, 0))
        draw = ImageDraw.Draw(frame)
        draw.text((12, 7), "日历清单", font=display.load_font(18, bold=True), fill=(222, 247, 255), anchor="lt")
        draw.text((226, 10), "完成 %d/%d" % (snapshot["done"], snapshot["total"]), font=display.load_font(11), fill=(81, 221, 199), anchor="lt")
        heading = snapshot["date"]+"  "+snapshot["weekday"]+" · "+snapshot["account"]
        heading_font = display.fit_single_line_font(draw, heading, 296, 11, 8)
        draw.text((12, 33), heading, font=heading_font, fill=(120, 155, 169), anchor="lt")
        draw.line((12, 51, 308, 51), fill=(30, 57, 65), width=1)
        if not page:
            draw.text((75, 116), "当日暂无任务", font=display.load_font(20), fill=(133, 166, 174), anchor="lt")
        for number, show_number, text, done, top in page:
            color = (121, 146, 154) if done else (227, 241, 247)
            if show_number:
                draw.text((12, top+2), "%02d" % number, font=display.load_font(10), fill=(81, 221, 199), anchor="lt")
            draw.text((38, top), text, font=font, fill=color, anchor="lt")
            if done:
                box = draw.textbbox((38, top), text, font=font, anchor="lt")
                draw.line((box[0], (box[1]+box[3])//2, box[2], (box[1]+box[3])//2), fill=color, width=1)
        draw.line((12, 220, 308, 220), fill=(30, 57, 65), width=1)
        draw.text((12, 225), "本地只读", font=display.load_font(10), fill=(120, 155, 169), anchor="lt")
        draw.text((158, 225), "仅第一页 · 还有内容" if truncated else "固定显示第一页", font=display.load_font(10), fill=(120, 155, 169), anchor="lt")
        output.append(display.encode_jpeg(frame))
    return output
