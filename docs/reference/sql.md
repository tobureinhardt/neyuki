# @neyuki/sql

PostgreSQL, MySQL, MariaDB and SQLite through drivers built into the executable (no libpq, MySQL client library or system SQLite needed). The network drivers use TLS through the operating system's own library; SQLite runs inside the process on a local file.

```lua
local sql = require("@neyuki/sql")
local db = sql.connect(os.getenv("DATABASE_URL"))

db:execute("CREATE TABLE IF NOT EXISTS users (id SERIAL PRIMARY KEY, name TEXT NOT NULL)")
local user = db:queryOne("INSERT INTO users (name) VALUES ($1) RETURNING id", { "Ada" })

for _, row in db:query("SELECT id, name FROM users ORDER BY id") do
    print(row.id, row.name)
end
db:close()
```

SQLite works the same way, with no server to run:

```lua
local db = sql.connect("sqlite:app.db") -- created if missing; "sqlite::memory:" for a throwaway database

db:execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
local _, id = db:execute("INSERT INTO users (name) VALUES (?)", { "Ada" })
print(db:queryOne("SELECT name FROM users WHERE id = ?", { id }).name)
db:close()
```

Parameters are positional in the database's own syntax (`$1, $2` on PostgreSQL, `?` on MySQL and SQLite, which also takes `?1, ?2`) and are sent separately from the SQL text, so values are never spliced into it. A parameter may be `nil` (NULL), a boolean, a number or a string.

Results keep their types: integers stay integers, floats stay floats, PostgreSQL booleans are booleans (MySQL's and SQLite's are `0` and `1`), text is a string and a PostgreSQL array is an array. `numeric`/`decimal` come back as strings so no digit is rounded; dates, times, timestamps, `uuid` and `json` come back in their text form (`timestamptz` in UTC). A NULL column is absent from its row. Types the module does not know (`interval`, `inet`, ranges, composites) raise an error naming them; cast them to `text` in the query. SQLite values keep their storage class (`INTEGER`, `REAL`, `TEXT`), and a `BLOB` comes back as a string.

## sql.connect(target: string | table): Connection

Connects to a database. `target` is a URL (`"postgres://user:pass@host:5432/db"`, `"mysql://user:pass@host/db"`, `"sqlite:app.db"`) or a table with `driver` (`"postgres"`, `"mysql"` or `"sqlite"`), `host`, `port`, `user`, `password`, `database`, `ssl` and `timeout` (seconds to wait). A table may also carry `url`, with its other fields overriding parts of it. TLS is required for any host other than this machine unless `ssl` is `false` (or a PostgreSQL URL sets `sslmode`).

For SQLite only `database` (the file path) and `timeout` matter. The file is created if it does not exist, relative paths are relative to the working directory, and `":memory:"` opens a private in-memory database. In a URL the `//` is optional: `sqlite:app.db` and `sqlite://app.db` name the relative file, `sqlite:///var/lib/app.db` an absolute one, and `sqlite::memory:` an in-memory database. SQLite `file:` URIs pass through, so `sqlite:file:app.db?mode=ro` opens the file read-only. `timeout` is how long a statement waits for another process's lock before failing (5 seconds by default).

## Connection:query(statement: string, params: {any}?): {table}

Runs a statement and returns its rows as an array of `{ column = value }` tables. `rows.columns` lists the column names in order.

## Connection:queryOne(statement: string, params: {any}?): table?

The first row of the result, or `nil`.

## Connection:execute(statement: string, params: {any}?): (number, number?)

Runs a statement for its effect and returns the number of rows it touched, then the auto-increment id of a MySQL insert or the rowid of an SQLite insert. Without parameters the statement uses the simple protocol, so `BEGIN` and schema changes work everywhere; on SQLite the text may then hold several `;`-separated statements, which run in order.

## Connection:transaction(body: (Connection) -> any): any

Runs `body(connection)` in a transaction: commits when it returns, rolls back and re-raises when it fails, and returns what `body` returned.

```lua
db:transaction(function(tx)
    tx:execute("UPDATE accounts SET balance = balance - $1 WHERE id = $2", { 25, 1 })
    tx:execute("UPDATE accounts SET balance = balance + $1 WHERE id = $2", { 25, 2 })
end)
```

## Connection:begin(): ()

Starts a transaction by hand.

## Connection:commit(): ()

Commits the transaction started with `begin`.

## Connection:rollback(): ()

Rolls back the transaction started with `begin`.

## Connection:ping(): boolean

Whether the server still answers. Always `true` for an open SQLite database.

## Connection:close(): ()

Closes the connection. Calling it again does nothing.

## Connection:isOpen(): boolean

Whether the connection has not been closed.
