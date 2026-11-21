//! Native database primitives backing `@neyuki/sql`.
//!
//! `__sql_open` connects to a PostgreSQL server through the `postgres` crate
//! or to a MySQL/MariaDB server through `mysql`, or opens an SQLite database
//! in-process through `rusqlite`, and parks the connection here under an id;
//! `__sql_query` and `__sql_execute` run statements on it and `__sql_close`
//! drops it. All drivers are synchronous; the network ones use TLS through
//! the platform's `native-tls` like `@neyuki/http`.
//!
//! Statements run as prepared statements with positional parameters (`$1` on
//! PostgreSQL, `?` on MySQL and SQLite), so values never get spliced into SQL
//! text. PostgreSQL parameters are sent in text format so the server converts
//! them to whatever type the statement expects, the way `psql` does; results
//! come back in binary and are decoded here.
//! `lib/sql.nyk` builds the `Connection` object on top.

use crate::vm::permissions::{PermissionScope, Permissions};
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Write as _;
use std::time::Duration;

use fallible_iterator::FallibleIterator;
use mysql::prelude::Queryable;
use postgres::types::private::BytesMut;
use postgres::types::{Format, FromSql, IsNull, Kind, ToSql, Type};
use postgres_protocol::types as pg;

use crate::vm::libs::args::array as new_table;
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, StrRef, Value};

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__sql_open",
        func: builtin_open,
    },
    NativeDef {
        name: "__sql_close",
        func: builtin_close,
    },
    NativeDef {
        name: "__sql_query",
        func: builtin_query,
    },
    NativeDef {
        name: "__sql_execute",
        func: builtin_execute,
    },
    NativeDef {
        name: "__sql_ping",
        func: builtin_ping,
    },
];

enum Connection {
    // Boxed: the client embeds its tokio runtime and dwarfs the MySQL side.
    Postgres(Box<postgres::Client>),
    MySql(mysql::Conn),
    Sqlite(rusqlite::Connection),
}

thread_local! {
    static CONNECTIONS: RefCell<HashMap<u64, Connection>> = RefCell::new(HashMap::new());
    static NEXT_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

fn next_id() -> u64 {
    NEXT_ID.with(|id| {
        let value = id.get();
        id.set(value + 1);
        value
    })
}

/// Runs `body` on the connection `id`. Natives cannot call back into Neyuki,
/// so nothing can reach for the map while the connection is borrowed.
fn with_connection<T>(
    id: u64,
    body: impl FnOnce(&mut Connection) -> Result<T, String>,
) -> Result<T, String> {
    CONNECTIONS.with(|connections| {
        let mut connections = connections.borrow_mut();
        let connection = connections
            .get_mut(&id)
            .ok_or_else(|| "connection is closed".to_string())?;
        body(connection)
    })
}

// --- argument helpers -------------------------------------------------------

fn string_arg(args: &[Value], index: usize, name: &str) -> Result<String, String> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value.to_string()),
        Some(value) => Err(format!(
            "{} must be a string, got {}",
            name,
            value.type_name()
        )),
        None => Err(format!("{} must be provided", name)),
    }
}

fn integer_arg(args: &[Value], index: usize, name: &str) -> Result<u64, String> {
    let value = args.get(index).unwrap_or(&Value::Nil);
    let number =
        crate::vm::libs::args::number(value).map_err(|_| format!("{} must be a number", name))?;
    if number < 0.0 || number.fract() != 0.0 {
        return Err(format!("{} must be a non-negative integer", name));
    }
    Ok(number as u64)
}

/// The array part of the parameter table; nil or absent means no parameters.
/// Holes (`{ "a", nil, "c" }`) are kept, since they bind as NULL.
fn params_arg(args: &[Value], index: usize) -> Result<Vec<Value>, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(Vec::new()),
        Some(Value::Table(table)) => {
            let table = table.borrow();
            if !table.fields.is_empty() || table.general.is_some() {
                return Err(
                    "params must be an array; named parameters are not supported".to_string(),
                );
            }
            Ok(table.array.clone())
        }
        Some(value) => Err(format!("params must be a table, got {}", value.type_name())),
    }
}

fn integer(value: u64) -> Value {
    Value::from_u64(value)
}

/// Builds the result of a query: an array of `{ column = value }` rows that
/// also carries the column names, in select order, under `columns`.
fn result_table(columns: Vec<String>, rows: Vec<Value>) -> Value {
    let names = new_table(columns.into_iter().map(Value::string).collect());
    let table = new_table(rows);
    if let Value::Table(inner) = &table {
        inner
            .borrow_mut()
            .fields
            .insert(StrRef::from("columns"), names);
    }
    table
}

fn row_table(columns: &[String], values: Vec<Value>) -> Value {
    let table = new_table(Vec::new());
    if let Value::Table(inner) = &table {
        let mut inner = inner.borrow_mut();
        for (name, value) in columns.iter().zip(values) {
            // Only nil-free fields are stored so `row.column == nil` reads as
            // NULL and iteration skips it, like any other absent key.
            if !matches!(value, Value::Nil) {
                inner.fields.insert(StrRef::from(name.as_str()), value);
            }
        }
    }
    table
}

/// Neyuki strings are UTF-8, so binary columns that are not valid UTF-8 have
/// their offending bytes replaced with U+FFFD rather than failing the query.
fn bytes_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// --- connection options -----------------------------------------------------

struct OpenOptions {
    driver: Option<String>,
    url: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    database: Option<String>,
    ssl: Option<bool>,
    timeout: Option<Duration>,
}

fn options_arg(args: &[Value], index: usize) -> Result<OpenOptions, String> {
    let table = match args.get(index) {
        Some(Value::Table(table)) => table.clone(),
        Some(value) => {
            return Err(format!(
                "options must be a table, got {}",
                value.type_name()
            ));
        }
        None => return Err("options must be provided".to_string()),
    };
    let table = table.borrow();
    let string = |name: &str| -> Result<Option<String>, String> {
        match table.fields.get(name) {
            None | Some(Value::Nil) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.to_string())),
            Some(value) => Err(format!(
                "{} must be a string, got {}",
                name,
                value.type_name()
            )),
        }
    };
    let port = match table.fields.get("port") {
        None | Some(Value::Nil) => None,
        Some(value) => {
            let number = crate::vm::libs::args::number(value)
                .map_err(|_| "port must be a number".to_string())?;
            if number < 1.0 || number > f64::from(u16::MAX) || number.fract() != 0.0 {
                return Err(format!("port {} is out of range (1-65535)", value));
            }
            Some(number as u16)
        }
    };
    let ssl = match table.fields.get("ssl") {
        None | Some(Value::Nil) => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(value) => return Err(format!("ssl must be a boolean, got {}", value.type_name())),
    };
    let timeout = match table.fields.get("timeout") {
        None | Some(Value::Nil) => None,
        Some(value) => {
            let seconds = crate::vm::libs::args::number(value)
                .map_err(|_| "timeout must be a number".to_string())?;
            if seconds < 0.0 || !seconds.is_finite() {
                return Err("timeout must be a non-negative number of seconds".to_string());
            }
            if seconds == 0.0 {
                None
            } else {
                Some(Duration::from_secs_f64(seconds))
            }
        }
    };
    Ok(OpenOptions {
        driver: string("driver")?,
        url: string("url")?,
        host: string("host")?,
        port,
        user: string("user")?,
        password: string("password")?,
        database: string("database")?,
        ssl,
        timeout,
    })
}

