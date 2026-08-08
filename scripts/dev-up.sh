#!/usr/bin/env bash
# dev-up.sh — 本地开发一键启动:omni-scout + aura-daemon + dp-models(模型服务)。
#
# 用法:
#   ./scripts/dev-up.sh [start|stop|status] [scout|aura|models|all]
#   ./scripts/dev-up.sh                  # 等价 start all
#   ./scripts/dev-up.sh start aura       # 只起 aura
#   ./scripts/dev-up.sh stop             # 全部停止
#
# 环境变量(可选):
#   SCOUT_MOCK=路径    scout 用 mock 模式(容器/无 PipeWire 环境必用;默认
#                      assets/models/testwavs/zh-standard-1.wav)
#   MODELS_PORT=8080   dp-models 监听端口
#   AURA_PORT=9091     aura-daemon 端口
#   SCOUT_PORT=7878    omni-scout 端口
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG_DIR="${LOG_DIR:-/tmp/desk-pilot-dev}"
PID_DIR="$LOG_DIR"
mkdir -p "$LOG_DIR"

SCOUT_PORT="${SCOUT_PORT:-7878}"
AURA_PORT="${AURA_PORT:-9091}"
MODELS_PORT="${MODELS_PORT:-8080}"
# --mock-audio 期望音频**目录**(循环播放);scout 自带 apps/omni-scout/assets/mock-audio。
SCOUT_MOCK="${SCOUT_MOCK:-$ROOT/apps/omni-scout/assets/mock-audio}"
MODELS_DIR="${MODELS_DIR:-$ROOT/apps/dp-models/models}"

pid_file() { echo "$PID_DIR/$1.pid"; }
log_file()  { echo "$LOG_DIR/$1.log"; }

is_running() {
    local pid
    [ -f "$(pid_file "$1")" ] && pid="$(cat "$(pid_file "$1")")" && kill -0 "$pid" 2>/dev/null
}

start_one() {
    local name="$1"
    if is_running "$name"; then
        echo "  $name 已在运行 (pid $(cat "$(pid_file "$name")"))"
        return
    fi
    case "$name" in
        scout)
            # 纯音频 mock(--mock-audio,容器/无 PipeWire 环境);真实环境
            # 去掉 SCOUT_MOCK 或改用 --mock <video>。
            echo "  scout  (omni-scout :$SCOUT_PORT, mock-audio=$SCOUT_MOCK)"
            (cd "$ROOT" && cargo run -p omni-scout -- --port "$SCOUT_PORT" --mock-audio "$SCOUT_MOCK" \
                >"$(log_file scout)" 2>&1 & echo $! >"$(pid_file scout)")
            ;;
        aura)
            # required-features = [asr, cuda];无 GPU 时 cuda 仅编译开关(运行走 CPU)。
            # scout_addr 是位置参数(不是 flag)。
            echo "  aura   (aura-daemon :$AURA_PORT → scout :$SCOUT_PORT)"
            (cd "$ROOT" && cargo run -p aura-daemon --features asr,cuda -- \
                --port "$AURA_PORT" "127.0.0.1:$SCOUT_PORT" \
                >"$(log_file aura)" 2>&1 & echo $! >"$(pid_file aura)")
            ;;
        models)
            echo "  models (dp-models LocalAI :$MODELS_PORT, models=$MODELS_DIR)"
            (cd "$ROOT/apps/dp-models" && go run . --models "$MODELS_DIR" --addr ":$MODELS_PORT" \
                >"$(log_file models)" 2>&1 & echo $! >"$(pid_file models)")
            ;;
        *) echo "  未知服务: $name (scout|aura|models)"; exit 1 ;;
    esac
}

stop_one() {
    local name="$1" pid
    if is_running "$name"; then
        pid="$(cat "$(pid_file "$name")")"
        kill "$pid" 2>/dev/null || true
        pkill -P "$pid" 2>/dev/null || true
    fi
    # cargo/go run 的子进程会继续存活——按命令特征兜底清理。
    case "$name" in
        scout)  pkill -f "target/debug/omni-scout --port" 2>/dev/null || true ;;
        aura)   pkill -f "target/debug/aura-daemon" 2>/dev/null || true ;;
        models) pkill -f "dp-models --models" 2>/dev/null || true ;;
    esac
    rm -f "$(pid_file "$name")"
    echo "  $name 已停止"
}

status_one() {
    local name="$1"
    if is_running "$name"; then
        echo "  $name  运行中 (pid $(cat "$(pid_file "$name")"), 日志 $(log_file "$name"))"
    else
        echo "  $name  已停止"
    fi
}

run_all() {
    local action="$1"
    for svc in scout aura models; do
        case "$action" in
            start) start_one "$svc" ;;
            stop)  stop_one "$svc" ;;
            status) status_one "$svc" ;;
        esac
    done
}

# ── 入口 ────────────────────────────────────────────────────────────────
action="${1:-start}"
target="${2:-all}"

case "$action" in
    start|stop|status) ;;
    *) echo "用法: $0 [start|stop|status] [scout|aura|models|all]"; exit 1 ;;
esac

echo "── dev-up: $action $target ──"
if [ "$target" = "all" ]; then
    run_all "$action"
else
    case "$action" in
        start)  start_one "$target" ;;
        stop)   stop_one "$target" ;;
        status) status_one "$target" ;;
    esac
fi

if [ "$action" = "start" ]; then
    echo ""
    echo "  探活:"
    [ "$target" = "all" ] || [ "$target" = "scout" ]  && echo "    scout  → http://127.0.0.1:$SCOUT_PORT"
    [ "$target" = "all" ] || [ "$target" = "aura" ]   && echo "    aura   → http://127.0.0.1:$AURA_PORT"
    [ "$target" = "all" ] || [ "$target" = "models" ] && echo "    models → http://127.0.0.1:$MODELS_PORT"
    echo "  日志: $LOG_DIR/*.log (tail -f 查看)"
fi
