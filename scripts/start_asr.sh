#!/bin/bash

function main() {
    local aura_pid=0
    cargo aura &
    aura_pid=$!

    local scout_pid=0
    cargo scout &
    scout_pid=$!

    local mloader_pid=0
    uv run mloader-serve asr --model assets/models/qwen3-asr-1.7b-hf &
    mloader_pid=$!

    wait $aura_pid $scout_pid $mloader_pid
}


main