# OmniParser 代码精读（as-built）

> 源码位置：`/workspaces/gui_agent/OmniParser`（microsoft/OmniParser，分支 master）。
> 本文以**代码为准**，记录仓库实际实现，并对照本项目（VRover）的 `Platform` / `GroundingSource` / SoM 设计给出对接映射。
> 与 [index.md](./index.md) 中「OmniParser 范式」一节的区别：那里是概念性综述，这里是**逐文件的工程级精读**。

OmniParser 是微软的「纯视觉 GUI 解析器」：输入一张界面截图，输出**带编号标注（Set-of-Mark）的图 + 一个结构化元素列表**。它的核心价值是把「让 VLM 直接回归像素坐标」这件不可靠的事，换成「让模型选一个编号」，从而把定位精度交给真实的检测框。这套思路与 VRover 的 SoM 设计**理念完全一致**——事实上本项目 `GroundingSource` 的类型注释里就明写着 "ML vision detection (OmniParser-style)"。

仓库分两层：

- **解析核心**（`util/` + `omnitool/omniparserserver/`）：CV/OCR/小模型流水线，截图进 → 元素出。
- **OmniTool**（`omnitool/`）：把解析器包成一个「控制 Windows 11 虚拟机」的完整 agent：解析服务 + Docker 化 Win11 VM（OmniBox）+ Gradio UI/大脑/执行器。

---

## 一、仓库结构

```
OmniParser/
├── util/
│   ├── omniparser.py        # Omniparser 类：parse(image_b64) → (som_img, content_list)
│   ├── utils.py             # 【核心】解析流水线：YOLO + OCR + caption + 去重 + 标注
│   └── box_annotator.py     # 基于 supervision 的编号框绘制器（SoM 出图）
├── omnitool/
│   ├── omniparserserver/
│   │   └── omniparserserver.py  # FastAPI：POST /parse/、GET /probe/
│   ├── gradio/              # 大脑 + 执行 + 工具 + UI
│   │   ├── loop.py                          # sampling_loop_sync：主循环（生成器）
│   │   ├── agent/
│   │   │   ├── anthropic_agent.py           # AnthropicActor：Claude Computer Use 原生路径
│   │   │   ├── vlm_agent.py                 # VLMAgent：GPT-4o/o1/o3/R1/Qwen + OmniParser
│   │   │   └── vlm_agent_with_orchestrator.py # 带 plan + 进度账本（轨迹记录）
│   │   ├── executor/anthropic_executor.py   # 把 tool_use 块跑成 tool_result
│   │   ├── tools/
│   │   │   ├── computer.py      # ComputerTool：Anthropic computer-use 动作面（远程到 VM）
│   │   │   ├── screen_capture.py # get_screenshot：向 VM :5000 要截图
│   │   │   └── base.py          # ToolResult / ToolError 抽象
│   │   └── agent/llm_utils/
│   │       ├── omniparserclient.py  # OmniParserClient：截图→POST /parse/→重排成 screen_info
│   │       ├── oaiclient.py / groqclient.py  # OpenAI / Groq / Qwen(兼容OpenAI) 调用
│   ├── omnibox/             # Docker 化 Win11 VM（QEMU + KVM）
│   │   ├── compose.yml                  # privileged + /dev/kvm，NoVNC :8006
│   │   ├── vm/win11setup/setupscripts/
│   │   │   └── server/main.py           # VM 内 Flask :5000：/execute、/screenshot、/probe
│   │   └── scripts/manage_vm.sh         # create/start/stop/delete
│   └── readme.md
├── eval/                    # ScreenSpot Pro 评测（V2 达 39.5%）
├── gradio_demo.py           # 单机解析 demo（不含 agent）
└── requirements.txt
```

---

## 二、解析核心：截图进，元素出

入口类 `Omniparser`（`util/omniparser.py`）很薄，真正的流水线在 `util/utils.py:get_som_labeled_img()`。一张图经过五道工序：

