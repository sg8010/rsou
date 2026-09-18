# rsou

一个本地文档资料库与全文检索桌面应用:把 Word / Excel / PPT / PDF / 文本 / 电子书
放进资料库,之后即可对正文与标题做中文全文检索。

解析、分块、索引全部在本机完成,不联网、不上传;索引存成单个 SQLite 文件,
文档库有多大、索引占多大,设置页里直接看得到。

## 功能

- **支持的格式**(按扩展名识别,不支持的文件导入时直接跳过)

  | 类型 | 扩展名 |
  |---|---|
  | Word | doc docx docm odt rtf |
  | Excel | xls xlsx xlsm xlsb ods csv |
  | PowerPoint | ppt pps pot pptx pptm ppsx ppsm odp |
  | PDF | pdf |
  | 文本 | txt md |
  | 电子书 | epub |

- **解析**:Office/PDF/EPUB 由 anydoc 转 Markdown;txt/md 先按 UTF-8 严格解码,
  失败自动回退 GB18030;解析失败的文件记入失败清单,附中文原因(加密、损坏、
  需要 OCR、超体积等)
- **导入**:文件或整个文件夹递归;按内容哈希去重,未变更的文档跳过,改过的自动
  重解析;可强制全部重导;导入进度与取消按钮;失败清单支持单条重试/移除
- **检索语法**

  | 写法 | 含义 |
  |---|---|
  | `文档 管理` | 隐式 AND:两个词都要命中 |
  | `a OR b` | 或:任一侧命中(两侧都必须是正向条件) |
  | `合同 -草稿` | 排除:命中合同但不命中草稿(`NOT` 同义) |
  | `"公文 号"` | 引号短语:按原样整体匹配(可含空格) |
  | `title:合同` | 只查标题列 |
  | `content:合同` | 只查正文列 |
  | `(a b) OR c` | 括号分组 |

  不支持 `*` 通配符与 NEAR;词里出现 `*`/`"` 会直接报语法错误。

- **检索模式**:**精确**(默认)按输入原样做短语匹配;**宽松**先用 jieba 把
  查询切成若干段、各段都要命中,能找回「文档…管理」这类非连续文本。
  编译时去掉 `jieba` feature 后宽松自动退化为精确
- **检索语义是文档级的**:多个词只要出现在**同一篇文档**里就算命中,
  不会被分块边界切断;短语可以跨段落、跨章节匹配(`文 档` 也算命中,`文、档` 不算)
- **结果展示**:按文档分组,标题/片段两级高亮;片段按原文分块切出,点击片段
  右侧预览原文并自动定位;可按文件类型、目录前缀、时间范围(最近 7 天/30 天/
  一年)过滤;每篇最多展示 20 个片段(超出只计入首段的命中数)
- **索引维护**(设置页):完整性检查(SQLite + FTS 自检 + 双向差集 + 标题/正文抽样)、
  重建全文索引、optimize + VACUUM、清空资料库(二次确认)
- **单文件体积上限**:默认 100 MB,可在设置页调 1–2048 MB
- **中文界面**:Windows / Linux 均自动使用系统中文字体
- **Linux 不依赖外部组件**:文件选择用内置对话框(egui 自绘),不需要 GTK /
  XDG Portal / zenity 这些精简桌面上常常缺失的东西

## 下载

- **Linux arm64**(麒麟 / UOS / 树莓派等):从 Releases 下载 Debian 软件包或裸二进制
- **Windows 7+ x64 / x86**:单个 exe,免安装;见下方「从源码构建」

### Debian 软件包安装

Debian 软件包按系统规范安装,并自动创建桌面菜单项:

```bash
sudo dpkg -i ./rsou_版本_arm64.deb
# 或:sudo apt install ./rsou_版本_arm64.deb
# 启动器:/usr/bin/rsou(真正的二进制在 /usr/lib/rsou/rsou)
```

`/usr/bin/rsou` 是一个启动脚本,负责把启动日志写到固定位置(见下),真正的
程序在 `/usr/lib/rsou/rsou`。包内已声明 X11/OpenGL 运行库依赖——这些库
由 glutin/winit 在运行时动态加载,`dpkg` 从二进制的依赖表里看不到,不显式声明的
话会出现“包装得上、一启动就退出”。完整依赖列表:`libgl1 | libgl1-mesa-glx,
libegl1, libx11-6, libx11-xcb1, libxcb1, libxkbcommon0, libxkbcommon-x11-0,
libxcursor1, libxi6, libxrandr2, libxfixes3, libxrender1`(另推荐
`libgl1-mesa-dri`)。

如果只下载裸二进制文件,可直接运行:

```bash
chmod +x rsou-linux-arm64
./rsou-linux-arm64
```

直接用裸二进制时日志照常写入,只是脚本那一层负责的记录(见下)不参与。

## 使用

