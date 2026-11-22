# @neyuki/yaml

Reading and writing YAML 1.2 (Core Schema), the configuration format of Docker Compose, GitHub Actions and Kubernetes.

```lua
local yaml = require("@neyuki/yaml")

local config = yaml.decode([[
name: neyuki-app
server:
  host: 127.0.0.1
  port: 8080
features:
  - http
  - log
]])
print(config.server.port) --> 8080

config.server.port = 9090
print(yaml.encode(config))
```

Decoding follows the **YAML 1.2 Core Schema**: `true`/`false` are booleans while `yes`/`no`/`on`/`off` remain strings, decimal/octal (`0o`) and hex (`0x`) integers keep exact precision (including arbitrary-precision integers), `.inf` and `.nan` become floats, block scalars (`|` and `>` with `-`/`+` chomping) are decoded, and anchors (`&`), aliases (`*`) and merge keys (`<<`) are resolved under strict depth and node-count limits to prevent Billion Laughs expansion attacks.

## yaml.null: table

Sentinel value representing an explicit YAML `null` (identical to `json.null`). Returned for `null` and `~` when `keepNull = true` is passed to `decode` or `decodeAll`, and encoded back as `null`.

## yaml.decode(text: string, options: table?): any

Parses a single YAML 1.2 document. An empty or comment-only string returns `nil` (or `yaml.null` when `options.keepNull` is `true`). Raises an error with the line and column for malformed YAML, duplicate mapping keys, complex (non-scalar) mapping keys, cyclic or undefined aliases, multi-document streams (use `yaml.decodeAll`), nesting deeper than 128 levels, or documents exceeding the node/alias expansion cap.

```lua
local data = yaml.decode("a: null\nb: 42", { keepNull = true })
print(data.a == yaml.null, data.b) --> true	42
```

## yaml.decodeAll(text: string, options: table?): table

Parses a YAML 1.2 stream with zero, one or more documents separated by `---` / `...` and returns them as an array table. Accepts the same `options` (`{ keepNull?: boolean }`) as `yaml.decode`.

```lua
local docs = yaml.decodeAll("---\nkind: Config\n---\nkind: Service\n")
print(#docs, docs[2].kind) --> 2	Service
```

## yaml.encode(value: any, options: table?): string

Writes a value as YAML 1.2 text. `options` may hold `indent` (number of spaces from `1` to `8`, default `2`) and `sortKeys` (boolean, default `true`). Plain tables with keys write block mappings, array tables (and tables marked with `json.array`) write block sequences, multi-line strings write block literals (`|` / `|-`), and strings that could be misread as booleans, nulls, numbers or YAML indicators are double-quoted. Raises an error for functions, tables that contain themselves, nesting deeper than 128 levels, and output over 64 MiB.

```lua
print(yaml.encode({ name = "demo", ports = { 80, 443 } }))
-- name: demo
-- ports:
--   - 80
--   - 443
```
