//! Native database primitives backing `@neyuki/sql`.
//!
//! `__sql_open` connects to a PostgreSQL server through the `postgres` crate
//! or to a MySQL/MariaDB server through `mysql` and parks the connection here
//! under an id; `__sql_query` and `__sql_execute` run statements on it and
//! `__sql_close` drops it. Both drivers are synchronous, with
//! TLS through the platform's `native-tls` like `@neyuki/http`.
//!
//! Statements run through each server's prepared-statement protocol with
//! positional parameters (`$1` on PostgreSQL, `?` on MySQL), so values never
//! get spliced into SQL text. PostgreSQL parameters are sent in text format
//! so the server converts them to whatever type the statement expects, the
//! way `psql` does; results come back in binary and are decoded here.
//! `lib/sql.nyk` builds the `Connection` object on top.

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
            Some(url) => url
                .split_once("://")
                .map(|(scheme, _)| scheme.to_ascii_lowercase())
                .ok_or_else(|| {
                    format!("`{}` is not a database URL (expected scheme://...)", url)
                })?,
            None => return Err("driver must be provided (\"postgres\" or \"mysql\")".to_string()),
        },
    };
    match name.as_str() {
        "postgres" | "postgresql" | "pg" => Ok("postgres"),
        "mysql" | "mariadb" => Ok("mysql"),
        other => Err(format!(
            "unknown database driver `{}` (expected \"postgres\" or \"mysql\")",
            other
        )),
    }
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

/// `__sql_open(options)` connects and returns the connection id followed by
/// its driver name. `options` holds `driver` and/or `url` plus optional
/// `host`, `port`, `user`, `password`, `database`, `ssl` and `timeout`
/// fields, which override what the URL says.
fn builtin_open(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let options = options_arg(args, 0)?;
    let target = options
        .url
        .as_deref()
        .or(options.host.as_deref())
        .unwrap_or("localhost");
    vm.permissions.check_net(target)?;
    let driver = driver_name(&options)?;
    let connection = match driver {
        "postgres" => Connection::Postgres(Box::new(open_postgres(&options)?)),
        _ => Connection::MySql(open_mysql(&options)?),
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

/// `__sql_ping(connection)` returns whether the server still answers.
fn builtin_ping(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "connection")?;
    let alive = with_connection(id, |connection| {
        Ok(match connection {
            Connection::Postgres(client) => client.is_valid(Duration::from_secs(30)).is_ok(),
            Connection::MySql(conn) => conn.ping().is_ok(),
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
    })?;
    Ok(vec![result])
}

/// `__sql_execute(connection, sql, params?)` runs one statement and returns
/// the number of rows it affected followed by the last inserted id (MySQL
/// auto-increment only; nil elsewhere). Without parameters the statement
/// goes through the simple protocol, which also accepts what servers refuse
/// to prepare (`BEGIN`, DDL on some versions).
fn builtin_execute(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "connection")?;
    let sql = string_arg(args, 1, "sql")?;
    let params = params_arg(args, 2)?;
    let (affected, last_id) = with_connection(id, |connection| match connection {
        Connection::Postgres(client) => pg_execute(client, &sql, &params),
        Connection::MySql(conn) => my_execute(conn, &sql, &params),
    })?;
    Ok(vec![
        integer(affected),
        match last_id {
            Some(id) => integer(id),
            None => Value::Nil,
        },
    ])
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
) -> Result<(u64, Option<u64>), String> {
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
) -> Result<(u64, Option<u64>), String> {
    if params.is_empty() {
        let result = conn.query_iter(sql).map_err(my_error)?;
        Ok((
            result.affected_rows(),
            result.last_insert_id().filter(|id| *id != 0),
        ))
    } else {
        let result = conn.exec_iter(sql, my_params(params)?).map_err(my_error)?;
        Ok((
            result.affected_rows(),
            result.last_insert_id().filter(|id| *id != 0),
        ))
    }
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
        assert!(
            driver_name(&options(None, Some("sqlite://x")))
                .unwrap_err()
                .contains("sqlite")
        );
        assert!(driver_name(&options(None, None)).is_err());
        assert!(driver_name(&options(None, Some("nope"))).is_err());
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
}
