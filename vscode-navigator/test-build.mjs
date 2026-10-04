import { build } from "esbuild";

await build({
  entryPoints: ["src/commands.ts", "src/eventView.ts", "src/cognition.ts", "src/chatView.ts", "src/chatHtml.ts", "src/contextMeter.ts", "src/webview/conversationIdentity.ts"],
  bundle: true,
  platform: "node",
  format: "cjs",
  outdir: "out/tests",
  external: ["vscode"],
});
