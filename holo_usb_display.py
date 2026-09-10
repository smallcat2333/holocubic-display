"""Show images or send native quota-animation state over a direct USB link."""

from __future__ import annotations

import argparse
import base64
import io
import json
import os
import sys
import time
import zlib
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence, Tuple

import serial
from PIL import Image, ImageDraw, ImageFont, ImageOps
from serial.tools import list_ports


SCREEN_SIZE = (320, 240)
USB_VID = 0x303A
USB_PID = 0x1001
PROTOCOL_PREFIX = b"@HCUSB/1 "
CHUNK_SIZE = 96


class HoloCubicError(RuntimeError):
    """Describe a host-side or device-reported USB display failure."""


def available_ports() -> List[Dict[str, object]]:
    """Return serial ports with the identifiers needed to find an ESP32-S3."""
    result: List[Dict[str, object]] = []
    for port in list_ports.comports():
        result.append(
            {
                "device": port.device,
                "description": port.description,
                "vid": port.vid,
                "pid": port.pid,
                "serial_number": port.serial_number,
            }
        )
    return result


def find_holocubic_port(requested_port: Optional[str] = None) -> str:
    """Use an explicit port or auto-detect Espressif USB Serial/JTAG."""
    if requested_port:
        return requested_port
    matches = [
        item["device"]
        for item in available_ports()
        if item["vid"] == USB_VID and item["pid"] == USB_PID
    ]
    if not matches:
        raise HoloCubicError(
            "未找到 Espressif 303A:1001 串口，请检查 USB 线或用 --port COMx 指定。"
        )
    if len(matches) > 1:
        joined = ", ".join(str(item) for item in matches)
        raise HoloCubicError(f"找到多个 ESP32-S3 串口：{joined}，请用 --port 指定。")
    return str(matches[0])


def parse_color(value: str) -> Tuple[int, int, int]:
    """Parse an RGB color in #RRGGBB or RRGGBB form."""
    raw = value.strip().lstrip("#")
    if len(raw) != 6 or any(char not in "0123456789abcdefABCDEF" for char in raw):
        raise ValueError(f"颜色必须是 #RRGGBB：{value}")
    return tuple(int(raw[index : index + 2], 16) for index in (0, 2, 4))


def font_candidates(bold: bool) -> Iterable[Path]:
    """Yield Windows fonts in an order that supports Chinese first."""
    font_dir = Path(os.environ.get("WINDIR", r"C:\Windows")) / "Fonts"
    names = (
        ("msyhbd.ttc", "simhei.ttf", "arialbd.ttf")
        if bold
        else ("msyh.ttc", "simsun.ttc", "arial.ttf")
    )
    for name in names:
        yield font_dir / name


def load_font(size: int, bold: bool = False) -> ImageFont.FreeTypeFont:
    """Load a Chinese-capable Windows font at the requested size."""
    for path in font_candidates(bold):
        if path.exists():
            return ImageFont.truetype(str(path), size=size)
    raise HoloCubicError("未找到微软雅黑、黑体或 Arial 字体。")


def text_width(draw: ImageDraw.ImageDraw, text: str, font: ImageFont.FreeTypeFont) -> int:
    """Measure one text line in pixels."""
    left, _, right, _ = draw.textbbox((0, 0), text or " ", font=font)
    return right - left


def wrap_text(
    draw: ImageDraw.ImageDraw,
    text: str,
    font: ImageFont.FreeTypeFont,
    max_width: int,
) -> List[str]:
    """Wrap mixed Chinese and Latin text by measured pixel width."""
    lines: List[str] = []
    for paragraph in text.splitlines() or [""]:
        current = ""
        for char in paragraph:
            candidate = current + char
            if not current or text_width(draw, candidate, font) <= max_width:
                current = candidate
            else:
                lines.append(current)
                current = char
        lines.append(current)
    return lines


