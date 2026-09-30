//! The pool a live source is read through, and the single row shape the readers share.
//!
//! Every source goes through `sqlx::any`, so the metadata readers have one row type and one
//! placeholder style to work with. Doris is the exception, and it is a connect-time one:
//! `sqlx::any` hands its MySQL driver a URL and nothing else, so right after the handshake the
//! driver runs its own session setup —
//!
//! ```text
//! SET sql_mode=(SELECT CONCAT(@@sql_mode, ',PIPES_AS_CONCAT,NO_ENGINE_SUBSTITUTION')),
//!     time_zone='+00:00', NAMES utf8mb4 COLLATE utf8mb4_general_ci;
//! ```
//!
//! and Doris answers `1105 (HY000): errCode = 2, detailMessage = Set statement does't support
//! non-constant expr.`, then drops the connection. None of those four parts can be turned off from
//! the URL; through the driver's own options all of them can, so Doris gets `sqlx::mysql` directly
//! and its rows are converted back to `sqlx::any`'s row type — the same conversion the Any driver
//! performs on a MySQL source, which leaves the readers with one code path and Doris with exactly
//! the columns a MySQL source shows.
//!
//! The same FE can also answer with `Failed to do prepared command, server side prepared statement
//! is disabled`, so a Doris statement is never prepared: it goes out as text with its values
//! written into it, which is what the Java version's driver did by default too.
//!
//! Reading Doris back is the third reason to leave the shared path. `sqlx::any` builds its row by
//! mapping every column's type, and that map holds neither `TINYINT` nor `DECIMAL` nor any date
//! type, so a row carrying one of those fails to convert at all — `Any driver does not support MySql
//! type MySqlTypeInfo { type: Tiny, .. }`. The typed path therefore reads each value itself, and a
//! column the driver has no Rust type for is handed over as the text the server sent for it.

use futures_util::TryStreamExt;
use futures_util::stream::Stream;
use serde_json::{Value, json};
use sqlx::any::{AnyArguments, AnyConnectOptions, AnyPoolOptions, AnyRow};
use sqlx::error::BoxDynError;
use sqlx::mysql::{
    MySql, MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlRow, MySqlValueRef,
};
use sqlx::{Arguments as _, Column as _, ConnectOptions, Connection, Decode, Row as _};

use super::dialect;
use super::{ConnectError, query_error};
use crate::db::{AnyPool, map_placeholders};
use crate::model::datasource::ConnectionSpec;

/// How long a first connection may take before the caller hears that the database is silent.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Pooled connections to one database, in the shape its driver needs.
#[derive(Clone)]
pub(crate) enum Live {
    Any(AnyPool),
    MySql(MySqlPool),
}

impl Live {
    /// Opens `url`, which must already be the driver URL [`super::dialect::connection_url`] built.
    ///
    /// One connection is opened by hand first: `Pool::connect` reports a refused or unreachable
    /// address as a bare "pool timed out", and the driver's own words are the only diagnosis the
    /// console can show.
    pub(crate) async fn open(
        spec: &ConnectionSpec,
        url: &str,
    ) -> std::result::Result<Self, ConnectError> {
        let (driver, opened) = if dialect::typed_mysql(spec.db_type) {
            ("mysql", Self::open_mysql(url).await)
        } else {
            ("any", Self::open_any(url).await)
        };
        let pool = opened?;
        // Which driver dialled a source is the first thing to check when a database complains about
        // the session setup, and a pool is opened once per credential set.
        tracing::info!(
            db_type = spec.db_type.as_str(),
            driver,
            "data source pooled"
        );
        Ok(pool)
    }

    async fn open_any(url: &str) -> std::result::Result<Self, ConnectError> {
        sqlx::any::install_default_drivers();
        let options: AnyConnectOptions = match url.parse() {
            Ok(options) => options,
            Err(err) => return Err(ConnectError::Opening(err.to_string())),
        };
        hand_open(&options).await?;
        let created = AnyPoolOptions::new()
            .max_connections(4)
            .min_connections(0)
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect_with(options)
            .await
            .map_err(|err| ConnectError::Opening(err.to_string()))?;
        Ok(Self::Any(created))
    }