```text
PIL 截图
   │
   ├─① predict_yolo()           # YOLOv8 微调模型 → 交互图标框（icon，interactivity=True）
   ├─② check_ocr_box()          # EasyOCR / PaddleOCR → 文本框（text，interactivity=False）
   │
   ├─③ remove_overlap_new()     # IoU 去重：OCR-in-icon→把文字并入图标；icon-in-OCR→丢图标
   │
   ├─④ get_parsed_content_icon()# 对「无文字的图标」裁剪→64×64→Florence-2/BLIP2 批量生成语义描述
   │
   └─⑤ annotate() via BoxAnnotator  # supervision 画带编号的框 → SoM 图
                                   ↓
        返回：(base64 SoM 图, label_coordinates, filtered_boxes_elem[])
```

**元素的数据结构**（贯穿全流程的中间表示）：

```python
{'type': 'icon'|'text',
 'bbox': [x1,y1,x2,y2],     # 归一化到 [0,1]（ratio）
 'interactivity': bool,
 'content': str|None,        # icon 初始为 None，由 caption 模型填；text 直接是 OCR 文本
 'source': 'box_yolo_content_yolo'|'box_yolo_content_ocr'|'box_ocr_content_ocr'}
```

几个值得注意的工程细节：

- **两路检测互补**：YOLO 负责找「能点的图标」，OCR 负责找「文字」。两者框会有重叠，靠 `remove_overlap_new` 合并——文字落在图标里就把文字作为该图标的 `content`，图标整个落在文字里就丢弃图标（文字粒度更细）。
- **图标语义补全只针对「无文字」的框**：先排序把 `content is None` 的挪到队尾、记下 `starting_idx`，再只对这些裁剪送进 caption 模型（Florence-2 `<CAPTION>`，批大小 128，约 4GB 显存）。这是个明显的省算力设计。
- **bbox 用归一化 ratio 传输**，到需要画图/点击时再乘以屏幕宽高。这与 VRover 的 `Bounds`（绝对像素 + width/height）需要一层转换。
- **BOX_TRESHOLD 默认 0.01**（服务端 CLI 默认 0.05）：检测阈值压得很低，宁可多框再靠 IoU 去重。
- 出图文字大小随分辨率自适应：`box_overlay_ratio = max(image.size) / 3200`。

---

## 三、解析服务：FastAPI `/parse/`

`omnitool/omniparserserver/omniparserserver.py` 把上面的核心包成一个无状态 HTTP 服务：

```python
POST /parse/   { "base64_image": "..." }
  → { "som_image_base64": "...", "parsed_content_list": [...], "latency": float }
GET /probe/    → { "message": "Omniparser API ready" }
```

启动参数（CLI）：`--som_model_path`（YOLO 权重）、`--caption_model_name florence2`、`--caption_model_path`、`--device cuda|cpu`、`--BOX_TRESHOLD`、`--host`（默认 127.0.0.1）、`--port`（默认 8000）。权重布局（V2）：`weights/icon_detect/{model.pt,...}` + `weights/icon_caption_florence/`。license：`icon_detect` 是 **AGPL**（继承自 YOLO），caption 模型 **MIT**。

> 设计含义：解析器是**重计算、有状态（加载一次模型常驻）**的进程，应跑在 GPU 机上，与 agent/执行层解耦。这正是 OmniTool 把它单独拆成一个服务的原因，也是 VRover 将来对接时该遵循的边界——解析是一个**远程副作用服务**，不是内联函数。

---

## 四、OmniTool 三组件

`omnitool/readme.md` 定义了三个独立进程：

| 组件 | 角色 | 跑在哪 |
|------|------|--------|
| **omniparserserver** | OmniParser V2 解析服务 | GPU 机（CPU 也能跑，慢） |
| **omnibox** | Docker 里的 Windows 11 VM（QEMU+KVM） | CPU 机，依赖 KVM（仅 Win/Linux） |
| **gradio** | UI + 大脑（选模型）+ 执行器，驱动 omnibox | CPU 机，建议与 omnibox 同机 |

