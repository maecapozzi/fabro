import { withAlto } from "@workos/alto";

const BACKEND_ORIGIN_SECRET = "FABRO_BACKEND_ORIGIN";
const BACKEND_PATH_PREFIXES = ["/api/", "/auth/"];
const BACKEND_EXACT_PATHS = new Set(["/api", "/auth", "/health"]);
const ASSET_IMMUTABLE = /(?:^|[._-])[a-z0-9]{8}(?:[._-]|$)/i;
const HOP_BY_HOP_HEADERS = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
]);

export function isBackendPath(pathname) {
  return BACKEND_EXACT_PATHS.has(pathname)
    || BACKEND_PATH_PREFIXES.some((prefix) => pathname.startsWith(prefix));
}

function isWebSocketUpgrade(request) {
  return request.headers.get("upgrade")?.toLowerCase() === "websocket";
}

function error(status, code, detail) {
  return Response.json(
    { error: code, ...(detail ? { detail } : {}) },
    {
      status,
      headers: {
        "cache-control": "no-store",
        "content-type": "application/json; charset=utf-8",
      },
    },
  );
}

function hasPrivateAddress(hostname) {
  const lower = hostname.toLowerCase().replace(/\.$/, "");
  if (lower === "localhost" || lower.endsWith(".localhost") || lower.endsWith(".local")) {
    return true;
  }

  // URL.hostname retains brackets around IPv6 literals in Worker and Node
  // runtimes. Reject all literals rather than risk overlooking an alternate
  // spelling of loopback, private, link-local, or IPv4-mapped space.
  if (lower.includes(":")) {
    return true;
  }

  const octets = lower.split(".").map(Number);
  if (octets.length !== 4 || octets.some((octet) => !Number.isInteger(octet) || octet < 0 || octet > 255)) {
    return false;
  }

  const [first, second] = octets;
  return (
    first === 0
    || first === 10
    || first === 127
    || (first === 169 && second === 254)
    || (first === 172 && second >= 16 && second <= 31)
    || (first === 192 && second === 168)
    || (first === 100 && second >= 64 && second <= 127)
  );
}

export function parseBackendOrigin(value) {
  let origin;
  try {
    origin = new URL(value);
  } catch {
    throw new Error("must be an absolute HTTPS URL");
  }

  if (origin.protocol !== "https:" || origin.username || origin.password || origin.pathname !== "/" || origin.search || origin.hash) {
    throw new Error("must be a bare HTTPS origin");
  }
  if (hasPrivateAddress(origin.hostname)) {
    throw new Error("must use a public DNS hostname or public IPv4 address");
  }
  return origin;
}

function requestHeaders(request, { preserveWebSocketUpgrade = false } = {}) {
  const headers = new Headers();
  for (const [name, value] of request.headers) {
    const normalized = name.toLowerCase();
    if (
      (HOP_BY_HOP_HEADERS.has(normalized)
        && !(preserveWebSocketUpgrade && (normalized === "connection" || normalized === "upgrade")))
      || normalized.startsWith("cf-")
      || normalized.startsWith("x-alto-")
      || normalized === "host"
    ) {
      continue;
    }
    headers.append(name, value);
  }
  return headers;
}

function hasSameOriginWebSocketOrigin(request, browserUrl) {
  const value = request.headers.get("origin");
  if (!value) return false;

  try {
    const origin = new URL(value);
    return (
      origin.origin === browserUrl.origin
      && origin.pathname === "/"
      && !origin.search
      && !origin.hash
      && !origin.username
      && !origin.password
    );
  } catch {
    return false;
  }
}

