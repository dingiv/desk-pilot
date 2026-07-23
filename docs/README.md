# desk-pilot 文档索引

> desk-pilot = **系统级 AI 秘书**的五子系统开发仓。北极星：[[ai-secretary-north-star]]。

## 子系统

| 子系统 | 目录 | 职责 | 现状 |
|---|---|---|---|
| **audio-aura**（语音） | `docs/aura/` | 语音助手前端 + 中间守护：录音 → 三阶段提交（ASR → 整流路由 → agent）→ socket | 脊柱立全，基建齐，真麦测试中 |
| **omni-scout**（视觉哨兵） | `docs/scout-server.md` `docs/som.md` | 屏幕 + 音频采集 daemon（PipeWire） | HTTP 服务生产版 |
| **visual-rover**（视觉漫游者） | `docs/design.md` `docs/decisions.md` | 视觉 GUI agent（SoM 标记 → LLM → 操作） | 设计阶段，部分落地 |
| **geek-familiar**（使魔引擎） | `docs/familiar/` | 桌面精灵悬浮窗：皮肤渲染 + 秘书 UI + agent 调度 | 渲染层验证通过；M1 接入 daemon 待做 |
| **ime**（输入法） | `docs/ime/` | 输入法增强引擎：snippets 展开 + 语音缓冲插入 + 动作触发；跨平台 ibus/TSF/IMK/TextExpander | 设计完成，待 Phase 1 `ime-core` |

## 阅读路径

1. 先读各子系统 architecture/README
2. 跨子系统决策在 `docs/decisions.md`（视觉 rover）
3. 语音细节近/中/长期路线在 `docs/aura/roadmap.md`
4. IME 完全设计在 `docs/ime/design.md`
5. 北极星：[[ai-secretary-north-star]]

## 其他

- `docs/research/` — agent / omniparser / 传统方法 研究笔记
- `docs/ui-tars/` — UI-TARS 平台算子与 agent 循环参考
