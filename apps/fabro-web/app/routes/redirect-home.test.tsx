import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import TestRenderer, { act } from "react-test-renderer";
import { MemoryRouter } from "react-router";

import { ApiError } from "../lib/api-client";
import { setupReactTestEnv } from "../lib/test-utils";

const retryAuthMe = mock(() => Promise.resolve(undefined));
let authMe: {
  data?: { user: { login: string } };
  error?: Error;
  mutate: typeof retryAuthMe;
};

mock.module("../lib/queries", () => ({
  useAuthMe: () => authMe,
}));

const { default: RedirectHome } = await import("./redirect-home");

function text(node: ReturnType<TestRenderer.ReactTestRenderer["toJSON"]>): string {
  if (!node) return "";
  if (typeof node === "string") return node;
  if (Array.isArray(node)) return node.map(text).join("");
  return (node.children ?? []).map(text).join("");
}

describe("RedirectHome", () => {
  let teardownReactEnv: (() => void) | undefined;
  let renderer: TestRenderer.ReactTestRenderer | undefined;

  beforeEach(() => {
    teardownReactEnv = setupReactTestEnv();
    retryAuthMe.mockClear();
    authMe = {
      error: new ApiError({
        status:    503,
        message:   "backend_unavailable",
        requestId: null,
        body:      null,
      }),
      mutate: retryAuthMe,
    };
  });

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    teardownReactEnv?.();
    mock.restore();
  });

  test("explains a backend failure and retries the authentication bootstrap", async () => {
    await act(async () => {
      renderer = TestRenderer.create(
        <MemoryRouter>
          <RedirectHome />
        </MemoryRouter>,
      );
    });

    expect(text(renderer!.toJSON())).toContain("Backend not connected");
    expect(text(renderer!.toJSON())).toContain("live runs cannot load yet");

    const retry = renderer!.root.findByType("button");
    await act(async () => {
      retry.props.onClick();
    });
    expect(retryAuthMe).toHaveBeenCalledTimes(1);
  });
});