/// The driver named by `options.driver` or by the URL scheme.
fn driver_name(options: &OpenOptions) -> Result<&'static str, String> {
    let name = match &options.driver {
        Some(driver) => driver.to_ascii_lowercase(),
        None => match &options.url {
            // `sqlite:` URLs may skip the `//` (`sqlite::memory:`, `sqlite:app.db`).
            Some(url) if sqlite_url_path(url).is_some() => "sqlite".to_string(),
            Some(url) => url
                .split_once("://")
                .map(|(scheme, _)| scheme.to_ascii_lowercase())
                .ok_or_else(|| {
                    format!("`{}` is not a database URL (expected scheme://...)", url)
                })?,
            None => {
                return Err(
                    "driver must be provided (\"postgres\", \"mysql\" or \"sqlite\")".to_string(),
                );
            }
        },
    };
    match name.as_str() {
        "postgres" | "postgresql" | "pg" => Ok("postgres"),
        "mysql" | "mariadb" => Ok("mysql"),
        "sqlite" | "sqlite3" => Ok("sqlite"),
        other => Err(format!(
            "unknown database driver `{}` (expected \"postgres\", \"mysql\" or \"sqlite\")",
            other
        )),
    }
}

/// The file name in an `sqlite:` or `sqlite3:` URL, with the optional `//`
/// removed: `sqlite:///tmp/app.db` names `/tmp/app.db`, `sqlite://app.db`
/// and `sqlite:app.db` the relative `app.db`, and `sqlite::memory:` an
/// in-memory database. None when the URL has another scheme.
fn sqlite_url_path(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("sqlite") && !scheme.eq_ignore_ascii_case("sqlite3") {
        return None;
    }
    Some(rest.strip_prefix("//").unwrap_or(rest))
}

/// Whether `host` names this machine, so a connection to it never crosses a
/// network.
fn is_local_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn open_postgres(options: &OpenOptions) -> Result<postgres::Client, String> {
    let mut config = match &options.url {
        Some(url) => url
            .parse::<postgres::Config>()
            .map_err(|err| format!("invalid PostgreSQL URL: {}", err))?,
        None => postgres::Config::new(),
    };
    if let Some(host) = &options.host {
        config.host(host);
    }
    if let Some(port) = options.port {
        config.port(port);
    }
    if let Some(user) = &options.user {
        config.user(user);
    }
    if let Some(password) = &options.password {
        config.password(password);
    }
    if let Some(database) = &options.database {
        config.dbname(database);
    }
    if let Some(timeout) = options.timeout {
        config.connect_timeout(timeout);
    }
    if config.get_hosts().is_empty() {
        config.host("localhost");
    }
    match options.ssl {
        Some(true) => {
            config.ssl_mode(postgres::config::SslMode::Require);
        }
        Some(false) => {
            config.ssl_mode(postgres::config::SslMode::Disable);
        }
        None => {
            // Left unset, the driver would try TLS but fall back to plaintext,
            // which anyone on the path can force. Remote servers must use TLS
            // unless the URL picks its own `sslmode`.
            let url_sets_mode = options
                .url
                .as_ref()
                .is_some_and(|url| url.to_ascii_lowercase().contains("sslmode="));
            let all_local = config.get_hosts().iter().all(|host| match host {
                postgres::config::Host::Tcp(name) => is_local_host(name),
                #[allow(unreachable_patterns)]
                _ => true,
            });
            if !url_sets_mode && !all_local {
                config.ssl_mode(postgres::config::SslMode::Require);
            }
        }
    }
    if config.get_user().is_none() {
        return Err("user must be provided".to_string());
    }
    let tls =
        native_tls::TlsConnector::new().map_err(|err| format!("cannot set up TLS: {}", err))?;
    config
        .connect(postgres_native_tls::MakeTlsConnector::new(tls))
        .map_err(pg_error)
}

fn open_mysql(options: &OpenOptions) -> Result<mysql::Conn, String> {
    let mut builder = match &options.url {
        Some(url) => {
            let opts =
                mysql::Opts::from_url(url).map_err(|err| format!("invalid MySQL URL: {}", err))?;
            mysql::OptsBuilder::from_opts(opts)
        }
        None => mysql::OptsBuilder::new(),
    };
    if let Some(host) = &options.host {
        builder = builder.ip_or_hostname(Some(host.clone()));
    }
    if let Some(port) = options.port {
        builder = builder.tcp_port(port);
    }
    if let Some(user) = &options.user {
        builder = builder.user(Some(user.clone()));
    }
    if let Some(password) = &options.password {
        builder = builder.pass(Some(password.clone()));
    }
    if let Some(database) = &options.database {
        builder = builder.db_name(Some(database.clone()));
    }
    if let Some(timeout) = options.timeout {
        builder = builder.tcp_connect_timeout(Some(timeout));
    }
    match options.ssl {
        Some(true) => builder = builder.ssl_opts(mysql::SslOpts::default()),
        Some(false) => builder = builder.ssl_opts(None),
        None => {
            // Unset, the driver never uses TLS. Remote servers must, unless
            // the URL already configures it.
            let opts = mysql::Opts::from(builder.clone());
            let remote = opts.get_socket().is_none() && !is_local_host(&opts.get_ip_or_hostname());
            if remote && opts.get_ssl_opts().is_none() {
                builder = builder.ssl_opts(mysql::SslOpts::default());
            }
        }
    }
    mysql::Conn::new(builder).map_err(my_error)
}

fn sqlite_target_path(options: &OpenOptions) -> Result<&str, String> {
    let path = match (&options.database, &options.url) {
        (Some(database), _) => database.as_str(),
        (None, Some(url)) => sqlite_url_path(url).unwrap_or(""),
        (None, None) => "",
    };
    // SQLite would quietly open a temporary database for an empty name.
    if path.is_empty() {
        return Err(
            "database must be provided (a file path, or \":memory:\" for an in-memory database)"
                .to_string(),
        );
    }
    Ok(path)
}

/// Percent-decodes `s` the way SQLite does for `file:` URI paths (`%HH`
/// with two hex digits; anything else stays literal). Rejects a decoded NUL,
/// which would truncate the name SQLite opens.
fn percent_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    if out.contains(&0) {
        return Err("invalid SQLite database name: contains a NUL byte".to_string());
    }
    String::from_utf8(out).map_err(|_| "invalid SQLite database name: not UTF-8".to_string())
}

