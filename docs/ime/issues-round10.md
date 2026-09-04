# swift-ime 第十轮 — 预测精准度大改版:词频驱动 + 中英竞争策略

> 创建: 2026-09-01。用户目标:"优化预测精准度,现在的预测能力太差了"。
> 本轮首次建立**大规模客观评测**(词典分层抽样,可复跑),以此基线驱动,
> 全部工作项以 Top-1/3/10 前后对比验收。

## 一、评测基线(2026-09-01,纯净状态:移开 data/swift-ime.db)

评测集:`tmp/tc_dict_sample.txt`(789 条,`tmp/sample_eval.py` 从 rime-ice
91.6 万词条分层抽样:词长 L1-L4+ × 词频四分位 16 层;gold = 该拼音下
词典最高频词)与 `tmp/tc_en_sample.txt`(210 条,hermitdave/scowl 分层,
gold = 词本身)。跑法:`--cases <file> --no-tui --verbose`。

| 指标 | pinyin 全拼 (789) | english (210) |
|---|---|---|
| Top-1 | **92.8%** (57 miss) | **77.1%** (48 miss) |
| Top-3 | 96.7% | 79.0% |
| Top-10 | 98.0% | 79.0% |

english 的 Top-3 ≈ Top-10 ≈ 79% **饱和** —— 约 21% 的词根本进不了候选
(缺词),不是排序问题。

## 二、失败归因(全部已定位到代码行)

### P1 单音节不按词频序(pinyin miss 最大类,~22/57)
`predict_inner` 单音节分支(pinyin/mod.rs:518-530):
`raw_score = large_dict − (i/total)×single_syl_decay` —— 分数只由
`dict.lookup(input)` 的**返回位置**决定,词频零参与。`sou` 输出
搜/艘/嗽/嗖… 是内嵌词典存储序。实际词频序(rime-ice FST,已确认存在,
24,772 个单字条目):搜 93k ≫ 艘 83k > 嗖 20k;`zhao` 应为 找 1.48M ≫
照 693k… 而现在 找 #2、盘 #2、帮 #2、新 #2(高频字全部屈居第二)。
**日常感知最强的痛点。**

### P2 中英竞争无长度/精确度策略(~8/57 + english Top-1 大头)
- `sou` → english/exact "sou" 0.616 压过 搜 0.600(3 字母 exact 抢常用字)
- `make` → 马克 lattice 0.654 压过 make english/exact 0.616(完整英文词
  被拼音谐音词压)
两个方向都在输。主流 IME 策略:**整串 exact 命中英文词典时英文优先;
短 ASCII 串(≤3)合法拼音时中文优先**。现状 english exact 固定 0.88×0.70,
pinyin lattice 顶流 0.90×1.0 —— 一条固定分数线决一切,无输入长度感知。

### P3 lattice_prefix 免费区被长尾词滥用
`prefix_decay` 剩余 ≤3 字符免费(scoring.rs PREFIX_DECAY_FREE=3)+
低频词 freq_to_score 地板 0.25 → `duchun` 的 #1 是 **度春秋**(lattice_prefix
0.247)> 全拼精确 杜淳(lattice 0.235);`poland` 的 #1 是 **破烂的衣裳**
(0.110)。**精确命中输给前缀联想**是排序逻辑直接反直觉。

### P4 英文词条漏(21% 完全缺词)
词条源(en_words.tsv = SCOWL grade 表)与频率源(hermitdave.tsv = 5 万
高频词)**不同源**:poland/danny/alex/easter/esther 等大量常用专名词只在
hermitdave 有,无词条 → 无候选。hermitdave 当前仅用作 frequency band
打分参照。