def fit_single_line_font(
    draw: ImageDraw.ImageDraw,
    text: str,
    max_width: int,
    start_size: int,
    minimum_size: int,
    bold: bool = False,
) -> ImageFont.FreeTypeFont:
    """Choose the largest font that fits one line within the screen."""
    for size in range(start_size, minimum_size - 1, -1):
        font = load_font(size, bold=bold)
        if text_width(draw, text, font) <= max_width:
            return font
    return load_font(minimum_size, bold=bold)


def fit_body(
    draw: ImageDraw.ImageDraw,
    text: str,
    max_width: int,
    max_height: int,
) -> Tuple[ImageFont.FreeTypeFont, List[str], int]:
    """Fit wrapped body text into its allotted rectangle."""
    for size in range(27, 13, -1):
        font = load_font(size)
        lines = wrap_text(draw, text, font, max_width)
        line_height = size + 7
        if len(lines) * line_height <= max_height:
            return font, lines, line_height
    font = load_font(14)
    lines = wrap_text(draw, text, font, max_width)
    return font, lines[: max(1, max_height // 21)], 21


def aligned_x(
    draw: ImageDraw.ImageDraw,
    text: str,
    font: ImageFont.FreeTypeFont,
    alignment: str,
    left: int,
    width: int,
) -> int:
    """Return the horizontal origin for left, center, or right alignment."""
    measured = text_width(draw, text, font)
    if alignment == "left":
        return left
    if alignment == "right":
        return left + width - measured
    return left + (width - measured) // 2


def render_text_frame(
    title: str,
    body: str,
    footer: str = "USB DIRECT",
    background: str = "#02070B",
    foreground: str = "#F4FBFF",
    accent: str = "#35E7FF",
    alignment: str = "center",
) -> Image.Image:
    """Render a prism-friendly 320x240 text card on the computer."""
    bg = parse_color(background)
    fg = parse_color(foreground)
    highlight = parse_color(accent)
    image = Image.new("RGB", SCREEN_SIZE, bg)
    draw = ImageDraw.Draw(image)

    draw.rounded_rectangle((8, 8, 311, 231), radius=13, outline=highlight, width=2)
    draw.line((22, 61, 297, 61), fill=highlight, width=1)
    draw.ellipse((19, 20, 27, 28), fill=highlight)

    title_font = fit_single_line_font(draw, title, 252, 31, 17, bold=True)
    title_x = aligned_x(draw, title, title_font, alignment, 34, 263)
    draw.text((title_x, 17), title, font=title_font, fill=fg)

    body_font, lines, line_height = fit_body(draw, body, 274, 124)
    block_height = len(lines) * line_height
    body_y = 73 + max(0, (124 - block_height) // 2)
    for line in lines:
        line_x = aligned_x(draw, line, body_font, alignment, 23, 274)
        draw.text((line_x, body_y), line, font=body_font, fill=fg)
        body_y += line_height

    footer_font = fit_single_line_font(draw, footer, 260, 14, 11, bold=True)
    footer_x = aligned_x(draw, footer, footer_font, "center", 30, 260)
    draw.text((footer_x, 207), footer, font=footer_font, fill=highlight)
    return image


def render_image_frame(path: Path, background: str = "#000000") -> Image.Image:
    """Fit an arbitrary local image into 320x240 without cropping."""
    with Image.open(path) as source:
        source = ImageOps.exif_transpose(source)
        source.thumbnail(SCREEN_SIZE, Image.Resampling.LANCZOS)
        if source.mode in ("RGBA", "LA") or "transparency" in source.info:
            rgba = source.convert("RGBA")
            base = Image.new("RGBA", rgba.size, parse_color(background) + (255,))
            source_rgb = Image.alpha_composite(base, rgba).convert("RGB")
        else:
            source_rgb = source.convert("RGB")
        frame = Image.new("RGB", SCREEN_SIZE, parse_color(background))
        x = (SCREEN_SIZE[0] - source_rgb.width) // 2
        y = (SCREEN_SIZE[1] - source_rgb.height) // 2
        frame.paste(source_rgb, (x, y))
    return frame


def encode_jpeg(image: Image.Image, quality: int = 88) -> bytes:
    """Encode one exact-size baseline JPEG for the HoloCubic decoder."""
    if image.size != SCREEN_SIZE:
        raise ValueError(f"画面必须是 {SCREEN_SIZE[0]}x{SCREEN_SIZE[1]}。")
    output = io.BytesIO()
    image.convert("RGB").save(
        output,
        format="JPEG",
        quality=quality,
        optimize=True,
        progressive=False,
        subsampling=2,
    )
    return output.getvalue()


class HoloCubicUSB:
    """Send framed commands and JPEG payloads over ESP32-S3 USB CDC."""

    def __init__(self, port: str, baudrate: int = 115200) -> None:
        """Store serial settings without opening the device yet."""
        self.port = port
        self.baudrate = baudrate
        self.serial: Optional[serial.Serial] = None
        self.request_number = 0

    def open(self) -> None:
        """Open USB CDC with DTR and RTS deasserted to avoid bootloader entry."""
        connection = serial.Serial()
        connection.port = self.port
        connection.baudrate = self.baudrate
        connection.timeout = 0.15
        connection.write_timeout = 3.0
        connection.dsrdtr = False
        connection.rtscts = False
        connection.dtr = False
        connection.rts = False
        connection.open()
        self.serial = connection
        time.sleep(0.2)
        connection.reset_input_buffer()

    def close(self) -> None:
        """Close USB CDC while keeping both control lines deasserted."""
        if self.serial and self.serial.is_open:
            self.serial.dtr = False
            self.serial.rts = False
            self.serial.close()
        self.serial = None

    def __enter__(self) -> "HoloCubicUSB":
        """Open the serial session for a context manager."""
        self.open()
        return self

    def __exit__(self, exc_type, exc_value, traceback) -> None:
        """Close the serial session when leaving a context manager."""
        self.close()

    def _next_id(self) -> str:
        """Create a connection-local request identifier."""
        self.request_number += 1
        return f"{int(time.time() * 1000)}-{self.request_number}"

    def request(self, command: Dict[str, object], timeout: float = 3.0) -> Dict[str, object]:
        """Send one JSON command and wait for its matching prefixed response."""
        if not self.serial or not self.serial.is_open:
            raise HoloCubicError("串口尚未打开。")
        request_id = self._next_id()
        payload = dict(command)
        payload["id"] = request_id
        raw = PROTOCOL_PREFIX + json.dumps(
            payload, ensure_ascii=True, separators=(",", ":")
        ).encode("ascii") + b"\n"
        self.serial.write(raw)
        self.serial.flush()

        deadline = time.monotonic() + timeout
        pending = bytearray()
        while time.monotonic() < deadline:
            pending.extend(self.serial.read_until(b"\n"))
            if len(pending) > 65536:
                raise HoloCubicError("USB 回执超过 64KB 上限。")
            if not pending.endswith(b"\n"):
                continue
            line = bytes(pending)
            pending.clear()
            if not line.startswith(PROTOCOL_PREFIX):
                continue
            try:
                response = json.loads(line[len(PROTOCOL_PREFIX) :].decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            if response.get("id") != request_id:
                continue
            if response.get("status") == "multipart":
                sampled_at = time.monotonic()
                size, checksum = response["size"], response["crc32"]
                if type(size) is not int or not 1 <= size <= 16384 or not isinstance(checksum, str) or len(checksum) != 8:
                    raise HoloCubicError("USB 分块回执元数据无效。")
                data = bytearray()
                while len(data) < size:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise HoloCubicError("USB 分块回执超时。")
                    part = self.request({"cmd": "response_read", "token": request_id, "offset": len(data)}, timeout=remaining)
                    chunk = base64.b64decode(part["data"], validate=True)
                    if part["status"] != "part" or part["offset"] != len(data) or not 1 <= len(chunk) <= min(96, size-len(data)):
                        raise HoloCubicError("USB 分块回执顺序或长度不匹配。")
                    data.extend(chunk)
                if f"{zlib.crc32(data) & 0xFFFFFFFF:08x}" != checksum:
                    raise HoloCubicError("USB 分块回执 CRC32 不匹配。")
                response = json.loads(data.decode("utf-8"))
                if response.get("id") != request_id:
                    raise HoloCubicError("USB 分块回执身份不匹配。")
                response["_transport_ms"] = round((time.monotonic() - sampled_at) * 1000)
            if response.get("status") == "error":
                raise HoloCubicError(str(response.get("message", "设备返回未知错误")))
            return response
        raise HoloCubicError(
            "设备未响应：请先在 HoloCubic Launcher 中启动“USB直连显示”应用。"
        )

    def wait_until_ready(self, timeout: float = 6.0) -> Dict[str, object]:
        """Retry the hello handshake while the foreground Lua app starts."""
        deadline = time.monotonic() + timeout
        last_error: Optional[Exception] = None
        while time.monotonic() < deadline:
            try:
                return self.request({"cmd": "hello"}, timeout=1.5)
            except HoloCubicError as exc:
                last_error = exc
                time.sleep(0.2)
        raise HoloCubicError(str(last_error or "设备未就绪。"))

    def upload_jpeg(self, data: bytes, target: str = "frame") -> Dict[str, object]:
        """Upload a complete JPEG with ordered chunks and CRC32 validation."""
        checksum = zlib.crc32(data) & 0xFFFFFFFF
        checksum_hex = f"{checksum:08x}"
        calendar_target = len(target) == 13 and target[:9] == "calendar_" and target[9] in "ab" and target[10] == "_" and target[11:].isdigit() and 1 <= int(target[11:]) <= 32
        if target not in ("frame", "task_labels", "home_text", "home_image") and not calendar_target:
            raise ValueError("不支持的图片目标。")
        self.request(
            {"cmd": "image_begin", "size": len(data), "crc32": checksum_hex, "target": target}, timeout=4.0
        )
        chunk_count = (len(data) + CHUNK_SIZE - 1) // CHUNK_SIZE
        for sequence, offset in enumerate(range(0, len(data), CHUNK_SIZE)):
            encoded = base64.b64encode(data[offset : offset + CHUNK_SIZE]).decode("ascii")
            self.request(
                {"cmd": "image_chunk", "seq": sequence, "data": encoded}, timeout=3.0
            )
            percent = int((sequence + 1) * 100 / chunk_count)
            print(f"\rUSB 发送：{percent:3d}%", end="", flush=True)
        print()
        response = self.request({"cmd": "image_end"}, timeout=5.0)
        if (
            response["status"] != ("displayed" if target == "frame" else "stored")
            or response["bytes"] != len(data)
            or response["crc32"] != checksum_hex
        ):
            raise HoloCubicError("设备完成回执与发送画面的大小或 CRC32 不一致。")
        # Hex avoids the firmware's 32-bit floating-point JSON precision loss.
        response["crc32"] = int(response["crc32"], 16)
        return response

    def set_quota(
        self, remaining: int, reset_seconds: int, demo: bool = False
    ) -> Dict[str, object]:
        """Send one small state packet; demo simulates usage, otherwise quota stays fixed."""
        if type(remaining) is not int or not 0 <= remaining <= 100:
            raise ValueError("剩余额度必须是 0～100 的整数。")
        if type(reset_seconds) is not int or not 0 <= reset_seconds <= 604800:
            raise ValueError("重置倒计时必须是 0～604800 秒的整数。")
        if type(demo) is not bool:
            raise ValueError("demo 必须是布尔值。")
        result = self.request(
            {"cmd": "quota", "remaining": remaining, "reset_seconds": reset_seconds, "demo": demo},
            timeout=8.0,
        )
        if result["status"] != "ok" or result["mode"] != "quota":
            raise HoloCubicError("设备没有进入沙漏动画模式。")
        return result


def print_ports() -> None:
    """Print detected ports and mark the expected Espressif interface."""
    ports = available_ports()
    if not ports:
        print("未找到串口。")
        return
    for item in ports:
        vid = item["vid"]
        pid = item["pid"]
        identifier = f"{vid:04X}:{pid:04X}" if vid is not None and pid is not None else "----:----"
        marker = "  < HoloCubic/ESP32-S3" if vid == USB_VID and pid == USB_PID else ""
        print(f"{item['device']:>6}  {identifier}  {item['description']}{marker}")


def build_parser() -> argparse.ArgumentParser:
    """Build the command-line interface shared by all display actions."""
    parser = argparse.ArgumentParser(description="HoloCubic USB 直连显示控制器")
    parser.add_argument("--port", help="串口，默认自动识别 303A:1001")
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser("ports", help="列出本机串口")
    subparsers.add_parser("ping", help="检查设备端 Lua 应用")
    subparsers.add_parser("clear", help="清空显示")

    quota_parser = subparsers.add_parser("quota", help="本地沙漏动画，只传额度状态")
    quota_parser.add_argument("--remaining", type=int, default=72, help="剩余额度百分比，0～100")
    quota_parser.add_argument("--reset-seconds", type=int, default=3600, help="重置倒计时秒数，最多 7 天")
    quota_parser.add_argument("--demo", action="store_true", help="循环模拟消耗，非真实 Codex 额度")

    text_parser = subparsers.add_parser("text", help="显示电脑端渲染的文字卡片")
    text_parser.add_argument("--title", default="HoloCubic", help="标题")
    text_parser.add_argument("--body", required=True, help="正文，可含中文")
    text_parser.add_argument("--footer", default="USB DIRECT", help="底部小字")
    text_parser.add_argument("--bg", default="#02070B", help="背景色 #RRGGBB")
    text_parser.add_argument("--fg", default="#F4FBFF", help="文字色 #RRGGBB")
    text_parser.add_argument("--accent", default="#35E7FF", help="强调色 #RRGGBB")
    text_parser.add_argument(
        "--align", choices=("left", "center", "right"), default="center", help="对齐"
    )

    image_parser = subparsers.add_parser("image", help="完整显示本地图片")
    image_parser.add_argument("path", type=Path, help="PNG/JPEG/BMP/WebP 路径")
    image_parser.add_argument("--bg", default="#000000", help="留白区域背景色")
    return parser


def run_device_command(args: argparse.Namespace) -> None:
    """Connect once, handshake, and execute the selected device action."""
    port = find_holocubic_port(args.port)
    with HoloCubicUSB(port) as device:
        hello = device.wait_until_ready()
        if args.command == "ping":
            print(
                f"已连接 {port}：{hello.get('app', 'usb_display')} "
                f"{hello.get('width', 320)}x{hello.get('height', 240)}"
            )
            return
        if args.command == "clear":
            device.request({"cmd": "clear"})
            print("已清空显示。")
            return
        if args.command == "quota":
            result = device.set_quota(args.remaining, args.reset_seconds, args.demo)
            source = "模拟数据" if args.demo else "Python 指定值"
            print(f"沙漏已启动：{source}，剩余 {result['remaining']}%，倒计时 {result['reset_seconds']} 秒。")
            print("状态已通过 USB 发送；程序退出后，屏幕继续本地动画。")
            return
        if args.command == "text":
            frame = render_text_frame(
                title=args.title,
                body=args.body,
                footer=args.footer,
                background=args.bg,
                foreground=args.fg,
                accent=args.accent,
                alignment=args.align,
            )
        elif args.command == "image":
            if not args.path.is_file():
                raise HoloCubicError(f"图片不存在：{args.path}")
            frame = render_image_frame(args.path, background=args.bg)
        else:
            raise HoloCubicError(f"未知命令：{args.command}")
        jpeg = encode_jpeg(frame)
        result = device.upload_jpeg(jpeg)
        print(
            f"已显示：{result['bytes']} bytes，"
            f"CRC32={result['crc32']:08X}"
        )


def main(argv: Optional[Sequence[str]] = None) -> int:
    """Parse arguments and return a shell-friendly process status."""
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.command == "ports":
            print_ports()
        else:
            run_device_command(args)
        return 0
    except (HoloCubicError, OSError, ValueError, serial.SerialException) as exc:
        print(f"错误：{exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
