# 社区方案

| agent               | 模型               | 平台           | 备注                                                  | 开发团队       |
| :------------------ | :----------------- | :------------- | :---------------------------------------------------- | :------------- |
| UI-TARS             | UI-TARS (7B/72B)   | all            | open，开源纯视觉 Grounding 像素定位基石               | 字节           |
| OpenAPA             | gemini+Doubao-seed | desktop        | open，SOTA，Agent S 架构延申                          | 来也           |
| Holo3               | Holo3 (35B-A3B)    | all            | open，SOTA，高性价比商用 MoE 架构                     | H.co           |
| Agent S             | UI-TARS 等         | all            | open，端侧多模态+像素定位对齐分离                     | Simular AI     |
| Finalrun            | Gemini             | mobile         | open，vision+a11y                                     | Finalrun AI    |
| vePhone             | Seed1.8-GUI        | mobile         | open+closed，MobileUseAgent                           | 火山引擎       |
| MobileUse           | Qwen2.5-VL         | mobile         | open，移动端 Computer Use 官方框架                    | 阿里通义       |
| MAI-UI              | MAI-UI (2B~235B)   | mobile         | open，Qwen底座，AndroidWorld 榜单前列                 | 阿里通义       |
| VLAA-GUI            | UI-TARS            | all            | open，Agent S 架构延申                                | UCSC           |
| CogAgent            | CogAgent           | desktop        | open，经典早期多模态高分辨率 UI 认知底座              | 智谱 AI        |
| *                   | Qwen2.5-VL         | all            | open，纯原生多模态底座，需要额外做 SFT/LoRA 注入      | 阿里通义       |
| *                   | OmniParser         | all            | open，SoM 模型                                        | 微软           |
| *                   | GUI-Owl            | all            | open，Qwen底座，早期达摩院视觉定位探索                | 阿里通义       |
| *                   | EvoCUA             | all            | open，Qwen底座，强化学习驱动                          | 美团           |
| *                   | UI-Venus           | mobile         | open，Qwen底座，10B tokens UI 预训练+TIES模型融合     | 蚂蚁集团       |
| *                   | OpenCUA            | all            | open，Qwen底座，包含 AgentNet 真实轨迹集与长思维链    | 香港大学 XLANG |
| *                   | AutoGLM-Mobile     | mobile         | open，端侧商业级手机代办落地框架                      | 智谱 AI        |
| *                   | DART-GUI           | all            | open，Qwen底座，引入动态搜索树与反思机制              | 港大 / 腾讯    |
| OS-Copilot          | *                  | desktop        | open，多执行器架构：使用 GUI/CLI 对等架构             | 上海交大       |
| Browser Use         | *                  | browser        | open，基于 Playwright 网页自动化天花板                | 开源社区       |
| Midscene.js         | *                  | browser/mobile | open，前端/移动端 UI 自动化测试                       | 字节           |
| DroidRun            | *                  | mobile         | open，安卓端动作导航框架                              | 腾讯           |
| AutoDevice          | Gemini             | mobile         | open，谷歌移动设备自动化探索，完成度不高              | Google         |
| Pointer             | claude opus        | desktop        | closed，SOTA，commercial                              | Pointer AI     |
| OpenAI Operator     | GPT 系列           | all            | closed，commercial，自主接管操作                      | OpenAI         |
| Claude Computer Use | claude sonnet      | all            | closed，commercial，基于 Xvfb 虚拟桌面原生像素直驱    | Anthropic      |
| AGI-0               | AGI-0              | mobile         | closed                                                | theagi         |
| K²-Agent            | Qwen2.5-VL *2      | mobile         | closed，主模型外挂记忆库(SRLR)+小模型单步强化(C-GRPO) | 阿里通义       |
| HIPPO               | *                  | desktop        | closed                                                | lenovo         |
| *                   | DeepMiner-Mano     | -              | closed                                                | 明略科技       |