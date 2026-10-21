# 开发与贡献

维护对象为 `rust/` 工作区，集成分支为 `rust`。不要重新加入旧 Python/Qt 应用、IDE 缓存、编译产物或模型。旁路 MCP 学习包在 `pinshot-mcp/`，不要并进 Rust 工作区。

- 跨平台逻辑与渲染放在 `pinshot-core`，窗口及平台 API 放在 `pinshot`。
- 修改交互时同时检查截图浮层与贴图窗口，补充回归测试。
- 提交前执行 `cargo test --workspace` 和 `cargo test --workspace --features ocr`。
- 发布使用 `tools/package-windows.ps1`，检查安装、启动与卸载。
- 标记 ignored 的性能测试需单独显式运行。
- 不提交令牌、API Key、用户截图、配置或日志。

发布步骤见 [docs/releasing.md](docs/releasing.md)。被移除的 Python 实现可通过 Git 历史恢复。
