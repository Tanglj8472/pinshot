import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { requestNative, preview } from "./automation.js";

const SERVER_NAME = "pinshot-mcp";
const SERVER_VERSION = "0.2.0";
const DEFAULT_PONG = "pong";

/**
 * 组装最小 MCP Server。
 * Native PinShot automation tools over a local named pipe.
 */
export function createServer(): McpServer {
  const server = new McpServer({
    name: SERVER_NAME,
    version: SERVER_VERSION,
  });

  server.registerTool(
    "ping",
    {
      title: "Ping",
      description: "连通性检查。返回 message 文本；省略时返回 pong。",
      inputSchema: {
        message: z
          .string()
          .optional()
          .default(DEFAULT_PONG)
          .describe("要返回的文本，默认 pong"),
      },
    },
    async ({ message }) => ({
      content: [{ type: "text", text: message }],
    }),
  );

  const failure = (error: unknown) => ({
    isError: true,
    content: [{ type: "text" as const, text: error instanceof Error ? error.message : String(error) }],
  });
  const withPreview = async (result: Record<string, unknown>) => {
    const image = await preview(Number(result.session_id));
    return { content: [
      { type: "text" as const, text: JSON.stringify({ ...result, preview: { width: image.width, height: image.height, scale: image.scale } }) },
      { type: "image" as const, mimeType: "image/png", data: image.data },
    ] };
  };
  const capture = async (args: Record<string, unknown>) => {
    try {
      const result = await requestNative("capture", args);
      try { return await withPreview(result); } catch (error) {
        await requestNative("close", { session_id: result.session_id }).catch(() => {});
        throw error;
      }
    } catch (error) { return failure(error); }
  };
  const PREVIEW_NOTE = "返回的图片是长边不超过 1600 像素的缩略图；标注坐标必须使用原图像素，即缩略图坐标除以 preview.scale。";

  server.registerTool(
    "capture_screen",
    {
      title: "截取屏幕",
      description: `立即截取 Windows 全部显示器的当前画面，返回 PNG 图片。无需手动框选。${PREVIEW_NOTE}`,
      inputSchema: { show: z.boolean().default(true).describe("是否同时显示 PinShot 原生预览窗口") },
    },
    async ({ show }) => capture({ show }),
  );

  server.registerTool("list_windows", {
    title: "列出窗口",
    description: "列出当前可见的顶层窗口，按从上到下的层叠顺序返回标题和屏幕物理像素位置。用于为 capture_window 选择标题关键字。",
    inputSchema: {},
  }, async () => {
    try { return { content: [{ type: "text" as const, text: JSON.stringify(await requestNative("windows")) }] }; }
    catch (error) { return failure(error); }
  });

  server.registerTool("capture_window", {
    title: "截取窗口",
    description: `按标题关键字（不区分大小写，取层叠最上面的匹配）截取单个窗口，效果与 PinShot 手动吸附窗口一致。默认先把窗口切到最前面再截，返回的 window.activated 表示是否切换成功。${PREVIEW_NOTE}`,
    inputSchema: {
      title: z.string().min(1).max(512).describe("窗口标题关键字，可先用 list_windows 查看"),
      activate: z.boolean().default(true).describe("截图前是否把窗口切到最前面；为 false 时被遮挡部分会截到上层窗口"),
      show: z.boolean().default(true).describe("是否同时显示 PinShot 原生预览窗口"),
    },
  }, async ({ title, activate, show }) => capture({ window: title, activate, show }));

  const session = z.number().int().positive().max(Number.MAX_SAFE_INTEGER);
  const point = z.object({ x: z.number().finite().nonnegative(), y: z.number().finite().nonnegative() });
  const guarded = async (action: () => Promise<Record<string, unknown>>) => {
    try { return { content: [{ type: "text" as const, text: JSON.stringify(await action()) }] }; }
    catch (error) { return { isError: true, content: [{ type: "text" as const, text: error instanceof Error ? error.message : String(error) }] }; }
  };
  server.registerTool("get_state", {
    description: "查询运行中的 PinShot、截图会话、图片尺寸、桌面原点和标注 ID。标注坐标为图片左上角起的物理像素。",
    inputSchema: {},
  }, async () => guarded(() => requestNative("status")));
  server.registerTool("add_annotation", {
    description: "向截图会话添加原生标注。text 使用一个点，其余使用两个点；width 为线宽或字号，strength 为模糊/马赛克程度。返回 annotation_id。",
    inputSchema: {
      session_id: session,
      kind: z.enum(["rect", "ellipse", "line", "arrow", "text", "blur", "mosaic"]),
      points: z.array(point).min(1).max(2),
      text: z.string().max(4096).optional(),
      width: z.number().int().min(1).max(200).optional(),
      strength: z.number().int().min(1).max(30).optional(),
      color: z.object({ r: z.number().int().min(0).max(255), g: z.number().int().min(0).max(255), b: z.number().int().min(0).max(255), a: z.number().int().min(0).max(255).default(255) }).optional(),
    },
  }, async ({ session_id, ...annotation }) => guarded(() => requestNative("annotate", { session_id, annotation })));
  server.registerTool("remove_annotation", {
    description: "删除指定会话中的指定标注。", inputSchema: { session_id: session, annotation_id: session },
  }, async (args) => guarded(() => requestNative("remove", args)));
  server.registerTool("preview_session", {
    description: `返回包含所有标注的原生 PNG 预览。${PREVIEW_NOTE}`, inputSchema: { session_id: session },
  }, async ({ session_id }) => {
    try { return await withPreview({ session_id }); }
    catch (error) { return failure(error); }
  });
  server.registerTool("save_session", {
    description: "将带标注截图保存为 PNG，path 必须是绝对路径，父目录必须存在，已有文件不会覆盖。",
    inputSchema: { session_id: session, path: z.string().min(1) },
  }, async (args) => guarded(() => requestNative("save", args)));
  server.registerTool("close_session", {
    description: "关闭截图会话并释放内存；最多同时保留四个会话。", inputSchema: { session_id: session },
  }, async (args) => guarded(() => requestNative("close", args)));

  server.registerTool("show_session", {
    description: "显示或重新打开截图会话的原生贴图预览。AI 后续添加、删除标注会自动刷新；可拖动、滚轮缩放，右键或 Escape 关闭预览。",
    inputSchema: { session_id: session },
  }, async (args) => guarded(() => requestNative("show", args)));
  server.registerTool("hide_session", {
    description: "关闭会话的可见预览，保留截图和标注，可使用 show_session 重新打开。",
    inputSchema: { session_id: session },
  }, async (args) => guarded(() => requestNative("hide", args)));

  server.registerTool("edit_session", {
    description: "打开原生标注工具栏并选择工具，供用户继续手工编辑。手工改动会同步到 get_state、预览和导出；未完成的笔迹/文字编辑会阻止 AI 覆盖。",
    inputSchema: {
      session_id: session,
      tool: z.enum(["select", "rect", "ellipse", "line", "arrow", "pen", "marker", "mosaic", "text", "number", "eraser"]).default("select"),
    },
  }, async (args) => guarded(() => requestNative("edit", args)));
  server.registerTool("undo_session", {
    description: "在可见原生编辑器中撤销一次标注编辑；手工和 AI 编辑使用同一撤销历史。", inputSchema: { session_id: session },
  }, async (args) => guarded(() => requestNative("undo", args)));
  server.registerTool("redo_session", {
    description: "在可见原生编辑器中重做一次标注编辑。", inputSchema: { session_id: session },
  }, async (args) => guarded(() => requestNative("redo", args)));

  return server;
}

async function main(): Promise<void> {
  const server = createServer();
  const transport = new StdioServerTransport();
  await server.connect(transport);
  // stdout 是 JSON-RPC 通道，只能往 stderr 打日志。
  console.error(`${SERVER_NAME} ${SERVER_VERSION} 已通过 stdio 启动`);
}

main().catch((error: unknown) => {
  const text =
    error instanceof Error ? (error.stack ?? error.message) : String(error);
  console.error(text);
  process.exit(1);
});