/// The filesystem path SQLite will actually open for `raw`, or `None` for an
/// in-memory database. `file:` URIs are decoded with SQLite's own rules
/// (authority must be empty or `localhost`, query and fragment are dropped,
/// the path is percent-decoded) so the permission check sees the real file.
/// A URI SQLite itself would reject is an error.
fn sqlite_fs_path(raw: &str) -> Result<Option<String>, String> {
    if raw == ":memory:" {
        return Ok(None);
    }
    let Some(after_scheme) = raw.strip_prefix("file:") else {
        return Ok(Some(raw.to_string()));
    };
    let core = after_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let path = if let Some(after_slashes) = core.strip_prefix("//") {
        let (authority, path) = match after_slashes.find('/') {
            Some(i) => after_slashes.split_at(i),
            None => (after_slashes, ""),
        };
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return Err(format!(
                "invalid SQLite URI '{}': authority must be empty or localhost",
                raw
            ));
        }
        path
    } else {
        core
    };
    let decoded = percent_decode(path)?;
    if decoded.eq_ignore_ascii_case(":memory:") {
        return Ok(None);
    }
    #[cfg(windows)]
    let decoded = match decoded.as_bytes() {
        [b'/', d, b':', ..] if d.is_ascii_alphabetic() => decoded[1..].to_string(),
        _ => decoded,
    };
    Ok(Some(decoded))
}

/// Installs an authorizer so a restricted connection cannot reach other files
/// through SQL: `ATTACH DATABASE` is limited to names that pass the read and
/// write checks (or in-memory / temporary databases), and `VACUUM INTO` is
/// denied outright because it writes a file the authorizer cannot name.
fn restrict_sqlite_files(
    conn: &mut rusqlite::Connection,
    permissions: &Permissions,
) -> Result<(), String> {
    use rusqlite::hooks::{AuthAction, Authorization};
    let permissions = permissions.clone();
    conn.authorizer(Some(
        move |ctx: rusqlite::hooks::AuthContext<'_>| match ctx.action {
            AuthAction::Attach { filename } => {
                if filename.is_empty() {
                    return Authorization::Allow;
                }
                match sqlite_fs_path(filename) {
                    Ok(None) => Authorization::Allow,
                    Ok(Some(path))
                        if permissions.check_read(&path).is_ok()
                            && permissions.check_write(&path).is_ok() =>
                    {
                        Authorization::Allow
                    }
                    _ => Authorization::Deny,
                }
            }
            _ => Authorization::Allow,
        },
    ))
    .map_err(lite_error)
}

/// Opens (creating if needed) the database file named by `database` or the
/// URL. `:memory:` gives a private in-memory database, and `file:` URIs pass
/// through to SQLite, so `file:app.db?mode=ro` opens read-only. `timeout` is
/// how long a statement waits for another process's lock before failing.
fn open_sqlite(
    options: &OpenOptions,
    permissions: &Permissions,
) -> Result<rusqlite::Connection, String> {
    let path = sqlite_target_path(options)?;
    let mut conn = rusqlite::Connection::open(path).map_err(lite_error)?;
    if permissions.read != PermissionScope::AllowAll
        || permissions.write != PermissionScope::AllowAll
    {
        restrict_sqlite_files(&mut conn, permissions)?;
    }
    if let Some(timeout) = options.timeout {
        conn.busy_timeout(timeout).map_err(lite_error)?;
    }
    Ok(conn)
}

/// `__sql_open(options)` connects and returns the connection id followed by
/// its driver name. `options` holds `driver` and/or `url` plus optional
/// `host`, `port`, `user`, `password`, `database`, `ssl` and `timeout`
/// fields, which override what the URL says.
fn builtin_open(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let options = options_arg(args, 0)?;
    let driver = driver_name(&options)?;
    if driver == "sqlite" {
        let target = sqlite_target_path(&options)?;
        if let Some(fs_path) = sqlite_fs_path(target)? {
            vm.permissions.check_read(&fs_path)?;
            vm.permissions.check_write(&fs_path)?;
        }
    } else {
        let target = options
            .url
            .as_deref()
            .or(options.host.as_deref())
            .unwrap_or("localhost");
        vm.permissions.check_net(target)?;
    }
    let connection = match driver {
        "postgres" => Connection::Postgres(Box::new(open_postgres(&options)?)),
        "mysql" => Connection::MySql(open_mysql(&options)?),
        _ => Connection::Sqlite(open_sqlite(&options, &vm.permissions)?),
    };
    let id = next_id();
    CONNECTIONS.with(|connections| connections.borrow_mut().insert(id, connection));
    Ok(vec![integer(id), Value::str(driver)])
}

/// `__sql_close(connection)` drops the connection; closing twice is fine.
fn builtin_close(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "connection")?;
    CONNECTIONS.with(|connections| connections.borrow_mut().remove(&id));
    Ok(vec![Value::Nil])
}

/// `__sql_ping(connection)` returns whether the server still answers. An
/// SQLite database lives in this process, so an open one always does.
fn builtin_ping(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "connection")?;
    let alive = with_connection(id, |connection| {
        Ok(match connection {
            Connection::Postgres(client) => client.is_valid(Duration::from_secs(30)).is_ok(),
            Connection::MySql(conn) => conn.ping().is_ok(),
            Connection::Sqlite(_) => true,
        })
    })?;
    Ok(vec![Value::Bool(alive)])
}

/// `__sql_query(connection, sql, params?)` runs one statement and returns
/// its rows as an array of `{ column = value }` tables with the column names
/// under `columns`.
fn builtin_query(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "connection")?;
    let sql = string_arg(args, 1, "sql")?;
    let params = params_arg(args, 2)?;
    let result = with_connection(id, |connection| match connection {
        Connection::Postgres(client) => pg_query(client, &sql, &params),
        Connection::MySql(conn) => my_query(conn, &sql, &params),
        Connection::Sqlite(conn) => lite_query(conn, &sql, &params),
    })?;
    Ok(vec![result])
}

/// `__sql_execute(connection, sql, params?)` runs one statement and returns
/// the number of rows it affected followed by the id of the inserted row
/// (MySQL auto-increment and SQLite rowid; nil on PostgreSQL). Without
/// parameters the statement goes through the simple protocol, which also
/// accepts what servers refuse to prepare (`BEGIN`, DDL on some versions);
/// SQLite then runs every `;`-separated statement in the text.
fn builtin_execute(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "connection")?;
    let sql = string_arg(args, 1, "sql")?;
    let params = params_arg(args, 2)?;
    let (affected, last_id) = with_connection(id, |connection| match connection {
        Connection::Postgres(client) => pg_execute(client, &sql, &params),
        Connection::MySql(conn) => my_execute(conn, &sql, &params),
        Connection::Sqlite(conn) => lite_execute(conn, &sql, &params),
    })?;
    Ok(vec![integer(affected), last_id.unwrap_or(Value::Nil)])
}

// --- PostgreSQL -------------------------------------------------------------

/// Server errors keep their message and SQLSTATE; client-side ones are
/// described only by their cause chain (`error deserializing column 0` says
/// nothing until the source is appended).
fn pg_error(err: postgres::Error) -> String {
    if let Some(db) = err.as_db_error() {
        return format!("{} (SQLSTATE {})", db.message(), db.code().code());
    }
    let mut message = err.to_string();
    let mut cause = err.source();
    while let Some(inner) = cause {
        write!(message, ": {}", inner).expect("writing to a String cannot fail");
        cause = inner.source();
    }
    message
}

