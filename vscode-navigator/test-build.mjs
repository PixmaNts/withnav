import { build } from "esbuild";

await build({
  entryPoints: ["src/eventView.ts", "src/cognition.ts", "src/chatView.ts", "src/webview/conversationIdentity.ts"],
  bundle: true,
  platform: "node",
  format: "cjs",
  outdir: "out/tests",
  external: ["vscode"],
});
await build({ entryPoints: ["src/webview/chat.ts"], bundle: true, platform: "browser", format: "iife", outfile: "out/tests/chat-browser.js" });
