//! JSON-RPC 2.0 over stdio 传输层（手写，零异步依赖）。
//!
//! 帧格式：`Content-Length: <n>\r\n\r\n{json}`。仅 stdout 承载协议消息，
//! 诊断/日志走 stderr，避免污染协议流。

use std::io::{self, Read, Write};

use serde_json::Value;

use crate::server::Server;

/// 启动 stdio 事件循环；进程被 LSP `exit` 请求终止，正常不返回。
pub fn run_stdio() -> io::Result<()> {
    let mut server = Server::new();
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    while let Some(body) = read_message(&mut reader)? {
        let msg: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => continue, // 坏请求：跳过（无可用的 id 构造 parse error）
        };
        if let Some(resp) = server.handle(&msg) {
            write_message(&mut out, &resp)?;
        }
        for note in server.drain_pending() {
            write_message(&mut out, &note)?;
        }
    }
    Ok(())
}

/// 读取一条消息体（含头部）。EOF 返回 None。
fn read_message<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = r.read(&mut byte)?;
            if n == 0 {
                return Ok(None); // 头部未读完即 EOF
            }
            if byte[0] == b'\n' {
                break;
            }
            if byte[0] != b'\r' {
                line.push(byte[0]);
            }
        }
        if line.is_empty() {
            break; // 空行：头部结束
        }
        let s = String::from_utf8_lossy(&line);
        if let Some(rest) = s.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = match content_length {
        Some(l) => l,
        None => return Ok(Some(Vec::new())),
    };
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_message<W: Write>(w: &mut W, value: &Value) -> io::Result<()> {
    let s = serde_json::to_string(value)?;
    write!(w, "Content-Length: {}\r\n\r\n{}", s.len(), s)?;
    w.flush()
}