    async fn open_mysql(url: &str) -> std::result::Result<Self, ConnectError> {
        let options: MySqlConnectOptions = match url.parse() {
            Ok(options) => options,
            Err(err) => return Err(ConnectError::Opening(err.to_string())),
        };
        // With all four off the driver sends no statement at all; the character set the handshake
        // negotiated still applies, and timestamps are read as the server stores them.
        let options = options
            .pipes_as_concat(false)
            .no_engine_substitution(false)
            .timezone(None::<String>)
            .set_names(false);
        hand_open(&options).await?;
        let created = MySqlPoolOptions::new()
            .max_connections(4)
            .min_connections(0)
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect_with(options)
            .await
            .map_err(|err| ConnectError::Opening(err.to_string()))?;
        Ok(Self::MySql(created))
    }

    /// The rows of a statement, no more than `limit` of them: the fetch stops there the way
    /// `Statement.setMaxRows` stopped it.
    pub(crate) async fn select(
        &self,
        sql: &str,
        binds: &[Value],
        limit: usize,
    ) -> std::result::Result<Vec<LiveRow>, ConnectError> {
        match self {
            Self::Any(pool) => {
                let mut connection = pool.acquire().await.map_err(query_error)?;
                collect(
                    sqlx::query_with(sql, arguments(binds)?).fetch(&mut *connection),
                    limit,
                    any_row,
                )
                .await
            }
            Self::MySql(pool) => {
                let mut connection = pool.acquire().await.map_err(query_error)?;
                let statement = inlined(sql, binds)?;
                // `raw_sql` is the only entry point that skips the prepare; `sqlx::query` prepares.
                collect(
                    sqlx::raw_sql(&statement).fetch(&mut *connection),
                    limit,
                    mysql_row,
                )
                .await
            }
        }
    }

    /// Runs a statement that answers with nothing and reports how many rows it changed.
    pub(crate) async fn write(
        &self,
        sql: &str,
        binds: &[Value],
    ) -> std::result::Result<u64, ConnectError> {
        match self {
            Self::Any(pool) => sqlx::query_with(sql, arguments(binds)?)
                .execute(pool)
                .await
                .map(|result| result.rows_affected())
                .map_err(query_error),
            Self::MySql(pool) => {
                let statement = inlined(sql, binds)?;
                sqlx::raw_sql(&statement)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
                    .map_err(query_error)
            }
        }
    }

    pub(crate) async fn close(self) {
        match self {
            Self::Any(pool) => pool.close().await,
            Self::MySql(pool) => pool.close().await,
        }
    }
}

