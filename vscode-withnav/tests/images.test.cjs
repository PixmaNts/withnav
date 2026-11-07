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
