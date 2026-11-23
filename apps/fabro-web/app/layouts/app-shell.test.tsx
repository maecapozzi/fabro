import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import TestRenderer, { act } from "react-test-renderer";
import { createMemoryRouter, RouterProvider } from "react-router";

import { ApiError } from "../lib/api-client";
import { setupReactTestEnv, textContent } from "../lib/test-utils";

const retryAuthConfig = mock(() => Promise.resolve(undefined));
const retryAuthMe = mock(() => Promise.resolve(undefined));
let authConfig: {
  data?: { methods: string[] };
  error?: Error;
  mutate: typeof retryAuthConfig;
};
let authMe: {
  data?: {
    user: {
      login: string;
      name: string;
      email: string;
      avatarUrl: string;
      userUrl: string;
    };
    provider: string;
    demoMode: boolean;
  };
  error?: Error;
  isLoading?: boolean;
  mutate: typeof retryAuthMe;
};

mock.module("../lib/queries", () => ({
  useAuthConfig: () => authConfig,
  useAuthMe: () => authMe,
}));

const { default: AppShell } = await import("./app-shell");

describe("AppShell", () => {
  let teardownReactEnv: (() => void) | undefined;
  let renderer: TestRenderer.ReactTestRenderer | undefined;
  let router: ReturnType<typeof createMemoryRouter> | undefined;

  beforeEach(() => {
    teardownReactEnv = setupReactTestEnv();
    retryAuthConfig.mockClear();
    retryAuthMe.mockClear();
    const backendUnavailable = new ApiError({
      status:    503,
      message:   "Invalid or missing FABRO_BACKEND_ORIGIN",
      requestId: null,
      body:      { error: "backend_unavailable" },
    });
    authConfig = {
      error: backendUnavailable,
      mutate: retryAuthConfig,
    };
    authMe = {
      error: backendUnavailable,
      isLoading: false,
      mutate: retryAuthMe,
    };
    router = createMemoryRouter(
      [{
        path: "/",
        element: <AppShell />,
        children: [{ path: "runs", element: <p>Live runs</p> }],
      }],
      { initialEntries: ["/runs"] },
    );
  });

  afterEach(() => {
    if (renderer) act(() => renderer?.unmount());
    router?.dispose();
    teardownReactEnv?.();
    mock.restore();
  });

  test("retries the disconnected bootstrap and preserves the recovered app route", async () => {
    await act(async () => {
      renderer = TestRenderer.create(<RouterProvider router={router!} />);
    });

    expect(textContent(renderer!.root)).toContain("Backend not connected");
    expect(textContent(renderer!.root)).toContain("live runs cannot load yet");

    await act(async () => {
      renderer!.root.findByType("button").props.onClick();
    });
    expect(retryAuthConfig).toHaveBeenCalledTimes(1);
    expect(retryAuthMe).toHaveBeenCalledTimes(1);

    authConfig = {
      data: { methods: ["github"] },
      mutate: retryAuthConfig,
    };
    authMe = {
      data: {
        user: {
          login:     "mae",
          name:      "Mae",
          email:     "mae@example.com",
          avatarUrl: "/images/logo.svg",
          userUrl:   "/profile",
        },
        provider: "github",
        demoMode: false,
      },
      isLoading: false,
      mutate: retryAuthMe,
    };
    await act(async () => {
      await router!.navigate("/runs?recovered=1");
    });

    expect(textContent(renderer!.root)).toContain("Live runs");
    expect(textContent(renderer!.root)).not.toContain("Backend not connected");
  });
});
