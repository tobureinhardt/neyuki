# @neyuki/http

An HTTP/1.1 client and server, with TLS through the operating system's own library. Bodies are strings; response bytes that are not valid UTF-8 are replaced with U+FFFD. The interpreter is single-threaded, so a server and its client can share a script only when the request runs in a task, where it waits on a background thread instead of blocking (see `task`). Responses are decoded when they are gzip or brotli compressed.

```lua
local http = require("@neyuki/http")

local response = http.get("https://api.github.com/repos/neyuki-lang/neyuki", {
    headers = { ["user-agent"] = "neyuki" },
    timeout = 10,
})
if response.ok then
    print(json.decode(response.body).stargazers_count)
end
```

## http.request(url: string, options: table?): Response

Performs one request. `options` may hold `method` (default `"GET"`), `headers` (a `{ name = value }` table), `body`, `timeout` (seconds for the whole exchange; unlimited by default), `follow` (whether to follow redirects; default `true`) and `maxBody` (the largest response body accepted, default 64 MiB).

Only transport failures (DNS, connection, TLS, timeout) raise errors. Every status code comes back as a `Response` with `status`, `ok` (true for 2xx), `headers` (names lower-cased), `body`, `url` (after redirects), `header(name)` (a case-insensitive lookup), `headerValues(name)` (all values for repeated headers such as `set-cookie`) and `bytes()` (the raw binary response body as a `buffer`).

## http.get(url: string, options: table?): Response

A `GET` request.

## http.head(url: string, options: table?): Response

A `HEAD` request.

## http.delete(url: string, options: table?): Response

A `DELETE` request.

## http.post(url: string, body: string?, options: table?): Response

A `POST` request with `body`.

```lua
http.post("https://example.com/api/items", json.encode({ name = "tea" }), {
    headers = { ["content-type"] = "application/json" },
})
```

## http.put(url: string, body: string?, options: table?): Response

A `PUT` request with `body`.

## http.patch(url: string, body: string?, options: table?): Response

A `PATCH` request with `body`.

## http.encode(s: string): string

Percent-encodes a string for use in a URL.

## http.decode(s: string): string

Reverses `encode`.

## http.query(params: table): string

Builds an `a=1&b=2` query string from a table, sorted by key.

```lua
http.get("https://example.com/search?" .. http.query({ q = "snow", page = 2 }))
```

## http.parseQuery(target: string): (string, table)

Splits a `path?query` target into the path and a table of decoded parameters.

## http.listen(options: number | table?): Server

Binds a server. `options` is a port or `{ host?, port?, maxBody? }`, defaulting to `127.0.0.1`, port `8080` and 64 MiB; port `0` picks a free one. A request whose body is over `maxBody` is answered with 413 and never returned.

The `Server` has `host`, `port`, `url`, `accept(timeout?)` (the next `Request`, or `nil` once `timeout` seconds pass without one), `isOpen()` and `close()`.

A `Request` has `method`, `path`, `headers`, `body`, `remote`, `header(name)`, `query()` (the path and its decoded parameters), `multipart(options?)` (parses a `multipart/form-data` upload losslessly from the raw request bytes), `forward(target, options?)` and `respond(reply)`. `respond` may be called once, with `nil` (204), a string (200, `text/plain`), a `buffer` (200, `application/octet-stream`) or `{ status?, headers?, body? }` (where a header value may be a string, number or non-empty array of strings for repeated headers such as `set-cookie`).

## http.serve(options: number | table?, handler: ((Request) -> any) | Router): never

Runs `listen` and calls `handler(request)` (or `handler:handle(request)` when given a `Router`) for every request, forever, sending its return value with `respond` unless the handler already answered. A handler error is printed to standard error and answered with a 500.

```lua
http.serve(8080, function(request)
    local path, params = request:query()
    if path == "/hello" then return "hello " .. (params.name ?? "world") end
    if path == "/api/time" then
        return { headers = { ["content-type"] = "application/json" }, body = json.encode({ now = os.time() }) }
    end
    return { status = 404, body = "not found" }
end)
```

## http.forward(options: number | table?, target: number | table): Forward

A TCP port forward: binds `options` (as for `listen`) and relays every connection to `target` (a local port or `{ host?, port }`) byte for byte, so HTTP, WebSockets and TLS all pass through. Relaying runs on background threads. The `Forward` has `host`, `port`, `url`, `target`, `isOpen()`, `close()` and `wait(timeout?)`, which blocks for `timeout` seconds or for as long as the forward runs.

```lua
http.forward(1234, 5173):wait() -- expose a dev server on port 1234
```

`request:forward(target, options?)` answers a single request with whatever `target` (a port, `{ host?, port }` or a base URL) replies, passing the method, path, headers and body through unchanged; `options.timeout` bounds the upstream call. If the upstream cannot be reached the client gets a 502 and the error is raised. WebSocket upgrades need `http.forward` instead.

## http.json(value: any, status: number?, headers: table?): table

Returns a reply table with `content-type: application/json; charset=utf-8` and `json.encode(value)` as the body (default status `200`).

## http.html(body: string, status: number?, headers: table?): table

Returns a reply table with `content-type: text/html; charset=utf-8` (default status `200`).

## http.redirect(location: string, status: number?): table