/// A parameter in PostgreSQL's text format: the server parses it as whatever
/// type it inferred for the placeholder, so `WHERE id = $1` takes a Neyuki
/// integer and `WHERE created > $1` a timestamp string.
#[derive(Debug)]
struct Param(Option<String>);

impl Param {
    fn from_value(value: &Value, index: usize) -> Result<Param, String> {
        Ok(Param(match value {
            Value::Nil => None,
            Value::Bool(value) => Some(if *value { "t" } else { "f" }.to_string()),
            Value::Int(value) => Some(value.to_string()),
            Value::BigInt(value) => Some(value.to_string()),
            Value::Float(value) => Some(float_text(*value)),
            Value::String(value) => Some(value.to_string()),
            other => {
                return Err(format!(
                    "parameter {} must be nil, a boolean, a number or a string, got {}",
                    index + 1,
                    other.type_name()
                ));
            }
        }))
    }
}

/// Rust prints floats in the shortest form that round-trips, which PostgreSQL
/// reads back exactly; only the special values are spelled differently.
fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        value.to_string()
    }
}

impl ToSql for Param {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        match &self.0 {
            None => Ok(IsNull::Yes),
            Some(text) => {
                out.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    fn to_sql_checked(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        self.to_sql(ty, out)
    }
}

/// One decoded column value.
enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    List(Vec<Cell>),
}

impl Cell {
    fn into_value(self) -> Value {
        match self {
            Cell::Null => Value::Nil,
            Cell::Bool(value) => Value::Bool(value),
            Cell::Int(value) => Value::Int(value),
            Cell::Float(value) => Value::Float(value),
            Cell::Text(value) => Value::string(value),
            Cell::List(items) => new_table(items.into_iter().map(Cell::into_value).collect()),
        }
    }
}

impl<'a> FromSql<'a> for Cell {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Cell, Box<dyn Error + Sync + Send>> {
        decode_pg(ty, raw)
    }

    fn from_sql_null(_ty: &Type) -> Result<Cell, Box<dyn Error + Sync + Send>> {
        Ok(Cell::Null)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

/// Decodes PostgreSQL's binary format for the types Neyuki has a natural
/// value for. Dates, times, uuids and numerics become strings in their usual
/// text form; anything else has to be cast to text in the query.
fn decode_pg(ty: &Type, raw: &[u8]) -> Result<Cell, Box<dyn Error + Sync + Send>> {
    match ty.kind() {
        Kind::Array(member) => {
            let array = pg::array_from_sql(raw)?;
            if array.dimensions().count()? > 1 {
                return Err(
                    "multidimensional arrays are not supported; cast the column to text".into(),
                );
            }
            let mut values = array.values();
            let mut items = Vec::new();
            while let Some(item) = values.next()? {
                items.push(match item {
                    None => Cell::Null,
                    Some(bytes) => decode_pg(member, bytes)?,
                });
            }
            return Ok(Cell::List(items));
        }
        Kind::Enum(_) => return Ok(Cell::Text(pg::text_from_sql(raw)?.to_string())),
        Kind::Domain(inner) => return decode_pg(inner, raw),
        _ => {}
    }
    Ok(match ty.name() {
        "bool" => Cell::Bool(pg::bool_from_sql(raw)?),
        "int2" => Cell::Int(i64::from(pg::int2_from_sql(raw)?)),
        "int4" => Cell::Int(i64::from(pg::int4_from_sql(raw)?)),
        "int8" => Cell::Int(pg::int8_from_sql(raw)?),
        "oid" => Cell::Int(i64::from(pg::oid_from_sql(raw)?)),
        "float4" => Cell::Float(f64::from(pg::float4_from_sql(raw)?)),
        "float8" => Cell::Float(pg::float8_from_sql(raw)?),
        "numeric" => Cell::Text(numeric_from_sql(raw)?),
        "text" | "varchar" | "bpchar" | "name" | "unknown" | "xml" | "json" | "citext" => {
            Cell::Text(pg::text_from_sql(raw)?.to_string())
        }
        "char" => Cell::Text(bytes_to_string(raw)),
        // jsonb is its text prefixed with a format version byte.
        "jsonb" => match raw.split_first() {
            Some((1, text)) => Cell::Text(pg::text_from_sql(text)?.to_string()),
            _ => return Err("unsupported jsonb encoding version".into()),
        },
        "bytea" => Cell::Text(bytes_to_string(pg::bytea_from_sql(raw))),
        "uuid" => Cell::Text(format_uuid(pg::uuid_from_sql(raw)?)),
        "date" => Cell::Text(format_pg_date(pg::date_from_sql(raw)?)),
        "time" => Cell::Text(format_time_of_day(pg::time_from_sql(raw)?)),
        "timestamp" => Cell::Text(format_pg_timestamp(pg::timestamp_from_sql(raw)?, false)),
        "timestamptz" => Cell::Text(format_pg_timestamp(pg::timestamp_from_sql(raw)?, true)),
        other => {
            return Err(format!(
                "column type `{}` is not supported; cast it to text in the query",
                other
            )
            .into());
        }
    })
}

/// Renders PostgreSQL's binary `numeric` as decimal text, keeping every
/// digit (a float would round money and ids past 2^53).
fn numeric_from_sql(mut buf: &[u8]) -> Result<String, Box<dyn Error + Sync + Send>> {
    fn read_i16(buf: &mut &[u8]) -> Result<i16, Box<dyn Error + Sync + Send>> {
        let bytes: [u8; 2] = buf.get(..2).ok_or("invalid numeric")?.try_into()?;
        *buf = &buf[2..];
        Ok(i16::from_be_bytes(bytes))
    }
    let ndigits = read_i16(&mut buf)? as u16;
    let weight = i32::from(read_i16(&mut buf)?);
    let sign = read_i16(&mut buf)? as u16;
    let dscale = read_i16(&mut buf)? as u16 as usize;
    let mut digits = Vec::with_capacity(usize::from(ndigits));
    for _ in 0..ndigits {
        digits.push(read_i16(&mut buf)?);
    }
    let negative = match sign {
        0x0000 => false,
        0x4000 => true,
        0xC000 => return Ok("NaN".to_string()),
        0xD000 => return Ok("Infinity".to_string()),
        0xF000 => return Ok("-Infinity".to_string()),
        _ => return Err("invalid numeric sign".into()),
    };
    // Digits are base 10000; `weight` is the power of 10000 of the first.
    let digit = |index: i32| -> i16 {
        if index < 0 {
            0
        } else {
            digits.get(index as usize).copied().unwrap_or(0)
        }
    };
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        for index in 0..=weight {
            if index == 0 {
                write!(out, "{}", digit(index)).expect("writing to a String cannot fail");
            } else {
                write!(out, "{:04}", digit(index)).expect("writing to a String cannot fail");
            }
        }
    }
    if dscale > 0 {
        let mut fraction = String::new();
        let mut index = weight + 1;
        while fraction.len() < dscale {
            write!(fraction, "{:04}", digit(index)).expect("writing to a String cannot fail");
            index += 1;
        }
        fraction.truncate(dscale);
        out.push('.');
        out.push_str(&fraction);
    }
    Ok(out)
}

fn format_uuid(bytes: [u8; 16]) -> String {
    let hex: Vec<String> = bytes.iter().map(|byte| format!("{:02x}", byte)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

/// Proleptic Gregorian date for a day count from 1970-01-01
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn format_date(days_since_unix: i64) -> String {
    let (year, month, day) = civil_from_days(days_since_unix);
    if year > 0 {
        format!("{:04}-{:02}-{:02}", year, month, day)
    } else {
        format!("{:04}-{:02}-{:02} BC", 1 - year, month, day)
    }
}

/// `HH:MM:SS` with the microseconds appended only when they are not zero.
fn format_time_of_day(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    let fraction = micros.rem_euclid(1_000_000);
    let mut out = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    );
    if fraction != 0 {
        write!(out, ".{:06}", fraction).expect("writing to a String cannot fail");
        while out.ends_with('0') {
            out.pop();
        }
    }
    out
}

/// PostgreSQL counts days from 2000-01-01, which is day 10957 of the Unix
/// epoch.
const PG_EPOCH_DAYS: i64 = 10_957;

fn format_pg_date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".to_string(),
        i32::MIN => "-infinity".to_string(),
        _ => format_date(i64::from(days) + PG_EPOCH_DAYS),
    }
}

/// Timestamps are microseconds from 2000-01-01; `timestamptz` values are in
/// UTC and say so.
fn format_pg_timestamp(micros: i64, with_zone: bool) -> String {
    match micros {
        i64::MAX => return "infinity".to_string(),
        i64::MIN => return "-infinity".to_string(),
        _ => {}
    }
    let days = micros.div_euclid(86_400_000_000);
    let rest = micros.rem_euclid(86_400_000_000);
    let mut out = format!(
        "{} {}",
        format_date(days + PG_EPOCH_DAYS),
        format_time_of_day(rest)
    );
    if with_zone {
        out.push_str("+00");
    }
    out
}

fn pg_params(params: &[Value]) -> Result<Vec<Param>, String> {
    params
        .iter()
        .enumerate()
        .map(|(index, value)| Param::from_value(value, index))
        .collect()
}

fn pg_query(client: &mut postgres::Client, sql: &str, params: &[Value]) -> Result<Value, String> {
    let statement = client.prepare(sql).map_err(pg_error)?;
    if statement.params().len() != params.len() {
        return Err(format!(
            "statement takes {} parameter(s) but {} were given",
            statement.params().len(),
            params.len()
        ));
    }
    let bound = pg_params(params)?;
    let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|param| param as _).collect();
    let rows = client.query(&statement, &refs).map_err(pg_error)?;
    let columns: Vec<String> = statement
        .columns()
        .iter()
        .map(|column| column.name().to_string())
        .collect();
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut values = Vec::with_capacity(columns.len());
        for (index, column) in row.columns().iter().enumerate() {
            let cell: Cell = row
                .try_get(index)
                .map_err(|err| format!("column `{}`: {}", column.name(), pg_error(err)))?;
            values.push(cell.into_value());
        }
        out.push(row_table(&columns, values));
    }
    Ok(result_table(columns, out))
}

