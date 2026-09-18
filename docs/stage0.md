# 阶段 0 验证记录

最后验证：2026-09-18。复现入口：`scripts/stage0.sh`。

## 结论

阶段 0 的两个选型性风险已通过，第三个性能 spike 也达到目标；anydoc 的
Linux arm64 已完成交叉 `cargo check`，但本机没有 arm64 C 交叉链接器/sysroot，
因此还没有生成可运行的 arm64 二进制。这个环境项应在 CI 的 arm64 runner 或
交叉工具链镜像中补齐后再作为发布门禁。

| Spike | 结果 | 证据 |
|---|---|---|
| anydoc 目标矩阵 | 通过（arm64 为编译检查） | `anydoc 0.2.4`；docx/xlsx/文本 PDF 均转 Markdown；扫描 PDF 返回 `needsOcr`；Win7 x64/x86 完整构建并通过 PE 导入审计 |
| tokenizer 闭环 | 通过 | 内存库建 `tokenize='rsou 0'`、中文短语查询、`highlight()`、读/写连接分别注册均通过；与 `libsimple.so` 的 ASCII/CJK token+range 差分通过 |
| 10 万文档基准 | 通过 | 插入 100,000 行约 605.3 ms；查询约 87.5 ms；索引 25,710,592 B；命中 100,000 行 |

## 目标矩阵实测

- Linux x64：`cargo run -p rsou-anydoc-matrix-spike --release` 通过。
- Linux arm64：`cargo check --target aarch64-unknown-linux-gnu` 通过；完整链接因本机缺少
  `aarch64-linux-gnu-gcc`/arm64 sysroot 暂未执行成功，失败发生在链接环境而非 Rust 源码。
- Windows 7 x64：`nightly + -Z build-std=std,panic_unwind` 构建通过，产物为 PE32+；
  导入表仅含 `ADVAPI32/KERNEL32/msvcrt/ntdll`，未发现 `combase`、`WaitOnAddress`、
  `ProcessPrng`、`PathCchStripPrefix` 等禁用项。
- Windows 7 x86：同上，产物为 PE32 Intel 80386，导入审计通过。

fixture 覆盖：最小合法 docx、xlsx、文本 PDF、图片-only PDF。最后一个得到
`ConvertError::NeedsOcr { pages: [1], page_count: 1 }`。

## tokenizer 实测

`spikes/tokenizer` 使用 `rusqlite-ext 0.39.0` 与 `rusqlite 0.39.0(bundled)`。
3 个单测覆盖：

1. token 的 UTF-8 byte range；
2. 中文短语命中和原文坐标高亮；
3. 文件库的写连接、读连接都经统一注册路径。

与 `/home/ljw/wsou/resources/simple/linux-x64/libsimple.so` 的 oracle 差分由
`scripts/tokenizer-oracle.sh` 执行。ASCII/CJK token 和范围一致；已知差异是
libsimple 还把 Unicode 标点/符号作为 token，而 rsou 将其作为分隔符。由此实际
复现了计划中的误配：`"文档"` 会命中 `文、档`，所以二次原文子串过滤是必须的。

## 体积与性能

基准使用单列 FTS5、逐字 tokenizer、关闭 SQLite journal 以测纯导入上限；真实导入
仍需 WAL/事务设置下再测。一次 release 运行输出：

```text
documents=100000
open_ms=0.792
insert_ms=605.295
query_ms=87.504
hit_count=100000
ranked_rows=100
index_bytes=25710592
```

查询低于计划中的 300 ms 目标。该结果只证明 tokenizer/SQLite 路径可行，不替代
阶段 2 的真实文档吞吐测试。

## 后续门禁

1. 在 arm64 CI 镜像安装交叉 gcc、libc sysroot，补一次完整 `cargo build`。
2. 阶段 1 前将 `rusqlite-ext` 源码和许可证 vendor 到 `vendor/`，并继续保留内存库冒烟测试。
3. Windows 产物尚未在真实 Win7 虚拟机运行；当前结论是编译与导入表兼容，不是运行时验收。
