# @neyuki/log

Structured logging with log levels, ANSI-colored console output, JSON Lines formatting, child loggers and rotating file sinks.

```lua
local log = require("@neyuki/log")

log.setLevel("debug")
log.info("server started", { host = "127.0.0.1", port = 8080 })

local app = log.new({
    name = "api",
    level = "info",
    fields = { service = "billing" },
    sinks = {
        { type = "console", stream = "stderr", format = "pretty", color = "auto" },
        { type = "file", path = "app.jsonl", format = "json", maxSize = 10 * 1024 * 1024, maxFiles = 5 },
    },
})

local reqLog = app:child({ reqId = "9f1a" })
reqLog:warn("slow query", { ms = 412 })
app:close()
```

Levels are ordered `trace` (10) < `debug` (20) < `info` (30) < `warn` (40) < `error` (50) < `fatal` (60) < `off` (100). Messages below the logger's threshold (or below an individual sink's threshold) are dropped before formatting.

In JSON Lines format (`format = "json"`), each record is written as one compact JSON object with `time`, `level`, optional `logger`, `msg`, and any structured fields sorted by key. Reserved keys (`time`, `level`, `msg`, and `logger` when named) stay authoritative; a user field with a colliding name is prefixed with `field_`. Non-JSON values (`NaN`, `Infinity`, functions, buffers, cycles) are formatted as descriptive strings so logging never crashes the caller.

In pretty format (`format = "pretty"`), ANSI color is enabled automatically on terminal streams unless `NO_COLOR` is set or `TERM=dumb`, and can be forced with `color = true` or `color = false` (or `FORCE_COLOR=1`). Control characters and ANSI escape bytes in messages are escaped.

File sinks (`type = "file"`) default to JSON Lines (`format = "json"`) and support size-based rotation (`maxSize` bytes, naming rotated files `<path>.1`, `<path>.2`, ... when `rotation = "none"`), time-based rotation (`rotation = "daily"` or `"hourly"`, naming archives `<stem>.YYYY-MM-DD<ext>` or `<stem>.YYYY-MM-DD-HH<ext>`, and `<stem>.<date>.1<ext>`, `<stem>.<date>.2<ext>`, ... when combined with `maxSize` within the same period), retention (`maxFiles`, default `5`; `0` keeps all), and optional gzip compression (`compress = true`).

## log.LEVEL: table

Frozen table of numeric level thresholds: `trace` (`10`), `debug` (`20`), `info` (`30`), `warn` (`40`), `error` (`50`), `fatal` (`60`) and `off` (`100`).

## log.new(options: table?): Logger

Creates an independent `Logger`. `options` may hold `name` (string), `level` (level name or `log.LEVEL` value, default `"info"`), `fields` (table of default structured fields attached to every record), `utc` (boolean, default `true`), and `sinks` (array of sink definitions or functions). When `sinks` is omitted, a single console sink is created using `stream` (default `"stderr"`), `format` (default `"pretty"`) and `color` (default `"auto"`).

Each sink table accepts `type` (`"console"`, `"file"`, or `"fn"`), optional `level`, `format` (`"pretty"` or `"json"`), `color` (`"auto"`, `true` or `false`) and `utc`. File sinks require `path` and accept `maxSize`, `rotation` (`"none"`, `"daily"`, `"hourly"`), `maxFiles` and `compress`. Function sinks require `write(record)` receiving `{ time, level, levelValue, logger, message, fields, line }`.

## log.configure(options: table?): ()

Replaces the module's default logger with `log.new(options)` and closes any file sinks owned by the previous default logger. Existing child loggers created before `log.configure` continue referencing the old sinks, so writing to a file sink on such a child after its parent was closed or reconfigured raises a closed-sink error.

## log.setLevel(level: string | number): ()

Sets the minimum level on the default logger.

## log.getLevel(): string

Returns the current minimum level name of the default logger.

## log.child(fields: table?, options: table?): Logger

Creates a child logger from the default logger with extra bound `fields` and optional `{ name?, level? }`. Child loggers share their parent's sinks and do not close them when the child is closed.

## log.log(level: string | number, message: string, fields: table?): ()

Writes a record at `level` through the default logger.

## log.trace(message: string, fields: table?): ()

Logs a `trace` record through the default logger.

## log.debug(message: string, fields: table?): ()

Logs a `debug` record through the default logger.

## log.info(message: string, fields: table?): ()

Logs an `info` record through the default logger.

## log.warn(message: string, fields: table?): ()

Logs a `warn` record through the default logger.

## log.error(message: string, fields: table?): ()

Logs an `error` record through the default logger.

## log.fatal(message: string, fields: table?): ()

Logs a `fatal` record through the default logger.

## log.flush(): ()

Flushes all file sinks attached to the default logger.

## log.rotate(): ()

Rotates all file sinks attached to the default logger immediately.

## log.close(): ()

Flushes and closes all file sinks owned by the default logger.

## Logger:setLevel(level: string | number): ()

Sets the logger's minimum level (`"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`, `"fatal"`, `"off"` or the matching `log.LEVEL` number).

## Logger:getLevel(): string

Returns the logger's current level name.

## Logger:child(fields: table?, options: table?): Logger

Returns a new `Logger` sharing this logger's sinks, merging `fields` on top of existing bound fields. `options` may override `name` (appended with `.` to the parent's name if present) and `level`.

## Logger:log(level: string | number, message: string, fields: table?): ()

Emits a record at `level` with `message` and optional structured `fields`.

## Logger:trace(message: string, fields: table?): ()

Emits a `trace` record.

## Logger:debug(message: string, fields: table?): ()

Emits a `debug` record.

## Logger:info(message: string, fields: table?): ()

Emits an `info` record.

## Logger:warn(message: string, fields: table?): ()

Emits a `warn` record.

## Logger:error(message: string, fields: table?): ()

Emits an `error` record.

## Logger:fatal(message: string, fields: table?): ()

Emits a `fatal` record.

## Logger:flush(): ()

Flushes buffered data on every attached file sink.

## Logger:rotate(): ()

Rotates every attached file sink immediately.

## Logger:close(): ()

Closes the logger and any file sinks it owns. Child loggers mark themselves closed without closing their parent's sinks; if the parent logger is closed, any subsequent write through a child logger's file sink raises a closed-sink error.
