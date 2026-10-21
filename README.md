# PinShot

PinShot 是一款面向 Windows 的开源截图与屏幕标注工具，使用 Rust 构建。它将区域截图、即时标注、图片贴图、文字识别和翻译整合在一个轻量工作流中。

## 功能

- 快捷键、托盘菜单和命令行启动截图。
- 支持多显示器、窗口区域识别和高 DPI 屏幕。
- 直线、箭头、矩形、椭圆、画笔、记号笔、序号、文字、橡皮擦、马赛克和模糊。
- 标注可编辑，支持移动、调整大小、撤销和取消选择。
- 复制到剪贴板、保存图片、历史记录和截图贴图。
- OCR 文字识别，以及可配置的翻译服务商。
- DXGI 优先的屏幕捕获，并在不兼容环境中自动回退到 GDI。
- 分级运行日志和按日期保存的诊断文件。

## 系统要求

- Windows 10 版本 2004（Build 19041）或更高版本。
- x64 处理器。
- OCR 功能需要下载对应模型文件。

## 快速开始

### 使用发布版本

从 GitHub Releases 下载 Windows 安装包，运行安装程序后即可通过托盘图标或快捷键开始截图。

### 从源码构建

需要安装 Rust stable 工具链和 Windows MSVC 编译工具链。

```powershell
cd rust
cargo run --release --bin pinshot --features ocr -- --debug
```

不需要 OCR 时可以省略 `--features ocr`。

## 常用操作

截图完成后，可以使用工具栏编辑图片。左键拖动用于创建区域和笔迹；右键用于结束部分多步骤操作、取消选择或关闭当前交互。工具栏会记住常用的大小、模糊程度和其他工具参数。

截图可以复制到剪贴板、保存到图片目录或固定到桌面上。贴图窗口支持缩放和拖动。

## 配置与日志

设置页可以配置快捷键、保存目录、文件名格式、语言、OCR、翻译服务商和日志等级。

日志文件保存在 `%APPDATA%\\PinShot`，每天生成一个 TXT 文件，默认只保留最近两天。日志等级包括关闭、普通、详细和调试；排查截图卡顿或兼容性问题时可以临时使用调试级别。

## 项目结构

- `rust/crates/pinshot-core/`：跨模块的数据结构、标注模型、渲染和配置逻辑。
- `rust/crates/pinshot/`：Windows/Linux 平台集成、截图窗口、工具栏、OCR 和翻译。
- `packaging/`：Windows 安装程序配置和安装回归测试。
- `tools/`：构建、运行库检查和系统验证脚本。
- `docs/`：设计说明、日志说明和验证记录。
- `pinshot-mcp/`：旁路最小 MCP Server（stdio + `ping`），不参与截图主程序。见该目录下的中文 README。

## 开发与验证

```powershell
cd rust
cargo fmt --all -- --check
cargo test --locked --workspace
cargo test --locked --workspace --features ocr
cargo check --locked --workspace
```

Windows 安装程序由 GitHub Actions 构建，并执行安装、升级、卸载、用户数据保留和运行库检查。

## 许可

项目源代码和第三方依赖的许可信息请参阅仓库中的许可证及 `packaging/licenses/` 目录。
