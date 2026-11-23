import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import TestRenderer, { act } from "react-test-renderer";
import { MemoryRouter } from "react-router";

import { ApiError } from "../lib/api-client";
import { setupReactTestEnv } from "../lib/test-utils";

const retryAuthConfig = mock(() => Promise.resolve(undefined));
let authConfig: {
  data?: { methods: string[] };
  error?: Error;
  isLoading?: boolean;
  mutate: typeof retryAuthConfig;
};

mock.module("../lib/queries", () => ({
  useAuthConfig: () => authConfig,
}));

mock.module("../lib/mutations", () => ({
  useLoginDevToken: () => ({ trigger: mock() }),
}));

const { default: AuthLogin } = await import("./auth-login");

function text(node: ReturnType<TestRenderer.ReactTestRenderer["toJSON"]>): string {
  if (!node) return "";
  if (typeof node === "string") return node;
  if (Array.isArray(node)) return node.map(text).join("");
  return (node.children ?? []).map(text).join("");
}

describe("AuthLogin", () => {
  let teardownReactEnv: (() => void) | undefined;
  let renderer: TestRenderer.ReactTestRenderer | undefined;

  beforeEach(() => {
    teardownReactEnv = setupReactTestEnv();
    retryAuthConfig.mockClear();
    authConfig = {
      error: new ApiError({
        status:    503,
        message:   "Invalid or missing FABRO_BACKEND_ORIGIN",
        requestId: null,
        body:      { error: "backend_unavailable" },
      }),
      isLoading: false,
      mutate: retryAuthConfig,
    };
  });

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    teardownReactEnv?.();
    mock.restore();
  });

  test("explains a failed authentication-config request and retries into recovered sign-in options", async () => {
    await act(async () => {
      renderer = TestRenderer.create(
        <MemoryRouter>
          <AuthLogin />
        </MemoryRouter>,
      );
    });

    expect(text(renderer!.toJSON())).toContain("Backend not connected");
    expect(text(renderer!.toJSON())).toContain("live runs cannot load yet");

    const retry = renderer!.root.findByType("button");
    await act(async () => {
      retry.props.onClick();
    });
    expect(retryAuthConfig).toHaveBeenCalledTimes(1);

    authConfig = {
      data: { methods: ["github"] },
      isLoading: false,
      mutate: retryAuthConfig,
    };
    await act(async () => {
      renderer!.update(
        <MemoryRouter>
          <AuthLogin />
        </MemoryRouter>,
      );
    });

    expect(text(renderer!.toJSON())).toContain("Sign in with GitHub");
    expect(text(renderer!.toJSON())).not.toContain("Backend not connected");
  });
});
