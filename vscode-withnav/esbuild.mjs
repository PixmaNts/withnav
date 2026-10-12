import esbuild from "esbuild";

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
  const settings = await esbuild.context({
    entryPoints: ["src/webview/settings.ts", "src/webview/settings.css"],
    bundle: true,
    outdir: "out",
    platform: "browser",
    format: "iife",
    minify: production,
    sourcemap: !production,
    sourcesContent: false,
    logLevel: "info",
  });
  if (watch) {
    await Promise.all([ctx.watch(), webview.watch(), styles.watch(), settings.watch()]);
  } else {
    await Promise.all([ctx.rebuild(), webview.rebuild(), styles.rebuild(), settings.rebuild()]);
    await Promise.all([ctx.dispose(), webview.dispose(), styles.dispose(), settings.dispose()]);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