1. **资料库**页 → 「添加文件」或「添加文件夹」(文件夹递归导入);导入中可看
   进度条并取消;失败文件在「显示失败清单」里查看中文原因并重试/移除
2. **检索**页 → 输入关键词回车;按需切换「精确/宽松」、字段(全部/标题/正文)、
   类型、目录前缀与时间范围;点击左侧片段,右侧预览自动定位高亮
3. **设置**页 → 查看数据位置与索引统计;做完整性检查、重建索引、优化或清空;
   调整单文件体积上限



## 数据目录与日志

索引文件 `index.sqlite3`、临时目录 `tmp/` 放在应用数据目录:

- **Linux**:`$XDG_DATA_HOME/rsou`,未设置时 `~/.local/share/rsou`
- **Windows**:`%LOCALAPPDATA%\rsou`,未设置时 `%APPDATA%\rsou`
- 设置环境变量 `RSOU_DATA_DIR` 可覆盖(测试与便携场景用)

启动日志位置见「启动失败排查」一节;数据目录与日志路径在设置页也能直接看、
直接打开。

## 索引文件只应由本程序打开

`index.sqlite3` 里的全文表 `documents_fts` 使用的是**本程序内置注册的自定义
分词器 `rsou`**(参数 `0`,关闭拼音)。SQLite 的分词器注册是 per-connection
的:用系统 `sqlite3` 命令行或第三方工具打开这个库后,对 `documents_fts` 执行
`MATCH` 会报 `no such tokenizer: rsou`;普通表的查询、`integrity_check`
不受影响。

因此:只读翻看 `documents`/`chunks` 等普通表没有问题;任何涉及 `documents_fts`
的读写请通过本程序 (GUI) 进行,不要手工改这个文件。



## 启动失败排查

程序启动时会检查桌面显示环境和图形运行库。若窗口无法创建,会直接提示底层错误,
并把 eframe / OpenGL 初始化日志写入:

```text
~/.cache/rsou/startup.log
```

如果系统设置了 `XDG_STATE_HOME`,日志位于
`$XDG_STATE_HOME/rsou/startup.log`。日志中包含程序架构、内核版本、
`DISPLAY` 等桌面环境变量以及 X11/OpenGL 动态库探测结果,反馈问题时请一并提供该文件。
时间戳是 UTC(形如 `2026-09-15T00:32:14Z`),后面跟的 `+0.012s` 是相对本次启动的
耗时,卡在哪一步、卡了多久可以直接看出来。图形初始化期间的 glutin/winit 调试细节
只记到界面显示为止——之后收回到 Info 级别,否则空闲时每帧的窗口调用会把日志写成一个
很大的文件。

排查时还需要知道这几件事:

- **`startup.log.1` 是上一次启动的日志。** 每次启动都会把上一个日志轮转成 `.1`,
  所以“失败一次、再启动一次就正常”这种情况下,失败证据仍然保留在 `.1` 里。
  偶发故障请把两个文件一起提供。
- **动态链接器报错也在日志里。** 缺少运行库、glibc 版本不够这类失败发生在程序自己
  的代码运行之前,程序来不及写日志;Debian 包的启动脚本会先把标准错误重定向进同一
  个日志文件,所以“双击了但没有任何反应”时,日志开头可能有
  `error while loading shared libraries: ...` 这样的内容。裸二进制没有这一层。
- **崩溃会留下最后一条记录。** 段错误、总线错误、非法指令、中止这些会直接杀死进程
  的信号,会在日志末尾补一行 `异常终止: 收到信号 11 (段错误)`,紧邻的上一行就是
  崩溃前最后执行的步骤。被 `SIGKILL`(例如 OOM killer)杀掉时无法捕获,日志只会
  中断在最后一条记录上。
- **看不到提示时日志里有原因。** 程序会依次尝试 zenity / kdialog / xmessage /
  notify-send / x-terminal-emulator / xdg-open 来展示错误;目标机上这些工具可能都
  没有,此时用户看不到任何提示。每次尝试的结果都会记进日志,可以用它确认用户到底
  看到过什么。
- **30 秒看门狗。** 首帧如果在 30 秒内没能画到屏幕上(图形栈卡死、显示服务不响应),
  程序会按启动超时自行退出并在日志里记明原因,不会无声地挂着。

Linux 图形版需要 X11 和 OpenGL/EGL 运行库;不同设备的显卡驱动和运行库可能不同,
所以“系统版本相同”不代表运行环境完全相同。**Wayland-only 桌面**(没有 XWayland
的新版桌面)当前无法启动:eframe 只启用了 x11 特性,启动日志会明确提示这一点。

## 已知限制

- 取消导入是文件粒度:已开始的单文件解析不可中断(解析器没有取消接口)
- PDF 只提取纯文本:没有页坐标、没有页图,结果无法定位到页;多栏/表格版面
  的阅读顺序可能不理想
