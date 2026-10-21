# 原生会话验收

2026-10-19，Windows 本机，使用 GitHub Actions 对提交 `6a3ac81` 构建的无 OCR 原生程序。

- Windows 构建 `39220988317` 完整通过；Linux 构建 `39220988195` 完整通过。
- `npm run build` 和 `npm run smoke` 通过。
- `npm run test:workflow` 通过，退出码 0。
- 从同一原生进程捕获 `4160x1400` 桌面，本次实际后端 `gdi-virtual`，即 PinShot 原生回退链路。
- 添加 rect、ellipse、line、arrow、text、blur、mosaic 七类标注，状态查询确认七个标注。
- 原生渲染预览并保存 PNG，重复保存被拒绝，删除标注并关闭会话成功。
- 测试使用独立配置和数据目录，临时导出在测试结束后删除；实验进程退出后恢复原有 PinShot。

该结果验证 MCP 到常驻原生文档会话的闭环，不表示已验证交互式窗口同步、OCR、翻译或贴图自动化。
