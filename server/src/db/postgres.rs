//! Postgres backend: per-worker pools, TLS with libpq's `sslmode`, query
//! cancellation, streaming cursors.
//!
//! A connection whose call timed out, or whose result was abandoned midway,
//! may still be busy on the server: its query is cancelled and the
//! connection is discarded rather than returned to the pool.

use super::{MAX_ROWS, TX_ABORTED, chain};
use crate::bindings::sql::{Error, QueryResult, Value};
use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod, Runtime};
use futures_util::StreamExt;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::error::Error as StdError;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, timeout_at};
use tokio_postgres::config::SslMode;
use tokio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use tokio_postgres::{Row, RowStream, Statement};
use tokio_postgres_rustls::MakeRustlsConnect;
use wasmtime::{Result, format_err};

/// Prepared statements cached per connection are keyed by SQL text, which
/// guests control; cap the cache so unique statements cannot grow it forever.
const MAX_CACHED_STATEMENTS: usize = 512;

/// Trust anchors for database TLS: Mozilla's root CAs plus, optionally, a
/// private CA. How (and whether) each connection uses them follows its URL's
/// `sslmode`, with libpq's meaning (see `SslPolicy`).
#[derive(Clone)]
pub struct DbTls {
    verifier: Arc<WebPkiServerVerifier>,
    provider: Arc<CryptoProvider>,
}

pub fn tls(ca_file: Option<&Path>) -> Result<DbTls> {
    let mut roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    if let Some(path) = ca_file {
        let certs = CertificateDer::pem_file_iter(path)
            .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
            .map_err(|e| format_err!("reading CA file {}: {e}", path.display()))?;
        if certs.is_empty() {
            wasmtime::bail!("CA file {} contains no certificates", path.display());
        }
        for cert in certs {
            roots.add(cert).map_err(|e| format_err!("CA file {}: {e}", path.display()))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| format_err!("TLS setup: {e}"))?;
    Ok(DbTls { verifier, provider })
}

impl DbTls {
    fn connector(&self, policy: SslPolicy) -> Result<MakeRustlsConnect> {
        let verifier = Arc::new(Verifier { policy, inner: self.verifier.clone(), provider: self.provider.clone() });
        let config = rustls::ClientConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| format_err!("TLS setup: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        Ok(MakeRustlsConnect::new(config))
    }
}

/// libpq's `sslmode` values. tokio-postgres only knows disable, prefer and
/// require, so the URL's mode is parsed here and certificate checks are done
/// by `Verifier`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SslPolicy {
    /// Plaintext.
    Disable,
    /// TLS if the server offers it; certificate not checked. (libpq's
    /// default; `allow` is treated the same.)
    Prefer,
    /// TLS, certificate not checked.
    Require,
    /// TLS, certificate chain checked against the trusted CAs.
    VerifyCa,
    /// TLS, chain and host name checked.
    VerifyFull,
}

impl SslPolicy {
    fn name(self) -> &'static str {
        match self {
            SslPolicy::Disable => "disable",
            SslPolicy::Prefer => "prefer",
            SslPolicy::Require => "require",
            SslPolicy::VerifyCa => "verify-ca",
            SslPolicy::VerifyFull => "verify-full",
        }
    }

    fn warning(self) -> Option<&'static str> {
        match self {
            SslPolicy::Disable => Some("plaintext connection"),
            SslPolicy::Prefer => Some("may fall back to plaintext and doesn't check the server certificate"),
            SslPolicy::Require => Some("doesn't check the server certificate"),
            SslPolicy::VerifyCa => Some("doesn't check the host name in the certificate"),
            SslPolicy::VerifyFull => None,
        }
    }
}

