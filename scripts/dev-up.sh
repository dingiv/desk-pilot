#!/usr/bin/env bash
# dev-up.sh — 本地开发一键启动:omni-scout + dp-router(LLM + ASR 统一)+ aura-daemon。
#
# 用法:
#   ./scripts/dev-up.sh [start|stop|status] [scout|router|aura|all]
#   ./scripts/dev-up.sh                  # 等价 start all
#   ./scripts/dev-up.sh start aura       # 只起 aura
#   ./scripts/dev-up.sh stop             # 全部停止
#
# 环境变量(可选):
#   SCOUT_MOCK=目录        可选:启用纯音频 mock(--mock-audio,容器/无 PipeWire
#                          环境用);默认不设 = 真实麦克风
#   SCOUT_PORT=7878        omni-scout 端口
#   ROUTER_PORT=8080       dp-router 对外端口(LLM + ASR 统一;被 aura Stage1 batch + Stage2 调用)
#   ROUTER_CONFIG=路径     dp-router 配置覆盖(默认 apps/dp-router/dp-router.yaml)
#   AURA_PORT=9091         aura-daemon 端口
#
# 注:本地 LLM + ASR 一律由 dp-router 接管(见 docs/dp-router-migration.md):
#   - LLM → llama-server spawn(--model <gguf>)
#   - ASR → llama-server spawn(--model <gguf> --mmproj <mmproj>),llama.cpp 原生暴露
#           /v1/audio/transcriptions(OpenAI 兼容),aura HttpAsr 直透即可。
# aura 的 Stage1 流式 ASR 仍在 aura 进程内(sherpa-onnx,dp-models::onnx),与 dp-router 无关。
# 启动顺序:scout / router 并行起,aura 最后起;谁先就绪谁先用。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG_DIR="${LOG_DIR:-/tmp/desk-pilot-dev}"
PID_DIR="$LOG_DIR"
mkdir -p "$LOG_DIR"

SCOUT_PORT="${SCOUT_PORT:-7878}"
ROUTER_PORT="${ROUTER_PORT:-8080}"
ROUTER_CONFIG="${ROUTER_CONFIG:-}"
AURA_PORT="${AURA_PORT:-9091}"
# --mock-audio 期望音频**目录**(循环播放);默认不设 = 真实麦克风。
SCOUT_MOCK="${SCOUT_MOCK:-}"


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
            # 默认真实麦克风。容器/无 PipeWire 环境才显式 SCOUT_MOCK=<目录> 启用
            # 纯音频 mock(--mock-audio 循环播放)。
            echo "  scout  (omni-scout :$SCOUT_PORT, 真实麦克风)"
            local scout_cmd=(cargo run -p omni-scout -- --port "$SCOUT_PORT")
            [ -n "${SCOUT_MOCK:-}" ] && scout_cmd+=(--mock-audio "$SCOUT_MOCK")
            (cd "$ROOT" && "${scout_cmd[@]}"\
                >"$(log_file scout)" 2>&1 & echo $! >"$(pid_file scout)")
            ;;
        router)
            # dp-router:启 axum 服务(:$ROUTER_PORT),自动 spawn yaml 中所有本地
            # llama-server 子进程。配置走 shared::loader!(CONF::dp-router.yaml)
            # (dev 默认 apps/dp-router/dp-router.yaml),也可 --config 覆盖。
            echo "  router (dp-router 127.0.0.1:$ROUTER_PORT → llama-server 子进程池)"
            local cmd=(cargo run -p dp-router -- --addr "127.0.0.1:$ROUTER_PORT")
            [ -n "$ROUTER_CONFIG" ] && cmd+=(--config "$ROUTER_CONFIG")
            (cd "$ROOT" && "${cmd[@]}" \
                >"$(log_file router)" 2>&1 & echo $! >"$(pid_file router)")
            ;;
        aura)
            # scout_addr 是位置参数(不是 flag)。
            # Stage1 batch ASR + Stage2 LLM 一律走 dp-router(:$ROUTER_PORT)。
            # 流式 ASR 仍在 aura 进程内(sherpa-onnx,不需要外部依赖)。
            # 不在这里等 router 就绪——aura 起来后,Stage1 / Stage2 第一次
            # 调用若 model 未在线,SDK 走 dp-router 的 POST /admin/models/load
            # 动态拉起(load 是异步的,SDK 轮询 /admin/models 直到 online 再发请求)。
            echo "  aura   (aura-daemon :$AURA_PORT → scout :$SCOUT_PORT / router :$ROUTER_PORT [LLM + ASR])"
            (cd "$ROOT" && cargo run -p aura-daemon -- \
                --port "$AURA_PORT" "127.0.0.1:$SCOUT_PORT" \
                >"$(log_file aura)" 2>&1 & echo $! >"$(pid_file aura)")
            ;;
        *) echo "  未知服务: $name (scout|router|aura)"; exit 1 ;;
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
        router) pkill -f "target/debug/dp-router" 2>/dev/null || true
                # dp-router 启的 llama-server 子进程随父进程退出,但保险起见再清一次
                pkill -f "llama-server" 2>/dev/null || true ;;
        aura)   pkill -f "target/debug/aura-daemon" 2>/dev/null || true ;;
    esac
    rm -f "$(pid_file "$name")"
    echo "  $name 已停止"
}

# (无 wait_for / 同步机制 —— SDK 通过 dp-router 的 POST /admin/models/load
# 动态拉起模型;谁先就绪谁先用)

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
    # 启动顺序:scout / router 互不依赖 → aura 依赖前二者(LLM + ASR 都走 router)
    for svc in scout router aura; do
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
    *) echo "用法: $0 [start|stop|status] [scout|router|aura|all]"; exit 1 ;;
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
    if [ "$target" = "all" ] || [ "$target" = "scout" ]; then
        echo "    scout  → http://127.0.0.1:$SCOUT_PORT"
    fi
    if [ "$target" = "all" ] || [ "$target" = "router" ]; then
        echo "    router → http://127.0.0.1:$ROUTER_PORT/health / /admin/models / /v1/chat/completions / /v1/audio/transcriptions"
    fi
    if [ "$target" = "all" ] || [ "$target" = "aura" ]; then
        echo "    aura   → http://127.0.0.1:$AURA_PORT"
    fi
    echo "  日志: $LOG_DIR/*.log (tail -f 查看)"
fi