```text
┌────────────┐  截图/执行   ┌──────────────────┐
│  gradio    │ ───────────▶ │ omnibox (Win11)  │  Flask :5000
│ (大脑+执行) │ ◀─────────── │  QEMU in Docker  │  /execute /screenshot /probe
└─────┬──────┘              └──────────────────┘  NoVNC :8006
      │ POST /parse/ {base64_image}
      ▼
┌──────────────────┐
│ omniparserserver │  YOLO + OCR + Florence-2
│   FastAPI :8000   │  → SoM 图 + parsed_content_list
└──────────────────┘
```

**OmniBox**（`omnibox/compose.yml`）：`privileged: true` + `/dev/kvm` + `/dev/net/tun`，`RAM_SIZE=8G / CPU_CORES=4 / DISK_SIZE=20G`。`manage_vm.sh create` 用 Win11 Enterprise 评测 ISO 首次装机（20–90 分钟，自动装一堆 app），之后 `start/stop/delete` 管理。VM 内监听 `10.0.2.15:5000`（QEMU 默认客户机地址）。

---

## 五、大脑与执行（`omnitool/gradio/`）

### 5.1 主循环 `sampling_loop_sync`（`loop.py`）

一个同步生成器，按模型分两条路径，**每一步都先解析屏幕**：

```python
omniparser_client = OmniParserClient(url=f"http://{omniparser_url}/parse/")
while True:
    parsed_screen = omniparser_client()        # 截图 + 解析 + 重排成 screen_info 文本
    if model == "claude-3-5-sonnet-20241022":
        # 把 screen_info 作为「辅助无障碍信息」塞进 user 消息，交给 Claude Computer Use
        messages.append({"role":"user","content":[TextBlock(screen_info)]})
        resp = actor(messages=messages)
    else:  # omniparser + gpt-4o / o1 / o3-mini / R1 / qwen2.5vl
        resp, plan = actor(messages=messages, parsed_screen=parsed_screen)
    for msg, tool_result in executor(resp, messages): yield msg
    if not tool_result: return messages
```

### 5.2 两条大脑路径

**AnthropicActor**（`anthropic_agent.py`）— Claude Computer Use 原生路径：用 `betas=["computer-use-2024-10-22"]` + `ToolCollection(ComputerTool)` 调 Claude。OmniParser 的 `screen_info`（元素编号表）只是**喂给模型的文字提示**；定位仍由 Claude 通过 computer 工具自己回归坐标完成。OmniParser 在这里起「放大可点区域信息」的辅助作用。

**VLMAgent**（`vlm_agent.py`）— 非 Anthropic 模型路径，也是 OmniParser 真正发挥威力的地方：

1. 模型名映射到真实 id：`"omniparser + gpt-4o" → "gpt-4o-2024-11-20"`、`"omniparser + R1" → "deepseek-r1-distill-llama-70b"`、`"omniparser + qwen2.5vl" → "qwen2.5-vl-72b-instruct"` 等。
2. 系统提示里塞入**编号清单**（`ID: 0, Text: ...` / `ID: 3, Icon: search box`），并约束模型只输出一个 JSON：`{Reasoning, Next Action, Box ID, value?}`。可用动作：`type/left_click/right_click/double_click/hover/scroll_up/scroll_down/wait/None`。
3. 同时把**原图 + SoM 图**两张作为图像证据发给模型。
4. 模型返回 JSON 后，VLMAgent **把它翻译成 Anthropic 的 tool_use 块**：若含 `Box ID`，查 `parsed_content_list[Box ID].bbox`，算中心点 `centroid = ((x1+x2)/2*w, (y1+y2)/2*h)`，并在 SoM 图上画红圈；再构造一个 `mouse_move` 到 centroid 的 tool_use，外加 `Next Action` 对应的 tool_use。
5. 最终包成一个**合成的 `BetaMessage`**（`stop_reason='tool_use'`）。

> **关键设计**：VLM 本身不说 Anthropic 工具协议。VLMAgent 是个**适配器**，把「编号制 JSON」翻译成 Anthropic `tool_use` 块，从而**复用同一个 `AnthropicExecutor`** 执行。这跟 VRover「模型只出 mark，执行器把 mark 解析成坐标再调 Platform 原语」是同一个套路。