/// Splits `sslmode` off a connection string (URL or key=value form) and
/// parses the rest.
fn parse_url(url: &str) -> Result<(tokio_postgres::Config, SslPolicy)> {
    let mut mode = None;
    let rest = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let (base, query) = url.split_once('?').unwrap_or((url, ""));
        let kept: Vec<&str> = query
            .split('&')
            .filter(|kv| match kv.strip_prefix("sslmode=") {
                Some(value) => {
                    mode = Some(value.to_string());
                    false
                }
                None => !kv.is_empty(),
            })
            .collect();
        if kept.is_empty() { base.to_string() } else { format!("{base}?{}", kept.join("&")) }
    } else {
        url.split_whitespace()
            .filter(|kv| match kv.strip_prefix("sslmode=") {
                Some(value) => {
                    mode = Some(value.to_string());
                    false
                }
                None => true,
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let policy = match mode.as_deref().unwrap_or("prefer") {
        "disable" => SslPolicy::Disable,
        "allow" | "prefer" => SslPolicy::Prefer,
        "require" => SslPolicy::Require,
        "verify-ca" => SslPolicy::VerifyCa,
        "verify-full" => SslPolicy::VerifyFull,
        other => wasmtime::bail!("unknown sslmode {other:?}"),
    };
    let mut config: tokio_postgres::Config = rest.parse().map_err(|e| format_err!("{e}"))?;
    config.ssl_mode(match policy {
        SslPolicy::Disable => SslMode::Disable,
        SslPolicy::Prefer => SslMode::Prefer,
        _ => SslMode::Require,
    });
    Ok((config, policy))
}

/// Certificate checks per `SslPolicy`. Handshake signatures are always
/// verified, so even unchecked certificates can't be swapped mid-session.
#[derive(Debug)]
struct Verifier {
    policy: SslPolicy,
    inner: Arc<WebPkiServerVerifier>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let checked = || self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now);
        match self.policy {
            SslPolicy::Disable | SslPolicy::Prefer | SslPolicy::Require => Ok(ServerCertVerified::assertion()),
            SslPolicy::VerifyFull => checked(),
            SslPolicy::VerifyCa => match checked() {
                Err(rustls::Error::InvalidCertificate(
                    CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
                )) => Ok(ServerCertVerified::assertion()),
                other => other,
            },
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// One worker's pool for one database.
pub struct DbPool {
    pool: Pool,
    pub policy: SslPolicy,
    /// Also needed to cancel queries, which opens a separate connection.
    tls: MakeRustlsConnect,
}

impl DbPool {
    /// Creates the pool without connecting; connections open lazily on the
    /// calling worker's runtime.
    pub fn new(name: &str, url: &str, size: usize, tls: &DbTls) -> Result<Self> {
        let (config, policy) = parse_url(url).map_err(|e| format_err!("database {name:?}: {e}"))?;
        let connector = tls.connector(policy)?;
        let manager = Manager::from_config(config, connector.clone(), ManagerConfig { recycling_method: RecyclingMethod::Fast });
        let pool = Pool::builder(manager)
            .max_size(size)
            .runtime(Runtime::Tokio1)
            .wait_timeout(Some(Duration::from_secs(5)))
            .create_timeout(Some(Duration::from_secs(5)))
            .build()
            .map_err(|e| format_err!("database {name:?}: {e}"))?;
        Ok(Self { pool, policy, tls: connector })
    }

    /// Connects once; describes the connection, e.g. `TLS, sslmode=verify-full`.
    pub async fn check(&self) -> Result<String> {
        let client = self.pool.get().await.map_err(|e| format_err!("{}", chain(&e)))?;
        let ssl = client
            .query_one("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()", &[])
            .await
            .map_err(|e| format_err!("{}", chain(&e)))?;
        let mode = if ssl.get::<_, bool>(0) { "TLS" } else { "plaintext" };
        Ok(format!("postgres, {mode}, sslmode={}", self.policy.name()))
    }

    pub async fn init(&self, sql: &str) -> Result<()> {
        let client = self.pool.get().await.map_err(|e| format_err!("{}", chain(&e)))?;
        client.batch_execute(sql).await.map_err(|e| format_err!("{}", chain(&e)))
    }

    pub fn security_warning(&self) -> Option<String> {
        self.policy.warning().map(|w| format!("sslmode={} {w}; use sslmode=verify-full", self.policy.name()))
    }

    async fn connect(&self, deadline: Instant) -> Result<Object, Error> {
        let client = timeout_at(deadline, self.pool.get())
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|e| Error::Unavailable(chain(&e)))?;
        if client.statement_cache.size() > MAX_CACHED_STATEMENTS {
            client.statement_cache.clear();
        }
        Ok(client)
    }

    /// Runs `op` on a pooled connection within the deadline. On timeout the
    /// connection is cancelled and discarded.
    async fn on_connection<T, F>(&self, deadline: Instant, op: F) -> Result<T, Error>
    where
        F: for<'c> FnOnce(&'c Object) -> Pin<Box<dyn Future<Output = Result<T, Abandon>> + Send + 'c>>,
    {
        let client = self.connect(deadline).await?;
        match timeout_at(deadline, op(&client)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(Abandon::Keep(e))) => Err(e),
            Ok(Err(Abandon::Discard(e))) => {
                discard(client, self.tls.clone());
                Err(e)
            }
            Err(_) => {
                discard(client, self.tls.clone());
                Err(Error::Timeout)
            }
        }
    }

    pub async fn query(&self, deadline: Instant, statement: String, params: Vec<Value>) -> Result<QueryResult, Error> {
        self.on_connection(deadline, move |c| Box::pin(async move { run_query(c, &statement, &params).await })).await
    }

    pub async fn execute(&self, deadline: Instant, statement: String, params: Vec<Value>) -> Result<u64, Error> {
        self.on_connection(deadline, move |c| Box::pin(async move { run_execute(c, &statement, &params).await })).await
    }

    pub async fn begin(&self, deadline: Instant) -> Result<Tx, Error> {
        let client = self.connect(deadline).await?;
        match timeout_at(deadline, client.batch_execute("BEGIN")).await {
            Ok(Ok(())) => Ok(Tx { client: Some(client), tls: self.tls.clone() }),
            Ok(Err(e)) => Err(query_error(e)),
            Err(_) => {
                discard(client, self.tls.clone());
                Err(Error::Timeout)
            }
        }
    }

    pub async fn open_cursor(&self, deadline: Instant, statement: String, params: Vec<Value>) -> Result<Cursor, Error> {
        let client = self.connect(deadline).await?;
        match timeout_at(deadline, start_stream(&client, &statement, &params)).await {
            Ok(Ok((columns, stream))) => {
                Ok(Cursor { client: Some(client), stream: Some(Box::pin(stream)), columns, tls: self.tls.clone() })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                discard(client, self.tls.clone());
                Err(Error::Timeout)
            }
        }
    }
}

