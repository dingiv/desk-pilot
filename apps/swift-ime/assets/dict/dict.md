# assets/dict — 词典资产(as-built)

> 目录已入 git(2026-09-04 起)。以下为当前内容;raw 源与可再生大文件
> 不入库,由脚本再生。

| 路径 | 内容 | 再生 |
|---|---|---|
| `rime/rime-ice.fst` | 中文词频域(雾凇拼音,91.6 万条)—— **不入库**(手工标注词频,`build_rime_dict.sh` + `build_dict` 再生,~2 分钟) | `scripts/build_rime_dict.sh` → `cargo run --release --bin build_dict -- <tsv> rime/rime-ice.fst` |
| `hermitdave/en_freq.tsv` | 英文词条+真实词频(43,152 条,字幕语料 count≥150,短词≥3000) | raw `en_full.txt` + `python3 scripts/build_en_dict.py` |
| `emoji/emoji.tsv` | emoji 词典 v2(1974 个,emoji 主键 + 流行度 + ≤3 精选触发词) | 清洗管线:`fetch_emoji.sh` → `clean_emoji_llm.py` → `merge_emoji_clean.py` → `refine_emoji_llm.py`(见 emoji/readme.md) |
| `dict.md` | 本文件 + 词典资源调研 | — |

不入库:`rime/`(编译产物,可再生)、`*.fst.idx`(运行时缓存,首启自动
重建)、raw 源(`hermitdave/en_full.txt` 20MB,URL 见脚本注释)。

## License(第三方资产声明)

本目录包含/派生自以下第三方数据资产,各随源数据继承其许可:

| 数据 | 上游 | 许可 | 声明位置 |
|---|---|---|---|
| `rime/rime-ice.fst` | [iDvel/rime-ice](https://github.com/iDvel/rime-ice)(cn_dicts 词表转换编译) | **GPL-3.0** | `rime/readme.md` + `rime/LICENSE.txt` |
| `hermitdave/en_freq.tsv` | [hermitdave/FrequencyWords](https://github.com/hermitdave/FrequencyWords) en_full(OpenSubtitles 统计) | **MIT** | `hermitdave/readme.md` + `hermitdave/LICENSE.txt` |
| `emoji/emoji.tsv` | [unicode-org/cldr](https://github.com/unicode-org/cldr) common/annotations(经 fetch_emoji.sh + LLM 清洗管线) | **Unicode License v3** | `emoji/readme.md` + 本表 |

派生说明:各词表在入库/使用前均经过格式转换与过滤(规则见各目录
readme 与 `scripts/` 内对应脚本);上游许可随源数据传递。

---

在输入法引擎开发与日常使用中，不同场景（如拼音转换、英文前缀检索、专业词库扩展）需要的“词典”形态截然不同。