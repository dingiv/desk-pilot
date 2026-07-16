# 草案

## UI DSL
UI DSL 的作用：
1. 作为一种 SoM 方案
2. 持久化高频使用 APP 的 UI 结构，介绍重复计算
3. 提供上层 UI 操作抽象，让 AI 模型可以调用
4. 提供可编程接口，让人类程序员也能够通过编写脚本的方式操作 UI
5. AI 反哺 DSL，让高级多模态大模型代替人类程序员编写 DSL 脚本

识别层：
将一个截图识别成一个 scout 的 UI DSL。

ML 视觉检测，在无法通过原生 API 拿到 DOM 或者 Accessibility Tree 的时候，通过本地视觉模型将截图解析成可交互元素的列表。

## 典型端到端链路

### 初见一个 App

```calltree
尝试解析当前界面为 graph map 中的某一个已知 node
    if 无法匹配:
        执行“初见 SoM” 解析，返回初见 UI SoM 


```


## 宏录制
由人类操作示范，AI 分析行为，生成脚本，最终持久化为连续自动化脚本，用于处理高度重复化的内容。


```
Screen Capture
        ↓

OmniParser
        ↓

Element List
        ↓

Grounding
        ↓

Planner
        ↓

Action


屏幕截图
      ↓

OmniParser
      ↓

[按钮]
[输入框]
[图标]
[菜单]

      ↓

SoM
      ↓

Grounding
      ↓

Planner
      ↓

Action
```


