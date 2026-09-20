# WSL 下运行 rsou GUI 与截图验证

记录在无桌面 / WSL 环境里启动 rsou 界面、灌测试数据、截图和模拟键鼠输入的完整方法。

## 1. 直接运行(WSLg)

WSLg 内置 X/Wayland 显示服务,`DISPLAY` / `WAYLAND_DISPLAY` 默认就绪,直接跑:

```bash
cargo run -p rsou --bin rsou    # 包内还有 rsou-cli,必须 --bin 指定
```

`libEGL warning: DRI3 error` 是软件渲染警告,不影响界面渲染。

非登录 shell 里 cargo 可能不在 PATH:`source "$HOME/.cargo/env"`。

## 2. 隔离数据目录

默认数据目录是 `$XDG_DATA_HOME/rsou`(否则 `~/.local/share/rsou`)。
测试时用 `RSOU_DATA_DIR` 指到临时目录,不污染真实数据:

```bash
RSOU_DATA_DIR=/tmp/rsou_data cargo run -p rsou --bin rsou
```

## 3. 灌入测试数据(rsou-cli)

GUI 没有 CLI 导入口,用仓库自带的 `rsou-cli`(`src/bin/cli.rs`)直接灌库:

```bash
cargo build
./target/debug/rsou-cli --db /tmp/rsou_data/index.sqlite3 import <文件或目录...>
```

子命令还有 `search` / `parse` / `text` / `clear` 等,见 `rsou-cli` 无参运行或 cli.rs 顶部注释。
注意:必须经 `--db` 打开库而不是系统 sqlite3 CLI——库里注册了自定义 tokenizer,
直接 MATCH 会报错。

## 4. 无头验证:Xvfb + ffmpeg 截图 + xdotool 输入

### 4.1 启动虚拟显示器和应用

```bash
Xvfb :99 -screen 0 1440x900x24 &
RSOU_DATA_DIR=/tmp/rsou_data DISPLAY=:99 setsid ./target/debug/rsou >/tmp/rsou_run.log 2>&1 &
DISPLAY=:99 xdotool search --name "rsou"    # 拿窗口 id
```

`setsid` 防止 shell 退出时带走进程。

### 4.2 截图

最小镜像没有 `xwd`,用 ffmpeg 的 x11grab:

```bash
ffmpeg -y -f x11grab -video_size 1440x900 -i :99 -frames:v 1 /tmp/shot.png
```

`-video_size` 要等于 Xvfb 的 `-screen` 尺寸;比窗口大的部分是黑边,正常。

### 4.3 键鼠输入(xdotool)

Xvfb 没有窗口管理器,`xdotool windowactivate` 不生效,焦点停在 root 窗口
(报错特征:`XGetInputFocus returned the focused window of 1`)。
必须先 `windowfocus` 直接设置 X 输入焦点:

```bash
DISPLAY=:99 xdotool windowfocus <窗口id>
DISPLAY=:99 xdotool mousemove X Y click 1   # 点击
DISPLAY=:99 xdotool type --delay 80 "测试"   # 输入,支持中文
DISPLAY=:99 xdotool key ctrl+a Delete        # 组合键 / 单键
```

点击坐标直接从截图像素位置读;窗口在 (0,0) 时屏幕坐标 = 窗口坐标。

### 4.4 高 DPI / 缩放验证

合成按键模拟 Ctrl+± 不一定能到应用,改用 winit 环境变量:

```bash
WINIT_X11_SCALE_FACTOR=1.5 DISPLAY=:99 ./target/debug/rsou
```

## 5. 清理

```bash
pkill -f 'target/debug/rsou$'   # $ 锚定,避免误杀 rsou-cli
# Xvfb 可留着复用;pkill Xvfb 关闭
```

## 6. 常见坑速查

| 现象 | 原因 / 解法 |
|---|---|
| `xdotool type` 打不进字 | 焦点在 root:先 `xdotool windowfocus <id>` |
| `xwd: command not found` | 没装,改用 ffmpeg x11grab |
| `cargo: command not found` | 非登录 shell:`source "$HOME/.cargo/env"` |
| libEGL / DRI3 warning | WSLg 软件渲染,可忽略 |
| 改了代码界面没变化 | 确认跑的是新二进制:`cargo build` 后重启进程 |