/// One connection, then nothing: a pool would hide the reason the address refused us.
async fn hand_open<O>(options: &O) -> std::result::Result<(), ConnectError>
where
    O: ConnectOptions,
    O::Connection: Sized,
{
    match tokio::time::timeout(CONNECT_TIMEOUT, options.connect()).await {
        Ok(Ok(connection)) => {
            let _ = connection.close().await;
            Ok(())
        }
        Ok(Err(err)) => Err(ConnectError::Opening(err.to_string())),
        Err(_) => Err(ConnectError::Opening(format!(
            "the database did not answer within {} seconds",
            CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

async fn collect<S, R, F>(
    mut rows: S,
    limit: usize,
    read: F,
) -> std::result::Result<Vec<LiveRow>, ConnectError>
where
    S: Stream<Item = sqlx::Result<R>> + Unpin,
    F: Fn(&R) -> LiveRow,
{
    let mut out = Vec::new();
    while out.len() < limit {
        let Some(row) = rows.try_next().await.map_err(query_error)? else {
            break;
        };
        out.push(read(&row));
    }
    Ok(out)
}

/// One row a live source answered with, read into JSON where it came from.
///
/// The readers take this rather than `sqlx`'s own row type because a row of the shared path is
/// built by mapping every column's type, and that map has holes a real result set walks into.
#[derive(Debug)]
pub(crate) struct LiveRow {
    columns: Vec<String>,
    values: Vec<Value>,
}

impl LiveRow {
    fn new<R: sqlx::Row>(row: &R, values: Vec<Value>) -> Self {
        Self {
            columns: row
                .columns()
                .iter()
                .map(|column| column.name().to_string())
                .collect(),
            values,
        }
    }

    pub(crate) fn column_names(&self) -> &[String] {
        &self.columns
    }

    pub(crate) fn values(&self) -> &[Value] {
        &self.values
    }

    /// A value as text, which is how the catalog stores what a live source showed it: the digits of
    /// a number, the bytes of a name, and nothing for a value that is neither.
    pub(crate) fn text(&self, index: usize) -> Option<String> {
        match self.values.get(index)? {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) if number.is_i64() => Some(number.to_string()),
            _ => None,
        }
    }
}

fn any_row(row: &AnyRow) -> LiveRow {
    let values = (0..row.columns().len())
        .map(|index| any_value(row, index))
        .collect();
    LiveRow::new(row, values)
}

/// Any column of any driver as JSON. The Java reader called `ResultSet.getObject`, which left the
/// driver's own type to be serialized, so the same ladder of reads is tried here: whatever the
/// driver can hand over, in the order `sqlx::any` allows it to.
fn any_value(row: &AnyRow, index: usize) -> Value {
    macro_rules! read {
        ($ty:ty) => {
            if let Ok(value) = row.try_get::<Option<$ty>, _>(index) {
                return match value {
                    Some(value) => json!(value),
                    None => Value::Null,
                };
            }
        };
    }
    read!(bool);
    read!(i16);
    read!(i32);
    read!(i64);
    read!(f32);
    read!(f64);
    read!(String);
    if let Ok(value) = row.try_get::<Option<Vec<u8>>, _>(index) {
        return match value {
            Some(bytes) => json!(String::from_utf8_lossy(&bytes)),
            None => Value::Null,
        };
    }
    tracing::debug!(index, "column type has no read the driver allows");
    Value::Null
}

fn mysql_row(row: &MySqlRow) -> LiveRow {
    let values = (0..row.columns().len())
        .map(|index| mysql_value(row, index))
        .collect();
    LiveRow::new(row, values)
}

/// One column of the typed MySQL read. What the driver has a Rust type for keeps the JSON shape the
/// shared path gives it; the rest is the text the server sent for it, which is what a console shows.
///
/// There is no `bool` read here on purpose: the driver takes a `TINYINT` for one, and a column of
/// that type has to keep its digits, which is the very column the shared path cannot carry.
fn mysql_value(row: &MySqlRow, index: usize) -> Value {
    macro_rules! read {
        ($ty:ty) => {
            if let Ok(value) = row.try_get::<Option<$ty>, _>(index) {
                return match value {
                    Some(value) => json!(value),
                    None => Value::Null,
                };
            }
        };
    }
    read!(i64);
    read!(u64);
    read!(f64);
    read!(String);
    if let Ok(value) = row.try_get::<Option<Vec<u8>>, _>(index) {
        return match value {
            Some(bytes) => json!(String::from_utf8_lossy(&bytes)),
            None => Value::Null,
        };
    }
    if let Ok(value) = row.try_get_unchecked::<Option<AsText>, _>(index) {
        return match value {
            Some(AsText(text)) => Value::String(text),
            None => Value::Null,
        };
    }
    tracing::debug!(index, "column has no read the driver allows");
    Value::Null
}

/// A MySQL column read as the text the server sent it in, which is how every value of a
/// text-protocol result set arrives. `sqlx` gates its own reads on the column's type, so this is
/// the one read that accepts a type the driver has no Rust type for: `DECIMAL`, `DATE`, `TIMESTAMP`,
/// `BIT`.
struct AsText(String);

impl<'r> Decode<'r, MySql> for AsText {
    fn decode(value: MySqlValueRef<'r>) -> std::result::Result<Self, BoxDynError> {
        Ok(Self(<String as Decode<MySql>>::decode(value)?))
    }
}

/// The `?` values of a statement, as the Any driver takes them. A value the driver cannot send is
/// reported as the statement's own failure, which is when the JDBC driver threw it.
fn arguments(binds: &[Value]) -> std::result::Result<AnyArguments<'_>, ConnectError> {
    let mut arguments = AnyArguments::default();
    for value in binds {
        let added = match value {
            Value::Null => arguments.add(None::<String>),
            Value::Bool(inner) => arguments.add(*inner),
            Value::Number(inner) => match inner.as_i64() {
                Some(int) => arguments.add(int),
                None => arguments.add(inner.as_f64().unwrap_or_default()),
            },
            Value::String(inner) => arguments.add(inner.clone()),
            other => arguments.add(other.to_string()),
        };
        added.map_err(|err| ConnectError::Query(err.to_string()))?;
    }
    Ok(arguments)
}

/// The same statement with its values written into it, for a source that cannot prepare one: this
/// is the substitution the JDBC driver the Java version used performed before it sent anything.
fn inlined(sql: &str, binds: &[Value]) -> std::result::Result<String, ConnectError> {
    let mut placed = 0usize;
    let text = map_placeholders(sql, &mut |_| {
        let value = binds.get(placed);
        placed += 1;
        value.map_or_else(String::new, literal)
    });
    if placed != binds.len() {
        return Err(ConnectError::Query(format!(
            "the statement has {placed} placeholder(s) but {} value(s) were given",
            binds.len()
        )));
    }
    Ok(text)
}

/// One value as a MySQL literal, escaped the way the server's default `sql_mode` reads them.
fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(inner) => i64::from(*inner).to_string(),
        Value::Number(inner) => inner.to_string(),
        Value::String(inner) => quoted(inner),
        // An object or an array binds as its JSON text, the way the Any driver takes it.
        other => quoted(&other.to_string()),
    }
}

fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for ch in text.chars() {
        match ch {
            '\0' => out.push_str("\\0"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\x1a' => out.push_str("\\Z"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    use serde_json::json;

    use super::*;
    use crate::model::datasource::DbType;

    /// A captured MariaDB 10.4 initial handshake. It negotiates `mysql_native_password` and no TLS,
    /// which is the shortest way to the moment under test: what the driver puts on the wire *after*
    /// authentication.
    const GREETING: &[u8] = b"\n5.5.5-10.4.7-MariaDB-1:10.4.7+maria~bionic\x00\x0b\x00\x00\x00t6L\\j\"dS\x00\xfe\xf7\x08\x02\x00\xff\x81\x15\x00\x00\x00\x00\x00\x00\x07\x00\x00\x00U14Oph9\"<H5n\x00mysql_native_password\x00";

    /// An OK packet: nothing affected, no last id, autocommit on, no warnings.
    const OK: &[u8] = &[0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00];

    /// The end of a text result set: with `DEPRECATE_EOF` it is an OK packet behind a `0xfe` marker.
    const ROWS_DONE: &[u8] = &[0xfe, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00];

    const COM_QUIT: u8 = 0x01;
    const COM_QUERY: u8 = 0x03;
    const COM_STMT_PREPARE: u8 = 0x16;

    /// The packet a text result set opens each of its columns with.
    fn column(name: &str, type_id: u8) -> Vec<u8> {
        let mut out = Vec::new();
        for text in ["def", "", "", "", name, name] {
            out.push(text.len() as u8);
            out.extend_from_slice(text.as_bytes());
        }
        out.push(0x0c);
        out.extend_from_slice(&255u16.to_le_bytes());
        out.extend_from_slice(&255u32.to_le_bytes());
        out.push(type_id);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.push(0x1f);
        out
    }

    /// One row of the text protocol: every value a length-prefixed string, NULL its own marker.
    fn text_row(values: &[Option<&str>]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            match value {
                None => out.push(0xfb),
                Some(text) => {
                    out.push(text.len() as u8);
                    out.extend_from_slice(text.as_bytes());
                }
            }
        }
        out
    }

    /// The three column types `sqlx::any` has no mapping for: `TINYINT`, `DECIMAL`, `DATE`.
    fn probe_rows() -> Vec<Vec<u8>> {
        vec![
            vec![0x03],
            column("flag", 0x01),
            column("amount", 0x00),
            column("made_on", 0x0a),
            text_row(&[Some("7"), Some("12.3400"), Some("2024-05-06")]),
            ROWS_DONE.to_vec(),
        ]
    }

    fn spec(db_type: DbType, url: &str) -> ConnectionSpec {
        ConnectionSpec {
            db_type,
            url: url.to_string(),
            user: None,
            password: None,
        }
    }

    fn write_packet(socket: &mut TcpStream, sequence: u8, payload: &[u8]) -> std::io::Result<()> {
        let length = payload.len() as u32;
        let header = [
            length as u8,
            (length >> 8) as u8,
            (length >> 16) as u8,
            sequence,
        ];
        socket.write_all(&header)?;
        socket.write_all(payload)
    }

    fn read_packet(socket: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
        let mut header = [0u8; 4];
        socket.read_exact(&mut header).ok()?;
        let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
        let mut payload = vec![0u8; length];
        socket.read_exact(&mut payload).ok()?;
        Some((header[3], payload))
    }

    /// One connection: greeting, authentication accepted without being looked at, then every command
    /// the client sends until it hangs up. `false` means the test has gone away.
    fn serve(socket: &mut TcpStream, sent: &mpsc::Sender<(u8, String)>) -> bool {
        if write_packet(socket, 0, GREETING).is_err() {
            return true;
        }
        let Some((sequence, _response)) = read_packet(socket) else {
            return true;
        };
        if write_packet(socket, sequence.wrapping_add(1), OK).is_err() {
            return true;
        }
        while let Some((sequence, payload)) = read_packet(socket) {
            let Some((command, body)) = payload.split_first() else {
                continue;
            };
            if *command == COM_QUIT {
                break;
            }
            let statement = String::from_utf8_lossy(body).into_owned();
            let asks_for_rows = *command == COM_QUERY && statement.contains("PROBE");
            if sent.send((*command, statement)).is_err() {
                return false;
            }
            if asks_for_rows {
                let mut reply = sequence.wrapping_add(1);
                let mut broken = false;
                for packet in probe_rows() {
                    if write_packet(socket, reply, &packet).is_err() {
                        broken = true;
                        break;
                    }
                    reply = reply.wrapping_add(1);
                }
                if broken {
                    break;
                }
                continue;
            }
            if write_packet(socket, sequence.wrapping_add(1), OK).is_err() {
                break;
            }
        }
        true
    }

    /// Reports every command the connector sends, with the text that follows its command byte.
    fn fake_server() -> (u16, mpsc::Receiver<(u8, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let port = listener.local_addr().expect("the bound address").port();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { break };
                if !serve(&mut socket, &sender) {
                    break;
                }
            }
        });
        (port, receiver)
    }

    /// Everything the fake server saw. Every command is reported before the client is told it
    /// succeeded, so a test that has awaited that call needs no waiting here.
    fn drained(receiver: &mpsc::Receiver<(u8, String)>) -> Vec<(u8, String)> {
        let mut out = Vec::new();
        while let Ok(entry) = receiver.try_recv() {
            out.push(entry);
        }
        out
    }

    /// The behaviour Doris cannot live with: every source on the shared path is sent a session
    /// setup, and its `sql_mode=(SELECT CONCAT(...))` is the part Doris refuses.
    #[tokio::test]
    async fn the_shared_path_sends_a_session_setup() {
        let (port, sent) = fake_server();
        let url = format!("mysql://127.0.0.1:{port}/dw");
        let pool = Live::open(&spec(DbType::MySql, &url), &url)
            .await
            .expect("the fake server takes the connection");
        pool.close().await;

        assert!(
            drained(&sent)
                .iter()
                .any(|(command, sql)| *command == COM_QUERY
                    && sql.starts_with("SET sql_mode=(SELECT CONCAT")),
            "the shared path must send the setup Doris refuses"
        );
    }

    /// What the typed path exists for: after the handshake Doris is sent nothing at all, so there is
    /// no statement for its FE to refuse.
    #[tokio::test]
    async fn doris_is_sent_nothing_after_the_handshake() {
        let (port, sent) = fake_server();
        let url = format!("mysql://127.0.0.1:{port}/dw");
        let pool = Live::open(&spec(DbType::Doris, &url), &url)
            .await
            .expect("the fake server takes the connection");
        pool.close().await;

        assert!(
            drained(&sent).is_empty(),
            "Doris must not be sent anything at connect time"
        );
    }

    /// A Doris FE can have server-side prepares switched off, so a statement reaches it as text with
    /// its values written in — the substitution the JDBC driver used to do on the way out.
    #[tokio::test]
    async fn doris_statements_go_out_as_text_with_their_values_written_in() {
        let (port, sent) = fake_server();
        let url = format!("mysql://127.0.0.1:{port}/dw");
        let pool = Live::open(&spec(DbType::Doris, &url), &url)
            .await
            .expect("the fake server takes the connection");
        pool.select(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = ? AND table_name = ?",
            &[json!("dwi"), json!("orders")],
            5,
        )
        .await
        .expect("the fake server answers the statement");
        pool.close().await;

        let seen = drained(&sent);
        assert!(
            !seen.iter().any(|(command, _)| *command == COM_STMT_PREPARE),
            "a source that cannot prepare must never be asked to: {seen:?}"
        );
        // The pool pings a connection before it hands it out, which Doris answers like MySQL does.
        let queries: Vec<String> = seen
            .into_iter()
            .filter(|(command, _)| *command == COM_QUERY)
            .map(|(_, statement)| statement)
            .collect();
        assert_eq!(
            queries,
            vec![
                "SELECT table_name FROM information_schema.tables WHERE table_schema = 'dwi' \
                 AND table_name = 'orders'"
                    .to_string()
            ]
        );
    }

    /// The third Doris hole: a real result set carries columns `sqlx::any` has no mapping for, and
    /// through the shared path the whole row then fails to build. Read column by column the same row
    /// is fine — the integer still arrives as a number, the rest as the text the server sent.
    #[tokio::test]
    async fn a_doris_row_of_types_the_shared_path_cannot_hold_is_still_read() {
        let (port, _sent) = fake_server();
        let url = format!("mysql://127.0.0.1:{port}/dw");
        let pool = Live::open(&spec(DbType::Doris, &url), &url)
            .await
            .expect("the fake server takes the connection");
        let rows = pool
            .select("SELECT flag, amount, made_on FROM PROBE", &[], 5)
            .await
            .expect("the fake server answers with a text result set");
        pool.close().await;

        assert_eq!(rows.len(), 1, "one row was sent: {rows:?}");
        let row = &rows[0];
        assert_eq!(
            row.column_names(),
            &[
                "flag".to_string(),
                "amount".to_string(),
                "made_on".to_string()
            ]
        );
        assert_eq!(
            row.values(),
            &[json!(7), json!("12.3400"), json!("2024-05-06")]
        );
        assert_eq!(row.text(1), Some("12.3400".to_string()));
        assert_eq!(row.text(2), Some("2024-05-06".to_string()));
    }

    #[test]
    fn values_become_literals_the_server_reads_back_unchanged() {
        assert_eq!(
            inlined(
                "WHERE note = ?",
                &[json!("O'Brien\\line\nend\u{1a} \"quoted\"")]
            )
            .unwrap(),
            r#"WHERE note = 'O\'Brien\\line\nend\Z \"quoted\"'"#
        );
        assert_eq!(
            inlined(
                "?, ?, ?, ?, ?",
                &[
                    json!(true),
                    json!(false),
                    json!(12),
                    json!(1.5),
                    json!(null)
                ]
            )
            .unwrap(),
            "1, 0, 12, 1.5, NULL"
        );
        // A question mark that is part of the statement, not of its parameter list, stays put.
        assert_eq!(
            inlined("WHERE a = 'x?y' AND b = ?", &[json!(7)]).unwrap(),
            "WHERE a = 'x?y' AND b = 7"
        );
    }

    #[test]
    fn a_statement_and_its_values_have_to_agree() {
        assert!(
            inlined("SELECT ?", &[json!(1), json!(2)])
                .unwrap_err()
                .reason()
                .contains("1 placeholder(s) but 2 value(s)")
        );
        assert!(
            inlined("SELECT ?", &[])
                .unwrap_err()
                .reason()
                .contains("1 placeholder(s) but 0 value(s)")
        );
    }
}
