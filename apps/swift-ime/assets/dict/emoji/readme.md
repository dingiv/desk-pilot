# emoji/ — emoji 词典(v2:emoji 主键 + 频率)

`emoji.tsv`:1974 个 emoji,**一行一个 emoji**,携带流行度与触发拼写:

```
# @type: emoji-freq
😍	4543	hearteye	chimi	huachi
👍	9500	zan	hao	tongyi
```

- **freq**:中文聊天/社交场景流行度(1~10000),由本地 LLM
  (Qwen3.8)分批清洗 + 批内横向校准得出,替代旧版全家族统一权重
  (旧版 exact 硬编码 1.0 / prefix 0.6,候选要么见不到要么压过文字);
- **kw**:每 emoji 1~3 个精选触发拼写(小写英文/无声调拼音/≤4 字汉字),
  由 LLM 从 CLDR 自动标注(平均每 emoji 11 个,含大量 face/happy 类
  泛化噪声)中精选;
- 引擎打分:`exact = 0.88 + 0.08×band(freq)`,
  `prefix = 0.6×decay×(0.55+0.45×band)`(emoji.rs,band 与英文同阈值);
- 字段按**空白字符**分割(空格/tab 均可),`#` 为注释。

## 再生(清洗管线,依赖本地 Qwen 服务)

```bash
# 0. 原料:CLDR annotations → 平表 keyword<TAB>emoji
./scripts/fetch_emoji.sh                      # → tmp/emoji_cldr_raw.tsv

# 1. LLM 分批清洗(批内精选 kw + 打流行度分,断点续跑)
python3 scripts/clean_emoji_llm.py

# 2. 合并 20 批 → freq 降序
python3 scripts/merge_emoji_clean.py

# 3. 第二轮:批内横向校准(99 段位 × 20 批交错抽样,只调分不生成)
python3 scripts/refine_emoji_llm.py           # → emoji_cleaned_r2.tsv

# 4. 产物落位
cp tmp/emoji_clean/emoji_cleaned_r2.tsv assets/dict/emoji/emoji.tsv
```

## License

数据源 [unicode-org/cldr](https://github.com/unicode-org/cldr)
common/annotations(**Unicode License v3**),经 `fetch_emoji.sh` 转换、
LLM 精选与校准;许可随源数据传递(汇总见 `../dict.md`)。
