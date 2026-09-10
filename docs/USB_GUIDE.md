> 本文保留设备安装、USB 协议与早期验收说明。历史“已安装”“本机实测”指原开发设备；新设备仍需安装 Lua 应用。文中的相对命令均从仓库根目录执行。

# HoloCubic USB 直连显示

桌面控制页见 [rs_holocubic](../rs_holocubic/README.md)：Rust/egui 多模式编辑、任务名称、时长、两位小数沙漏及暂停/继续/重置。

这套 demo 日常只使用 USB，不依赖 Wi-Fi 或互联网。图片模式由 Python 发送 `320×240 JPEG`；沙漏模式只发送额度状态，由设备本地绘图和播放动画。设备端已经通过电脑直连屏幕热点安装完成，无需取出 microSD。

## 首次安装设备端

当前设备已安装，本节仅供以后重新安装。首次安装可通过设备自身热点完成，也不需要设备连接互联网：

1. 在设备上启用 Wi-Fi，电脑连接本机实测热点 `clocteck_cubic`。
2. 打开 [DevTools 文件管理](http://192.168.18.1/devtools/)，建立 `/sd/apps/usb_display/`。
3. 把 `device_app/app.info`、`device_app/main.lua` 和 `device_app/quota_scene.lua` 上传到该目录。
4. 返回 Launcher，短按 `DOWN` 重扫，启动“USB Display / USB直连显示”。重扫方式见[项目说明](https://github.com/clocteck/holocubic-apps)。
5. 屏幕出现 `USB DISPLAY / Waiting for Python` 后，电脑可以断开热点，用 USB 发送画面。

以后开机只需启动该应用，不必重新安装。USB 端口不是 U 盘，不能直接拖入文件。

## Python 环境

```powershell
python -m pip install -r requirements.txt
python holo_usb_display.py ports
python holo_usb_display.py ping
```

本机实测设备为 `COM4` / `303A:1001`，程序按 VID/PID 自动选择，端口号变化不影响使用。

若设备正常亮屏但电脑找不到串口，检查 VMware 是否接管了 `USB JTAG/serial debug unit`。应将该设备连接到主机，而不是虚拟机；本次关闭占用它的虚拟机后，COM4 立即恢复。

## 显示文字

```powershell
python holo_usb_display.py text --title "HoloCubic" --body "USB直连成功" --footer "2026-09-05"
```

可选参数：`--bg #02070B`、`--fg #F4FBFF`、`--accent #35E7FF`、`--align left|center|right`。中文由 Windows 字体在电脑端渲染，不依赖设备字库。

## 显示图片

```powershell
python holo_usb_display.py image "C:\path\picture.png"
```

图片会等比缩放到 `320×240`，不裁剪。清屏：

```powershell
python holo_usb_display.py clear
```

## 沙漏动画 Demo

```powershell
python holo_usb_display.py quota --demo
```

发送一次状态后，Python 自动退出，设备继续本地动画，不逐帧传图、不反复写卡。界面用原生字体显示 `CODEX`、剩余百分比、`RESET IN` 倒计时和 `COUNT` 计数。

- `DEMO` / `SIMULATED DATA` 明确表示模拟数据：默认从 72% 开始，每秒减少 2 个百分点，归零后补满并循环；未接入真实 Codex 额度。
- `RESET IN` 是重置倒计时；`COUNT` 是本场景本地累计运行的秒数，不是请求次数或 token 数。
- 动画定时器间隔为 50ms；倒计时按设备实际经过的秒数计算，不按动画帧数扣减。

使用 Python 指定数值（不自动消耗额度，只运行流沙动效和倒计时）：

```powershell
python holo_usb_display.py quota --remaining 49 --reset-seconds 7200
```

剩余百分比范围为 `0～100`，倒计时范围为 `0～604800` 秒。后续再执行同一命令即可更新数值；`clear` 返回等待界面，`text` / `image` 自动停止沙漏动画并切换回图片模式。

## Python 代码调用

```python
from holo_usb_display import HoloCubicUSB, encode_jpeg, find_holocubic_port, render_text_frame

frame = render_text_frame("HoloCubic", "Python USB 直连")
with HoloCubicUSB(find_holocubic_port()) as device:
    device.wait_until_ready()
    device.upload_jpeg(encode_jpeg(frame))
```

直接更新沙漏：

```python
with HoloCubicUSB(find_holocubic_port()) as device:
    device.wait_until_ready()
    device.set_quota(remaining=49, reset_seconds=7200, demo=False)
```

程序在打开串口前将 DTR/RTS 设为未激活，不执行刷机、擦除或主动复位。串口协议使用 8 位十六进制字符串传递 CRC32，避免固件的 32 位浮点数损失校验精度；Python 对完成回执的状态、大小和 CRC32 再次核对。

设备从校验通过的 JPEG 字节加载每帧图像，避免覆盖固定文件名后仍显示缓存中的旧画面。

设备端 API 依据：[clocteck/holocubic-apps](https://github.com/clocteck/holocubic-apps)。

## 验证与变更

2026-09-05 | 0.1.0 | 完成热点免拆卡部署，修复 CRC32 有符号整数、浮点精度及图片路径缓存问题，验证断开 Wi-Fi 后的纯 USB 连续刷新。

- 真机：固件 `1.002`，屏幕 `320×240`；用户确认中文测试画面显示正常。
- 最终断开电脑 Wi-Fi 后连续发送三帧，分别为 `11753 bytes / D8BE7241`、`12606 bytes / 01C7BAC5`、`12938 bytes / 6A408955`；回执状态、大小和 CRC32 全部匹配，用户确认最后的“USB TEST 3 / 刷新成功 第三帧”显示正常。
- 自测：`python -m unittest -v test_holo_usb_display.py`，3 项通过，包含错误 CRC32 回执拒绝测试。

2026-09-06 | 新增设备本地沙漏 Demo、USB 状态更新、模拟额度循环及秒级倒计时；保留图片显示和清屏。

- 当前自测 5 项通过；包括单个状态包小于 160 字节、非法范围不发送、图片 CRC32 校验。
- 真机实测手动状态包为 95 字节；关闭 Python 串口约 3 秒后重新读取，动画帧计数从 `2481` 增至 `2546`，固定额度仍为 `49%`，倒计时从 `90` 降至 `86` 秒。
- `0% / 0秒`、`100% / 7天`、清屏、JPEG 显示、重新启动 Demo 均通过真机验证；验证时电脑 Wi-Fi 已断开。

2026-09-07 | 新增 rs_holocubic 桌面控制器；扩展 USB 任务指令、静态中文标签、精确百分位和本地计时控制，原有图片和额度演示入口保留。

2026-09-07 | 沙漏任务标题调整到底部居中、说明位于顶部；预览与设备标签一致，仅需 USB 更新。

2026-09-07 | 沙漏背景统一为纯黑；桌面使用设备实时状态及 CRC 校验标签同步画面，重连回读实际内容，不再播放独立草稿动画。

2026-09-07 | 桌面与设备改为共享任务参数、分别本地计时，取消周期查询；启动/暂停/继续/重置及手动检测时才进行 USB 通信和校准。

2026-09-08 | Rust 控制台新增主页轮播与平滑淡出淡入，静态素材预加载、沙漏跨页续计；USB 大回执按96字节分块回读并校验CRC，接收端保留半行数据；具体使用和拍击接口核查见 rs_holocubic/README.md。

2026-09-08 | 新增 CalendarTask 当日日历显示，识别完成删除线，只读源 SQLite；固定紧凑首页、不自动翻页，双缓冲槽原子更新，内容不变不重传。使用方法见 rs_holocubic/README.md。
