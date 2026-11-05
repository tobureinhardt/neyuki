# The Neyuki language

Neyuki is a small scripting language in the Lua family: tables are the one data structure, functions are values, and a script is a list of statements run top to bottom. It adds exact integers of any size, optional type annotations that are checked before a script runs, string interpolation, `??`, compound assignment and `continue`.

This page describes the language. The functions you can call are listed in the [reference](reference/README.md).

## A first script

```lua
local name = os.args()[1] ?? "world"
print(`Hello, {name}!`)

local scores = { ada = 92, yuki = 87, lin = 95 }
local best, bestScore = nil, 0
for player, score in scores do
    if score > bestScore then
        best, bestScore = player, score
    end
end
print(`{best} wins with {bestScore}`)
```

Run it with `neyuki hello.nyk Yuki`.

## Comments

```lua
-- a line comment
--[[ a block comment
     over several lines ]]
--[==[ a block comment that may contain ]] ]==]
```

## Variables

`local` declares a variable visible until the end of the enclosing block. `const` declares one that cannot be reassigned. `global` sets a variable visible to every module. Using a name that was never declared is an error the checker reports before the script runs.

```lua
local count = 0
const limit = 10
global appName = "demo"

local a, b = 1, 2
a, b = b, a -- swap
```

Annotations are optional and checked before running: `local count: number = "x"` is rejected. The types are `nil`, `boolean`, `number` (with `int` and `float`), `string`, `table`, `function`, `buffer`, `thread` and `any`, plus `T?` (T or nil), `A | B`, `{T}` (array of T), `{[K]: V}` (map), `{ name: T }` (record), `(A, B) -> R` (function) and `(A, B)` for several return values.

```lua
local function area(w: number, h: number?): number
    h = h ?? w          -- after this, h is a number
    return w * h
end
```

## Values

- `nil`: the absence of a value. Reading a missing variable field gives `nil`.
- `true` and `false`.
- Numbers: integers are exact at any size (`2 ^ 100` is exact), floats are 64-bit. Integers may be written in decimal, hex (`0xff`) or binary (`0b1010`), with `_` separators (`1_000_000`). `/` always gives a float; `//` (floor division), `%`, `^` with integer operands and the bitwise operators keep integers exact. Integers are capped at about 315,000 digits.
- Strings: immutable UTF-8 text. `#s` is the length in bytes.
- Tables: arrays and maps in one, compared by identity.
- Functions, buffers (`buffer` library) and threads (`coroutine` library).

Only `nil` and `false` count as false in conditions; `0` and `""` are true.

## Strings

```lua
local single = 'it\'s'
local double = "tab\tnewline\n"
local long = [[
raw text, no escapes, may span lines
]]
local message = `{count} items, {count * 2} halves`
```

Escapes in quoted and backtick strings: `\n`, `\t`, `\r`, `\a`, `\b`, `\f`, `\v`, `\\`, `\"`, `\'`, `\ddd` (a decimal character code up to 255), `\xHH`, `\u{1F600}`, `\z` (skips the following whitespace and line breaks) and a backslash at the end of a line (keeps the line break).

Backtick strings interpolate: each `{expression}` is evaluated and converted to text. Write `\{` for a literal brace and `` \` `` for a backtick.

Every string has the `string` library as methods: `("hello"):upper()`, `name:sub(1, 3)`, `line:split(",")`.

## Tables

```lua
local list = { "a", "b", "c" }            -- list[1] == "a"
local point = { x = 3, y = 4 }            -- point.x == 3
local mixed = { 10, 20, label = "pair" }
local headers = { ["content-type"] = "text/plain" }
local byKey = { [true] = "yes", [1.5] = "float key" }

