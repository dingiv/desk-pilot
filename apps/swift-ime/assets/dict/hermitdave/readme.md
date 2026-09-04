# hermitdave/ — 英文词库(单一来源)

`en_freq.tsv`:英文词条 + 真实使用频率,43,152 条,英文家族的唯一
数据源(P0 清洗:SCOWL 词表退役 —— 其 grade 全 10000 零区分度,且与
频率源不同源)。数据来自
[hermitdave/FrequencyWords](https://github.com/hermitdave/FrequencyWords)
的 OpenSubtitles 字幕语料统计(MIT,见 LICENSE.txt)。

## 格式与用法

`word\tcount` TSV(raw 字幕出现次数)。引擎装配时按
`DictType::Frequency` 十分位归一到 1000-10000,再由
`frequency_band` 分档(english.rs)—— 词条与频率同源,prefix 排序
天然反映真实使用频率。

## 再生

```bash
# 1. 下载 raw 词频(~20MB,不入库)
# https://raw.githubusercontent.com/hermitdave/FrequencyWords/master/content/2016/en/en_full.txt
#    存为 tmp/en_full.txt(格式:`词 count` 空格分隔)

# 2. 转换 + 过滤
python3 scripts/build_en_dict.py
```

## 过滤规则(scripts/build_en_dict.py)

| 规则 | 阈值 | 理由 |
|---|---|---|
| 纯字母 `[A-Za-z]{2,20}` | — | 剔除数字/撇号碎片('s/'t)等不可输入词条 |
| 长词(>4 字母)count ≥ **150** | 低频长尾 | 字幕 OCR 噪声/生僻专名(pisani/cutaneous 级)会以 exact/prefix 压过中文 |
| 短词(≤4 字母)count ≥ **3000** | 高门槛 | 短词区伪影密度最高(clea/th/im 截断与撇号丢失);ok/cd/us 级真高频保留 |
| 缩略去撇号黑名单 | — | didnt/cant/thats 类(I'm→im 已被短词门槛覆盖) |

大小写保留(专有名词原样,引擎匹配大小写不敏感)。