- 扫描件 PDF 没有可提取文本,记为“需要 OCR”,本版本不支持 OCR
- 内置文件对话框是单选,每次「添加文件」只能选一个(文件夹导入不受影响)
- Windows 上表格的“更新时间”按固定 UTC+8 显示(不接系统时区检测)
- 宽松模式内嵌约 5 MB 的 jieba 词典;不需要宽松模式时可用
  `cargo build --release --no-default-features` 去掉

## 从源码构建

### 环境要求

- Rust stable(2024 edition)
- Linux 桌面需系统自带 CJK 字体(Noto Sans CJK / 文泉驿等),Windows 用微软雅黑,均可自动识别
- Linux 的文件对话框是内置的,不需要额外安装 GTK / XDG Desktop Portal / zenity

### Linux 本机

```bash
cargo build --release            # 产物：target/release/rsou
```

### Linux arm64 deb(走 CI)

打 tag 即自动在 GitHub Actions 的 arm64 runner(容器内 glibc 2.28)上测试、
编译、打包并发布 Release;也可以在装有 cargo-deb 的 arm64 机器上本地
`cargo deb`。

### Windows 7+ x64/x86(在 Linux 上交叉编译)

```bash
# 依赖: mingw-w64 + rustup
sudo apt install -y \
  gcc-mingw-w64-x86-64 binutils-mingw-w64-x86-64 \
  gcc-mingw-w64-i686 binutils-mingw-w64-i686

# 脚本会自动安装 nightly 的 rust-src(Win7 target 没有预编译 std)
./scripts/build-win.sh           # 同时构建 x64 与 x86
```

产物分别为:

```text
target/x86_64-win7-windows-gnu/release/rsou.exe
target/i686-win7-windows-gnu/release/rsou.exe
```

该构建使用 Rust 官方 `x86_64-win7-windows-gnu` / `i686-win7-windows-gnu`
target 与 nightly `build-std`、release `fat LTO`,并在构建结束时用
`scripts/check-win7-imports.sh` 审计两个 PE 的导入表(不引用 Windows 8+
的 API)。交叉链接器配置见 `.cargo/config.toml`(分别使用
`x86_64-w64-mingw32-gcc` 和 `i686-w64-mingw32-gcc`)。

## 测试

```bash
cargo test -p rsou   # 解析/文本化/分块/导入/查询/检索/维护全链路单测
```

## 技术栈

| 层 | 选型 |
|---|---|
| GUI | egui / eframe 0.36(即时模式,自带虚拟化表格) |
| 文档解析 | anydoc 0.2.4(Office/PDF/EPUB → Markdown)+ encoding_rs(txt/md 编码探测) |
| 全文检索 | SQLite FTS5(bundled)+ 自注册 `rsou` 分词器(vendored rusqlite-ext);`documents_fts` 一行一篇文档 |
| 中文分词 | jieba-rs(仅宽松模式查询用,可关 feature) |
| 文件对话框 | Linux:内置 egui 对话框;Windows:rfd(原生) |
| 并行导入 | rayon 解析 + 单线程串行写库 |
| 核心逻辑 | 独立 lib(`rsou_lib`),与 GUI 解耦、可单测 |

代码结构:

```
src/
├── main.rs        # GUI 入口
├── app.rs         # 应用状态 + App::ui;src/app/ 下按职责拆子模块
├── startup.rs     # 启动诊断:启动日志、超时看门狗、崩溃留痕、错误提示
├── file_dialog.rs # 内置文件对话框(Linux;不依赖 Portal / zenity)
├── lib.rs         # 库入口(纯逻辑,不依赖 egui)
├── store.rs       # 索引库打开/PRAGMA/schema;tokenizer 注册的唯一入口
├── tokenize.rs    # 自定义 FTS5 分词器 rsou
├── parse.rs       # 扩展名路由 + anydoc/文本解析 + 中文错误映射
├── text.rs        # Markdown → 纯文本(保留块级字节偏移)
├── chunk.rs       # 标题感知分块(400–900 字符;只用于展示片段)
├── repo.rs        # documents/contents/chunks/fts/import_*/settings 读写
├── import.rs      # 扫描 + rayon 并行解析 + 串行写库 + 进度/取消
├── query.rs       # 检索语法编译(照搬 wsou query-parser 的规则子集)
├── search.rs      # FTS 查询(文档级)+ 原文定位 + 按展示分块切片段
├── maintain.rs    # 统计 / 完整性检查 / 重建 / optimize / 清空
├── filebrowser.rs # 目录列举/排序/过滤等文件浏览纯逻辑
build.rs           # Windows 目标时把 assets/icon.ico 嵌入 exe(交叉编译也生效)
assets/            # icon.png / icon.ico、rsou-launcher.sh、rsou.desktop
tests/             # 解析 fixture 矩阵与端到端导入测试
```

## 发布流程

打 tag 即自动在 GitHub Actions 的 arm64 runner 上测试、编译并发布 Release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

## License

MIT(待补 LICENSE 文件)
