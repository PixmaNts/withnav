const { test } = require("node:test");
const assert = require("node:assert/strict");
const { eventView, imageRefs } = require("../out/tests/eventView.js");
const { imageHtml } = require("../out/tests/imageHtml.js");

const env = (kind, payload) => ({ seq: 1, kind, payload });

test("a Captain message keeps its image references and drops malformed ones", () => {
  const image = { attachment_id: "A1", name: "shot.png", media_type: "image/png" };
  const view = eventView(env("message.received", { source: "user:http", content: "", images: [image, { name: "x" }] }));
  assert.deepEqual(view.images, [image]);
  assert.equal(imageRefs("nope"), undefined);
  assert.equal(imageRefs([]), undefined);
});

test("the image panel shows only an image data URL, escapes the name and runs no script", () => {
  const ok = imageHtml("<b>shot</b>", "data:image/png;base64,iVBORw0K");
  assert.match(ok, /src="data:image\/png;base64,iVBORw0K"/);
  assert.match(ok, /alt="&lt;b&gt;shot&lt;\/b&gt;"/);
  assert.match(ok, /default-src 'none'; img-src data:;/);
  assert.doesNotMatch(ok, /script-src/);
  assert.match(imageHtml("x", "javascript:alert(1)"), /src=""/);
  assert.match(imageHtml("x", "data:image/svg+xml;base64,PHN2Zz4="), /src=""/);
});

test("the transcript image cache drops the least recently used URLs past its entry and character budgets", () => {
  const { ImageCache } = require("../out/tests/webview/imageCache.js");
  const cache = new ImageCache(4, 100);
  for (let i = 0; i < 1000; i++) cache.set(`A${i}`, "x".repeat(10));
  assert.equal(cache.size, 4);
  assert.equal(cache.totalChars, 40);
  assert.equal(cache.get("A0"), undefined);
  assert.equal(cache.get("A999"), "x".repeat(10));

  const sized = new ImageCache(10, 100);
  sized.set("old", "a".repeat(40)); sized.set("used", "b".repeat(40));
  assert.equal(sized.get("old"), "a".repeat(40)); // now the most recent
  sized.set("new", "c".repeat(40));
  assert.equal(sized.get("used"), undefined);
  assert.deepEqual([sized.size, sized.totalChars], [2, 80]);

  sized.set("new", "d".repeat(10)); assert.equal(sized.totalChars, 50);
  sized.set("new", "e".repeat(60)); assert.equal(sized.totalChars, 100);
  sized.set("new", false); assert.equal(sized.totalChars, 40);
  sized.set("gone", false); assert.equal(sized.get("gone"), false);
  sized.set("huge", "f".repeat(101));
  assert.equal(sized.get("huge"), undefined);
  assert.equal(sized.get("old"), "a".repeat(40));
  assert.deepEqual([sized.size, sized.totalChars], [3, 40]);
});

test("the shipped transcript keeps its image cache bounded and asks once per attachment in flight", () => {
  const fs = require("node:fs");
  const { JSDOM } = require("jsdom");
  const { chatHtml } = require("../out/tests/chatHtml.js");
  const dom = new JSDOM(chatHtml("n", "chat.js", "chat.css", "resource:"), { runScripts: "outside-only" });
  const posts = [];
  dom.window.acquireVsCodeApi = () => ({ postMessage: m => posts.push(m), getState() {}, setState() {} });
  dom.window.eval(fs.readFileSync("out/chat.js", "utf8"));
  dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data: { type: "tabs", tabs: [{ id: "t1", title: "S" }], active: "t1" } }));
  const receive = data => dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data: { tab: "t1", generation: 1, ...data } }));
  const loads = id => posts.filter(m => m.type === "loadImage" && m.attachmentId === id).length;
  const show = (id, n) => receive({ type: "message", id: `m${n}`, author: "captain", text: "", images: [{ attachment_id: id, name: "shot.png" }] });
  receive({ type: "conversationOpened", id: "S", title: "S" });

  show("A0", 0); show("A0", 1);
  assert.equal(loads("A0"), 1, "a second thumbnail waits for the request in flight");
  receive({ type: "imageData", attachmentId: "A0", url: "data:image/png;base64,AAAA" });
  for (let i = 1; i <= 200; i++) { show(`A${i}`, i + 1); receive({ type: "imageData", attachmentId: `A${i}`, url: `data:image/png;base64,${i}` }); }
  show("A200", 999);
  assert.equal(loads("A200"), 1, "a recent picture comes from the cache");
  show("A0", 1000);
  assert.equal(loads("A0"), 2, "the oldest picture was evicted and is asked again");
  receive({ type: "imageData", attachmentId: "A0", url: "data:image/png;base64,BBBB" });
  const thumbs = dom.window.document.querySelectorAll('.thumb[data-attachment="A0"]');
  assert.equal(thumbs.length, 3);
  for (const thumb of thumbs) { assert.equal(thumb.querySelector("img").src, "data:image/png;base64,BBBB"); assert.equal(thumb.classList.contains("missing"), false); }
  dom.window.close();
});

test("the picker hint names the picker's row limit", () => {
  const { narrowHint, PICKER_LIMIT } = require("../out/tests/webview/pickerSearch.js");
  assert.equal(narrowHint(PICKER_LIMIT), "");
  assert.equal(narrowHint(PICKER_LIMIT + 1), ` · Type to narrow (first ${PICKER_LIMIT} shown)`);
});
