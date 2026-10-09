import { build } from "esbuild";

await build({
  entryPoints: ["src/outcome.ts", "src/settings.ts", "src/settingsProtocol.ts", "src/settingsHtml.ts", "src/settingsHost.ts", "src/settingsView.ts", "src/webview/settingsModel.ts", "src/commands.ts", "src/eventView.ts", "src/cognition.ts", "src/chatView.ts", "src/chatHtml.ts", "src/contextMeter.ts", "src/webview/conversationIdentity.ts"],
  bundle: true,
  platform: "node",
  format: "cjs",
  outdir: "out/tests",
  external: ["vscode"],
});
