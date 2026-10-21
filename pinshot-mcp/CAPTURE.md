# 原生截图与编辑会话

完整启动、工具和验收说明见 [README.md](README.md)。

当前 MCP 连接运行中的 `pinshot.exe --automation`，会话位于 Rust 原生进程内。
截图复用 `Capture::grab` 的 DXGI/GDI 回退链路，标注与预览复用 `Shape`、`render::compose`。

先编译并启动本分支 PinShot，再执行 `npm run build` 和 `npm run test:workflow`。
该验收会验证截图、添加标注、预览、保存、防止覆盖、删除和关闭会话。

`--capture-to <output.png>` 保留为单次截图 CLI；MCP 会话工具已改用本机双向命名管道。