fn pg_execute(
    client: &mut postgres::Client,
    sql: &str,
    params: &[Value],
) -> Result<(u64, Option<Value>), String> {
    if params.is_empty() {
        let messages = client.simple_query(sql).map_err(pg_error)?;
        let affected = messages
            .iter()
            .map(|message| match message {
                postgres::SimpleQueryMessage::CommandComplete(count) => *count,
                _ => 0,
            })
            .sum();
        return Ok((affected, None));
    }
    let statement = client.prepare(sql).map_err(pg_error)?;
    if statement.params().len() != params.len() {
        return Err(format!(
            "statement takes {} parameter(s) but {} were given",
            statement.params().len(),
            params.len()
        ));
    }
    let bound = pg_params(params)?;
    let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|param| param as _).collect();
    let affected = client.execute(&statement, &refs).map_err(pg_error)?;
    Ok((affected, None))
}

// --- MySQL ------------------------------------------------------------------

/// Server errors keep their message and SQLSTATE; the crate wraps its own
/// kinds as `DriverError { .. }`, so those are unwrapped to their message.
fn my_error(err: mysql::Error) -> String {
    match err {
        mysql::Error::MySqlError(err) => format!("{} (SQLSTATE {})", err.message, err.state),
        mysql::Error::DriverError(err) => err.to_string(),
        mysql::Error::IoError(err) => err.to_string(),
        mysql::Error::UrlError(err) => err.to_string(),
        mysql::Error::TlsError(err) => err.to_string(),
        other => other.to_string(),
    }
}

