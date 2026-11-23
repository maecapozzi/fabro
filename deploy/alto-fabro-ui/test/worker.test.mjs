import assert from "node:assert/strict";
import test from "node:test";

import {
  forwardToBackend,
  handleRequest,
  isBackendPath,
  parseBackendOrigin,
} from "../src/worker.js";

test("reserves API, auth, and health routes for the backend", () => {
  for (const path of ["/api", "/api/v1/runs", "/auth", "/auth/login", "/health"]) {
    assert.equal(isBackendPath(path), true, path);
  }
  for (const path of ["/", "/runs/run_1", "/apiary", "/author"]) {
    assert.equal(isBackendPath(path), false, path);
  }
});

test("only accepts a public HTTPS backend origin", () => {
  assert.equal(parseBackendOrigin("https://fabro.example").href, "https://fabro.example/");
  for (const value of [
    "http://fabro.example",
    "https://127.0.0.1",
    "https://10.0.0.1",
    "https://localhost",
    "https://fabro.example/path",
    "https://user:password@fabro.example",
  ]) {
    assert.throws(() => parseBackendOrigin(value), value);
  }
});

test("forwards method, query, body, cookies, and a streaming response without buffering", async () => {
  let upstreamRequest;
  const streamed = new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode("first\n"));
      controller.enqueue(new TextEncoder().encode("second\n"));
      controller.close();
    },
  });
  const response = await forwardToBackend(
    new Request("https://ui.example/api/v1/runs?state=active", {
      method: "POST",
      headers: { cookie: "fabro_session=per-user", "content-type": "application/json", "x-alto-user": "remove-me" },
      body: '{"name":"run"}',
    }),
    new URL("https://backend.example"),
    async (request, init) => {
      upstreamRequest = { request, init };
      const headers = new Headers({
        location: "https://backend.example/runs/run_1",
        "content-type": "text/event-stream",
      });
      headers.append("set-cookie", "one=1; Secure");
      headers.append("set-cookie", "two=2; Secure");
      return new Response(streamed, {
        status: 201,
        headers,
      });
    },
  );

  assert.equal(upstreamRequest.request.href, "https://backend.example/api/v1/runs?state=active");
  assert.equal(upstreamRequest.init.method, "POST");
  assert.equal(await new Response(upstreamRequest.init.body).text(), '{"name":"run"}');
  assert.equal(upstreamRequest.init.headers.get("cookie"), "fabro_session=per-user");
  assert.equal(upstreamRequest.init.headers.get("x-alto-user"), null);
  assert.equal(response.headers.get("location"), "/runs/run_1");
  assert.deepEqual(response.headers.getSetCookie(), ["one=1; Secure", "two=2; Secure"]);
  assert.equal(response.headers.get("content-type"), "text/event-stream");
  assert.equal(await response.text(), "first\nsecond\n");
});

test("does not serve a backend route as an SPA fallback and fails safely without settings", async () => {
  let assetsCalls = 0;
  const response = await handleRequest(
    new Request("https://ui.example/api/v1/runs"),
    { ASSETS: { fetch: async () => { assetsCalls += 1; return new Response("spa"); } } },
    { alto: { governance: { secrets: { get: async () => { throw new Error("not declared"); } } } } },
  );
  assert.equal(response.status, 503);
  assert.equal(assetsCalls, 0);
  assert.match(await response.text(), /FABRO_BACKEND_ORIGIN/);
});

test("uses index.html only for non-backend HTML navigations", async () => {
  const seen = [];
  const response = await handleRequest(
    new Request("https://ui.example/runs/run_1", { headers: { accept: "text/html" } }),
    {
      ASSETS: {
        fetch: async (request) => {
          seen.push(new URL(request.url).pathname);
          return seen.length === 1 ? new Response("not found", { status: 404 }) : new Response("<html>Fabro</html>");
        },
      },
    },
    { alto: { governance: { secrets: { get: async () => "https://backend.example" } } } },
  );
  assert.deepEqual(seen, ["/runs/run_1", "/index.html"]);
  assert.equal(response.headers.get("cache-control"), "no-cache");
  assert.equal(await response.text(), "<html>Fabro</html>");
});

test("passes terminal WebSocket upgrades through without Alto identity headers", async () => {
  let upstreamRequest;
  const upstreamResponse = new Response(null, { status: 200, headers: { "x-upstream": "websocket" } });
  const response = await forwardToBackend(
    new Request("https://ui.example/api/v1/runs/run_1/terminal", {
      headers: {
        connection: "Upgrade",
        upgrade: "websocket",
        "sec-websocket-key": "browser-key",
        "x-alto-user": "remove-me",
      },
    }),
    new URL("https://backend.example"),
    async (request, init) => {
      upstreamRequest = { request, init };
      return upstreamResponse;
    },
  );
  assert.equal(upstreamRequest.request.href, "https://backend.example/api/v1/runs/run_1/terminal");
  assert.equal(upstreamRequest.init.headers.get("connection"), "Upgrade");
  assert.equal(upstreamRequest.init.headers.get("upgrade"), "websocket");
  assert.equal(upstreamRequest.init.headers.get("sec-websocket-key"), "browser-key");
  assert.equal(upstreamRequest.init.headers.get("x-alto-user"), null);
  assert.equal(response, upstreamResponse);
});
