# 文档检索工具(工作名 rsou)实施方案

> 阶段 0 验证已执行，结果见 [docs/stage0.md](stage0.md)；可复现脚本为
> `scripts/stage0.sh`。

从 [wsou](../../wsou) 中裁出**文档解析**与**全文检索**两项能力,用 [excelookup](../../temp/excelookup) 的
UI 方案与兼容性工程重建为一个 Rust + egui/eframe 单文件桌面应用。

本方案中的每个技术判断都锚定到参考仓库的现有实现或已核实的官方文档/crate 元数据;
风险项集中在 [阶段 0](#阶段-0兼容性验证先行必须) 用可编译的最小工程先证伪。

## 1. 结论摘要

| 维度 | 选型 | 依据 |
|---|---|---|
| 语言/GUI | Rust + eframe/egui 0.36(glow + x11,`default-features=false`) | 与 excelookup 一致,可直接复用其兼容性补丁与打包链路 |
| 文档解析 | `anydoc` crate 0.2.4(MIT,纯 Rust) | wsou 通过 `@firecrawl/anydoc` N-API 调用的就是这个 crate 的 Node 绑定,换成 Rust 依赖即去掉 `.node` 二进制与 Node 运行时 |
| 解析产物 | 统一 GFM Markdown(放弃 wsou 的结构化 `Document` 模型) | 解析结果只有两个消费者:分块与预览,二者用 markdown + 偏移即可满足;换来所有格式一条代码路径 |
| 存储 | SQLite(rusqlite `bundled`)+ FTS5,单文件 `index.sqlite3` | 自带 SQLite 源码,目标平台无需系统 sqlite;单文件便于备份迁移 |
| 索引分词 | **自建 FTS5 tokenizer(纯 Rust)+ vendored `rusqlite-ext` 注册胶水**;`tokenize='rsou 0'` | 语义与 wsou 的 `simple 0` 逐字索引一致;highlight 落在原文坐标,无需偏移映射;不引入 C/C++ 与原生扩展,详见 §6.0 |
| 查询分词 | `jieba-rs` 0.11(仅宽松模式;可关);候选方案已调研见 §6.0.1 | wsou 的 `@node-rs/jieba` 就是 jieba-rs 的绑定,语义可平移;其余 Rust 中文分词库均不胜任或不可用 |
| 排序 | `bm25(chunks_fts, 5.0, 2.0, 1.0)` | 照搬 wsou 的列权重 |
| 高亮 | FTS5 内建 `highlight()` | FTS5 **没有** `offsets()`(官方在 FTS3/4 对比章节明确说明);自定义 tokenizer 上报的就是原文区间,标记直接落在原文上 |
| 分块 | 单层、标题感知、目标 400–900 字符 | wsou 的父子双层自适应分块只服务语义检索,裁掉 |
| 明确不做 | OCR、图片文档、语义/向量检索、AI 问答、PDF 页图渲染、标签体系、同义词、混检 | 用户要求只保留 anydoc 解析 + 全文检索 |

**检索链路里没有任何 C/C++ 代码,也没有需要交叉编译的原生扩展**。唯一一处 unsafe 是 vendored 的
`rusqlite-ext`(约 330 行,FTS5 官方扩展 API 的标准接法,带 `catch_unwind` 与区间断言),
自己的代码全是纯逻辑。这是本方案相对 wsou 最大的结构性简化。

## 2. 能力边界

**保留**
- anydoc 的全部格式:`.doc/.docx/.docm`、`.ppt/.pps/.pot/.pptx/.pptm/.ppsx/.ppsm`、`.xls/.xlsx/.xlsm/.xlsb`、`.odt/.ods/.odp`、`.rtf`、`.epub`、`.csv`、`.pdf`
- 纯文本 `.txt/.md`(anydoc 的 `Format` 枚举里没有这两项,自己读,含编码探测)
- 文件夹递归导入、单文件导入、失败清单与中文失败原因
- 中文全文检索:字段限定、短语、排除、精确/宽松模式、BM25 排序、片段与正文高亮、按文档聚合

**去掉(与 wsou 的差异)**
| 去掉的东西 | wsou 中的代价 | 去掉后的影响 |
|---|---|---|
| pdfjs 解析链路 | 997 LOC + 5.2 MB pdfjs 资源 + `@napi-rs/canvas` 原生模块 | PDF 改用 anydoc 的 `pdf-inspector`,纯文本抽取,无页面坐标、无页图预览 |
| OCR | `onnxruntime-node` + `ppu-paddle-ocr` + 6.2 MB 模型 | 扫描件 PDF/图片文档报「需要 OCR,本版本不支持」,不做静默失败 |
| 语义检索 | `@zvec/zvec` 向量库 + embedding 服务 + 父子分块 + RRF | 只保留 BM25;`rank-fusion.ts` 不移植 |
| 结构化文档模型 | `parser-types.ts` 120 LOC + 各解析器的块/内联/表格映射 | 表格跨行跨列、脚注等结构不再保留,只留 markdown |
| 嵌入图片资产 | 资产持久化、哈希、上下文窗口、资产表 | 图片不入库 |
| `libsimple` FTS5 扩展 | 6.6 MB 的 5 平台原生扩展 | 换成自建纯 Rust tokenizer,见 §6.0 |
| Electron 运行时 | asar、`.node` 解包、IPC/preload、renderer 全套 | 单二进制,无 Node 依赖 |

## 3. 参考仓库可复用清单

### 3.1 从 excelookup 直接搬(兼容性工程的主体)

| 来源 | LOC/大小 | 用途 |
|---|---|---|
| `src/startup.rs` | 852 | 启动日志(候选路径顺序、`.1` 轮转、纯整数时间戳)、30s 看门狗、崩溃信号处理器、zenity/kdialog/xmessage/notify-send/终端/xdg-open 兜底提示链 |
| `src/file_dialog.rs` | 881 | 内置 egui 文件对话框(不依赖 XDG Portal/zenity),含无头布局冒烟测试写法 |
| `src/filebrowser.rs` | 460 | 目录列举/排序/过滤、XDG 用户目录(含 `~/桌面` 中文回退)、路径展开、体积格式化 |
| `src/app/theme.rs` | 297 | 色板、`configure_ui_style`(全零圆角)、`work_panel`/`sub_panel`/`status_badge`/`primary_button`/`toggle_switch` 等控件 |
| `src/app/shell.rs` | 321 | 侧栏 + 顶栏 + 工作区滚动容器的布局骨架、侧栏条目自绘状态点 |
| `src/app.rs` 的轮询骨架 | 约 150 | 每帧末统一 poll 后台 channel、代际失效丢弃陈旧结果、`drop_in_background` 把重析构挪出 UI 线程 |
| `build.rs` | 3.2 K | 版本注入、Windows 图标 `windres` 嵌入(交叉编译可用) |
| `vendor/glutin-winit` | 目录 | **必须**:Linux X11 强制 EGL 优先,否则 UOS 20 的 Mesa 18.3.6 在 GLX 路径崩 |
| `vendor/windows-link` | 目录 | **必须**:`CoTaskMemFree` 从 `ole32.dll` 导入,Win7 无 `combase.dll` |
| `.cargo/config.toml` | 354 B | 三个 target 的 mingw 链接器配置 |
| `scripts/build-win.sh` | 2.3 K | Win7 x64/x86 交叉编译 + `-Z build-std` |
| `scripts/check-win7-imports.sh` | 1.8 K | PE 导入表审计(禁止 combase/PathCchPrefix 等 Win8+ API) |
| `.github/workflows/linux-arm64.yml` | 5 K | debian:buster 容器内构建、glibc ≤ 2.28 门禁、deb 结构校验、tag 触发发版 |
| `debian/`、`assets/`(desktop/icon/launcher) | 小 | 打包与启动器脚本(捕获 `main()` 之前的动态链接错误写进启动日志) |

以上均为 MIT,与 wsou 同一作者,复制后改品牌名与路径即可。

### 3.2 从 wsou 移植的逻辑(改写为 Rust)

| 来源 | wsou LOC | 移植方式 |
|---|---|---|
| `src/core/parsing/anydoc-parser.ts` | 662 | 换成 `anydoc` crate 直调,只保留:格式判定、`ConvertError` → 中文原因映射、资源超限原因解析 |
| `src/core/parsing/text-parser.ts` | 85 | `encoding_rs` + BOM 探测,UTF-8 → GB18030 回退 |
| `src/core/search/fts-schema.ts` | 60 | 表定义 + 建库时校验 |
| `src/core/search/fts-repository.ts` | 388 | SQL 与过滤条件编译(去掉 `source_kind='pdf_page'` 页行,统一 chunk 行) |
| `src/core/search/query-parser.ts` | 269 | 手写递归下降解析器照搬语法:隐式 AND、短语、`-`/NOT、OR、`title:`/`content:`;去掉 NEAR |
| `src/core/search/jieba-query.ts` | 14 | `jieba-rs` 切词 → 每段加引号 → `AND` 连接(仅宽松模式) |
| `src/core/search/search-service.ts` 关键词路径 | 约 300 | 高亮区间计算、按文档聚合(每篇最多 3 处)、标题命中过滤 |
| `src/core/chunking/*` | 约 700 | 只用标题感知单层分块思路,自写约 250 LOC |
| `src/renderer/features/search/*` | 约 1000 | 视觉与交互参考(结果列表 / 片段面板 / 命中类型标签),用 egui 重写 |

### 3.3 两个仓库都不要的

`src/core/{semantic,ai,ocr,models,taxonomy,jobs,imports,indexing}`、`migrations/*.sql`(16 个版本)、
`src/renderer/components/ui/*`(shadcn-vue 全套)、`resources/{models,pdfjs,simple}`。
excelookup 的 `join.rs`/`read_xlsx`/`export.rs` 与业务无关;但 `read_xlsx` 的流式读取、
`Arc<Table>` 共享、`JoinOutcome` 的计数布局可作参考。

## 4. 解析层

### 4.1 anydoc crate(已核实)

```
anydoc 0.2.4 · MIT · edition 2024 · MSRV 1.88(本机 rustc 1.98.1)
依赖:cfb, csv, encoding_rs, flate2, log, pdf-inspector, quick-xml, zip  ← 全纯 Rust,无原生资产
API:anydoc::to_markdown(path) / to_markdown_bytes(bytes, Option<Format>)
     anydoc::to_document(bytes, Option<Format>) -> Document   (PDF 不支持)
     Format::{from_bytes, from_extension, from_path}
     ConvertError::{Unsupported, Malformed, Encrypted, ResourceLimit, MissingPart, Io, NeedsOcr}
```

关键收益:wsou 打包里那个 8.2 MB 的 `anydoc.*.node` 与 asar 解包逻辑全部消失;
`to_markdown_bytes` 是纯函数式调用,可直接放进 `rayon` 线程池。

### 4.2 路由

```
按扩展名归入文档类(anydoc 支持)  → read bytes → Format::from_bytes() ?? Format::from_path() → to_markdown_bytes
.txt/.md                          → 自读(encoding_rs:BOM → UTF-8 严格 → GB18030 回退)
其余扩展名                        → UNSUPPORTED(导入时即拒绝,不落库)
```

- PDF 走 `to_markdown`:`pdf-inspector` 直接产出 Markdown。扫描件返回
  `ConvertError::NeedsOcr`,映射为中文「该 PDF 没有可提取文本,需要 OCR,本版本不支持」。
- 与 wsou 相比,格式白名单放宽到 anydoc 全集(`.docm/.xlsm/.xlsb/.epub/.ppsx` 等在 wsou 里被
  `SUPPORTED_DOCUMENT_EXTENSIONS` 挡掉)。
- 解析产物:`markdown: String` + `warnings: Vec<String>`,不建结构化模型。
- 大小上限:自设可配置上限(默认 100 MB),超限记 `RESOURCE_LIMIT`。

### 4.3 失败原因中文映射

| 来源 | 文案 |
|---|---|
| `Unsupported` | 无法识别的格式,或该格式无法转换(例如纯图片 PDF) |
| `Encrypted` | 文件已加密或受密码保护 |
| `Malformed` / `MissingPart` | 文件结构损坏,或缺少必要组成部件 |
| `ResourceLimit` | 文件超出安全上限(解压体积/节点数/表格格数) |
| `NeedsOcr` | 需要 OCR 才能识别,本版本不支持 |
| `Io` | 无法读取文件(权限/占用/路径失效) |

## 5. 存储

### 5.1 位置

```
数据目录  $XDG_DATA_HOME/rsou  |  ~/.local/share/rsou
          ├── index.sqlite3        (documents / contents / chunks / chunks_fts / import_* / settings)
          └── tmp/                 (解析中间文件,失败可清理)
日志      $XDG_STATE_HOME/rsou/startup.log  |  ~/.cache/rsou/startup.log  |  /tmp/rsou-startup.log
```

### 5.2 表结构(单版本,不做 wsou 那套 16 个 migration 的版本演进)

```sql
CREATE TABLE documents (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,            -- 规范化绝对路径
  canonical_path TEXT NOT NULL,         -- realpath,用于同文件去重
  file_name TEXT NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  ext TEXT NOT NULL,
  file_type TEXT NOT NULL,              -- word/excel/ppt/pdf/text/web
  file_size INTEGER NOT NULL,
  file_mtime_ms INTEGER NOT NULL,
  content_hash TEXT NOT NULL,           -- 解析前 sha256,未变更即跳过
  parse_status TEXT NOT NULL CHECK (parse_status IN ('parsed','failed')),
  parse_error_code TEXT,
  parse_error_message TEXT,
  parser_name TEXT, parser_version TEXT,
  text_length INTEGER NOT NULL DEFAULT 0,
  chunk_count INTEGER NOT NULL DEFAULT 0,
  indexed_at INTEGER,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
) STRICT;

-- 每篇文档唯一的原文;预览、片段切片、重分块都以它为准
CREATE TABLE document_contents (
  document_id INTEGER PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
  markdown TEXT NOT NULL,               -- anydoc 产物,可选展示
  plain_text TEXT NOT NULL,             -- 从 markdown 提取的纯文本,偏移基准
  warnings_json TEXT NOT NULL DEFAULT '[]'
) STRICT;

-- 只存定位与元数据
CREATE TABLE chunks (
  id INTEGER PRIMARY KEY,
  document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  chunk_index INTEGER NOT NULL,
  context_header TEXT NOT NULL DEFAULT '',  -- 「标题 › H1 › H2」
  start_offset INTEGER NOT NULL,            -- 在 plain_text 中的字节偏移
  end_offset INTEGER NOT NULL,
  UNIQUE (document_id, chunk_index)
) STRICT;

-- 普通 FTS5 表:rowid 显式取 chunks.id,三列存分块原文(是原文,不是预分段文本)
-- tokenizer 'rsou' 由本程序注册(参数 '0' = 关闭拼音,与 wsou 的 simple 0 对齐)
CREATE VIRTUAL TABLE chunks_fts USING fts5(
  title, context_header, content,
  tokenize = 'rsou 0'
);

CREATE TABLE import_runs (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL, root_path TEXT,
  status TEXT NOT NULL CHECK (status IN ('running','done','failed','cancelled')),
  total_files INTEGER NOT NULL DEFAULT 0, processed_files INTEGER NOT NULL DEFAULT 0,
  ok_files INTEGER NOT NULL DEFAULT 0, failed_files INTEGER NOT NULL DEFAULT 0,
  skipped_files INTEGER NOT NULL DEFAULT 0,
  started_at INTEGER NOT NULL, finished_at INTEGER, message TEXT
) STRICT;

CREATE TABLE import_items (
  id INTEGER PRIMARY KEY,
  run_id INTEGER NOT NULL REFERENCES import_runs(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending','running','ok','failed','skipped')),
  error_code TEXT, error_message TEXT,
  document_id INTEGER REFERENCES documents(id) ON DELETE SET NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (run_id, path)
) STRICT;

CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
```

设计取舍:
- **文本存两份是有意的**:`document_contents.plain_text` 是全文(预览、重分块、偏移基准),
  `chunks_fts.content` 是分块原文——高亮与片段必须能读到列值,而 FTS5 只对普通表提供
  `xColumnText`。wsou 是同样的结构(`cleaned_text` + chunk content),不是本方案引入的浪费。
- **FTS5 用普通表而非外部内容表**:删除即 `DELETE FROM chunks_fts WHERE rowid = ?`,
  不需要外部内容表那套「先喂旧值再删」的协议;rowid 显式等于 `chunks.id`,两表天然对齐。
- **查询必须经过本程序**(或注册了同名 tokenizer 的连接):用系统 `sqlite3` CLI 直接
  `MATCH` 会因找不到 tokenizer 报错。wsou 用 `libsimple` 时也有同一性质,写进 README。
- **不做内容版本**:wsou 的 `content_version`/`ready_content_version` 是为「解析-OCR-清洗-分块-索引」
  多阶段异步流水线准备的。本应用解析与索引在一次事务里完成(解析在内存/临时文件里做,
  完成后一个 `BEGIN IMMEDIATE` 写 contents + chunks + fts),天然不出现半成品可检索。
- 单库单写者:导入线程持有写连接,检索/UI 各开只读连接(WAL + `busy_timeout=5000`);
  每个连接都必须先注册 tokenizer(注册是 per-connection 的)。
- `documents.path UNIQUE` + `canonical_path` 双列:重复导入按规范路径命中并跳过;
  文件已变更(大小或 mtime 不同)才重新解析。

## 6. 检索

### 6.0 方案对比:为什么不用 simple 扩展,也不用预分段

FTS5 的分词行为由 tokenizer 决定,而内建 tokenizer 对中文都不合用(unicode61 会把连续的
汉字当成**一个** token)。分岔在于「怎么把中文切成可索引的词元」以及「谁来切」。候选方案摊开:

| 方案 | 中文匹配语义 | 新增依赖/代码 | 工程与兼容性风险 | 结论 |
|---|---|---|---|---|
| **A. 自建 FTS5 tokenizer(纯 Rust 逻辑)+ vendored `rusqlite-ext` 注册胶水** | 逐字子串语义,与 wsou 的 `simple 0` 一致 | 约 150 行 tokenizer 实现 + 330 行 vendored 胶水(MIT/Apache,源自广为引用的 gist) | 需较新的 SQLite(`fts5_tokenizer_v2` 接口;bundled 满足);外部 CLI 查库需先注册 tokenizer | **采用** |
| B. 自建分段器 + 内建 `unicode61`(预分段文本) | 同 A | 约 120 行分段器 + 一份 token→原文偏移映射 | 零 unsafe,但要多存一份分段文本,且高亮要自己做坐标映射(新增一类可错的地方) | 备选(若不愿引入任何 unsafe) |
| C. 内建 `trigram` tokenizer | 真子串,零代码 | 无 | 官方文档:**「少于 3 个 unicode 字符的子串在全文查询中不匹配任何行」** → 中文两字词(合同/发票/公文/报表)全部失效,只能退回全表 `LIKE` 扫描 | 否决 |
| D. `tantivy` + `tantivy-jieba` | 词级,内建 BM25 与 snippet | tantivy 0.26.2,约 40 个直接依赖(rayon/regex/serde/time/winapi…) | 第二套存储(索引目录),与 SQLite 的一致性要自己做;依赖面大,Win7 导入表审计风险高;本应用量级用不上 | 否决 |
| E. 手写倒排索引存 SQLite 表 | 完全可控 | 约 1000 行 | 中文逐字 posting 在十万文档量级是数亿行,用 SQL 表存不现实 | 否决 |
| F. 索引侧 jieba 分词 | 词级,丢跨词子串召回(「公文号」查不到「文号」) | jieba-rs | 词典版本一变就要全量重建索引 | 否(仅查询侧宽松模式使用) |
| G. 成品 FTS5 tokenizer 扩展(`sqlite-simple-tokenizer` 等)或 `libsimple`(C++ / 其 Rust 绑定) | 与 wsou 一致或接近 | C++ 工具链 / per-platform 原生件 | 预编译产物**没有 win32-ia32**;C++ 版带出 libstdc++/libgcc DLL,与单文件冲突;成品扩展仍要随包分发 `.so/.dll` | 否决(保留 Linux `.so` 作对照 oracle) |

**选定方案 A 的关键依据**:`rusqlite-ext` 把 FTS5 扩展 API 的 unsafe 部分封成一个安全 trait
(`Tokenizer`:`name()` / `new(args)` / `tokenize(text, push_token)`),内部做了 `catch_unwind`、
词元区间断言和 SQLite 错误码转换,注册入口是
`rusqlite_ext::register_tokenizer::<T>(&Connection, Global)`。
同一个 monorepo 里已经用它做出了三个成品 tokenizer(`sqlite-simple-tokenizer`、
`sqlite-jieba-tokenizer`、`sqlite-charabia-tokenizer`),说明这条路径走通过,不是我方首创。
我们只需要写一个纯逻辑的 `Tokenizer` 实现,并把胶水 vendor 进 `vendor/rusqlite-ext`
(与 excelookup 已 vendor 两个 crate 的做法一致,便于审计)。

相比方案 B,A 的决定性好处是**高亮不需要坐标映射**:tokenizer 上报的词元区间就是原文区间,
`highlight()` 把标记直接插在原文的汉字上。方案 B 需要自己维护「合成分段文本位置 → 原文字节偏移」
的映射,并靠一条往返不变式来防守,那是一整类可错的地方。存储上 A 也略优(不必多存一份分段文本)。

### 6.0.1 Rust 中文分词生态调研(2026-09 核实)

按「能不能用在本地单文件离线检索」筛一遍,事实如下:

| crate/项目 | 版本 · 许可 · 活跃度 | 性质 | 对本项目的判断 |
|---|---|---|---|
| `jieba-rs` | 0.11.0 · MIT · 387 万下载,2026-09 更新 | 纯 Rust,词典编译期嵌入(`default-dict` + `include-flate`),另有 TF-IDF/TextRank | **采用**(仅查询侧宽松模式);Rust 生态事实标准 |
| `cang-jie` | 0.20.0 · MIT · 5.2 万下载 | 依赖 `tantivy` + `jieba-rs`,是 tantivy 的 tokenizer | 不可用于 FTS5(绑定 tantivy) |
| `lindera` / `lindera-jieba` / `lindera-sqlite` | 6.0.0 · MIT · 224 万下载 | 多语言形态分析框架;`lindera-jieba` 提供由 jieba 词典构建的中文词典(`embed-jieba`);`lindera-sqlite` 是 FTS5 扩展 | 可用于 FTS5,但为单一中文场景引入整套多语言框架与词典,过重 |
| `icu_segmenter`(ICU4X) | 2.3.0 · **Unicode-3.0** · 1573 万下载,MSRV 1.88 | Unicode 官方的断词/断行算法,含 CJK 词典分词与 LSTM 路径 | 规范可信,但许可是 Unicode License(非 MIT/Apache),数据体积以 MB 计,且仍是词级分词,还得自己套 FTS5 tokenizer 外壳 |
| `unicode-segmentation` | 1.13.3 · MIT/Apache · 5.5 亿下载 | UAX#29 字素/词边界;对中文是**逐字**成词 | 可作分类基础,但其 ASCII 分组与 cppjieba 不同(`32.3`、`can't` 各算一个词元);要严格对齐 `simple 0` 还是自己写分类(§6.1) |
| `sqlite-simple-tokenizer` | 0.6.0 · MIT/Apache · 2662 下载 | **纯 Rust 复刻的 `simple`**,基于 `rusqlite-ext`;`disable_pinyin` 即逐字索引;默认开拼音 + 英文 Snowball 词干 + 停词表 | 重要参照物,但不直接采用:默认配置是「中文只索引拼音」(查询必须走它的 `simple_query()`);要当汉字索引用需关掉拼音/停词/词干,不如自己写 |
| `sqlite-jieba-tokenizer` | 0.6.0 · MIT/Apache · 221 下载 | 同家族,jieba 词级 tokenizer | 词级语义,同方案 F,不用 |
| `sqlite-charabia-tokenizer` / `lindera-sqlite` | 0.5.0 / 2.0.0 | 分别把 charabia、lindera 接到 FTS5 | 依赖重;charabia 内部也是 jieba |
| `libsimple` / `cjieba-sys` / `rust-jieba` | 0.9.0 / 0.1.1 / 0.1.0 | 绑定或编译 C++ cppjieba(`libsimple` 的 crate 有 6.9 MB) | 含 C++,需 C++ 工具链与 libstdc++ 运行时;**仅留 `libsimple.so` 作 Linux 对照 oracle** |
| `ik-rs` / `tantivy-ik` | 0.7.0 · **LGPL-2.1-only** · 2.3 万下载,2024-01 后未更新 | IK 分析器的 Rust 移植 | **不要用**:LGPL 对静态链接的桌面程序是许可证风险,且已停止维护 |
| `pinyin` | 0.11.0 · MIT · **零依赖** · 96 万下载 | 汉字转拼音(带声调/多音字选项) | 可选增强:将来做拼音搜索时用它;tokenizer 可同时发出汉字词元与 COLOCATED 拼音词元 |
| `opencc-jieba-rs` | 0.8.0 · MIT · 5.3 万下载 | jieba + OpenCC 简繁转换(词典内嵌,依赖 zstd/rayon/regex) | 可选增强:语料简繁混排时做归一化;依赖偏重,等真遇到混排语料再评估 |
| `ws-segment-rs` / `chinese_segmenter` / `mmseg` / `jieba-rs-siro` | 0.1.4 / 1.0.1 / 0.3.0 / 0.6.7 | 小众或已停更(最新的 2026-08 但仅 219 下载;其余停在 2022) | 不用 |

结论:查询侧 `jieba-rs` 没有更好的替代;索引侧自建逐字 tokenizer 优于所有成品的默认语义;
真正值得后续加的只有 `pinyin`(零依赖)与简繁归一化(等有需求)。

### 6.1 tokenizer 规范(与 wsou 的 `simple 0` 同语义)

```
按 UTF-8 首字节分类:
  >0x7F(含全部 CJK 与中文标点) → 取一个码点作为一个词元
  ASCII 字母串                   → 整串小写后作为一个词元
  ASCII 数字串                   → 整串作为一个词元
  字母↔数字的边界                → 切开(与 simple 一致,"A4" → "a","4")
  空白/控制/其余                 → 分隔符,丢弃(不产生词元)
上报给 FTS5 的每个词元:词元字节 + 在原文中的 byte range
```

- 拼音关闭(对应 wsou 的 `tokenize='simple 0'`)。tokenizer 接受 `0`/`1` 参数位,
  `1` 预留给将来的拼音模式(汉字词元 + COLOCATED 拼音词元,用 `pinyin` crate)。
- 实现要点:`tokenize()` 对同一段文本是纯函数;不得 panic(胶水层有 `catch_unwind`,
  但仍按其契约自己保证);上报区间必须落在输入长度内(胶水层有断言)。
- 已知与 `simple` 的差异:emoji 与部分符号在 `simple` 里各算一个词元,在我们这里被丢弃
  (搜不到)。差分测试里显式列为已知项。

### 6.2 查询侧

```
tokenize → 递归下降 parse → AST → 编译为 FTS5 MATCH 串
不支持:* 通配(query-parser 里本就显式拒绝)、NEAR
语法:隐式 AND、OR、-term/NOT、'…'/"…" 短语、title:、content:
```

- 查询词编译成 FTS5 短语串,再交给同一 tokenizer 切分:
  - **精确模式(默认开,与 wsou 一致)**:每个空白分隔的查询词整体作为一个短语,
    `文档` → `"文档"`;tokenizer 切成逐字词元后,等价于**连续子串**匹配(「公文号」能命中「文号」)。
  - **宽松模式**:词经 `jieba-rs` 切成若干段,每段一个短语,段间 `AND`;这是中文非连续命中的来源。
- 字段作用域:`all` / `title` / `content`;查询串上限 500 字符,结果上限 100 篇。
- `jieba-rs` 用 `default-dict`(编译期嵌入,二进制 +约 5 MB),做成可关的 cargo feature。

### 6.3 排序与聚合

```sql
SELECT c.id, c.document_id, c.context_header, c.start_offset, c.end_offset,
       bm25(chunks_fts, 5.0, 2.0, 1.0) AS rank
FROM chunks_fts
JOIN chunks c    ON c.id = chunks_fts.rowid
JOIN documents d ON d.id = c.document_id
WHERE chunks_fts MATCH ?
  [AND d.file_type IN (...)] [AND d.file_mtime_ms BETWEEN ? AND ?] [AND d.path LIKE ?||'%']
ORDER BY rank ASC, chunks_fts.rowid ASC
LIMIT ?
```

- 按 `document_id` 聚合,每篇最多取 3 处命中,文档顺序取其 chunk 结果中的首次出现位置
  (与 wsou 的 `search-service.ts` 聚合方式一致)。
- 过滤条件:文件类型、时间范围、目录前缀。wsou 的标签/来源/文档类型过滤随标签体系一起去掉。

### 6.4 片段与高亮

FTS5 没有 `offsets()`(官方在 FTS3/4 对比章节明确说明),因此:

1. 对命中行调用 `highlight(chunks_fts, 2, '\x01', '\x02')`,拿回**原文**并带出标记位置。
   自定义 tokenizer 上报的词元区间就是原文区间,标记天然落在原文字节上,不需要任何坐标换算。
   `highlight()` 对重叠短语会合并成一对标记——正合我们需要(高亮取并集)。
2. 扫描标记得到高亮区间 `Vec<(start, end)>`。chunk 的原文就是 `chunks_fts.content`,
   与 `document_contents.plain_text[start_offset..end_offset]` 一致(同一份文本,偏移只用于整篇预览定位)。
3. 用区间把片段切成 `Vec<(text, hit)>`,交给 egui `LayoutJob` 上底色;需要窗口时按 wsou 的做法
   取命中前 200 字节起、共 5000 字节(chunk 只有 400–900 字符,通常整块显示)。

**二次精确过滤(必须有,且按空白不敏感比对)**:逐字索引下标点与空白都不产生词元,词元位置因此相邻。
阶段 0 实测(见 [stage0.md](stage0.md)):`文 档`、`文、档`、`文\n档`、`文档` 四行都会被短语查询 `"文档"` 命中。
收口规则:

- 把高亮区间的原文与查询词**都剔除空白后**比对,相等才保留该处命中。空白(空格、换行)在中文文档里
  多是排版或 PDF 抽取的产物,若按原文逐字节比对会把这些合法命中全部丢弃——那是静默的召回损失。
- **标点不剔除**,所以 `文、档` 与 `文档` 判为不等而丢弃,这正是要杀掉的那类误配。

净效果:`文 档`(空格/换行)保留,`文、档` 丢弃。代价是每处命中一次字符串比较。

## 7. UI 方案(沿用 excelookup 的设计语言)

### 7.1 骨架

- 左侧 224px 深蓝 `navy()` 侧栏:品牌块、条目列表、底部版本号(`option_env!` 注入)。
- 顶部 66px 白色顶栏:面包屑 + 右侧状态徽标(索引中文档数 / 导入进行中)。
- 中央 `canvas()` 底色 + 纵向 `ScrollArea`,`available_width` 撑满。
- 全零圆角、1px 描边、白卡片 + 极浅投影,17px 正文,运行时加载系统中文字体
  (Windows 微软雅黑 / Linux Noto Sans CJK SC,取第一个存在者,不内嵌字体)。

### 7.2 四页

| 页面 | 内容 |
|---|---|
| 资料库 | 「添加文件」/「添加文件夹」按钮(内置文件对话框)、当前导入进度卡(进度条 + 已完成/失败计数 + 取消)、文档表(`egui_extras::TableBuilder` 虚拟滚动:文件名/类型/大小/文本量/分块数/状态/更新时间)、行操作(打开原文件 / 在文件管理器中显示 / 重新解析 / 移除)、失败清单入口 |
| 导入失败 | 抽屉式面板:路径 + 中文失败原因 + 「重试」,对应 wsou 的 `ImportFailuresDrawer` |
| 检索 | 顶部查询框 + 精确开关 + 过滤行;左结果列表(按文档分组,每组文档信息 + ≤3 条片段,命中处上底色),右预览面板(定位到首个命中并滚动);底部状态「命中 N 篇 · M 处 · 耗时 X ms」 |
| 设置 | 数据目录(展示 + 打开)、索引维护(重建 / 优化 / 完整性检查 / 文档与分块统计 / 索引体积)、解析选项(最大文件体积)、关于(版本、日志路径、打开日志) |

导入期间页面可自由切换(不像 excelookup 那样做步骤门禁——那是向导式任务,本应用是常驻资料库)。

### 7.3 线程模型

```
UI 线程 ── mpsc ── 导入 worker(1 个):扫描 → 逐文件解析 → 单事务写库 + 写 FTS
                                              └ 解析用 rayon 并行,写库串行
UI 线程 ── mpsc ── 检索 worker(1 个,只读连接):查询 → 聚合 → 高亮映射
```

- 任务取消:`Arc<AtomicBool>`;导入在两个文件之间检查,单文件解析不可中断(anydoc 无取消接口,
  在文档中写明这一限制)。
- 陈旧结果:沿用 excelookup 的代际号 + `drop_in_background`(`rsou-drop` 线程),不在 UI 线程做大析构。
- 每帧末统一 poll;`ctx.request_repaint()` 只在有在途任务时调用(省电,也避免 UOS 上空闲占 CPU)。

## 8. 兼容性工程清单

沿用 excelookup 已踩过的坑:

| 决策 | 理由 |
|---|---|
| eframe `default-features=false`,features `["default_fonts","links","glow","x11"]` | 去掉 wayland/wgpu/accesskit;UOS 20 是 X11 + 旧 Mesa |
| vendor `glutin-winit`(Linux X11 EGL 优先) | eframe 0.36 默认 `FallbackEgl` 走 GLX,Mesa 18.3.6 异步返回 `GLXBadContextTag` 导致首窗崩溃 |
| vendor `windows-link`(`CoTaskMemFree` → `ole32.dll`) | Win7 无 `combase.dll` |
| `lto = "fat"` | 顺带剔除引用 Win8 `PathCchStripPrefix` 的剪贴板代码,并使导入表可审计 |
| Linux 不用 rfd,用内置文件对话框 | 目标机常无 XDG Portal,回退 zenity 会「点了没反应」 |
| deb `depends` 显式列 X11/GL/XKB 运行库 | glutin/winit 用 dlopen,`dpkg-shlibdeps` 看不见 |
| `/usr/bin` 启动器 + `/usr/lib/rsou/rsou` 真实二进制 | 把动态链接错误也写进启动日志 |
| CI 在 `debian:buster` 容器内构建 + glibc ≤ 2.28 门禁 | 目标机 glibc 2.28;在 24.04 上编会要求 2.39 |
| Win7 目标 `nightly -Z build-std` + mingw 链接 + PE 导入表审计 | 官方无预编译 std;审计挡住 Win8+ API |
| 运行时加载系统中文字体 | 不内嵌字体,避免体积 |

本应用新增的兼容性决策:

| 决策 | 理由 |
|---|---|
| rusqlite `bundled`(自带 SQLite 源码) | 目标机不一定有 libsqlite3;`libsqlite3-sys` 的 bundled 构建已开 `SQLITE_ENABLE_FTS5`;自定义 tokenizer 需要较新的 `fts5_tokenizer_v2` 接口,bundled 版本满足,系统 SQLite 不保证 |
| vendor `rusqlite-ext`(约 330 行,MIT/Apache) | 把 FTS5 扩展 API 的 unsafe 收在一处、可审计,与 excelookup 已 vendor 两个 crate 的做法一致 |
| tokenizer 是纯 Rust,不链 C++ | 没有 libstdc++/libgcc 运行时 DLL,也没有 win32-ia32 缺产物的问题(现有 `libsimple` 正是缺这一项) |
| `jieba-rs` 编译期嵌入词典,做成可关 feature | 单文件部署不依赖外部词典;关掉省约 5 MB |
| 数据库/日志路径全部走 XDG 并可回退 | 精简桌面可能无 `XDG_DATA_HOME`;回退到 `~/.local/share` 与 `/tmp` |

## 9. 代码结构

单 package、lib/bin 分离(与 excelookup 相同),lib 内零 egui 依赖,保证解析/检索可无头测试:

```
src/
├── main.rs            # GUI 入口;release 下 windows_subsystem="windows"
├── lib.rs             # 导出 parse/text/tokenize/chunk/query/search/store/import
├── parse.rs           # anydoc 调用、扩展名路由、txt/md 读取、错误码映射
├── text.rs            # markdown → plain_text + 标题路径 + 块偏移
├── tokenize.rs        # 实现 rusqlite_ext::Tokenizer 的逐字 tokenizer(纯逻辑)+ 注册封装
├── chunk.rs           # 标题感知单层分块(目标 400–900 字符)
├── query.rs           # 递归下降查询解析 + jieba 切词(可选)+ MATCH 串编译
├── search.rs          # SQL、过滤、BM25 聚合、highlight() 区间、二次精确过滤、片段切片
├── store.rs           # 打开/建库/PRAGMA/注册 tokenizer/schema 校验
├── repo.rs            # documents/contents/chunks/import_* 的读写
├── import.rs          # 扫描、去重、流水线、进度回调(无 GUI 依赖)
├── app.rs             # 状态类型 + App::ui 入口
├── app/{shell,theme,workers,page_library,page_search,page_settings}.rs
├── file_dialog.rs     # 内置文件对话框(搬 excelookup)
├── filebrowser.rs     # 路径/列举纯逻辑(搬 excelookup)
└── startup.rs         # 启动诊断(搬 excelookup)
bin/cli.rs             # 可选小 CLI:解析并打印 markdown / 建索引 / 检索(无 GUI,用于验收与批处理)
vendor/{glutin-winit,windows-link,rusqlite-ext}
tests/{end_to_end.rs,tokenizer_parity.rs,fixtures/}
```

`bin/cli.rs` 的价值:在没有桌面环境的机器(CI、WSL 无 X)上直接验证解析与检索,
不必绕 GUI 截图;它同时是「检索必须注册 tokenizer」这条约束的活文档(CLI 与 GUI 都走 `store::open`)。

## 10. 验证方案

| 层次 | 手段 |
|---|---|
| tokenizer | 纯逻辑单测(词元序列 + 区间);与 wsou 的 `libsimple.so` 差分:同一批中英混排文本(全角标点、数字字母混排、URL、长英文词)比对词元序列与区间。仅 Linux 跑,`#[ignore]` 标给其他平台;已知偏差(emoji/符号)在测试里显式列出 |
| 注册与建表 | 内存库跑 `register_tokenizer` → 建 `tokenize='rsou 0'` 表 → 中文短语、两字词、`highlight()` 标记位置断言 |
| 二次精确过滤 | 四行样例(空格/顿号/换行/无分隔)对齐:`文 档`、`文\n档` 必须保留,`文、档` 必须丢弃;ASCII 词与数字串的命中不得被误杀 |
| 查询编译 | 表驱动单测:语法 → MATCH 串,覆盖隐式 AND、短语、`-`、`title:`、`*` 被拒、精确/宽松差异 |
| 分块 | 断言 chunk 偏移可还原、无重叠错位、标题路径正确、超长段落被切且不丢字符 |
| 解析 | `tests/fixtures/`:用 `zip` crate 手写最小合法 docx/pptx/xlsx/odt/epub,`rust_xlsxwriter` 生成 xlsx,自造 rtf/csv/txt(GB18030);断言 markdown 含关键文本、失败码正确(加密/损坏/NeedsOcr 用构造样例) |
| 端到端 | 导入 → 检索 → 命中与高亮区间断言 → 删除文档后 FTS 无残留;重建索引与增量索引结果一致 |
| 规模 | 生成 10 万份合成文档(含中文正文),测导入吞吐、查询延迟(目标 < 300 ms)、索引体积倍率 |
| GUI | 只保留无头布局冒烟测试(文件对话框、表格列构造);外观与交互由人工验收(沿用 excelookup 约定) |
| 兼容门禁 | `cargo test --release` + glibc 符号扫描 + Win7 导入表审计 + 交叉编译冒烟 |

真实文档语料不进仓库:本地放一批真实 `.docx/.xlsx/.pdf`(含扫描件、加密件、超大文件),
按需人工跑一遍导入并检查失败清单文案。

## 11. 分阶段实施

### 阶段 0:兼容性验证先行(必须)

状态:已完成,记录见 [stage0.md](stage0.md)(anydoc 目标矩阵、tokenizer 闭环、10 万文档基准三项通过)。
两处遗留随阶段 1 收口:① 计划中的 release 二进制体积与冷启动时间尚未测;
② arm64 目前只对 anydoc crate 做了 `cargo check`,沿用 bundled SQLite 的 C 编译要等 CI 的 arm64 runner。

三个 spike,每个都是可编译的小工程,结论决定后续选型:

1. **anydoc 目标矩阵**:Linux x64/arm64、`x86_64-win7-windows-gnu`、`i686-win7-windows-gnu`
   下编译并解析 docx/xlsx/pdf(含一份扫描件验证 `NeedsOcr`)。检查 Windows 产物导入表
   (pdf-inspector 带 `rayon`/`env_logger`,需确认没有引入 Win8+ API)。
   *不通过对策*:PDF 与 Office 分两个特性开关,或改用其他纯 Rust 解析器。
2. **tokenizer 闭环**:vendor `rusqlite-ext` + 自写 `rsou` tokenizer,在内存库上验证中文短语、
   两字词命中与 `highlight()` 标记落在原文上;再与 `libsimple.so` 做词元序列差分。
   同时确认「读连接与写连接都要注册 tokenizer」在代码里只有一处(统一走 `store::open`)。
3. **体积与性能实测**:合成 10 万文档测索引体积、导入吞吐、查询延迟、冷启动时间、
   release 二进制体积(目标 < 40 MB,冷启动 < 1 s)。

### 阶段 1:骨架与存储

搬 excelookup 的 shell/theme/startup/file_dialog/filebrowser/build.rs/vendor/CI/deb,
改品牌;建库、schema、迁移(单版本)、注册 tokenizer、CLI 打印文档表。
**完成标准**:arm64 deb 装得上、启动日志正常、空库可打开且能建 FTS 表。

### 阶段 2:解析与导入

`parse.rs` + `text.rs` + `chunk.rs` + `import.rs` + 资料库页;单文件与文件夹导入、
进度、取消、失败清单、去重与变更重解析。**完成标准**:fixtures 全绿;真实语料导入无 panic,
失败原因全部是中文且可解释;`chunks_fts.content` 与 `plain_text` 切片逐字节一致。

### 阶段 3:检索

`query.rs` + `search.rs` + 检索页 + 预览高亮。**完成标准**:§10 的 tokenizer/注册/查询/过滤
四层单测全绿;中文短语与两字词命中符合预期;10 万文档量级查询 < 300 ms。

### 阶段 4:设置与索引维护

设置页、重建/优化/完整性检查、统计口径(文档数/分块数/索引体积)、数据目录展示与打开日志。
注意重建与完整性检查都依赖 tokenizer 已注册(在 `store::open` 之后执行)。
**完成标准**:重建后结果集与增量索引一致;完整性检查能检出并修复 FTS 不一致。

### 阶段 5:发布

CI 发版、deb 结构校验、Win7 x64/x86 交叉编译与导入表审计、README 写「启动失败排查」
(照抄 excelookup 的组织方式:日志路径、`.1` 轮转、动态链接错误、崩溃留痕、兜底提示链),
并补一段「索引文件只应由本程序打开」的说明(自定义 tokenizer 的必然要求)。

## 12. 风险与待决项

**风险**
1. **anydoc 在 Win7 目标上的可编译性**(最高):`pdf-inspector` 依赖树较宽(`rayon`、`regex`、
   `ttf-parser`、`lopdf`、`env_logger`),可能出现高版本 Windows API 导入或不支持 `build-std`。
   阶段 0 先证。
2. **vendored 胶水的 unsafe**:约 330 行第三方 unsafe(虽为 MIT/Apache,且被三个成品 tokenizer 使用)。
   对策是 vendor 进来、纳入审计,并写一条「内存库注册 + 建表 + 查询」的冒烟测试;
   若评估后不愿接受任何 unsafe,退回方案 B(预分段 + 内建 `unicode61`),查询/过滤/SQL 代码不变。
3. **索引体积**:逐字索引的 posting 天然比词级索引长(中文每个字都要发一次 posting),
   叠加文本两份,整体约为原文的 2–3 倍量级。设置页暴露索引体积与 `optimize` 入口。
4. **逐字索引的过命中**:标点与空白不产生词元,「文、档」与「文档」的词元位置相邻,
   短语查询会误配。用 §6.4 的二次精确过滤(核对原文子串)兜住,代价是每处命中一次子串比较。
5. **PDF 文本质量下降**:`pdf-inspector` 与本机 pdfjs 不是同一实现,复杂版面(多栏、表格)
   的阅读顺序可能更差,且没有页坐标,结果无法定位到页。若不接受,PDF 需单独接 `pdfium`(原生库)或推迟。
6. **jieba 词典体积**:约 5 MB 常驻二进制。已做成可关 feature;关闭后只有精确模式。
7. **X11 之外的桌面**:wayland-only 桌面(新版 UOS)下 eframe 的 x11 特性无法启动;
   当前按「X11 优先」处理,启动日志会明确提示,后续按需加 wayland 特性(会牺牲精简性)。

**待决项(需要产品判断,不阻塞阶段 0/1)**
1. 产品名与包名:暂用 `rsou`(工作目录名),中文名待定。
2. PDF 是否进 MVP:建议进,但按「纯文本、无坐标」验收;版面质量不可接受则推迟。
3. OCR 是否预留:建议只在 `documents` 保留 `NEEDS_OCR` 语义与提示文案,不预留引擎接口。
4. 是否需要「监视文件夹自动导入」:本方案只做手动导入 + 变更重解析,自动监视留待后续。
5. 多实例并发:多进程开同一个库(WAL 可读并发)会导致重复解析。建议阶段 1 起加单实例锁。
6. 拼音搜索与简繁归一化:生态有现成件(`pinyin` 零依赖、96 万下载;`opencc-jieba-rs` 依赖偏重)。
   建议先不做,tokenizer 里预留 `1` 参数位与 COLOCATED 词元的位置。