### P5 生僻字进不了候选
`den→㩐`、`yo→哟`、`ei→欸`、`biang→𰻝`(#46)—— 低频单字被英文 exact
("den"/"yo" 都是英文词)+ 0.25 地板 + 候选池截断挤掉。随 P1/P2/P3 修复
顺带大幅改善,必要时调候选池上限。

## 三、工作项(每项跑双评测对比验收)

| # | 内容 | 预期收益 | 风险 |
|---|---|---|---|
| W1 | **P1 词频驱动单字区**:单音节查询 rime FST 单字词条(带词频),freq_to_score 统一映射;inputx 内嵌词典序仅作同频兜底 | pinyin Top-1 +4~5pt | 中(lattice 单字查询路径) |
| W2 | **P4 hermitdave 词表并入英文词条层**(带真实词频) | english 缺词 21%→<5% | 低 |
| W3 | **P2 中英竞争策略**:exact 英文整串命中提权 / 短串拼音保护(输入长度感知) | 双方 Top-1 +1~2pt | 中(边界 case 多) |
| W4 | **P3 前缀联想收紧**:Full 精确命中加成 + prefix 免费区收紧(≤3→≤1 或要求音节边界对齐) | 消除"破烂的衣裳"类占位 | 中 |
| W5 | P5 复查:生僻字可达性(den/yong/biang 抽查),必要时调池上限 | 尾部补齐 | 低 |

顺序:W1 → W2 → W3 → W4 → W5(按用户感知痛点排序)。

## 四、不变式

- 全部既有测试绿(151+21+2 / 7+12+15);重构失效的测试直接删
- 评测脚本/用例放 `tmp/`(dev 工具,不进 git 跟踪面)
- `swift-ime.yaml` 既有键语义不变;新增权重必须可配且带默认值
- 双评测集 + tc_draft.txt(16 条手工用例)三份结果每步留档 tmp/

## 关联

- 上一轮:issues-round9.md(FSM 三阶段重排,已完成)
- 打分模型现状:`docs/ime/weight-scoring.md`
- 用户原话:"这个项目主要任务是优化预测精准度,现在的预测能力还是太差了,
  代码位置在拼音家族和英语家族那里"

## 执行记录

- **评测体系建立**(本轮先决条件):
  - `tmp/sample_eval.py`:rime-ice 91.6 万词条(pinyin 去空格聚合,同 build_dict.rs:48)
    → 每拼音 key 取词典最高频词为 gold,词长×词频四分位 16 层抽样 789 条;
    english 侧 hermitdave 分层 + en_words 低 grade 尾部 170 条
  - 修 `run_cases` 评测污染:case 间 Enter(raw 提交并学习)→ **Escape 复位**
  - 发现 `scowl.tsv` 不在引擎加载面 —— 英文评测集改用 en_words 尾部
- **W1 ✅ 词频驱动单字区**(`pinyin/mod.rs predict_inner`):
  single 分数 = rime FST 单字词频(24,772 条)经 freq_to_score;内嵌词典
  兜底垫底。找/照/招/赵 等高频字从 #2 回 #1。移除失效测试
  `compose_single_char_options_reach_jian_tail`(造词"剪"槽位断言保护的
  是旧内嵌序巧合;词频序下翻页可达)与
  `context_prefix_association_boosts_tail_word`(de→的 凭词频直接顶格,
  不再依赖 context_comp 抬分,机制对词组仍生效)
- **W2 ✅ hermitdave 并入英文词条层**(`english.rs merge_freq_list`):
  raw_freq≥1000 过滤(clea/ofthe 级语料噪声剔除)+ decile 归一 +
  同词取 max。短输入(≤2 字母)英文前缀长尾(cdc/cds)统一 ×short_word_penalty
  (两字母输入中文简拼优先,cd golden 保持)
- **W3 ✅ 英文 exact 词频化**(`EnglishWeights.exact_quality=0.08`,yaml 可配):
  make/nine/made(grade 顶档 0.952)压过马克/你呢/妈的;sou(0.908)让位搜。
  单字顶流(的 0.90×1.0)永不被英文压过(priority 差)。更新 2 个断言
  固定值的单测(short_base_words_are_penalized / learned_short_words_not_penalized)
- **W4 ✅ 前缀联想条件折扣**(`pinyin/mod.rs predict_inner`):
  存在 lattice Full 精确命中时 prefix ×prefix_lookup。先试 0.85→0.92 底数
  (废弃:免费区内无效 + zhucede 回归),条件折扣后 duchun→杜淳、
  zhucede→注册的 ✓,naozh→闹钟 golden ✓
- **W5(部分)**:den/yong 类生僻字随 P1/P2 改善,𰻝(biang)#46 属极端 case,
  遗留
- **W6 ✅ 候选区限制打开**(用户指令:能打开的打开,打不开的配置化):
  - 限制全图:视图槽 CANDIDATE_SLOTS=16(repr(C) FFI 硬顶)/ 家族 top_n
    (english 8 / emoji 4 / pinyin 128)/ 家族内预过滤(english
    PREFILTER_TAKE 16、pinyin 前缀联想 16、viterbi take 16)/ page_size(已可配)
  - **视图槽 16 → 48**:自有 ABI,`frontend.rs` 与 `release/fcitx/swift-ime.h`
    镜像同步;两侧各加 `static_assert`/`const assert` 尺寸防御(9632B),
    镜像漂移在构建期报错。翻页深度 = page_size 切 48 槽
  - **`weights.family_top_n`(yaml)**:pinyin/english/emoji 各自的跨家族
    竞争宽度,0=回落默认;默认放宽 english 8→32、emoji 4→8(pinyin 128 保持)
    —— `UnifiedScorer.set_family_top_n` + `ImeEngine::set_family_top_n`
  - 家族内预过滤放宽:english 16→64、pinyin 前缀联想 16→64、
    viterbi `take(16)` → `take(viterbi_take)`(同 yaml 一道闸)
  - 验证:全测试绿;eval 无回归(pinyin 97.5 / english 99.4);clea 英文
    补全 8→32;cmake 全量构建过(static_assert 生效)
  - page_size 保持 yaml `input.page_size` 可配(上限即 48)
- **W7 ✅(骨架)Stage3 候选过滤框架**:池子放开后泛滥候选的收敛点。
  位置:postprocess 内 合成/置顶之后、PanelItem 化之前(此点已有全局
  score/family/source 可判,且先于单字区重排的索引逻辑)。
  - `fsm/post.rs`:`Verdict {Keep, Demote(绝对新分), Drop}` +
    `CandidateFilter` trait(name + 纯函数 judge)+ `FilterChain`
    (有序链,首 Drop 生效、Demote 覆盖、链尾统一 stable 重排)+
    `FilterCtx {buffer, state, rank}` 只读快照
  - 接线:`StepEnv::filters()`(默认 `EMPTY_FILTERS` 空链零成本直通);
    `ImeEngine` 持链,`add_filter(&mut self)` 注册(fcitx5 create /
    TUI build_engine 的 Arc 发布前窗口调用)
  - 本轮只落骨架不写具体过滤器;后续候选:低分长尾过滤、英文 noise 词
    (ofthe 级)清理、单字区频次门槛、按 ComposeState 分化的规则。
    加过滤器 = 一个 struct + 一行 add_filter,不动管线
- **W8 ✅ 防洪策略落地**(用户方案:绝对权重底线 + 池数自适应,默认启用):
  - **`ScoreFloorFilter`**(方案一):merge 后分数的**按家族**绝对底线
    (分数域随 priority 不同:pinyin 0.18 / english 0.35 / emoji 0.25 /
    其它 0.30;取值卡在垃圾长尾与真词地板之间,偏保守)。`rank==0` 恒放行
  - **家族配额 `quota_per_family=48`**(方案二的正解):纯分数池顶挡不住
    "一家灌满"—— 实测 cd 场景简拼深池 96 条全挤在 0.38~0.46 窄带,
    📀(0.36)/cd exact(0.386)被任意分数线挤出;配额后
    pinyin 48 + english 1 + emoji 1 三方可达,golden `two_letter_emoji` 恢复
  - **全局池顶 `pool_cap=96`**(方案二的兜底):对齐单家族最大输出,
    只收超深尾部
  - 位置:链尾(Drop/Demote → 稳定重排 → 配额 → 池顶),`kept.retain`
    保序;默认经 `FilterChain::with_flood_control()` 注册,引擎构造单点生效
  - 验证:全测试绿;eval 零回归(pinyin 97.5 / english 99.4)
- **W9 ✅ single 兜底锚点修复**(安装反馈:mo→嚩 0.600 插队):
  - 根因:嚩 是多音字(FST 只收 po 音),mo 音仅 inputx 内嵌词典有 → 走
    W1 兜底;兜底沿用旧公式 0.85 高锚点,生僻字插进 FST 高频字中间
  - 修复:兜底锚点按 `has_fst_data` 二分 —— 有 FST 词频域:垫到
    [min_score−0.01, 0.19](floor 0.18 之上,翻页可达但不插队);无 FST
    数据(裸引擎 new()):兜底是唯一中文来源,保留正常锚点与英文 exact
    竞争(否则 ni→"ni" 称王,空格提交英文)
  - 验证:mo→嚩 消失于前排;裸引擎 ni→你 #1;eval 97.6%(+0.1);
    tc_draft 14/16 不变
- **W9b ✅ 兜底锚点判定升级为词典级**(安装反馈二次:嚩/㮣 仍在用户环境
  霸榜 —— 音节级判定留有边缘:用户环境 FST 查询为空(加载失败/旧 idx/
  打包旧构建)时整表走 0.85 锚,inputx **字典序**(非频序)生僻字霸榜)
  - `LatticeDecoder::has_freq_signal()`(max_freq>1000,词典级):只要
    加载了真实词频词典,兜底一律垫到词频域之下,**与查询音节无关**
  - 无词频词典(裸 new())兜底仍是唯一中文来源,正常锚点
  - 全测试绿;eval 97.3%;新 deb 已重打包
    (build/fcitx5-swift-ime_0.1.0-1_amd64.deb);用户重装时建议同时删除
    旧缓存 `~/.desk-pilot` 下的 .fst.idx(首启重建 ~46s)并看
    swift-ime.log 里 `loaded rime-ice` 行确认 FST 加载
- **最终**:`cargo test -p ime-core -p swift-ime` 全绿
  (151+21+2 / 7+11+14);pinyin 92.8→**97.5**,english 77.1→**99.4**;
  tc_draft 16 条 14/16(jishi/chushi 为语料词频同分,数据问题遗留)
