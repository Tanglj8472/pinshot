import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const client = new Client({ name: "pinshot-workflow-test", version: "0.2.0" });
const transport = new StdioClientTransport({ command: process.execPath,
  args: [path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../dist/index.js")], stderr: "inherit" });
const directory = await mkdtemp(path.join(tmpdir(), "pinshot-workflow-"));
let session: number | undefined;
async function call(name: string, args: Record<string, unknown>) {
  const result = await client.callTool({ name, arguments: args });
  assert.ok(!result.isError, JSON.stringify(result));
  return result;
}
function text(result: Awaited<ReturnType<typeof call>>) {
  const item = (result.content as Array<{ type: string; text?: string }>).find((c) => c.type === "text");
  return JSON.parse(item?.text ?? "{}");
}
try {
  await client.connect(transport);
  const before = text(await call("get_state", {}));
  const captured = text(await call("capture_screen", {}));
  session = captured.session_id;
  assert.ok(session && captured.width > 40 && captured.height > 40);
  assert.equal(captured.view?.visible, true, "Capture must open a native preview");
  assert.ok(captured.view.window_id);
  const annotation = text(await call("add_annotation", { session_id: session,
    kind: "rect", points: [{ x: 5, y: 5 }, { x: 35, y: 35 }], width: 3 }));
  for (const kind of ["ellipse", "line", "arrow", "text", "blur", "mosaic"]) {
    await call("add_annotation", {
      session_id: session, kind,
      points: kind === "text" ? [{ x: 5, y: 5 }] : [{ x: 5, y: 5 }, { x: 35, y: 35 }],
      ...(kind === "text" ? { text: "PinShot 原生标注", width: 20 } : {}),
    });
  }
  const state = text(await call("get_state", {}));
  assert.equal(state.pid, before.pid, "All actions must use the same resident PinShot process");
  assert.equal(state.sessions.find((s: { session_id: number }) => s.session_id === session).annotations.length, 7);
  assert.equal(state.views.find((v: { session_id: number }) => v.session_id === session)?.window_id,
    captured.view.window_id, "Edits must refresh the same native window");
  const originalIds = state.sessions.find((s: { session_id: number }) => s.session_id === session).annotations.map((a: { annotation_id: number }) => a.annotation_id);
  const editor = text(await call("edit_session", { session_id: session, tool: "rect" }));
  assert.equal(editor.editing, true);
  await call("undo_session", { session_id: session });
  const undone = text(await call("get_state", {})).sessions.find((s: { session_id: number }) => s.session_id === session);
  assert.equal(undone.annotations.length, 6, "Native editor undo must synchronize back to MCP");
  await call("redo_session", { session_id: session });
  const redone = text(await call("get_state", {})).sessions.find((s: { session_id: number }) => s.session_id === session);
  assert.deepEqual(redone.annotations.map((a: { annotation_id: number }) => a.annotation_id), originalIds,
    "Native editor redo must preserve stable annotation IDs");
  await call("hide_session", { session_id: session });
  const hidden = text(await call("get_state", {}));
  assert.ok(!hidden.views.some((v: { session_id: number }) => v.session_id === session));
  assert.ok(hidden.sessions.some((s: { session_id: number }) => s.session_id === session));
  const reopened = text(await call("show_session", { session_id: session }));
  assert.equal(reopened.visible, true);
  const preview = await call("preview_session", { session_id: session });
  assert.ok((preview.content as Array<{ type: string }>).some((c) => c.type === "image"));
  const output = path.join(directory, "result.png");
  await call("save_session", { session_id: session, path: output });
  const bytes = await readFile(output);
  assert.equal(bytes.readUInt32BE(0), 0x89504e47);
  const duplicate = await client.callTool({ name: "save_session", arguments: { session_id: session, path: output } });
  assert.equal(duplicate.isError, true, "Must reject overwriting an existing file");
  await call("remove_annotation", { session_id: session, annotation_id: annotation.annotation_id });
  const removed = text(await call("get_state", {}));
  assert.equal(removed.views.find((v: { session_id: number }) => v.session_id === session)?.window_id,
    reopened.window_id, "Removing an annotation must preserve the native window");
  await call("close_session", { session_id: session });
  const closed = text(await call("get_state", {}));
  assert.ok(!closed.views.some((v: { session_id: number }) => v.session_id === session));
  assert.ok(!closed.sessions.some((s: { session_id: number }) => s.session_id === session));
  session = undefined;
  const headless = text(await call("capture_screen", { show: false }));
  session = headless.session_id;
  assert.ok(!(text(await call("get_state", {}))).views.some((v: { session_id: number }) => v.session_id === session));
  await call("add_annotation", { session_id: session, kind: "line", points: [{ x: 5, y: 5 }, { x: 35, y: 35 }] });
  assert.ok(!(text(await call("get_state", {}))).views.some((v: { session_id: number }) => v.session_id === session));
  await call("close_session", { session_id: session });
  session = undefined;
  console.error(`workflow ok: pid=${state.pid}, ${captured.width}x${captured.height}, backend=${captured.backend}`);
  console.error("native capture -> editable window -> 7 annotations -> shared undo/redo with stable IDs -> hide/reopen -> preview -> save -> refuse overwrite -> remove -> close");
} finally {
  if (session !== undefined) await client.callTool({ name: "close_session", arguments: { session_id: session } }).catch(() => {});
  await client.close();
  await rm(directory, { recursive: true, force: true });
}
