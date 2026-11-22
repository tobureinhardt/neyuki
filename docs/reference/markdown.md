# @neyuki/markdown

Parsing CommonMark and GitHub Flavored Markdown (GFM) into plain-table ASTs and rendering safe HTML.

```lua
local markdown = require("@neyuki/markdown")

local html = markdown.toHtml([[
# Hello **Neyuki**

- [x] CommonMark 0.31
- [x] GFM tables, task lists & strikethrough
]], { headingIds = true })

print(html)
```

AST nodes returned by `markdown.parse` are plain tables that scripts can inspect or transform directly, following the same convention as `@neyuki/html`:

| Node | Fields |
| --- | --- |
| document | `type = "document"`, `frontmatter?` (`{ format, raw }`), `children` |
| heading | `type = "heading"`, `level` (`1`..`6`), `id?`, `children` |
| paragraph | `type = "paragraph"`, `children` |
| codeBlock | `type = "codeBlock"`, `lang?`, `text` |
| blockquote | `type = "blockquote"`, `kind?` (`"note"`, `"tip"`, `"important"`, `"warning"`, `"caution"`), `children` |
| list | `type = "list"`, `ordered`, `start?`, `children` |
| listItem | `type = "listItem"`, `checked?`, `children` |
| table | `type = "table"`, `align`, `head`, `rows` |
| thematicBreak | `type = "thematicBreak"` |
| footnoteDefinition | `type = "footnoteDefinition"`, `label`, `children` |
| footnoteReference | `type = "footnoteReference"`, `label` |
| text / code | `type = "text"` or `"code"`, `text` |
| emphasis / strong / strikethrough | `type`, `children` |
| link | `type = "link"`, `url`, `title?`, `children` |
| image | `type = "image"`, `url`, `alt`, `title?`, `children` |
| softBreak / hardBreak | `type = "softBreak"` or `"hardBreak"` |
| htmlBlock / htmlInline | `type`, `text` |

All functions accept an optional `options` table:

- `gfm` (default `true`): enables GFM tables, strikethrough, task lists, footnotes and alert blockquotes.
- `breaks` (default `false`): turns soft line breaks into `<br />` / `hardBreak`.
- `smart` (default `false`): enables smart quotes, dashes and ellipses.
- `unsafeHtml` (default `false`): when `false`, raw HTML is escaped as text and non-allowlisted link/image URL schemes (`javascript:`, `vbscript:`, `data:`) are cleared.
- `headingIds` (default `false`): enables heading attributes (`# Title {#custom}`) and generates unique slug `id` attributes on headings.
- `frontmatter` (default `true`): recognizes leading `---` (YAML) and `+++` (TOML) frontmatter blocks and excludes them from rendered HTML.

## markdown.toHtml(text: string, options: table?): string

Renders Markdown `text` to HTML. Safe by default (`unsafeHtml = false`). Raises an error if nesting exceeds 256 levels or output exceeds 64 MiB.

```lua
print(markdown.toHtml("Read [docs](https://neyuki.cc) & `<safe>`"))
```

## markdown.parse(text: string, options: table?): table

Parses Markdown `text` into a `"document"` AST node table.

```lua
local doc = markdown.parse("# Title\n\nHello *world*")
print(doc.children[1].type, doc.children[1].level) --> heading	1
```

## markdown.frontmatter(text: string): (table?, string)

Extracts a leading YAML (`---`) or TOML (`+++`) frontmatter block from `text`. Returns `{ format = "yaml" | "toml", raw = "..." }` followed by the remaining Markdown body, or `(nil, text)` when no frontmatter block is present.

```lua
local fm, body = markdown.frontmatter("---\ntitle: Guide\n---\n# Body")
print(fm.format, body) --> yaml	# Body
```

## markdown.text(input: string | table): string

Extracts plain text without Markdown formatting from either a Markdown string or an AST node table produced by `markdown.parse`.

```lua
print(markdown.text("Hello **bold** `code`")) --> Hello bold code
```
