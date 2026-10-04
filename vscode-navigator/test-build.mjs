import { build } from "esbuild";

await build({
  entryPoints: ["src/eventView.ts", "src/cognition.ts"],
  bundle: true,
  platform: "node",
  format: "cjs",
  outdir: "out/tests",
  external: ["vscode"],
});
