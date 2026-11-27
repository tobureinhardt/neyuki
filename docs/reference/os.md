# os

Time, the environment, command-line arguments, the process, and per-user storage for apps.

## os.clock(): number

Seconds as a float, for measuring how long something takes.

```lua
local start = os.clock()
work()
print(("took %.3f s"):format(os.clock() - start))
```

## os.time(t: table?): number

Whole seconds since the Unix epoch (1970-01-01 UTC). Given a table with `year`, `month`, `day` and optionally `hour` (default 12), `min` and `sec`, returns that local time instead. Fields out of range carry over, so `month = 13` is January of the next year.

```lua
local deadline = os.time({ year = 2026, month = 12, day = 24, hour = 18 })
print(os.difftime(deadline, os.time()) // 86400, "days left")
```

## os.date(format: string?, time: number?): string | table

Formats `time` (default: now, in seconds since the epoch) in local time with strftime codes: `%Y` year, `%m` month, `%d` day, `%H` `%M` `%S` time, `%A`/`%a` weekday name, `%B`/`%b` month name, `%j` day of the year, `%Z` time zone and `%%` a percent sign, among others. The default format is `"%c"`. A leading `!` formats in UTC, and `"*t"` (or `"!*t"`) returns a table with `year`, `month`, `day`, `hour`, `min`, `sec`, `wday` (Sunday is 1) and `yday`.

```lua
print(os.date("%Y-%m-%d %H:%M"))          --> 2026-10-13 22:19
print(os.date("!%Y-%m-%dT%H:%M:%SZ", 0))  --> 1970-01-01T00:00:00Z
local today = os.date("*t")
print(today.year, today.month, today.day)
```

## os.difftime(t2: number, t1: number): number

`t2 - t1` in seconds, as a float.

## os.getenv(name: string): string?

The value of an environment variable, or `nil` when it is unset.

```lua
local port = tonumber(os.getenv("PORT") ?? "8080")
```

## os.args(): {string}

The command-line arguments that follow the script path.

```lua
-- neyuki greet.nyk Yuki
local name = os.args()[1] ?? "world"
print("hello, " .. name)
```

## os.app(): table

Describes the app that is running. In an app built with `neyuki build` (also a `.nykpak` from `--no-runtime`) it returns `{ bundled = true, name, version, identifier }`, taken from the `[package]` and `[app]` sections of `neyuki.toml` (`identifier` is present only when the manifest sets it). Under `neyuki run` it returns `{ bundled = false }`. The table is frozen.

```lua
local app = os.app()
if app.bundled then
    print(app.name .. " " .. app.version)
end
```

## os.run(command: string | {string}, options: table?): table

Runs a program, waits for it and returns `{ ok, code, stdout, stderr }`, where `ok` is true for exit code 0. A string runs through the system shell (`cmd /C` on Windows, `sh -c` elsewhere), so pipes and redirection work; a list runs its first element with the rest as arguments and no shell, so nothing needs quoting. `options` may hold `cwd` (the directory to run in), `env` (a table of variables to add or override), `stdin` (text to feed the program) and `capture` (default `true`; `false` lets the output go straight to the terminal). A program that cannot be started raises an error; one that fails just returns `ok = false`.

```lua
local result = os.run({ "git", "log", "--oneline", "-5" })
if result.ok then print(result.stdout) else print("git failed: " .. result.stderr) end

os.run("cargo build --release", { cwd = "../engine", capture = false })
```

## os.spawn(command: string | {string}, options: table?): Process

Starts a program and returns at once instead of waiting for it, so its output can be streamed. `command` is a string for the system shell or a list that runs without one, as in `os.run`. `options` may hold `cwd`, `env`, `stdin`, `stdout` and `stderr` (each `"pipe"`, `"inherit"` (default) or `"null"`) and `showWindow` (Windows only; by default the program gets no console window). The `Process` has `pid`, `stdin`, `stdout` and `stderr` (connections as in `@neyuki/net`, present for the streams set to `"pipe"`), `p:wait()`, `p:kill()` and `p:isRunning()`.

