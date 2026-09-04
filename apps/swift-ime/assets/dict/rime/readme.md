# rime/ — 中文词频域(不入库)

`rime-ice.fst`:引擎唯一的可调中文数据层(单字词频、lattice 整词/缩写/
前缀联想、词典 bigram 共现)。**本目录整体 gitignore** —— 词条 weight
是雾凇拼音上游的手工标注(非真实语料频率),不该固化进历史;任何机器
可按下面流程再生。

## 再生(~2 分钟)

```bash
# 1. 下载 rime-ice 并转换为 TSV(pinyin\tword\tweight)
./scripts/build_rime_dict.sh tmp/rime_full.tsv

# 2. 编译为 FST
cargo run --release --bin build_dict -- tmp/rime_full.tsv rime/rime-ice.fst
```

`rime-ice.fst.idx` 是 LatticeDecoder 的运行时缓存(首启自动构建,
~46s),随 .fst 字节数变化自动失效,同样不入库。

## License

数据源 [iDvel/rime-ice](https://github.com/iDvel/rime-ice) 整库 **GPL-3.0**
(全文见本目录 `LICENSE.txt`)。本目录的 `.fst` 是其 `cn_dicts/*.yaml`
词表的格式转换与编译产物,随源数据继承 GPL-3.0。

## 数据质量警示(见 docs/ime/overlay.md §2.1)

weight **两种量纲混用**:

| 词条 | 来源 | 量级 |
|---|---|---|
| 单字(8105 表) | 真实语料字频 | 的 ≈ 7.7e7 |
| 多字词组 | 上游作者手工标注 | 顶流 ≈ 1.3e7,且含人工抬顶词条 |

词表混有 moegirl(萌娘百科)等社区词表 —— 人名/网络词占池。
语料频率化清洗方案保留在 `apps/swift-ime/tmp/REFINE_PLAN.md` 附录 C,
执行等单独发令。