fn my_params(params: &[Value]) -> Result<mysql::Params, String> {
    if params.is_empty() {
        return Ok(mysql::Params::Empty);
    }
    let values = params
        .iter()
        .enumerate()
        .map(|(index, value)| {
            Ok(match value {
                Value::Nil => mysql::Value::NULL,
                Value::Bool(value) => mysql::Value::Int(i64::from(*value)),
                Value::Int(value) => mysql::Value::Int(*value),
                // Past i64 the server parses the digits itself (DECIMAL).
                Value::BigInt(big) => mysql::Value::Bytes(big.to_string().into_bytes()),
                Value::Float(value) => mysql::Value::Double(*value),
                Value::String(value) => mysql::Value::Bytes(value.as_bytes().to_vec()),
                other => {
                    return Err(format!(
                        "parameter {} must be nil, a boolean, a number or a string, got {}",
                        index + 1,
                        other.type_name()
                    ));
                }
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(mysql::Params::Positional(values))
}

/// Converts a value from MySQL's binary protocol. Strings, blobs, decimals,
/// json and bit fields all arrive as bytes and become strings; dates and
/// times are rendered the way the server prints them.
fn my_value(value: &mysql::Value, column: &mysql::Column) -> Value {
    use mysql::consts::ColumnType;
    match value {
        mysql::Value::NULL => Value::Nil,
        mysql::Value::Bytes(bytes) => Value::string(bytes_to_string(bytes)),
        mysql::Value::Int(value) => Value::Int(*value),
        mysql::Value::UInt(value) => Value::from_u64(*value),
        mysql::Value::Float(value) => Value::Float(f64::from(*value)),
        mysql::Value::Double(value) => Value::Float(*value),
        mysql::Value::Date(year, month, day, hour, minute, second, micros) => {
            let mut out = format!("{:04}-{:02}-{:02}", year, month, day);
            if column.column_type() != ColumnType::MYSQL_TYPE_DATE {
                write!(out, " {:02}:{:02}:{:02}", hour, minute, second)
                    .expect("writing to a String cannot fail");
                if *micros != 0 {
                    write!(out, ".{:06}", micros).expect("writing to a String cannot fail");
                    while out.ends_with('0') {
                        out.pop();
                    }
                }
            }
            Value::string(out)
        }
        mysql::Value::Time(negative, days, hours, minutes, seconds, micros) => {
            let mut out = format!(
                "{}{:02}:{:02}:{:02}",
                if *negative { "-" } else { "" },
                u64::from(*days) * 24 + u64::from(*hours),
                minutes,
                seconds
            );
            if *micros != 0 {
                write!(out, ".{:06}", micros).expect("writing to a String cannot fail");
                while out.ends_with('0') {
                    out.pop();
                }
            }
            Value::string(out)
        }
    }
}

fn my_query(conn: &mut mysql::Conn, sql: &str, params: &[Value]) -> Result<Value, String> {
    let params = my_params(params)?;
    // Always the binary protocol: the text one returns every value as bytes.
    let mut result = conn.exec_iter(sql, params).map_err(my_error)?;
    let columns: Vec<String> = result
        .columns()
        .as_ref()
        .iter()
        .map(|column| column.name_str().into_owned())
        .collect();
    let mut out = Vec::new();
    for row in result.by_ref() {
        let row = row.map_err(my_error)?;
        let types = row.columns_ref();
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            let value = row
                .as_ref(index)
                .ok_or_else(|| format!("column `{}` is missing from the row", columns[index]))?;
            values.push(my_value(value, &types[index]));
        }
        out.push(row_table(&columns, values));
    }
    // Only the first result set is returned; dropping `result` drains the rest.
    Ok(result_table(columns, out))
}

fn my_execute(
    conn: &mut mysql::Conn,
    sql: &str,
    params: &[Value],
) -> Result<(u64, Option<Value>), String> {
    if params.is_empty() {
        let result = conn.query_iter(sql).map_err(my_error)?;
        Ok((
            result.affected_rows(),
            result.last_insert_id().filter(|id| *id != 0).map(integer),
        ))
    } else {
        let result = conn.exec_iter(sql, my_params(params)?).map_err(my_error)?;
        Ok((
            result.affected_rows(),
            result.last_insert_id().filter(|id| *id != 0).map(integer),
        ))
    }
}

// --- SQLite -----------------------------------------------------------------

/// Errors from SQLite itself keep their message and extended result code
/// (2067 is `SQLITE_CONSTRAINT_UNIQUE`); the others are rusqlite's own.
fn lite_error(err: rusqlite::Error) -> String {
    match err {
        rusqlite::Error::SqliteFailure(code, message) => format!(
            "{} (SQLite error {})",
            message.unwrap_or_else(|| code.to_string()),
            code.extended_code
        ),
        other => other.to_string(),
    }
}

/// SQLite has no boolean type, so booleans bind as 1 and 0, the way its own
/// `TRUE` and `FALSE` read. Integers past 64 bits bind as their digits, which
/// a numeric column stores as a float.
fn lite_param(value: &Value, index: usize) -> Result<rusqlite::types::Value, String> {
    use rusqlite::types::Value as Lite;
    Ok(match value {
        Value::Nil => Lite::Null,
        Value::Bool(value) => Lite::Integer(i64::from(*value)),
        Value::Int(value) => Lite::Integer(*value),
        Value::BigInt(big) => Lite::Text(big.to_string()),
        Value::Float(value) => Lite::Real(*value),
        Value::String(value) => Lite::Text(value.to_string()),
        other => {
            return Err(format!(
                "parameter {} must be nil, a boolean, a number or a string, got {}",
                index + 1,
                other.type_name()
            ));
        }
    })
}

/// Binds `params` to `?` (or `?NNN`) in order. SQLite would leave a missing
/// parameter NULL, so the counts have to match.
fn lite_bind(statement: &mut rusqlite::Statement<'_>, params: &[Value]) -> Result<(), String> {
    if statement.parameter_count() != params.len() {
        return Err(format!(
            "statement takes {} parameter(s) but {} were given",
            statement.parameter_count(),
            params.len()
        ));
    }
    for (index, value) in params.iter().enumerate() {
        statement
            .raw_bind_parameter(index + 1, lite_param(value, index)?)
            .map_err(lite_error)?;
    }
    Ok(())
}

/// Each value keeps its storage class; text and blobs become strings.
fn lite_value(value: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => Value::Nil,
        ValueRef::Integer(value) => Value::Int(value),
        ValueRef::Real(value) => Value::Float(value),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => Value::string(bytes_to_string(bytes)),
    }
}

fn lite_query(conn: &rusqlite::Connection, sql: &str, params: &[Value]) -> Result<Value, String> {
    let mut statement = conn.prepare(sql).map_err(lite_error)?;
    lite_bind(&mut statement, params)?;
    let columns: Vec<String> = statement.column_names().map(str::to_string).collect();
    let mut rows = statement.raw_query();
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(lite_error)? {
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            values.push(lite_value(row.get_ref(index).map_err(lite_error)?));
        }
        out.push(row_table(&columns, values));
    }
    Ok(result_table(columns, out))
}

/// Steps `statement` to the end, discarding any rows (a `PRAGMA` may return
/// some), and reports what it changed. `changes()` keeps the count of the
/// last statement that wrote anything, so it only counts when the total
/// moved; likewise the rowid is only reported when this statement set it.
fn lite_run(
    conn: &rusqlite::Connection,
    statement: &mut rusqlite::Statement<'_>,
) -> Result<(u64, Option<i64>), String> {
    let total_before = conn.total_changes();
    let rowid_before = conn.last_insert_rowid();
    let mut rows = statement.raw_query();
    while rows.next().map_err(lite_error)?.is_some() {}
    drop(rows);
    if conn.total_changes() == total_before {
        return Ok((0, None));
    }
    let rowid = conn.last_insert_rowid();
    Ok((conn.changes(), (rowid != rowid_before).then_some(rowid)))
}

fn lite_execute(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[Value],
) -> Result<(u64, Option<Value>), String> {
    if !params.is_empty() {
        let mut statement = conn.prepare(sql).map_err(lite_error)?;
        lite_bind(&mut statement, params)?;
        let (affected, rowid) = lite_run(conn, &mut statement)?;
        return Ok((affected, rowid.map(Value::Int)));
    }
    // Like PostgreSQL's simple protocol, a script runs statement by statement;
    // the counts add up and the last rowid set wins.
    let mut batch = rusqlite::Batch::new(conn, sql);
    let mut affected = 0;
    let mut last_id = None;
    // rusqlite's `FallibleIterator` is a newer major than the one imported
    // above for the PostgreSQL arrays, so it is named in full.
    while let Some(mut statement) =
        rusqlite::fallible_iterator::FallibleIterator::next(&mut batch).map_err(lite_error)?
    {
        lite_bind(&mut statement, &[])?;
        let (count, rowid) = lite_run(conn, &mut statement)?;
        affected += count;
        last_id = rowid.or(last_id);
    }
    Ok((affected, last_id.map(Value::Int)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_renders_every_digit() {
        // 1234567.89 = digits [123, 4567, 8900], weight 1, dscale 2
        let raw = [0, 3, 0, 1, 0, 0, 0, 2, 0, 123, 0x11, 0xD7, 0x22, 0xC4];
        assert_eq!(numeric_from_sql(&raw).unwrap(), "1234567.89");
        // -0.0005 = digits [5], weight -1, sign negative, dscale 4
        let raw = [0, 1, 0xFF, 0xFF, 0x40, 0, 0, 4, 0, 5];
        assert_eq!(numeric_from_sql(&raw).unwrap(), "-0.0005");
        // 10000 = digits [1], weight 1, dscale 0
        let raw = [0, 1, 0, 1, 0, 0, 0, 0, 0, 1];
        assert_eq!(numeric_from_sql(&raw).unwrap(), "10000");
        // 0 = no digits, weight 0, dscale 0
        let raw = [0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(numeric_from_sql(&raw).unwrap(), "0");
        // 0.5 with dscale 1 = digits [5000], weight -1
        let raw = [0, 1, 0xFF, 0xFF, 0, 0, 0, 1, 0x13, 0x88];
        assert_eq!(numeric_from_sql(&raw).unwrap(), "0.5");
        let raw = [0, 0, 0, 0, 0xC0, 0, 0, 0];
        assert_eq!(numeric_from_sql(&raw).unwrap(), "NaN");
    }

    #[test]
    fn dates_and_times_format_like_the_server() {
        assert_eq!(format_pg_date(0), "2000-01-01");
        assert_eq!(format_pg_date(-10_957), "1970-01-01");
        assert_eq!(format_pg_date(8_766), "2024-01-01");
        assert_eq!(format_pg_date(8_825), "2024-02-29");
        assert_eq!(format_time_of_day(0), "00:00:00");
        assert_eq!(format_time_of_day(45_296_500_000), "12:34:56.5");
        assert_eq!(format_time_of_day(1), "00:00:00.000001");
        assert_eq!(format_pg_timestamp(0, false), "2000-01-01 00:00:00");
        assert_eq!(
            format_pg_timestamp(-1, true),
            "1999-12-31 23:59:59.999999+00"
        );
        assert_eq!(format_pg_timestamp(i64::MAX, false), "infinity");
        assert_eq!(
            format_uuid([
                0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38,
                0x0a, 0x11
            ]),
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"
        );
    }

    #[test]
    fn params_take_text_form() {
        assert_eq!(float_text(1.5), "1.5");
        assert_eq!(float_text(1e300), 1e300.to_string());
        assert_eq!(float_text(f64::INFINITY), "Infinity");
        assert_eq!(float_text(f64::NAN), "NaN");
        let param = Param::from_value(&Value::Bool(true), 0).unwrap();
        assert_eq!(param.0.as_deref(), Some("t"));
        let param = Param::from_value(&Value::Nil, 0).unwrap();
        assert!(param.0.is_none());
        assert!(
            Param::from_value(&new_table(Vec::new()), 2)
                .unwrap_err()
                .contains("parameter 3")
        );
    }

    #[test]
    fn driver_comes_from_the_url_scheme() {
        let options = |driver: Option<&str>, url: Option<&str>| OpenOptions {
            driver: driver.map(str::to_string),
            url: url.map(str::to_string),
            host: None,
            port: None,
            user: None,
            password: None,
            database: None,
            ssl: None,
            timeout: None,
        };
        assert_eq!(
            driver_name(&options(None, Some("postgresql://a@b/c"))).unwrap(),
            "postgres"
        );
        assert_eq!(
            driver_name(&options(None, Some("mysql://a@b/c"))).unwrap(),
            "mysql"
        );
        assert_eq!(
            driver_name(&options(Some("MariaDB"), None)).unwrap(),
            "mysql"
        );
        assert_eq!(
            driver_name(&options(None, Some("sqlite://x"))).unwrap(),
            "sqlite"
        );
        assert_eq!(
            driver_name(&options(None, Some("SQLite::memory:"))).unwrap(),
            "sqlite"
        );
        assert_eq!(
            driver_name(&options(Some("sqlite3"), None)).unwrap(),
            "sqlite"
        );
        assert!(
            driver_name(&options(None, Some("oracle://x")))
                .unwrap_err()
                .contains("oracle")
        );
        assert!(driver_name(&options(None, None)).is_err());
        assert!(driver_name(&options(None, Some("nope"))).is_err());
    }

    #[test]
    fn sqlite_urls_name_a_file() {
        assert_eq!(sqlite_url_path("sqlite:///tmp/app.db"), Some("/tmp/app.db"));
        assert_eq!(sqlite_url_path("sqlite://app.db"), Some("app.db"));
        assert_eq!(sqlite_url_path("sqlite:app.db"), Some("app.db"));
        assert_eq!(sqlite_url_path("sqlite::memory:"), Some(":memory:"));
        assert_eq!(sqlite_url_path("sqlite://:memory:"), Some(":memory:"));
        assert_eq!(
            sqlite_url_path("sqlite3://C:/data/app.db"),
            Some("C:/data/app.db")
        );
        assert_eq!(sqlite_url_path("postgres://a@b/c"), None);
        assert_eq!(sqlite_url_path("app.db"), None);
    }

    #[test]
    fn sqlite_round_trips_values() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let (affected, _) = lite_execute(
            &conn,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL, flag INTEGER, note TEXT); \
             INSERT INTO t (name) VALUES ('first');",
            &[],
        )
        .unwrap();
        assert_eq!(affected, 1);
        let (affected, rowid) = lite_execute(
            &conn,
            "INSERT INTO t (name, score, flag, note) VALUES (?, ?, ?, ?)",
            &[
                Value::str("ada"),
                Value::Float(1.5),
                Value::Bool(true),
                Value::Nil,
            ],
        )
        .unwrap();
        assert_eq!(affected, 1);
        assert!(matches!(rowid, Some(Value::Int(2))));
        // A statement that writes nothing reports no rowid, even after an insert.
        let (affected, rowid) = lite_execute(
            &conn,
            "UPDATE t SET flag = 0 WHERE id = ?",
            &[Value::Int(99)],
        )
        .unwrap();
        assert_eq!(affected, 0);
        assert!(rowid.is_none());
        let (affected, rowid) = lite_execute(&conn, "UPDATE t SET flag = 0", &[]).unwrap();
        assert_eq!(affected, 2);
        assert!(rowid.is_none());
        assert!(
            lite_execute(&conn, "INSERT INTO t (name) VALUES (?)", &[])
                .unwrap_err()
                .contains("takes 1 parameter")
        );
        assert!(
            lite_query(&conn, "SELECT * FROM missing", &[])
                .unwrap_err()
                .contains("SQLite error")
        );
        let rows = lite_query(
            &conn,
            "SELECT id, name, score, note FROM t WHERE id = ?",
            &[Value::Int(2)],
        )
        .unwrap();
        let Value::Table(rows) = rows else {
            panic!("query should return a table")
        };
        let rows = rows.borrow();
        assert_eq!(rows.array.len(), 1);
        let Value::Table(row) = &rows.array[0] else {
            panic!("rows should be tables")
        };
        let row = row.borrow();
        assert!(matches!(row.fields.get("id"), Some(Value::Int(2))));
        assert!(matches!(row.fields.get("score"), Some(Value::Float(score)) if *score == 1.5));
        assert!(
            matches!(row.fields.get("name"), Some(Value::String(name)) if name.to_string() == "ada")
        );
        assert!(!row.fields.contains_key("note"));
    }

    #[test]
    fn only_loopback_hosts_count_as_local() {
        for host in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.8.9.10",
            "::1",
            "[::1]",
        ] {
            assert!(is_local_host(host), "{} should be local", host);
        }
        for host in [
            "db.example.com",
            "10.0.0.5",
            "192.168.1.2",
            "::2",
            "localhost.evil.com",
        ] {
            assert!(!is_local_host(host), "{} should be remote", host);
        }
    }

    #[test]
    fn sqlite_enforces_read_write_permissions_and_allows_memory() {
        use crate::vm::permissions::{PermissionScope, Permissions};

        let dir = std::env::temp_dir().join(format!("neyuki_sql_perm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("app.db");
        let db_url = format!("sqlite://{}", db_path.display());

        let make_opts = |url: &str| {
            let mut tbl = crate::vm::value::VmTable::new();
            tbl.set_str("url", Value::str(url));
            Value::Table(std::rc::Rc::new(std::cell::RefCell::new(tbl)))
        };

        // 1. `:memory:` succeeds even with all permissions denied.
        let mut vm_mem = VM::new();
        vm_mem.set_permissions(Permissions::deny_all());
        let opened = builtin_open(&mut vm_mem, &[make_opts("sqlite::memory:")]).unwrap();
        let conn_id = opened[0].clone();
        builtin_close(&mut vm_mem, &[conn_id]).unwrap();

        // 2. File-backed sqlite:// URL must NOT check_net; instead requires read+write.
        let mut vm_deny = VM::new();
        vm_deny.set_permissions(Permissions::deny_all());
        let err = builtin_open(&mut vm_deny, &[make_opts(&db_url)]).unwrap_err();
        assert!(
            err.contains("read access"),
            "expected read check, got: {err}"
        );

        let mut vm_ro = VM::new();
        vm_ro.set_permissions(
            Permissions::deny_all().with_read(PermissionScope::AllowList(vec![
                dir.to_string_lossy().into_owned(),
            ])),
        );
        let err = builtin_open(&mut vm_ro, &[make_opts(&db_url)]).unwrap_err();
        assert!(
            err.contains("write access"),
            "expected write check, got: {err}"
        );

        let mut vm_rw = VM::new();
        let dir_str = dir.to_string_lossy().into_owned();
        vm_rw.set_permissions(
            Permissions::deny_all()
                .with_read(PermissionScope::AllowList(vec![dir_str.clone()]))
                .with_write(PermissionScope::AllowList(vec![dir_str])),
        );
        let opened = builtin_open(&mut vm_rw, &[make_opts(&db_url)]).unwrap();
        let conn_id = opened[0].clone();
        builtin_close(&mut vm_rw, &[conn_id]).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sandboxed_vm(dir: &std::path::Path) -> VM {
        use crate::vm::permissions::{PermissionScope, Permissions};
        let d = dir.to_string_lossy().into_owned();
        let mut vm = VM::new();
        vm.set_permissions(
            Permissions::deny_all()
                .with_read(PermissionScope::AllowList(vec![d.clone()]))
                .with_write(PermissionScope::AllowList(vec![d])),
        );
        vm
    }

    fn url_opts(url: &str) -> Value {
        let mut tbl = crate::vm::value::VmTable::new();
        tbl.set_str("url", Value::str(url));
        Value::Table(std::rc::Rc::new(std::cell::RefCell::new(tbl)))
    }

    #[test]
    fn sqlite_fs_path_decodes_like_sqlite() {
        assert_eq!(sqlite_fs_path(":memory:").unwrap(), None);
        assert_eq!(sqlite_fs_path("file::memory:?cache=shared").unwrap(), None);
        assert_eq!(
            sqlite_fs_path("file:%2Fetc%2Fpasswd").unwrap().as_deref(),
            Some("/etc/passwd")
        );
        assert_eq!(
            sqlite_fs_path("file:///abs/x.db?mode=ro")
                .unwrap()
                .as_deref(),
            Some("/abs/x.db")
        );
        assert_eq!(
            sqlite_fs_path("file://localhost/abs/x.db#f")
                .unwrap()
                .as_deref(),
            Some("/abs/x.db")
        );
        assert_eq!(
            sqlite_fs_path("file:a%2e%2fb").unwrap().as_deref(),
            Some("a./b")
        );
        assert!(sqlite_fs_path("file://evil.host/x.db").is_err());
        assert!(sqlite_fs_path("file:a%00b").is_err());
    }

    #[test]
    fn sqlite_percent_encoded_uri_cannot_escape_allow_list() {
        let dir = std::env::temp_dir().join(format!("neyuki_sql_pct_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut vm = sandboxed_vm(&dir);
        let outside =
            std::env::temp_dir().join(format!("neyuki_sql_out_{}.db", std::process::id()));
        let enc = outside.to_string_lossy().replace('/', "%2F");
        for url in [format!("sqlite:file:{enc}"), format!("sqlite://file:{enc}")] {
            let err = builtin_open(&mut vm, &[url_opts(&url)]).unwrap_err();
            assert!(err.contains("permission denied"), "{url}: {err}");
        }
        assert!(!outside.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_attach_and_vacuum_into_blocked_when_restricted() {
        let dir = std::env::temp_dir().join(format!("neyuki_sql_att_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside =
            std::env::temp_dir().join(format!("neyuki_sql_att_out_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        let mut vm = sandboxed_vm(&dir);
        let url = format!("sqlite://{}", dir.join("a.db").display());
        let id = builtin_open(&mut vm, &[url_opts(&url)]).unwrap()[0].clone();
        let exec = |vm: &mut VM, sql: &str| builtin_execute(vm, &[id.clone(), Value::str(sql)]);
        let q = outside.display();
        assert!(exec(&mut vm, &format!("ATTACH DATABASE '{q}' AS x")).is_err());
        assert!(
            exec(
                &mut vm,
                &format!(
                    "ATTACH DATABASE 'file:{}' AS x",
                    q.to_string().replace('/', "%2F")
                )
            )
            .is_err()
        );
        assert!(exec(&mut vm, "CREATE TABLE t(a)").is_ok());
        assert!(exec(&mut vm, &format!("VACUUM INTO '{q}'")).is_err());
        assert!(
            !outside.exists(),
            "VACUUM INTO created a file outside the sandbox"
        );
        // Allowed: in-memory and in-sandbox attachments.
        assert!(exec(&mut vm, "ATTACH DATABASE ':memory:' AS m").is_ok());
        let inside = dir.join("b.db");
        assert!(
            exec(
                &mut vm,
                &format!("ATTACH DATABASE '{}' AS b", inside.display())
            )
            .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_attach_unrestricted_when_allow_all() {
        let outside =
            std::env::temp_dir().join(format!("neyuki_sql_all_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        let mut vm = VM::new();
        let id = builtin_open(&mut vm, &[url_opts("sqlite::memory:")]).unwrap()[0].clone();
        builtin_execute(
            &mut vm,
            &[
                id.clone(),
                Value::str(&format!("VACUUM INTO '{}'", outside.display())),
            ],
        )
        .unwrap();
        assert!(outside.exists());
        let _ = std::fs::remove_file(&outside);
    }
}
