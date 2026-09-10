"""本地日格只读筛选、完成标记、中文分页和 USB 页面缓存测试。"""
import hashlib
import io
import json
import sqlite3
import tempfile
import unittest
import zlib
from datetime import date
from pathlib import Path
from unittest.mock import Mock, patch
from PIL import Image, ImageDraw
import bridge
import calendar_data as calendar
import calendar_usb


def sample(done=False):
    """无私人内容的固定日历样本。"""
    return {"date":"2026-09-08","weekday":"周二","account":"测试日历","total":1,
            "done":int(done),"items":[{"text":"A          B", "done":done}],"signature":"1234567890abcdef"}


class CalendarTests(unittest.TestCase):
    """使用临时数据库与虚拟设备，不写真实日历或原任务完成状态。"""
    def test_today_current_account_only_and_no_writes(self):
        """排除其它日期、子日历和团队，读取前后源数据库字节保持一致。"""
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/"calendar.db"
            accounts=json.dumps({"vdata":{"list":[{"id":1,"name":"工作"},{"id":2,"name":"生活"}]}},ensure_ascii=False).replace('"','|&quot;|')
            with sqlite3.connect(path) as db:
                db.execute("CREATE TABLE setting_table(st_name TEXT,st_nval INTEGER,st_sval TEXT)")
                db.executemany("INSERT INTO setting_table VALUES(?,?,?)",[("sys_current_sub_account",1,""),("sys_sub_account_list",0,accounts),("group_id",0,""),("user_token",0,"DO_NOT_READ")])
                db.execute("CREATE TABLE item_table(u_id INTEGER,pj_id INTEGER,group_id TEXT,it_unique_id TEXT,it_content TEXT)")
                db.executemany("INSERT INTO item_table VALUES(?,?,?,?,?)",[
                    (1,0,"","dkcal_mdays_20260908","待办\r\n[+]已完成"),
                    (2,0,"","dkcal_mdays_20260908","别的子日历"),
                    (1,0,"team","dkcal_mdays_20260908","别的团队"),
                    (1,0,"","dkcal_mdays_20260907","不是当日")])
            db.close()
            before=hashlib.sha256(path.read_bytes()).hexdigest()
            result=calendar.collect(path,date(2026,9,8))
            self.assertEqual(2,result["total"])
            self.assertEqual(1,result["done"])
            self.assertEqual("工作",result["account"])
            self.assertEqual([{"text":"待办","done":False},{"text":"已完成","done":True}],result["items"])
            self.assertNotIn("DO_NOT_READ",json.dumps(result))
            self.assertEqual(before,hashlib.sha256(path.read_bytes()).hexdigest())
            self.assertEqual(result["signature"],calendar.collect(path,date(2026,9,8))["signature"])
            self.assertEqual(0,calendar.collect(path,date(2026,9,9))["total"])

    def test_completion_and_color_markup(self):
        """完成标记可位于颜色标签中，正文中的 [+] 不被误判为完成。"""
        items=calendar.parse_items('普通[+]正文\r\n|&lt;|font color=|&quot;|#FF0000|&quot;||&gt;|[+]完成|&lt;|/font|&gt;|\r\n\r\n')
        self.assertEqual([{"text":"普通[+]正文","done":False},{"text":"完成","done":True}],items)

    def test_completed_line_is_actually_struck_through(self):
        """直接检查字间空白处的删除线像素，不依赖文字标签声称已完成。"""
        with patch.object(calendar.display,"encode_jpeg",side_effect=lambda image:image.copy()):
            done=calendar.render(sample(True))[0]
            pending=calendar.render(sample(False))[0]
        font=calendar.display.load_font(14)
        draw=ImageDraw.Draw(done)
        line=calendar.display.wrap_text(draw,"A          B",font,270)[0]
        space=line.index(" ")
        x=38+int(calendar.display.text_width(draw,line[:space],font)+calendar.display.text_width(draw," ",font)/2)
        box=draw.textbbox((38,60),line,font=font,anchor="lt")
        y=(box[1]+box[3])//2
        self.assertEqual((121,146,154),done.getpixel((x,y)))
        self.assertEqual((0,0,0),pending.getpixel((x,y)))

    def test_long_content_stays_on_first_page(self):
        """按用户要求只生成第一页，长内容不产生后续翻页。"""
        data=sample()
        data["items"][0]["text"]="任务内容"*200
        pages=calendar.render(data)
        self.assertEqual(len(pages),1)
        self.assertTrue(all(Image.open(io.BytesIO(page)).size==(320,240) for page in pages))
        data["items"][0]["text"]="任务内容"*2000
        self.assertEqual(len(calendar.render(data)),1)

    def test_upload_and_unchanged_cache(self):
        """设备确认所有页的 CRC 后才成为预览，相同签名不再次回读。"""
        pages=calendar.render(sample())
        response={"mode":"calendar","calendar_signature":sample()["signature"],"calendar_pages":[{"size":len(p),"crc32":"%08x"%(zlib.crc32(p)&0xffffffff)} for p in pages]}
        device=Mock()
        device.request.side_effect=[{"bank":"b"},response]
        result=calendar_usb.send(device,sample(),pages)
        device.upload_jpeg.assert_called_once_with(pages[0],target="calendar_b_01")
        self.assertEqual(list(pages[0]),result["calendar_pages"][0]["data"])
        device.request.reset_mock()
        calendar_usb.mirror(device,result,sample()["signature"])
        device.request.assert_not_called()


if __name__=="__main__":
    unittest.main()