function responseHeaders(upstream, browserOrigin, backendOrigin) {
  const headers = new Headers();
  for (const [name, value] of upstream.headers) {
    const normalized = name.toLowerCase();
    if (HOP_BY_HOP_HEADERS.has(normalized) || normalized === "location" || normalized === "set-cookie") {
      continue;
    }
    headers.append(name, value);
  }

  const location = upstream.headers.get("location");
  if (location) {
    const target = new URL(location, backendOrigin);
    if (target.origin === backendOrigin.origin || target.origin === browserOrigin.origin) {
      headers.set("location", `${target.pathname}${target.search}${target.hash}`);
    } else {
      headers.set("location", location);
    }
  }

  // Cloudflare exposes `getAll("set-cookie")`; newer Fetch implementations
  // expose `getSetCookie()`. Either form keeps distinct session cookies intact.
  const cookies = upstream.headers.getSetCookie?.() ?? upstream.headers.getAll?.("set-cookie") ?? [];
  for (const cookie of cookies) {
    headers.append("set-cookie", cookie);
  }
  headers.set("cache-control", "no-store");
  return headers;
}

function withCachePolicy(response, pathname) {
  const headers = new Headers(response.headers);
  if (pathname === "/" || pathname === "/index.html" || pathname === "/build-id.json") {
    headers.set("cache-control", "no-cache");
  } else if (ASSET_IMMUTABLE.test(pathname)) {
    headers.set("cache-control", "public, max-age=31536000, immutable");
  }
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
}

function isSpaNavigation(request, pathname) {
  return (
    (request.method === "GET" || request.method === "HEAD")
    && request.headers.get("accept")?.includes("text/html")
    && !pathname.split("/").at(-1).includes(".")
  );
}

export async function forwardToBackend(request, backendOrigin, fetchImpl = fetch) {
  const browserUrl = new URL(request.url);
  const target = new URL(`${browserUrl.pathname}${browserUrl.search}`, backendOrigin);
  const websocketUpgrade = isWebSocketUpgrade(request);
  if (websocketUpgrade && !hasSameOriginWebSocketOrigin(request, browserUrl)) {
    return error(403, "forbidden_websocket_origin", "The WebSocket Origin must match the Worker origin.");
  }

  const headers = requestHeaders(request, { preserveWebSocketUpgrade: websocketUpgrade });
  if (websocketUpgrade) {
    headers.set("origin", backendOrigin.origin);
  }
  const init = {
    method: request.method,
    headers,
    redirect: "manual",
    signal: request.signal,
  };
  if (request.method !== "GET" && request.method !== "HEAD") {
    init.body = request.body;
    init.duplex = "half";
  }

  const upstream = await fetchImpl(target, init);
  // A WebSocket response carries an opaque runtime-owned WebSocket endpoint.
  // Re-wrapping it would discard that endpoint, so return the native Worker
  // response after applying the same explicit request-header boundary above.
  if (websocketUpgrade) return upstream;
  return new Response(upstream.body, {
    status: upstream.status,
    statusText: upstream.statusText,
    headers: responseHeaders(upstream, browserUrl, backendOrigin),
  });
}

export async function handleRequest(request, env, ctx) {
  const url = new URL(request.url);
  if (isBackendPath(url.pathname)) {
    let backendOrigin;
    try {
      backendOrigin = parseBackendOrigin(await ctx.alto.governance.secrets.get(BACKEND_ORIGIN_SECRET));
    } catch (cause) {
      return error(503, "backend_unavailable", `Invalid or missing ${BACKEND_ORIGIN_SECRET}: ${cause.message}`);
    }

    try {
      return await forwardToBackend(request, backendOrigin);
    } catch {
      return error(502, "backend_unavailable", "The configured Fabro backend could not be reached.");
    }
  }

  let asset = await env.ASSETS.fetch(request);
  let assetPathname = url.pathname;
  if (asset.status === 404 && isSpaNavigation(request, url.pathname)) {
    const indexUrl = new URL("/index.html", url);
    asset = await env.ASSETS.fetch(new Request(indexUrl, request));
    assetPathname = indexUrl.pathname;
  }
  return withCachePolicy(asset, assetPathname);
}

export default withAlto(handleRequest);