list[#list + 1] = "d"   -- append
point.z = 0             -- add a field
point.x = nil           -- remove a field
```

Arrays start at 1 and `#t` is the length of the array part. Entries may be separated by `,` or `;`, and a trailing separator is allowed.

## Operators

| Kind | Operators |
| --- | --- |
| Arithmetic | `+` `-` `*` `/` `//` `%` `^` and unary `-` |
| Comparison | `==` `!=` `<` `<=` `>` `>=` |
| Logic | `and` `or` `not` |
| Nil fallback | `??` (the right side only when the left is `nil`) |
| Text | `..` (concatenation) `#` (length) |
| Bitwise | `&` `|` `~` (xor, or not when unary) `<<` `>>` (arithmetic) `<<<` `>>>` (logical, 64-bit) |

`and` and `or` return one of their operands, so `x and y or z` works as a conditional expression.

Compound assignment works for every binary operator: `+=`, `-=`, `*=`, `/=`, `//=`, `%=`, `^=`, `..=`, `&=`, `|=`, `<<=`, `>>=` and `??=` (assign only if the target is `nil`). `count++` and `count--` add or subtract one as statements; `--` after an expression is a decrement, while `--` at the start of a line or after whitespace begins a comment.

## Control flow

```lua
if temperature > 30 then
    print("hot")
elseif temperature < 5 then
    print("cold")
else
    print("mild")
end

while queue:hasWork() do queue:step() end

repeat
    line = io.read()
until line == nil or line == "quit"

for i = 1, 10 do print(i) end         -- 1 to 10
for i = 10, 1, -2 do print(i) end     -- 10, 8, 6, 4, 2

for index, value in { "a", "b" } do print(index, value) end
for key, value in config do print(key, value) end
for word in ("a b c"):gmatch("%a+") do print(word) end
```

`for k, v in t` walks any table: the array part in order, then the other keys. It also accepts an iterator function such as the ones `gmatch`, `fs.lines` and `utf8.codes` return, and `pairs(t)` or `ipairs(t)` as in Lua. `break` leaves a loop and `continue` skips to its next iteration.

## Functions

```lua
local function greet(name: string, greeting: string?): string
    return `{greeting ?? "Hello"}, {name}!`
end

local square = function(x) return x * x end

local function divide(a, b)
    if b == 0 then return nil, "division by zero" end
    return a / b
end
local result, err = divide(1, 0)
```

Functions are values: pass them around, store them in tables and return them. They capture the local variables around them (closures).

```lua
local function counter()
    local n = 0
    return function() n += 1; return n end
end
local next = counter()
print(next(), next()) --> 1	2
```

A final `...` parameter collects any extra arguments. `{...}` packs them into a table, `select("#", ...)` counts them, and `f(...)` passes them on.

```lua
local function sum(...: number): number
    local total = 0
    for _, n in { ... } do total += n end
    return total
end
```

## Methods and objects

`object:method(args)` calls `object.method(object, args)`, and `function T:method()` defines a function whose first parameter is `self`. With metatables this gives classes:

```lua
local Vector = {}
Vector.__index = Vector

function Vector.new(x: number, y: number)
    return setmetatable({ x = x, y = y }, Vector)
end

function Vector:length(): number
    return math.sqrt(self.x ^ 2 + self.y ^ 2)
end

Vector.__add = function(a, b) return Vector.new(a.x + b.x, a.y + b.y) end
Vector.__tostring = function(v) return `({v.x}, {v.y})` end

local v = Vector.new(3, 4) + Vector.new(0, 1)
print(tostring(v), v:length()) --> (3, 5)	5.830951894845301
```

Metatables also provide `__index` and `__newindex` for missing keys, the arithmetic and comparison operators, `__call`, `__len`, `__concat`, `__pairs`, `__tojson`, `__mode` for weak tables and `__gc` for finalizers. `table.freeze(t)` makes a table read-only; a `const function` field cannot be replaced.

## Errors

`error(message)` raises an error; `try(f, ...)` (or `pcall`) runs a function and catches one. An uncaught error stops the script and prints the message with a stack trace.

```lua
local ok, value = try(json.decode, input)
if not ok then
    print("bad input: " .. value)
end
```

`assert(condition, message)` raises `message` when the condition is false or nil.

## Modules

A module is a file that returns a value, usually a table. `require` runs it once and caches the result.

```lua
-- geometry.nyk
local geometry = {}
function geometry.area(w, h) return w * h end
return geometry

-- main.nyk
local geometry = require("geometry")
print(geometry.area(2, 3))
```

The standard libraries (`math`, `string`, `table`, `bit`, `buffer`, `coroutine`, `task`, `utf8`, `debug`, `json`, `os`) are always available as globals. The bundled modules are loaded with `require("@neyuki/name")`: `fs`, `io`, `http`, `net`, `sql`, `crypto`, `ui`, `hid`, `profile` and `webview`.

A `--!importall` line before the first line of code makes every bundled module available under its own name, without a `require` for each:

```lua
--!importall
fs.write("hello.txt", crypto.randomBytes(8))
```

Only the modules the file names are loaded, and a name the file declares itself keeps its own meaning. The directive applies to the file it is in, not to the modules that file requires.

## Limits

Statements and expressions may nest at most 120 levels deep, and each binary operator in a chain such as `a + b + c` counts as a level. A coroutine can only yield from its own frames, not from inside a function a native library called back (a `try` body, a `table.sort` comparator, a metamethod).
