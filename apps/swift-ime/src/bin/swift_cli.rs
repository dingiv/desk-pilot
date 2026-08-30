//! swift_cli — swift-ime 调试 server 的命令行客户端。
//!
//! 向 server 的 Unix socket 发送按键,收回 JSON 预测视图 —— 多行调试核心
//! 功能(链式预测、魔法命令、语音候选…),与 TUI 看到同一引擎状态。
//!
//! ```bash
//! swift_cli ti'an                # 逐字符发串,回最终视图
//! swift_cli space                # 特殊键:space/enter/backspace/escape/up/down/…
//! swift_cli "1"                  # 数字选词
//! swift_cli ctrl:q               # Ctrl 组合(透传验证)
//! swift_cli view                 # 只查当前视图
//! swift_cli reset                # Escape 取消组合
//! swift_cli --all ti'an          # 每个键都回显一行视图
//! swift_cli --pretty shijian''#concat
//! ```
//!
//! 默认 socket:/tmp/swift-ime.sock(--sock 覆盖)。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use clap::Parser;
use swift_ime::constants::SOCK_PATH;

#[derive(Parser)]
#[command(name = "swift_cli", about = "swift-ime debug client: send keys, get views")]
struct Args {
    /// 逐项发送:特殊键名或任意字符串(逐字符)。空格分隔,顺序发送。
    tokens: Vec<String>,
    /// 每个字符单独发送并回显(默认只回串末的最终视图)。
    #[arg(long, default_value = "false")]
    all: bool,
    /// 美化 JSON 输出。
    #[arg(long, default_value = "false")]
    pretty: bool,
    /// server socket 路径。
    #[arg(long, default_value = SOCK_PATH)]
    sock: PathBuf,
    /// 交互模式(REPL):逐行输入命令,实时回视图;exit/quit 退出。
    #[arg(long, default_value = "false")]
    repl: bool,
}

fn send_and_print(stream: &mut UnixStream, cmd: &str, pretty: bool) -> std::io::Result<()> {
    writeln!(stream, "{cmd}")?;
    stream.flush()?;
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        eprintln!("server closed the connection");
        std::process::exit(2);
    }
    let resp = line.trim();
    if pretty {
        let v: serde_json::Value = serde_json::from_str(resp).unwrap_or(serde_json::Value::String(resp.into()));
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        println!("{resp}");
    }
    Ok(())
}

fn main() {
    let args = Args::parse();
    let mut stream = match UnixStream::connect(&args.sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect {} failed: {e}\n(server 起了吗?cargo run --bin swift-ime)", args.sock.display());
            std::process::exit(1);
        }
    };

    if args.repl {
        let stdin = std::io::stdin();
        let mut buf = String::new();
        eprintln!("swift_cli REPL — {} (exit/quit 退出)", args.sock.display());
        loop {
            eprint!("> ");
            std::io::stderr().flush().ok();
            buf.clear();
            if stdin.read_line(&mut buf).unwrap_or(0) == 0 || buf.trim().is_empty() {
                continue;
            }
            let cmd = buf.trim();
            if cmd == "exit" || cmd == "quit" {
                break;
            }
            let _ = send_and_print(&mut stream, cmd, args.pretty);
        }
        return;
    }

    for token in &args.tokens {
        if args.all && token.chars().count() > 1 {
            // --all:字符串拆成单字符逐个发送,每键回显。
            for ch in token.chars() {
                let _ = send_and_print(&mut stream, &ch.to_string(), args.pretty);
            }
        } else {
            let _ = send_and_print(&mut stream, token, args.pretty);
        }
    }
    if args.tokens.is_empty() {
        // 无参数 → 查当前视图。
        let _ = send_and_print(&mut stream, "view", args.pretty);
    }
}