Returns a redirect reply with the `location` header (default status `302`). Rejects CR, LF and NUL bytes in `location`.

## http.parseCookies(header: string): table

Parses an HTTP `Cookie` request header into a `{ name = value }` table.

## http.formatCookie(name: string, value: string, options: table?): string

Formats a `Set-Cookie` header value with strict RFC 6265 validation. `options` may hold `path`, `domain`, `maxAge` (integer seconds), `expires` (Unix timestamp or HTTP GMT date string), `httpOnly`, `secure`, `sameSite` (`"Strict"`, `"Lax"`, `"None"`) and `partitioned`.

## http.signCookie(value: string, secret: string): string

Signs `value` with HMAC-SHA256 (`<value>.<base64url-mac>`) for tamper-evident cookies.

## http.unsignCookie(signed: string, secret: string): string?

Verifies a signed cookie in constant time and returns its original value, or `nil` if invalid.

## http.parseMultipart(body: string | buffer, contentType: string, options: table?): table

Parses an RFC 7578 `multipart/form-data` payload from a string or binary `buffer`. Returns `{ fields, files, parts }`, keeping uploaded file bytes lossless in `buffer` values (`{ name, filename, contentType, size, data }`). `options` accepts `maxParts` (default `256`), `maxFieldSize` (default 1 MiB), `maxFileSize` (default 16 MiB) and `maxHeaderBytes` (default 16 KiB). Note that `filename` comes directly from the client's `Content-Disposition` header and must not be used as a filesystem path without validation or sanitization.

## http.static(root: string, options: table?): function

Creates a static-file middleware serving files inside `root` (or bundled assets in built apps). Prevents path traversal (`..`, `%2e%2e`, backslashes, NUL bytes), symlink escapes and dotfile leakage (`dotfiles = "deny"` by default, or `"ignore"` / `"allow"`). Supports `index` (default `"index.html"`, or `false`), `maxAge` (seconds), `etag` (default `true`, with `If-None-Match` -> `304`) and `fallthrough` (default `true`). Disk files are streamed directly without buffering in VM memory.

## http.sessions(options: table?): function

Creates a session middleware that attaches `req.session` (a plain data table), `req.sessionId`, `req:regenerateSession()` and `req:destroySession()`. Uses 32-byte CSPRNG session IDs, optional HMAC-SHA256 signed cookies (`secret`), and a TTL-bounded in-memory store (`maxAge` default `86400`, `maxEntries` default `10000`) or a custom `store` (`{ get, set, delete }`). `options` also accepts `name` (default `"neyuki.sid"`), `rolling` (default `false`) and `cookie` attributes.

## http.router(options: table?): Router

Creates an HTTP `Router` with `:param` and `*wildcard` route matching (static > param > wildcard precedence), onion middleware (`use`), sub-router mounting (`mount`), automatic `HEAD` fallback from `GET`, `405 Method Not Allowed` with `Allow` headers, cookies and multipart support. `options` accepts `strictSlash` (default `false`).

```lua
local app = http.router()
app:use(http.sessions({ secret = "change-me" }))
app:use("/assets", http.static("./public"))
app:get("/users/:id", function(req)
    return http.json({ id = req.params.id })
end)
app:serve(8080)
```

## Router:route(method: string, pattern: string, ...handlers: function): Router

Registers one or more handlers for `method` and `pattern` (supporting `:param` and trailing `*wildcard` segments).

## Router:get(pattern: string, ...handlers: function): Router

Registers a `GET` route (also serving `HEAD` automatically unless overridden).

## Router:post(pattern: string, ...handlers: function): Router

Registers a `POST` route.

## Router:put(pattern: string, ...handlers: function): Router

Registers a `PUT` route.

## Router:patch(pattern: string, ...handlers: function): Router

Registers a `PATCH` route.

## Router:delete(pattern: string, ...handlers: function): Router

Registers a `DELETE` route.

## Router:head(pattern: string, ...handlers: function): Router

Registers a `HEAD` route.

## Router:options(pattern: string, ...handlers: function): Router

Registers an `OPTIONS` route.

## Router:all(pattern: string, ...handlers: function): Router

Registers a fallback route matching any HTTP method.

## Router:use(prefixOrMiddleware: string | function, middleware: function?): Router

Registers a middleware `function(req, next)` globally or under `prefix`. Inside a prefixed middleware, `req.pathname` is stripped to the path relative to `prefix` and restored when calling `next()`.

## Router:mount(prefix: string, subRouter: Router): Router

Mounts another `Router` under `prefix`, keeping its middlewares and routes scoped to that prefix.

## Router:notFound(handler: function): Router

Sets a custom 404 handler `function(req)` for unmatched paths.

## Router:onError(handler: function): Router

Sets a custom error handler `function(err, req)` for uncaught errors in middleware or route handlers.

## Router:handle(request: Request): ()

Dispatches a single accepted `Request` through the router's middleware and routes and sends its response. Calling `req:respond(reply)` directly inside a route or middleware flushes any pending `Set-Cookie` headers and session updates; `req:forward(target)` relays the upstream response directly as-is. When a handler raises an error (answered with a 500 or by `onError`), session changes made during that request are not saved. The request path is percent-decoded exactly once before middleware, mounts, routes and `http.static` see it.

## Router:serve(options: number | table?): never

Starts listening on `options` and handles every request through this router.