/// Cancels whatever `client` is running and closes it instead of returning
/// it to the pool.
fn discard(client: Object, tls: MakeRustlsConnect) {
    let token = client.cancel_token();
    drop(Object::take(client));
    tokio::spawn(async move {
        let _ = token.cancel_query(tls).await;
    });
}

/// An error from an operation, and whether its connection is still in a
/// known state.
enum Abandon {
    /// The connection is idle and can go back to the pool.
    Keep(Error),
    /// The connection may still be sending the rest of a result.
    Discard(Error),
}

impl From<Error> for Abandon {
    fn from(e: Error) -> Self {
        Abandon::Keep(e)
    }
}

/// A connection with an open `BEGIN`. If it is dropped without `commit`, the
/// connection is rolled back before it goes back to the pool.
pub struct Tx {
    /// `None` once a call timed out: the connection was discarded and the
    /// transaction can only be dropped.
    client: Option<Object>,
    tls: MakeRustlsConnect,
}

impl Drop for Tx {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            // Dropped with the store (the request ended or trapped): roll back
            // in the background, then let the connection return to the pool.
            tokio::spawn(rollback(client));
        }
    }
}

async fn rollback(client: Object) {
    if client.batch_execute("ROLLBACK").await.is_err() {
        // State unknown: never hand this connection to another request.
        drop(Object::take(client));
    }
}

