const { test } = require("node:test");
const assert = require("node:assert/strict");
const Module = require("node:module");

const originalLoad = Module._load;

class Position {
  constructor(line, character) {
    this.line = line;
    this.character = character;
  }
}

class Range {
  constructor(start, end) {
    this.start = start;
    this.end = end;
  }
}

class InlineCompletionItem {
  constructor(insertText, range) {
    this.insertText = insertText;
    this.range = range;
  }
}

let registeredProvider = null;
const configValues = {
  withnav: { "reflex.enabled": true },
  navai: {},
};

const vscode = {
  Position,
  Range,
  InlineCompletionItem,
  workspace: {
    getConfiguration(section) {
      return {
        inspect: (key) => ({ globalValue: configValues[section]?.[key] }),
        get: (key, fallback) => configValues[section]?.[key] ?? fallback,
      };
    },
  },
  languages: {
    registerInlineCompletionItemProvider(selector, provider) {
      registeredProvider = provider;
      return {
        dispose() {
          registeredProvider = null;
        },
      };
    },
  },
};

const State = {
  Stopped: 1,
  Running: 2,
  Starting: 3,
  StartFailed: 4,
};

Module._load = function (id, ...args) {
  if (id === "vscode") return vscode;
  if (id === "vscode-languageclient/node") return { State };
  if (id === "vscode-languageclient") {
    return {
      NotificationType: class {
        constructor(method) {
          this.method = method;
        }
      },
      RequestType: class {
        constructor(method) {
          this.method = method;
        }
      },
    };
  }
  return originalLoad.call(this, id, ...args);
};

const { registerReflex } = require("../out/tests/reflex.js");

function createMockDocument(uri = "file:///test.rs", version = 42) {
  return {
    uri: { toString: () => uri },
    version,
  };
}

function createCancellationToken(isCancelled = false) {
  return {
    isCancellationRequested: isCancelled,
    onCancellationRequested: () => ({ dispose() {} }),
  };
}

test("registerReflex registers provider and dispatches withnav/inlineCompletion immediately with token", async () => {
  configValues.withnav["reflex.enabled"] = true;

  let sendRequestCalled = false;
  let capturedMethod = "";
  let capturedParams = null;
  let capturedToken = null;
  const mockResult = {
    items: [{ text: "println!(\"hello\");" }],
    requestId: "req-999",
  };

  const client = {
    state: State.Running,
    sendRequest: async (method, params, token) => {
      sendRequestCalled = true;
      capturedMethod = method;
      capturedParams = params;
      capturedToken = token;
      return mockResult;
    },
    sendNotification: async () => {},
  };

  const disposables = registerReflex(client);
  assert.equal(disposables.length, 1);
  assert.ok(registeredProvider);

  const doc = createMockDocument("file:///workspace/main.rs", 10);
  const pos = new Position(5, 12);
  const token = createCancellationToken(false);

  // Measure that dispatch happens immediately without waiting for a debounce timer.
  const startTime = Date.now();
  const promise = registeredProvider.provideInlineCompletionItems(doc, pos, {}, token);
  assert.equal(sendRequestCalled, true, "client.sendRequest should be called immediately without timer delay");

  const result = await promise;
  const elapsed = Date.now() - startTime;
  assert.ok(elapsed < 100, `dispatch should not be delayed (elapsed: ${elapsed}ms)`);

  assert.equal(capturedMethod, "withnav/inlineCompletion");
  assert.deepEqual(capturedParams, {
    uri: "file:///workspace/main.rs",
    version: 10,
    line: 5,
    character: 12,
  });
  assert.equal(capturedToken, token, "CancellationToken must be forwarded as third argument");
  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].insertText, "println!(\"hello\");");
  assert.deepEqual(result.items[0].range, new Range(pos, pos));
  assert.equal(result.items[0].requestId, "req-999");
});

test("registerReflex suppresses request if token is already cancelled before dispatch", async () => {
  configValues.withnav["reflex.enabled"] = true;
  let sendRequestCalled = false;

  const client = {
    state: State.Running,
    sendRequest: async () => {
      sendRequestCalled = true;
      return { items: [] };
    },
    sendNotification: async () => {},
  };

  registerReflex(client);
  const doc = createMockDocument();
  const pos = new Position(0, 0);
  const token = createCancellationToken(true);

  const result = await registeredProvider.provideInlineCompletionItems(doc, pos, {}, token);
  assert.equal(sendRequestCalled, false, "should not dispatch if token is already cancelled");
  assert.deepEqual(result, { items: [] });
});