带编排的版本 `VLMOrchestratedAgent` 额外做了：首步生成 `plan.json`、每步更新「Task Progress Ledger」（`is_request_satisfied / is_in_loop / is_progress_being_made / instruction_or_question`），并把每步的截图 + SoM 图 + 响应 + 账本追加到 `trajectory.json`——这就是 README News 里说的「local logging of trajectory」，用来**造训练数据**。

### 5.3 执行器与动作面

**AnthropicExecutor**（`executor/anthropic_executor.py`）：遍历响应里的 `tool_use` 块，`asyncio.run(tool_collection.run(...))` 执行，把结果封成 `tool_result`（text/image）回灌消息，并 `yield` 给 Gradio 展示。

**ComputerTool**（`tools/computer.py`）实现了 Anthropic computer-use 的完整动作面（`key/type/mouse_move/left_click/right_click/double_click/middle_click/left_click_drag/scroll/cursor_position/screenshot/hover/wait`）。但**执行不在本地**——`send_to_vm()` 把每条动作拼成

```python
["python","-c","import pyautogui; pyautogui.FAILSAFE=False; pyautogui.moveTo(x,y)"]
```

`POST http://localhost:5000/execute` 发给 VM。截图走 `get_screenshot()`（`tools/screen_capture.py`）→ `GET http://localhost:5000/screenshot`。还有一套 `scale_coordinates`（目标分辨率 XGA/WXGA/FWXGA）适配 Anthropic computer-use 的坐标约定。

**VM 内的服务**（`omnibox/vm/win11setup/setupscripts/server/main.py`）：Flask，三个端点：

- `/execute`（POST）：默认实现 `execute_anything` 直接 `subprocess.run(command)`——**任意命令执行**（仅 `computer_control_lock` 串行化）。代码里另留了一个安全的 `execute_impl` 桩（"Not implemented"）可切换。这是整个仓库最敏感的点，近期几个 commit（`0.0.0.0→127.0.0.1`、`sec_updates`）都在收口它。
- `/screenshot`（GET）：`pyautogui.screenshot()` 后把鼠标光标 PNG 贴上去再返回。
- `/probe`（GET）：健康检查。

---

## 六、关键设计点小结

1. **编号制 = 把坐标回归问题降维成选择题**。OmniParser（VLM 路径）和 VRover 用的是同一哲学：模型选 Box ID / mark，定位交给真实框。差别只在 Anthropic 原生路径仍让 Claude 自己出坐标。
2. **解析是远程重计算服务**，与大脑/执行物理分离，靠 base64 图 + JSON 元素列表通信。
3. **适配器模式复用执行器**：VLMAgent 把异构模型的输出统一翻译成 Anthropic `tool_use`，省得每个模型写一套执行逻辑。
4. **两路检测 + IoU 去重**（YOLO 图标 + OCR 文字）比单路更稳，文字优先级高于图标语义猜测。
5. **动作通过 VM 内 HTTP 执行**，工具进程本身不碰 OS——这让 agent 跑在 CPU 机、目标跑在另一台机/容器里。
6. **安全债**：VM 的 `/execute` 默认是任意命令执行，绑定地址从 `0.0.0.0` 改到 `127.0.0.1` 是近期主要的安全修复。

---

## 七、对照 VRover：OmniParser 正好落在 `GroundingSource` 缝上

本项目 `packages/platform/src/types.ts` 里 `GroundingSource` 的注释明确写着：

> ML vision detection (OmniParser-style, onnxruntime) — catches custom-drawn controls the accessibility tree misses.

也就是说，**OmniParser 就是 VRover 预留给未来的那条「ML 视觉」元素来源**。逐点对照：

