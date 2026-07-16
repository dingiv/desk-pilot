# SoM 感知模块（`@vrover/som`）

> SoM **感知子模块**。「Visual Scout」整体设计见 [design.md](./design.md)；本文档的 SoM 是它的**感知引擎**（建图时识别控件、走图失败时重新定位）。

`@vrover/som` 把一张截图和一组 UI 元素变成模型能用来定位的东西 ——
**带编号红框的标注图 + 编号→元素表**。模型不靠猜像素坐标，而是说「点编号 3」，工具执行层（`@vrover/tools`）再把编号解析成元素中心坐标。

## 数据流

```
Platform.captureScreen() ──┐
                           ├─▶ annotate() ──▶ SoMResult
Platform.getElements()  ───┘                      │
                                                  ├─ annotated: Screenshot (画了编号框的 PNG，发给模型的 image 块)
                                                  └─ table: SoMElement[]   (mark → 元素描述，作为文本表发给模型)
```

- `annotate(screenshot, elements)`：在每个元素上画红色编号框 + 左上角数字标签，并生成 `1: [button] Login` 这样的表。
- `formatTable(table)`：把表渲染成文本，随标注图一起发给 LLM。
- 工具执行器把模型返回的 `mark` → 元素 → `centerOf(bounds)` → `Platform.performClick(x, y)` 等原语。

## 类型

```ts
interface SoMElement { mark: number; element: UiElement; description: string } // 例如 "[button] Login"
interface SoMResult  { annotated: Screenshot; table: SoMElement[] }
```

`UiElement`(id/role/label/bounds) 与 `Screenshot`(width/height/png) 来自 `@vrover/platform`（`UiElement`/`Bounds` 的源头是 `@vrover/scout-protocol`）。

## 元素从哪来（grounding）

今天 `Platform.getElements()` 直接给出元素（Mock 给的是合成的登录框），背后是预留的扩展缝 `GroundingSource.detect(): Promise<UiElement[]>`。目标形态是**三档感知源，由便宜到贵逐档兜底，结果合并去重**后交给 `annotate`：① 无障碍树 / DOM 边界框 → ② 传统 CV + OCR → ③ ML 视觉检测（OmniParser 类，onnxruntime 本地推理）。

> 三档**输出形态一致**、**都跑在 Scout 本地**、**都不是多模态大模型**。逐档兜底 + 合并去重的完整论述见 [decisions.md](./decisions.md) D11。
>
> ⚠️ 这里的 **ML 视觉检测**是 Scout 本地的**小专用模型**（找控件框），**不是**「视觉模型服务」（VRover 大脑调用的多模态大模型）——两者都涉及「视觉」，但规模 / 角色完全不同。

## SoM 在「图优先」架构里的角色

随着 graph walker 落地，`annotate` 不再每步无条件跑：先识别当前界面是否为已知 node（L0 nativeId / L1 图像层 / L2 结构签名，详见 [decisions.md](./decisions.md) D1），命中即复用库存元素；未命中才走 L3 全量感知建新 node——那时才需要 `annotate` 给 LLM 看 mark。统一的「UI 描述」货币 `NodeProfile = { nativeId?, imageHash, structuralSig, elements }` 也由 Scout 感知定义并产出（D1）。
