import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const serverEntry = path.resolve(here, "../src/index.ts");

function textFromToolResult(result: unknown): string {
  if (typeof result !== "object" || result === null || !("content" in result)) {
    throw new Error(`unexpected tool result: ${JSON.stringify(result)}`);
  }

  const content = (result as { content: unknown }).content;
  if (!Array.isArray(content)) {
    throw new Error(`tool result.content is not an array: ${JSON.stringify(result)}`);
  }

  const texts = content
    .filter(
      (item): item is { type: "text"; text: string } =>
        typeof item === "object" &&
        item !== null &&
        "type" in item &&
        item.type === "text" &&
        "text" in item &&
        typeof item.text === "string",
    )
    .map((item) => item.text);

  if (texts.length === 0) {
    throw new Error(`no text content in tool result: ${JSON.stringify(result)}`);
  }

  return texts.join("");
}

async function main(): Promise<void> {
  const transport = new StdioClientTransport({
    command: process.execPath,
    args: ["--import", "tsx", serverEntry],
    stderr: "pipe",
  });

  const client = new Client({
    name: "pinshot-mcp-smoke",
    version: "0.1.0",
  });

  await client.connect(transport);

  const listed = await client.listTools();
  const names = listed.tools.map((tool) => tool.name);
  if (names.length !== 15 || !names.includes("edit_session") || !names.includes("undo_session") || !names.includes("redo_session") || !names.includes("show_session") || !names.includes("hide_session") || !names.includes("ping") || !names.includes("capture_screen") || !names.includes("save_session") || !names.includes("list_windows") || !names.includes("capture_window")) {
    throw new Error(`expected 15 automation tools, got: ${names.join(",") || "(empty)"}`);
  }

  const defaultResult = await client.callTool({
    name: "ping",
    arguments: {},
  });
  const defaultText = textFromToolResult(defaultResult);
  if (defaultText !== "pong") {
    throw new Error(`default ping expected "pong", got ${JSON.stringify(defaultText)}`);
  }

  const customResult = await client.callTool({
    name: "ping",
    arguments: { message: "hello-夕下" },
  });
  const customText = textFromToolResult(customResult);
  if (customText !== "hello-夕下") {
    throw new Error(
      `custom ping expected "hello-夕下", got ${JSON.stringify(customText)}`,
    );
  }

  if (process.argv.includes("--capture")) {
    const screenshot = await client.callTool({ name: "capture_screen", arguments: {} });
    if (screenshot.isError || !Array.isArray(screenshot.content)) {
      throw new Error(`Screenshot failed: ${JSON.stringify(screenshot)}`);
    }
    const image = screenshot.content.find((item) => item.type === "image");
    if (!image || image.type !== "image" || image.mimeType !== "image/png") {
      throw new Error("Screenshot returned no PNG image");
    }
    const png = Buffer.from(image.data, "base64");
    if (png.readUInt32BE(0) !== 0x89504e47 || png.readUInt32BE(16) === 0 || png.readUInt32BE(20) === 0) {
      throw new Error("Invalid screenshot PNG");
    }
    console.error(`capture_screen(): PNG ${png.readUInt32BE(16)}x${png.readUInt32BE(20)}, ${png.length} bytes`);
    const session = JSON.parse(textFromToolResult(screenshot)).session_id;
    await client.callTool({ name: "close_session", arguments: { session_id: session } });
  }
  await client.close();

  console.error("smoke ok");
  console.error(`tools: ${names.join(",")}`);
  console.error(`ping(): ${defaultText}`);
  console.error(`ping({ message: "hello-夕下" }): ${customText}`);
}

main().catch((error: unknown) => {
  const text =
    error instanceof Error ? (error.stack ?? error.message) : String(error);
  console.error(text);
  process.exit(1);
});
