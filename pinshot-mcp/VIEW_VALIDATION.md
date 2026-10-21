# 原生可见预览验收

2026-10-19，Windows 本机，使用 Actions 运行 `39225318039` 对原生提交 `40cb2e2` 构建的 pinshot.exe。

- TypeScript 编译、十个 MCP 工具发现和 ping 测试通过。
- 原生 Windows 普通/OCR 测试通过，Release 程序构建成功；Linux 运行 `39225318042` 完整通过。
- `npm run test:workflow` 本机运行成功，退出码 0。
- 原生捕获 4160×1400 桌面，本次后端 gdi-virtual。
- capture_screen 默认建立可见原生预览；添加七类标注后查询到相同窗口 ID。
- hide_session 关闭窗口并保留会话；show_session 成功重新打开。
- 删除标注后窗口 ID 保持不变；close_session 移除窗口和会话。
- show:false 截图及其后续标注均未创建窗口。
- 原有 PNG 预览、导出和拒绝覆盖验证仍通过。
- 实验使用独立配置与数据目录，导出临时文件自动清理，结束后恢复原来的 PinShot。

预览支持拖动、滚轮缩放、右键/Escape 关闭；上述自动测试没有模拟这些物理输入，也没有验证手工编辑与 AI 双向同步。当前为只读可见预览。
