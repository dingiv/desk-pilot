# 传统 GUI 自动化技术

GUI 自动化的核心目标是代替人类完成图形界面上的重复操作。在 AI Agent 出现之前，GUI 自动化已经发展出较为成熟的技术体系。按照与目标程序的交互层次，可以分为以下几类。

## 键鼠模拟

最基础的自动化方式，通过模拟人类的键盘和鼠标操作来控制程序。

```text
脚本
 ↓
鼠标移动
鼠标点击
键盘输入
 ↓
目标程序
```

典型工具：

* AutoHotkey
* PyAutoGUI
* RobotJS
* xdotool（Linux）

优点：

* 通用性强
* 无需了解目标程序内部结构
* 可用于绝大多数 GUI 软件

缺点：

* 依赖屏幕布局
* 分辨率变化容易导致失效
* 难以处理复杂场景

---

## 图色脚本

通过截图获取屏幕内容，利用颜色匹配、模板匹配等计算机视觉方法识别界面元素，再执行对应操作。

```text
截图
 ↓
图像识别
 ↓
业务逻辑
 ↓
键鼠操作
```

典型技术：

* OpenCV
* 模板匹配（Template Matching）
* OCR
* 找色算法

优点：

* 不依赖程序接口
* 适用于游戏和自绘界面

缺点：

* 对界面变化敏感
* 维护成本较高

---

## 无障碍接口自动化

通过操作系统提供的 Accessibility API 获取界面结构信息。

```text
程序
 ↓
Accessibility Tree
 ↓
自动化脚本
```

常见平台：

* Windows UI Automation (UIA)
* Linux AT-SPI
* macOS Accessibility API

能够直接获取：

* 按钮
* 输入框
* 菜单
* 表格

优点：

* 稳定性高
* 不依赖视觉识别
* 能够直接调用控件行为

缺点：

* 依赖软件提供无障碍支持
* 对游戏和自绘界面效果较差

---

## 窗口消息自动化

直接向目标窗口发送消息，而不是模拟真实键鼠。

```text
脚本
 ↓
SendMessage
PostMessage
 ↓
目标窗口
```

Windows 常见技术：

* Win32 API
* SendMessage
* PostMessage

优点：

* 效率高
* 不影响用户操作

缺点：

* 与具体程序实现耦合
* 不同软件兼容性差异较大

---

## 浏览器 DOM 自动化

针对 Web 应用，通过操作 DOM 实现自动化。

```text
脚本
 ↓
DOM
 ↓
浏览器
```

典型工具：

* Selenium
* Playwright
* Puppeteer

优点：

* 稳定
* 精确
* 执行速度快

缺点：

* 仅适用于网页

---

## Hook 自动化

通过 Hook 技术拦截程序调用，实现自动化控制。

```text
程序
 ↓
Hook
 ↓
自动化逻辑
```

常见技术：

* API Hook
* DLL Injection
* Detours

优点：

* 控制能力强
* 可获取丰富上下文

缺点：

* 开发门槛高
* 容易被检测

---

## 内存自动化

直接读取或修改目标进程内存。

```text
脚本
 ↓
内存读写
 ↓
目标程序
```

常见技术：

* ReadProcessMemory
* WriteProcessMemory

扩展层次：

* 用户态
* 内核态
* Hypervisor
* DMA

优点：

* 性能最高
* 不依赖界面

缺点：

* 技术门槛高
* 兼容性差
* 存在法律和合规风险

---

# AI Agent 出现前的技术演进

大致可以理解为：

```text
键鼠模拟
    ↓
图色脚本
    ↓
无障碍接口
    ↓
DOM 自动化
    ↓
RPA
    ↓
GUI Agent
```

其中：

* Selenium、Playwright 代表 Web 自动化时代；
* UiPath、Power Automate 代表 RPA 时代；
* OmniParser、Computer Use、GUI Agent 代表当前 AI 驱动自动化时代。

传统自动化依赖规则和预设流程，而现代 GUI Agent 更强调视觉理解、任务规划和自主决策能力，能够在未知界面中完成复杂任务。
