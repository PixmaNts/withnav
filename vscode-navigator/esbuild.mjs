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
  if (watch) {
    await ctx.watch();
  } else {
    await ctx.rebuild();
    await ctx.dispose();
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
