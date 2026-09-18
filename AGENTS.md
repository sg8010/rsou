# AGENTS.md

rsou:Rust + egui/eframe 0.36 的本地文档资料库与全文检索应用。交付 Linux arm64(CI)与 Windows 7+ x64/x86(交叉编译)。

## 约定(代码看不出来)

- **lib/bin 严格分离**:解析/分块/索引/检索/维护都在 `rsou_lib`(`src/lib.rs` 以下),**lib 零 egui 依赖**,先 lib+单测再接 UI;GUI(`src/app.rs` + `src/app/*` 子模块,`impl RsouApp` 分文件)与 `rsou-cli` 都只调 lib
- **tokenizer 注册只有一处**:`store::open`/`open_in_memory` 是所有连接的唯一入口(per-connection 注册 `rsou` tokenizer);不要在别处 `Connection::open`,否则 `chunks_fts` MATCH 报 no such tokenizer
- **所有用户可见文案必须中文**(产品名与技术名词除外);错误文案进 `ParseError.message`/页面状态行,不弹窗刷屏
- **GUI 改动后的端到端验证由用户手动测试**,不要写程序自动截图/自动开库验证 UI;core 逻辑改动跑 `cargo test -p rsou`(内置文件对话框保留无头渲染冒烟,只防 panic)
- 写库纪律:文档「内容+分块+FTS」一个 `BEGIN IMMEDIATE` 事务;`chunks_fts.rowid == chunks.id`,`fts.content == plain_text[start..end]`(字节偏移);GUI 的连接只做读与小写,导入/维护线程自己开写连接
- eframe 0.36 的 `App` trait 入口是 `fn ui(&mut self, ui: &mut egui::Ui, …)`;`egui::FontData` 需包 `Arc`,CJK 字体运行时加载勿内嵌
- **文件对话框**:Linux 用内置 egui 对话框,Windows 用 rfd;请求记 `DialogRequest` 帧末 `drive_dialog()` 统一处理;大数据表格用 `TableBuilder::body().rows()` 虚拟滚动

## 验证命令(代码改动后必跑)

```bash
cargo fmt --all -- --check
cargo clippy -p rsou --all-targets -- -D warnings
cargo build -p rsou --bins
cargo test -p rsou
```

改 Windows 分支(`cfg(windows)`/`cfg(not(linux))`)再补:
`cargo check -p rsou --bins --target x86_64-pc-windows-gnu`。

## 交叉编译 Windows(勿走弯路)

开发环境 = WSL2 Ubuntu + Windows 宿主机。已配置 `.cargo/config.toml` → `linker = x86_64-w64-mingw32-gcc` / `i686-w64-mingw32-gcc`(apt 的 **Linux 原生 mingw**)。Win7 构建走 `./scripts/build-win.sh`(nightly `build-std`,`x86_64-win7-windows-gnu`/`i686-win7-windows-gnu`),结束后用 `scripts/check-win7-imports.sh` 审计 PE 导入表。

已废弃、勿重试:`x86_64-pc-windows-gnullvm` target、zig 当 linker、MSYS2 的 dlltool.exe。

## 发布

- CI 只响应 `v*` tag(main push 不触发),见 `.github/workflows/linux-arm64.yml`
- 发版:`git tag vX.Y.Z && git push origin vX.Y.Z` → 自动测试+构建+deb+Release
- 提交信息用中文

## 本机环境(WSL2,不进仓库)

- 工具链:`~/.cargo/bin`、`~/.local/bin`;cargo 不在 PATH 时先 `export PATH="$HOME/.cargo/bin:$PATH"`
- 外网需代理:HTTP(S)_PROXY=`http://127.0.0.1:10808`(仅环境变量,勿写入仓库)
- 无头跑 GUI:加 `XDG_RUNTIME_DIR=/mnt/wslg/runtime-dir WAYLAND_DISPLAY=wayland-0 LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe`(WSLg 默认 EGL 会崩);常规验证不实跑 GUI