test("registerReflex suppresses request if reflex is disabled", async () => {
  configValues.withnav["reflex.enabled"] = false;
  let sendRequestCalled = false;

  const client = {
    state: State.Running,
    sendRequest: async () => {
      sendRequestCalled = true;
      return { items: [] };
    },
    sendNotification: async () => {},
  };

  registerReflex(client);
  const doc = createMockDocument();
  const pos = new Position(0, 0);
  const token = createCancellationToken(false);

  const result = await registeredProvider.provideInlineCompletionItems(doc, pos, {}, token);
  assert.equal(sendRequestCalled, false, "should not dispatch if reflex is disabled");
  assert.deepEqual(result, { items: [] });
});

test("registerReflex suppresses request if client is not in Running state", async () => {
  configValues.withnav["reflex.enabled"] = true;
  let sendRequestCalled = false;

  const client = {
    state: State.Stopped,
    sendRequest: async () => {
      sendRequestCalled = true;
      return { items: [] };
    },
    sendNotification: async () => {},
  };

  registerReflex(client);
  const doc = createMockDocument();
  const pos = new Position(0, 0);
  const token = createCancellationToken(false);

  const result = await registeredProvider.provideInlineCompletionItems(doc, pos, {}, token);
  assert.equal(sendRequestCalled, false, "should not dispatch when client is stopped");
  assert.deepEqual(result, { items: [] });
});

test("registerReflex handles mid-flight cancellation and returns empty items without reflexShown", async () => {
  configValues.withnav["reflex.enabled"] = true;
  let notificationSent = false;
  const token = createCancellationToken(false);

  let resolveRequest;
  const client = {
    state: State.Running,
    sendRequest: () =>
      new Promise((resolve) => {
        resolveRequest = resolve;
      }),
    sendNotification: async () => {
      notificationSent = true;
    },
  };

  registerReflex(client);
  const doc = createMockDocument();
  const pos = new Position(1, 2);

  const promise = registeredProvider.provideInlineCompletionItems(doc, pos, {}, token);
  assert.ok(resolveRequest, "sendRequest should have been called and populated resolveRequest");

  // Cancel token while request is in flight
  token.isCancellationRequested = true;

  // Now resolve the server request
  resolveRequest({ items: [{ text: "some completion" }], requestId: "req-cancelled" });

  const result = await promise;
  assert.deepEqual(result, { items: [] }, "must return empty items when cancelled mid-flight");
  assert.equal(notificationSent, false, "must not send reflexShown when cancelled mid-flight");
});

test("registerReflex handles transport errors gracefully", async () => {
  configValues.withnav["reflex.enabled"] = true;
  let notificationSent = false;

  const client = {
    state: State.Running,
    sendRequest: async () => {
      throw new Error("LSP transport disconnected");
    },
    sendNotification: async () => {
      notificationSent = true;
    },
  };

  registerReflex(client);
  const doc = createMockDocument();
  const pos = new Position(1, 2);
  const token = createCancellationToken(false);

  const result = await registeredProvider.provideInlineCompletionItems(doc, pos, {}, token);
  assert.deepEqual(result, { items: [] }, "must return empty items on transport error");
  assert.equal(notificationSent, false, "must not send reflexShown on error");
});

test("registerReflex dispatches reflexShown notification when suggestions are accepted and displayed", async () => {
  configValues.withnav["reflex.enabled"] = true;
  let notifiedMethod = "";
  let notifiedParams = null;

  const client = {
    state: State.Running,
    sendRequest: async () => ({
      items: [{ text: "assert_eq!(a, b);" }],
      requestId: "req-shown-42",
    }),
    sendNotification: async (type, params) => {
      notifiedMethod = type.method;
      notifiedParams = params;
    },
  };

  registerReflex(client);
  const doc = createMockDocument();
  const pos = new Position(3, 4);
  const token = createCancellationToken(false);

  const beforeTime = Date.now();
  const result = await registeredProvider.provideInlineCompletionItems(doc, pos, {}, token);
  const afterTime = Date.now();

  assert.equal(result.items.length, 1);
  assert.equal(notifiedMethod, "withnav/reflexShown");
  assert.equal(notifiedParams.requestId, "req-shown-42");
  assert.ok(
    notifiedParams.shownAtMs >= beforeTime && notifiedParams.shownAtMs <= afterTime,
    `shownAtMs (${notifiedParams.shownAtMs}) should be timestamp between ${beforeTime} and ${afterTime}`,
  );
});
