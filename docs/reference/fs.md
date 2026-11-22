# @neyuki/fs

Files, directories and paths. Text files are read and written as UTF-8 strings.

```lua
local fs = require("@neyuki/fs")

fs.write("notes.txt", "first line\n")
fs.append("notes.txt", "second line\n")
for line in fs.lines("notes.txt") do print(line) end

for _, path in fs.walk("src") do
    if fs.extension(path) == ".nyk" then print(path, fs.info(fs.join("src", path)).size) end
end
```

## fs.read(path: string): string

The contents of a text file.

## fs.readBuffer(path: string): buffer

The contents of any file as a [buffer](buffer.md), for binary formats such as images and sounds. Like `fs.read`, it reads a built app's own files and checks the sandbox; files of 1 MiB or more are read without blocking other tasks. Raises an error for a file over the 1 GiB buffer limit.

```lua
local png = fs.readBuffer("logo.png")
print(buffer.readstring(png, 1, 3)) --> PNG
```

## fs.write(path: string, text: string): ()

Replaces the contents of a file, creating it if needed.

## fs.append(path: string, text: string): ()

Adds text to the end of a file, creating it if needed.

## fs.lines(path: string): () -> string?

An iterator over the lines of a file, without their `\n` or `\r\n` endings.

## fs.open(path: string, mode: string?): File

Opens a file as a `File` object. `mode` is `"r"` (read, the default; the file must exist), `"w"` (write, replacing the contents) or `"a"` (append). A `File` has the fields `path` and `mode` and these methods:

- `file:read()` returns the whole contents.
- `file:write(text, append?)` writes text. The first write to a `"w"` file replaces the contents and later ones append; `"a"` files always append.
- `file:lines()` returns an iterator over the lines.
- `file:close()` closes it; any later call raises an error.

## fs.exists(path: string): boolean

Whether a file or directory exists at `path`.

## fs.isFile(path: string): boolean

Whether `path` is a file.

## fs.isDir(path: string): boolean

Whether `path` is a directory.

## fs.info(path: string): table?

`{ size, isDir, isFile, readonly, modified, created }` for a path, or `nil` when nothing is there. `size` is in bytes and the times are seconds since the Unix epoch, ready for `os.date`.

```lua
local info = fs.info("notes.txt")
print(info.size, os.date("%Y-%m-%d %H:%M", info.modified))
```

## fs.copy(from: string, to: string): number

Copies a file, replacing `to`, and returns the number of bytes copied.

## fs.rename(from: string, to: string): ()

Moves or renames a file or directory.

## fs.remove(path: string, recursive: boolean?): ()

Deletes a file or an empty directory. With `recursive`, deletes a directory and everything in it.

## fs.mkdir(path: string, recursive: boolean?): ()

Creates a directory. With `recursive`, also creates missing parents and does nothing if it already exists.

## fs.list(path: string): {string}

The names of the entries in a directory, sorted.

## fs.walk(path: string): {string}

Every file under a directory, at any depth, as paths relative to it joined with `/`, sorted. Directories themselves are not listed.

## fs.join(...: string): string

Joins path parts with `/` (which every platform accepts) without doubling separators. A part that is an absolute path starts over.

```lua
print(fs.join("assets", "img/", "logo.png")) --> assets/img/logo.png
```

## fs.basename(path: string): string

The last part of a path: `"guide.md"` for `"docs/guide.md"`.

## fs.dirname(path: string): string

Everything before the last part: `"docs"` for `"docs/guide.md"`, and `"."` for a bare name.

## fs.extension(path: string): string

The extension of the last part including the dot (`".md"`), or `""` when there is none. A leading dot as in `".gitignore"` is not an extension.

## fs.absolute(path: string): string

`path` made absolute against the working directory, without resolving links.

## fs.cwd(): string

The working directory, which relative paths start from.