impl Tx {
    /// Runs `op` within the deadline. On timeout the connection is discarded
    /// and the transaction aborted.
    async fn run<T, F>(&mut self, deadline: Instant, op: F) -> Result<T, Error>
    where
        F: for<'c> FnOnce(&'c Object) -> Pin<Box<dyn Future<Output = Result<T, Abandon>> + Send + 'c>>,
    {
        let Some(client) = self.client.as_ref() else {
            return Err(Error::Query(TX_ABORTED.into()));
        };
        match timeout_at(deadline, op(client)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(Abandon::Keep(e))) => Err(e),
            Ok(Err(Abandon::Discard(e))) => {
                discard(self.client.take().unwrap(), self.tls.clone());
                Err(e)
            }
            Err(_) => {
                discard(self.client.take().unwrap(), self.tls.clone());
                Err(Error::Timeout)
            }
        }
    }

    pub async fn query(&mut self, deadline: Instant, statement: String, params: Vec<Value>) -> Result<QueryResult, Error> {
        self.run(deadline, move |c| Box::pin(async move { run_query(c, &statement, &params).await })).await
    }

    pub async fn execute(&mut self, deadline: Instant, statement: String, params: Vec<Value>) -> Result<u64, Error> {
        self.run(deadline, move |c| Box::pin(async move { run_execute(c, &statement, &params).await })).await
    }

    pub async fn commit(mut self, deadline: Instant) -> Result<(), Error> {
        let Some(client) = self.client.take() else {
            return Err(Error::Query(TX_ABORTED.into()));
        };
        match timeout_at(deadline, client.batch_execute("COMMIT")).await {
            Ok(Ok(())) => Ok(()),
            // A failed COMMIT ends the transaction but may leave the
            // connection in an unknown state.
            Ok(Err(e)) => {
                drop(Object::take(client));
                Err(query_error(e))
            }
            Err(_) => {
                discard(client, self.tls.clone());
                Err(Error::Timeout)
            }
        }
    }

    /// Rolls back now (the guest dropped the handle without committing).
    pub async fn rollback(mut self) {
        if let Some(client) = self.client.take() {
            rollback(client).await;
        }
    }
}

/// A connection streaming one result.
pub struct Cursor {
    client: Option<Object>,
    stream: Option<Pin<Box<RowStream>>>,
    pub columns: Vec<String>,
    tls: MakeRustlsConnect,
}

impl Drop for Cursor {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            if self.stream.is_some() {
                // Abandoned midway: the server may still be sending rows.
                discard(client, self.tls.clone());
            }
        }
    }
}

impl Cursor {
    pub async fn next(&mut self, deadline: Instant, max_rows: usize) -> Result<Vec<Vec<Value>>, Error> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::with_capacity(max_rows.min(1024));
        let fetched = timeout_at(deadline, async {
            while rows.len() < max_rows {
                match stream.next().await {
                    Some(row) => rows.push(convert_row(&row.map_err(query_error)?)?),
                    None => return Ok(true),
                }
            }
            Ok(false)
        })
        .await;
        match fetched {
            Ok(Ok(exhausted)) => {
                if exhausted {
                    // Done: the connection is idle again and can go back to
                    // the pool right away instead of when the guest drops it.
                    self.stream = None;
                    self.client = None;
                }
                Ok(rows)
            }
            Ok(Err(e)) => {
                self.close();
                Err(e)
            }
            Err(_) => {
                self.close();
                Err(Error::Timeout)
            }
        }
    }

    /// Ends the cursor after an error or timeout; later calls return no rows.
    fn close(&mut self) {
        self.stream = None;
        if let Some(client) = self.client.take() {
            discard(client, self.tls.clone());
        }
    }
}

async fn prepare(client: &Object, statement: &str) -> Result<Statement, Error> {
    client.prepare_cached(statement).await.map_err(query_error)
}

async fn start_stream(client: &Object, statement: &str, params: &[Value]) -> Result<(Vec<String>, RowStream), Error> {
    let stmt = prepare(client, statement).await?;
    let columns = stmt.columns().iter().map(|c| c.name().to_string()).collect();
    let params: Vec<Param> = params.iter().map(Param).collect();
    let stream = client.query_raw(&stmt, params.iter().map(|p| p as &(dyn ToSql + Sync))).await.map_err(query_error)?;
    Ok((columns, stream))
}

async fn run_query(client: &Object, statement: &str, params: &[Value]) -> Result<QueryResult, Abandon> {
    let (columns, stream) = start_stream(client, statement, params).await?;
    let mut stream = std::pin::pin!(stream);
    let mut rows = Vec::new();
    while let Some(row) = stream.next().await {
        if rows.len() == MAX_ROWS {
            return Err(Abandon::Discard(Error::TooManyRows(MAX_ROWS as u32)));
        }
        let row = row.map_err(query_error)?;
        // The rest of the result is still in flight.
        rows.push(convert_row(&row).map_err(Abandon::Discard)?);
    }
    Ok(QueryResult { columns, rows })
}