`p:wait()` suspends the task until the program ends and returns its exit code (-1 when a signal ended it). `p:kill()` ends the program and may be called on one that already ended. A program started with `os.spawn` is not stopped when the script ends.

```lua
local p = os.spawn({ "git", "log", "--oneline" }, { stdout = "pipe" })
while true do
    local chunk = p.stdout:read(4096)
    if chunk == nil then break end
    io.write(buffer.tostring(chunk))
end
print("exit code", p:wait())
```

## os.sleep(seconds: number): ()

Pauses the script for `seconds` (fractions allowed).

## os.exit(code: number | boolean?): never

Flushes output and ends the process with exit code 0 (nothing or `true`), 1 (`false`) or the given integer.

## os.dataDir(appName: string): string

The directory where an app keeps the files it owns, created (with any missing parents) if it does not exist yet:

| Platform | Path |
| --- | --- |
| Windows | `%APPDATA%\<appName>` |
| macOS | `~/Library/Application Support/<appName>` |
| Linux and BSD | `$XDG_DATA_HOME/<appName>`, by default `~/.local/share/<appName>` |

`appName` must be a single directory name: not empty, not `.` or `..`, and without path separators, control characters or any of `< > : " | ? *`, so a name that works on one platform works on all of them. `"com.example.notes"` and `"My Notes"` are fine.

```lua
local fs = require("@neyuki/fs")
local json = require("@neyuki/json")

local settings = fs.join(os.configDir("My Notes"), "settings.json")
fs.write(settings, json.encode({ theme = "dark" }))
```

## os.configDir(appName: string): string

The directory for an app's settings, created like `dataDir`: `%APPDATA%\<appName>` on Windows, `~/Library/Application Support/<appName>` on macOS and `$XDG_CONFIG_HOME/<appName>` (by default `~/.config/<appName>`) on Linux.

## os.cacheDir(appName: string): string

The directory for files an app can rebuild, such as downloaded thumbnails, created like `dataDir`: `%LOCALAPPDATA%\<appName>\cache` on Windows (which is not roamed to other machines), `~/Library/Caches/<appName>` on macOS and `$XDG_CACHE_HOME/<appName>` (by default `~/.cache/<appName>`) on Linux. The system or the user may clear it at any time.

## os.protect(data: string): string

Seals `data` so that only the same user on the same machine can read it back with `unprotect`, for keeping session cookies, tokens and similar secrets in a file. Returns base64 text.

```lua
local fs = require("@neyuki/fs")

local file = fs.join(os.dataDir("My Notes"), "session")
fs.write(file, os.protect(cookie))
-- later, or in the next run
local cookie = os.unprotect(fs.read(file))
```

- On **Windows** it uses DPAPI (`CryptProtectData`) for the current user. No setup is needed.
- On **macOS** and **Linux** it encrypts with AES-256-GCM under a random key that it creates on first use and keeps in the OS keychain (Keychain on macOS; the Secret Service, such as GNOME Keyring or KWallet, on Linux), filed under the service `neyuki` and the account `os.protect`. Where no keychain is reachable, as on a headless Linux server without a Secret Service, it raises an error saying so rather than storing the data unprotected.

What this protects against: other users of the machine, and anyone who copies the file to another machine or reads it from a backup. What it does not: other programs running as the same user can unprotect the data too (on Windows any of them; on macOS the keychain may ask the user first), and so can anyone who signs in as the user. Every Neyuki script a user runs shares one key, so it does not keep one app's secrets from another. Deleting the keychain entry, or having an administrator reset the user's Windows password, makes existing blobs unreadable, so protect only what can be recovered another way (a session can be signed in to again).

## os.unprotect(blob: string): string

The string `protect` sealed into `blob`. A blob that is damaged, was sealed by another user or machine, or was sealed on another operating system raises an error.
