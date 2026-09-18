# rsou 的 EGL 优先补丁

此目录基于 crates.io 的 `glutin-winit 0.5.0`。

上游 `eframe 0.36.1` 固定传入 `ApiPreference::FallbackEgl`，在同时启用 EGL 和
GLX 的 Linux X11 构建中会先尝试 GLX。目标 UOS 20 1031 使用 Mesa 18.3.6，
该 GLX 路径会在首窗显示时异步返回 `GLXBadContextTag`，使 winit panic。

本地补丁将 Linux/X11 下 `FallbackEgl` 的实际顺序从 `GlxThenEgl` 改为
`EglThenGlx`：

- 正常情况下通过 EGL 创建 OpenGL 上下文，绕开有问题的 GLX 请求；
- EGL 不可用时仍可回退到 GLX；
- Windows 的 `WglThenEgl` 逻辑保持不变。

升级 eframe/glutin-winit 时，应先确认上游是否已经开放图形 API 偏好配置；若已
开放，应删除本地补丁并在应用代码中直接选择 EGL 优先。