async fn run_execute(client: &Object, statement: &str, params: &[Value]) -> Result<u64, Abandon> {
    let stmt = prepare(client, statement).await?;
    let params: Vec<Param> = params.iter().map(Param).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    Ok(client.execute(&stmt, &refs).await.map_err(query_error)?)
}

fn query_error(e: tokio_postgres::Error) -> Error {
    // Parameter conversion failures surface as errors with a source of ours.
    match e.as_db_error() {
        Some(db) => Error::Query(format!("{}: {}", db.code().code(), db.message())),
        None => match e.source() {
            Some(source) => Error::Conversion(source.to_string()),
            None => Error::Query(e.to_string()),
        },
    }
}

/// Binds a guest `value` to whatever type Postgres inferred for the parameter.
#[derive(Debug)]
struct Param<'a>(&'a Value);

impl ToSql for Param<'_> {
    fn to_sql(&self, ty: &Type, out: &mut bytes::BytesMut) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        let mismatch = |what: &str| format!("cannot bind {what} to a parameter of type {ty}; cast it in SQL").into();
        match self.0 {
            Value::Null => Ok(IsNull::Yes),
            Value::Int(i) => match *ty {
                Type::INT2 => i16::try_from(*i)?.to_sql(ty, out),
                Type::INT4 => i32::try_from(*i)?.to_sql(ty, out),
                Type::INT8 => i.to_sql(ty, out),
                Type::FLOAT4 => (*i as f32).to_sql(ty, out),
                Type::FLOAT8 => (*i as f64).to_sql(ty, out),
                _ => Err(mismatch("int")),
            },
            Value::Float(f) => match *ty {
                Type::FLOAT4 => (*f as f32).to_sql(ty, out),
                Type::FLOAT8 => f.to_sql(ty, out),
                _ => Err(mismatch("float")),
            },
            Value::Text(s) if <&str as ToSql>::accepts(ty) => s.as_str().to_sql(ty, out),
            Value::Text(_) => Err(mismatch("text")),
            Value::Boolean(b) if *ty == Type::BOOL => b.to_sql(ty, out),
            Value::Boolean(_) => Err(mismatch("boolean")),
            Value::Blob(b) if *ty == Type::BYTEA => b.as_slice().to_sql(ty, out),
            Value::Blob(_) => Err(mismatch("blob")),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

fn convert_row(row: &Row) -> Result<Vec<Value>, Error> {
    row.columns()
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let ty = column.type_();
            let get = |r: Result<Option<Value>, tokio_postgres::Error>| {
                r.map(|v| v.unwrap_or(Value::Null)).map_err(|e| Error::Conversion(e.to_string()))
            };
            match *ty {
                Type::BOOL => get(row.try_get::<_, Option<bool>>(i).map(|v| v.map(Value::Boolean))),
                Type::INT2 => get(row.try_get::<_, Option<i16>>(i).map(|v| v.map(|n| Value::Int(n.into())))),
                Type::INT4 => get(row.try_get::<_, Option<i32>>(i).map(|v| v.map(|n| Value::Int(n.into())))),
                Type::INT8 => get(row.try_get::<_, Option<i64>>(i).map(|v| v.map(Value::Int))),
                Type::OID => get(row.try_get::<_, Option<u32>>(i).map(|v| v.map(|n| Value::Int(n.into())))),
                Type::FLOAT4 => get(row.try_get::<_, Option<f32>>(i).map(|v| v.map(|n| Value::Float(n.into())))),
                Type::FLOAT8 => get(row.try_get::<_, Option<f64>>(i).map(|v| v.map(Value::Float))),
                Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => {
                    get(row.try_get::<_, Option<String>>(i).map(|v| v.map(Value::Text)))
                }
                Type::BYTEA => get(row.try_get::<_, Option<Vec<u8>>>(i).map(|v| v.map(Value::Blob))),
                _ => Err(Error::Conversion(format!(
                    "column {:?} has type {ty}, which has no value mapping; select it as {}::text",
                    column.name(),
                    column.name()
                ))),
            }
        })
        .collect()
}
