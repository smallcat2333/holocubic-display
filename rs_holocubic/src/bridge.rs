//! 后台 JSON 桥接：ports/status/hello 走进程内 Rust USB；其余动作仍复用 Python。

use serde_json::Value;
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// 分发一条 GUI 后台请求。`ports` / `status` / `ping` / `hello` 由 Rust USB 层处理。
pub fn call(request: Value) -> Result<Value, String> {
    match request.get("action").and_then(Value::as_str) {
        Some("ports") => crate::usb::list_ports_result().map_err(|e| e.0),
        Some("status") => {
            let port = request.get("port").and_then(Value::as_str);
            let labels_crc = request.get("labels_crc").and_then(Value::as_str);
            let radar_crc = request.get("radar_crc").and_then(Value::as_str);
            let calendar_signature = request.get("calendar_signature").and_then(Value::as_str);
            let home_crcs = request.get("home_crcs");
            crate::usb::status_result_with_hints(
                port,
                labels_crc,
                radar_crc,
                calendar_signature,
                home_crcs,
            )
            .map_err(|e| e.0)
        }
        Some("ping") | Some("hello") => {
            let port = request.get("port").and_then(Value::as_str);
            crate::usb::ping_result(port).map_err(|e| e.0)
        }
        _ => call_python(request),
    }
}

/// 子进程只接收 JSON 标准输入，不经过 shell；超时后终止并回收进程。
fn call_python(request: Value) -> Result<Value, String> {
    // 发布包把 Python 脚本放在 EXE 旁；cargo run 使用源码目录中的同一套脚本。
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let bundled_script = executable.parent().unwrap().join("bridge.py");
    let script = if bundled_script.is_file() {
        bundled_script
    } else {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("bridge.py")
    };
    let mut command = Command::new("python");
    command
        .args(["-X", "utf8"])
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("无法启动 Python：{e}"))?;
    let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    let write_result = child.stdin.take().unwrap().write_all(&bytes);
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("发送后台请求失败：{error}"));
    }
    // 标签回读可超过 Windows 管道缓冲区；并行排空输出，避免子进程写满后卡死。
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let output_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let error_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let timeout = if request["action"] == "browse" {
        600
    } else {
        90
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(timeout) => {
                thread::sleep(Duration::from_millis(20))
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = output_reader.join();
                let _ = error_reader.join();
                return Err(match result {
                    Err(e) => e.to_string(),
                    _ => "后台操作超时，串口已释放".into(),
                });
            }
        }
    }
    let stdout = output_reader
        .join()
        .map_err(|_| "后台输出线程异常")?
        .map_err(|e| e.to_string())?;
    let stderr = error_reader
        .join()
        .map_err(|_| "后台错误线程异常")?
        .map_err(|e| e.to_string())?;
    let reply: Value = serde_json::from_slice(&stdout)
        .map_err(|e| format!("后台响应错误：{e} {}", String::from_utf8_lossy(&stderr)))?;
    if reply["ok"] != true {
        return Err(reply["error"]
            .as_str()
            .unwrap_or("后台执行失败")
            .to_owned());
    }
    Ok(reply["result"].clone())
}
