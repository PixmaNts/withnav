import esbuild from "esbuild";
import fs from "node:fs/promises";

const production = process.argv.includes("--production");
const watch = process.argv.includes("--watch");

async function main() {
  const ctx = await esbuild.context({
    entryPoints: ["src/extension.ts"],
    bundle: true,
    format: "cjs",
    minify: production,
    sourcemap: !production,
    sourcesContent: false,
    platform: "node",
    outfile: "out/extension.js",
    external: ["vscode"],
    logLevel: "info",
  });
  const webview = await esbuild.context({
    entryPoints: ["src/webview/chat.ts"],
    bundle: true,
    format: "iife",
    minify: production,
    sourcemap: !production,
    sourcesContent: false,
    platform: "browser",
    outfile: "out/chat.js",
    logLevel: "info",
  });
  const styles = await esbuild.context({
    entryPoints: ["src/webview/chat.css"],
    minify: production,
    outfile: "out/chat.css",
    logLevel: "info",
  });
  if (watch) {
    await Promise.all([ctx.watch(), webview.watch(), styles.watch()]);
  } else {
    await Promise.all([ctx.rebuild(), webview.rebuild(), styles.rebuild()]);
    await Promise.all([ctx.dispose(), webview.dispose(), styles.dispose()]);
    // vscode-languageclient spawns lib/node/terminateProcess.sh from __dirname
    // when force-killing the server; copy it next to the bundle.
    await fs.copyFile(
      "node_modules/vscode-languageclient/lib/node/terminateProcess.sh",
      "out/terminateProcess.sh"
    );
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