| 维度 | OmniParser（as-built） | VRover（现状/设计） |
|------|------------------------|----------------------|
| 元素来源接口 | `parse(image) → parsed_content_list` | `GroundingSource.detect() → UiElement[]`（未接线，今天 `Platform.getElements` 顶替） |
| 元素结构 | `{type, bbox:[x1,y1,x2,y2] ratio, interactivity, content}` | `UiElement` + `Bounds{x,y,w,h}`（绝对像素，来自 `@vrover/scout-protocol`） |
| 模型引用元素的方式 | **Box ID**（整数下标） | **mark 编号** |
| 编号 → 坐标解析 | `parsed_content_list[id].bbox` 中心 × 屏幕尺寸 | 工具执行器：mark → 元素 → `centerOf(bounds)` |
| SoM 出图 | `BoxAnnotator`（supervision） | `@vrover/som` |
| 截图来源 | OmniParserClient 自己向 VM `:5000/screenshot` 要 | `Platform.captureScreen()` |
| 动作面 | Anthropic computer-use（key/click/scroll/…），远程到 VM | `Action` 联合类型（click/type/scroll/keypress/done/wait），mark 导向 |
| 大脑-执行边界 | VLMAgent 把 JSON 适配成 `tool_use`，复用执行器 | 模型出 mark，执行器解析 mark 调 `Platform` 原语 |

**最小对接方案**（供将来实现 `GroundingSource` 时参考）：写一个 `OmniParserGroundingSource`，内部持有一个指向 `omniparserserver` 的 HTTP client（几乎照搬 `OmniParserClient` 的 POST `/parse/`），但**不自己截图**——接收 `Platform.captureScreen()` 产出的 PNG，base64 后发去解析，再把 `parsed_content_list` 转成 `UiElement[]`：

- `bbox:[x1,y1,x2,y2] (ratio)` → `Bounds{x: x1*W, y: y1*H, width:(x2-x1)*W, height:(y2-y1)*H}`；
- `content` → 元素 label（喂给 SoM 标注与 LLM）；
- `interactivity` 可作为元素属性（VRover 目前 SoM 不区分交互性，这是个可吸收的增强点）。

这样 VRover 的 observe→think→act 循环完全不用改：`Platform` 仍负责截屏与点击，OmniParser 只接管 `getElements()` 的「看见」环节。落地形态有两条路：

- **远程服务**（推荐，贴合现状）：scout/agent 进程通过 HTTP 调一个独立 `omniparserserver`，正如 OmniTool 的部署。优点是 GPU 隔离、Python 生态（YOLO/OCR/torch）不必进 TS/Rust 栈。
- **本地嵌入**：注释里设想的 onnxruntime 路线——把 YOLO + caption 模型导出成 onnx，塞进 `DesktopPlatform` 的 Rust 缝（`NativeLayer`）或一个 Node 原生模块。工程量大，但延迟更低、无外部进程。

**取舍提醒**：OmniParser 的解析延迟（README 称 V2 比 V1 快 60%，但 Florence-2 caption 仍是瓶颈）会直接加到每个 observe 步上；VRover 若同时有 AT-SPI/DOM 这条「无 ML」来源（`GroundingSource` 注释里规划的前半部分），应**优先用结构化来源**、OmniParser 仅作自绘控件的兜底——这正好对应 OmniTool 里 Claude 路径把 `screen_info` 当「辅助」而非唯一依据的做法。

---

## 附：跑起来（参考 `omnitool/readme.md`）

```bash
# 1) 解析服务（GPU 机）
conda create -n omni python==3.12 && conda activate omni
pip install -r requirements.txt
# 下 V2 权重到 weights/，caption 目录改名 icon_caption_florence
cd omnitool/omniparserserver && python -m omniparserserver   # :8000

# 2) Win11 VM（CPU 机，需 KVM + 30GB 空间）
cd omnitool/omnibox/scripts && ./manage_vm.sh create          # 首次装机 20–90min

# 3) Gradio UI + 大脑
cd omnitool/gradio && python app.py \
  --windows_host_url localhost:8006 \
  --omniparser_server_url localhost:8000
```

评测：`eval/ss_pro_gpt4o_omniv2.py` 是 ScreenSpot Pro 的 drop-in 推理脚本，V2 在该基准达 **39.5%**（2025/1 的 SOTA）。
