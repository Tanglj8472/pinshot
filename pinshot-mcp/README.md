# pinshot-mcp

用于练习 AI 驱动 PinShot 的 MCP stdio 服务。Node 服务负责协议，运行中的 Rust PinShot 负责捕获、会话、原生标注和渲染。

## 构建与启动

```powershell
cd rust
cargo build --release --bin pinshot
# 先退出旧 PinShot；自动化服务需要在主实例启动时开启。
.\target\release\pinshot.exe --automation
```

另一个终端：

```powershell
cd pinshot-mcp
npm ci
npm run build
npm run smoke
npm run test:workflow
```

普通 smoke 只测试工具发现与 ping；test:workflow 会截取真实桌面，添加方框、读取预览、保存到临时目录，验证拒绝覆盖、删除标注和关闭会话，最后清理测试文件。需要可用的 Windows 交互桌面和启用 --automation 的本分支程序。

## 连接 Codex

```powershell
codex mcp add pinshot-mcp -- node E:\Tool\pinshot\pinshot-mcp\dist\index.js
```

使用自己的绝对路径。重载 MCP 或重启 Codex 后，可以说：

> 截图，在左上角画一个红色方框，给我看预览，然后保存到指定 PNG 路径。

## 工具

| 工具 | 用途 |
| --- | --- |
| ping | 测试 MCP 连接 |
| get_state | 查询原生进程 PID、会话、尺寸、桌面原点、后端和标注 ID |
| capture_screen | 原生截取全部显示器，建立会话并返回 session_id 与 PNG |
| add_annotation | 添加 rect、ellipse、line、arrow、text、blur、mosaic |
| remove_annotation | 按 annotation_id 删除标注 |
| preview_session | 返回原生渲染的 PNG |
| save_session | 保存到绝对 PNG 路径，拒绝覆盖已有文件 |
| close_session | 释放会话，并关闭对应预览窗口 |
| show_session | 显示或重新打开会话预览 |
| hide_session | 关闭预览但保留会话 |

`capture_screen` 默认同时打开原生预览窗口，可传 `show: false` 仅创建后台会话。预览初始适配 960×640 范围，支持拖动、滚轮缩放，右键或 Escape 关闭。AI 添加、删除标注会刷新同一窗口；手动关闭预览后会话仍保留。预览不写入普通贴图历史，不提供手工标注编辑，避免与 AI 会话产生不一致。

坐标使用图片左上角为原点的物理像素。text 使用一个点，其余标注使用两个点。width 表示线宽或字号，strength 表示模糊/马赛克程度。最多保留四个会话，每个最多一千个标注。

## 通信和范围

MCP stdio → Windows 本机命名管道 PinShotAutomation-v1 → PinShot 自动化线程 → Capture::grab / pinshot-core::Shape / render::compose。

只有启动参数 --automation 才启用控制接口。截图和渲染在独立线程执行，自动化会话保存在原生进程内；已接入独立原生贴图预览；当前还未与用户手工编辑的截图窗口双向同步，也未提供 OCR、翻译、普通贴图和设置控制。

本阶段自动化传输仅支持 Windows。保留 --capture-to <output.png> 作为独立的一次性原生截图 CLI，MCP 会话工具使用常驻接口。

预览临时文件调用后清理。stdout 只传 MCP 消息，日志写 stderr；客户端配置直接使用 node dist/index.js，不要使用 npm start。
