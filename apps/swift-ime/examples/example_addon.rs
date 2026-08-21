//! Example addon server — 魔法命令插件(`magic.addons`)的演示后端。
//!
//! 运行:
//! ```bash
//! cargo run -p swift-ime --example example_addon
//! ```
//!
//! 监听 `127.0.0.1:9788`(与 `swift-ime.yaml` 里 `example_addon` 的 url 一致)。
//! 任意 GET 都返回 JSON 候选列表,并把请求的 path+query 回显成一条候选 ——
//! 方便验证 URL 映射:`#eg/name?nickname=1` → `GET /eg/name?nickname=1`。
//!
//! 用最小 `std::net::TcpListener` 实现,零额外依赖。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

fn main() {
    let addr = "127.0.0.1:9788";
    let listener = TcpListener::bind(addr).expect("bind 127.0.0.1:9788");
    eprintln!("[example_addon] listening on http://{addr}");
    eprintln!("[example_addon] 在 swift-ime 里输入 #eg/name?nickname=1 试试;Ctrl+C 退出");
    for conn in listener.incoming() {
        match conn {
            Ok(mut stream) => handle(&mut stream),
            Err(e) => eprintln!("[example_addon] accept error: {e}"),
        }
    }
}

fn handle(stream: &mut TcpStream) {
    let mut buf = [0u8; 16384];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let req_line = req.lines().next().unwrap_or("");
    // POST body 在空行之后。
    let req_body = req.split("\r\n\r\n").nth(1).unwrap_or("").trim();
    eprintln!("[example_addon] {req_line}");
    if !req_body.is_empty() {
        eprintln!("[example_addon]   body: {req_body}");
    }

    // 回显请求信息 + 一条交互候选(选中后 swift-ime 会带 pick 字段重发)。
    let body = serde_json::json!({
        "candidates": [
            { "text": req_line, "interactive": false, "commit_value": format!("你请求了: {req_line}") },
            { "text": "再预测一下", "interactive": true }
        ]
    })
    .to_string();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}
