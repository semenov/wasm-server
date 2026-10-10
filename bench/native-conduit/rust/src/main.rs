//! The RealWorld "Conduit" API as an ordinary Rust server, for comparison
//! with the wasm app in `guests/conduit`: the same routes, SQL, JWT and
//! argon2id parameters, and an in-process cache with the same TTLs.
//!
//! Stack: axum 0.8 on tokio, tokio-postgres with deadpool (cached prepared
//! statements), rustls.
//!
//! Environment: `DATABASE_URL`, `DB_CA_FILE` (enables TLS with certificate
//! and host name checks, like `sslmode=verify-full`), `JWT_SECRET` (at least
//! 16 bytes), `LISTEN` (default 127.0.0.1:8080), `POOL_SIZE` (default 32),
//! `WORKERS` (tokio worker threads, default: all cores).

use argon2::{Algorithm, Argon2, Params, Version};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use hmac::{Hmac, KeyInit, Mac};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Row, error::SqlState};

const TOKEN_TTL_SECS: u64 = 30 * 24 * 3600;
const MAX_BODY: usize = 1 << 20;
/// OWASP's minimum for argon2id: 19 MiB, 2 passes, 1 lane.
const ARGON2_M_KIB: u32 = 19 * 1024;
const ARGON2_T: u32 = 2;
/// How stale cached counts and popular tags may get.
const COUNT_TTL: Duration = Duration::from_secs(10);
const TAGS_TTL: Duration = Duration::from_secs(30);

static POOL: OnceLock<Pool> = OnceLock::new();
static SECRET: OnceLock<Vec<u8>> = OnceLock::new();
static CACHE: OnceLock<RwLock<HashMap<String, (Instant, Json)>>> = OnceLock::new();

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn main() {
    let workers = env("WORKERS").and_then(|w| w.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().unwrap().get());
    tokio::runtime::Builder::new_multi_thread().worker_threads(workers).enable_all().build().unwrap().block_on(run());
}

async fn run() {
    let secret = env("JWT_SECRET").expect("JWT_SECRET is not set");
    assert!(secret.len() >= 16, "JWT_SECRET must be at least 16 bytes");
    SECRET.set(secret.into_bytes()).unwrap();
    CACHE.set(RwLock::new(HashMap::new())).unwrap();

    // libpq's sslmode/sslrootcert aren't understood by tokio-postgres; TLS is
    // configured from DB_CA_FILE instead.
    let url = env("DATABASE_URL").expect("DATABASE_URL is not set");
    let (base, query) = url.split_once('?').unwrap_or((&url, ""));
    let kept: Vec<&str> = query.split('&').filter(|kv| !kv.is_empty() && !kv.starts_with("sslmode=") && !kv.starts_with("sslrootcert=")).collect();
    let url = if kept.is_empty() { base.to_string() } else { format!("{base}?{}", kept.join("&")) };
    let mut config: tokio_postgres::Config = url.parse().expect("bad DATABASE_URL");
    let manager_config = ManagerConfig { recycling_method: RecyclingMethod::Fast };
    let manager = match env("DB_CA_FILE") {
        Some(ca) => {
            config.ssl_mode(tokio_postgres::config::SslMode::Require);
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_file_iter(&ca).expect("reading DB_CA_FILE") {
                roots.add(cert.unwrap()).unwrap();
            }
            let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Manager::from_config(config, tokio_postgres_rustls::MakeRustlsConnect::new(tls), manager_config)
        }
        None => Manager::from_config(config, tokio_postgres::NoTls, manager_config),
    };
    let size = env("POOL_SIZE").and_then(|s| s.parse().ok()).unwrap_or(32);
    let pool = Pool::builder(manager).max_size(size).build().unwrap();
    drop(pool.get().await.expect("connecting to the database"));
    POOL.set(pool).unwrap();

    let app = Router::new()
        .route("/hello", get(|| async { "Hello, world!\n" }))
        .route("/api/users", post(register))
        .route("/api/users/login", post(login))
        .route("/api/user", get(current_user).put(update_user))
        .route("/api/profiles/{username}", get(profile))
        .route("/api/profiles/{username}/follow", post(follow).delete(unfollow))
        .route("/api/articles", get(list_articles).post(create_article))
        .route("/api/articles/feed", get(feed))
        .route("/api/articles/{slug}", get(get_article).put(update_article).delete(delete_article))
        .route("/api/articles/{slug}/favorite", post(favorite).delete(unfavorite))
        .route("/api/articles/{slug}/comments", get(comments).post(add_comment))
        .route("/api/articles/{slug}/comments/{id}", axum::routing::delete(delete_comment))
        .route("/api/tags", get(tags));

    let listen = env("LISTEN").unwrap_or_else(|| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&listen).await.unwrap();
    eprintln!("listening on {listen}, {size} database connections");
    axum::serve(listener, app).await.unwrap();
}

// ---------------------------------------------------------------- serving

type ApiResult = Result<(StatusCode, Json), ApiError>;

struct ApiError {
    status: StatusCode,
    body: Json,
}

impl ApiError {
    /// RealWorld's error shape: `{"errors": {"body": ["..."]}}`.
    fn new(status: u16, message: &str) -> Self {
        Self::field(status, "body", message)
    }

    fn field(status: u16, field: &str, message: &str) -> Self {
        Self { status: StatusCode::from_u16(status).unwrap(), body: json!({"errors": {field: [message]}}) }
    }
}

impl From<tokio_postgres::Error> for ApiError {
    fn from(e: tokio_postgres::Error) -> Self {
        if e.code() == Some(&SqlState::UNIQUE_VIOLATION) {
            return ApiError::field(409, "username or email", "has already been taken");
        }
        ApiError::new(500, &e.to_string())
    }
}

impl From<deadpool_postgres::PoolError> for ApiError {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        ApiError::new(503, &e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        json_response(self.status, &self.body)
    }
}

fn json_response(status: StatusCode, body: &Json) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], serde_json::to_vec(body).unwrap()).into_response()
}

/// Turns a handler's result into a response.
fn respond(result: ApiResult) -> Response {
    match result {
        Ok((status, body)) => json_response(status, &body),
        Err(e) => e.into_response(),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, ApiError> {
    if body.len() > MAX_BODY {
        return Err(ApiError::new(413, "body too large"));
    }
    serde_json::from_slice(body).map_err(|e| ApiError::new(422, &format!("invalid JSON: {e}")))
}

fn validate(fields: &[(&str, &str)]) -> Result<(), ApiError> {
    match fields.iter().find(|(_, value)| value.trim().is_empty()) {
        Some((name, _)) => Err(ApiError::field(422, name, "can't be blank")),
        None => Ok(()),
    }
}

type Param<'a> = &'a (dyn ToSql + Sync);

async fn query_all(sql: &str, params: &[Param<'_>]) -> Result<Vec<Row>, ApiError> {
    let client = POOL.get().unwrap().get().await?;
    let statement = client.prepare_cached(sql).await?;
    Ok(client.query(&statement, params).await?)
}

async fn query_opt(sql: &str, params: &[Param<'_>]) -> Result<Option<Row>, ApiError> {
    Ok(query_all(sql, params).await?.into_iter().next())
}

async fn execute(sql: &str, params: &[Param<'_>]) -> Result<u64, ApiError> {
    let client = POOL.get().unwrap().get().await?;
    let statement = client.prepare_cached(sql).await?;
    Ok(client.execute(&statement, params).await?)
}

// ------------------------------------------------------------------- auth

struct Viewer {
    id: i64,
    token: String,
}

/// Reads `Authorization: Token <jwt>` (or `Bearer`). A missing header is an
/// anonymous request; a bad token is an error.
fn authenticate(headers: &HeaderMap) -> Result<Option<Viewer>, ApiError> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let value = value.to_str().unwrap_or("");
    let token = value.strip_prefix("Token ").or_else(|| value.strip_prefix("Bearer ")).unwrap_or("");
    match verify_token(token) {
        Some(id) => Ok(Some(Viewer { id, token: token.to_string() })),
        None => Err(ApiError::new(401, "invalid or expired token")),
    }
}

fn require(headers: &HeaderMap) -> Result<Viewer, ApiError> {
    authenticate(headers)?.ok_or_else(|| ApiError::new(401, "authentication required"))
}

fn viewer_id(viewer: &Option<Viewer>) -> Option<i64> {
    viewer.as_ref().map(|v| v.id)
}

fn hmac(data: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.get().unwrap()).expect("HMAC accepts any key length");
    mac.update(data);
    mac
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// HS256 JWT with `sub` (user id) and `exp`.
fn sign(user_id: i64) -> String {
    let header = B64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = B64.encode(json!({"sub": user_id, "exp": now() + TOKEN_TTL_SECS}).to_string());
    let signing_input = format!("{header}.{claims}");
    let signature = B64.encode(hmac(signing_input.as_bytes()).finalize().into_bytes());
    format!("{signing_input}.{signature}")
}

fn verify_token(token: &str) -> Option<i64> {
    let (signing_input, signature) = token.rsplit_once('.')?;
    let signature = B64.decode(signature).ok()?;
    hmac(signing_input.as_bytes()).verify_slice(&signature).ok()?;
    let (_, claims) = signing_input.split_once('.')?;
    let claims: Json = serde_json::from_slice(&B64.decode(claims).ok()?).ok()?;
    if claims["exp"].as_u64()? < now() {
        return None;
    }
    claims["sub"].as_i64()
}

/// `argon2id$<m>$<t>$<salt>$<hash>`, all base64url. Runs on the blocking
/// pool: about 25 ms of CPU would stall an async worker.
async fn hash_password(password: String) -> String {
    tokio::task::spawn_blocking(move || {
        let salt = random_bytes::<16>();
        let hash = argon2_hash(&password, &salt, ARGON2_M_KIB, ARGON2_T).expect("valid argon2 parameters");
        format!("argon2id${ARGON2_M_KIB}${ARGON2_T}${}${}", B64.encode(salt), B64.encode(hash))
    })
    .await
    .unwrap()
}

async fn verify_password(password: String, stored: String) -> bool {
    tokio::task::spawn_blocking(move || {
        let parts: Vec<&str> = stored.split('$').collect();
        let ["argon2id", m, t, salt, hash] = parts.as_slice() else {
            return false;
        };
        let (Ok(m), Ok(t), Ok(salt), Ok(expected)) = (m.parse(), t.parse(), B64.decode(salt), B64.decode(hash)) else {
            return false;
        };
        match argon2_hash(&password, &salt, m, t) {
            // Constant-time comparison.
            Some(actual) => actual.len() == expected.len() && actual.iter().zip(&expected).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0,
            None => false,
        }
    })
    .await
    .unwrap()
}

fn argon2_hash(password: &str, salt: &[u8], m_kib: u32, t: u32) -> Option<[u8; 32]> {
    let params = Params::new(m_kib, t, 1, Some(32)).ok()?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(password.as_bytes(), salt, &mut out).ok()?;
    Some(out)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).unwrap();
    bytes
}

// ------------------------------------------------------------------ users

#[derive(Deserialize)]
struct UserBody<T> {
    user: T,
}

#[derive(Deserialize)]
struct NewUser {
    username: String,
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct Login {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct UserUpdate {
    email: Option<String>,
    username: Option<String>,
    password: Option<String>,
    bio: Option<String>,
    image: Option<String>,
}

const USER_COLUMNS: &str = "id, email, username, bio, image";

fn user_json(row: &Row, token: &str) -> Json {
    json!({"user": {
        "email": row.get::<_, &str>(1),
        "token": token,
        "username": row.get::<_, &str>(2),
        "bio": row.get::<_, &str>(3),
        "image": row.get::<_, Option<&str>>(4),
    }})
}

async fn register(body: Bytes) -> Response {
    respond(
        async {
            let UserBody { user } = parse::<UserBody<NewUser>>(&body)?;
            validate(&[("username", &user.username), ("email", &user.email), ("password", &user.password)])?;
            let hash = hash_password(user.password).await;
            let row = query_opt(
                &format!("INSERT INTO users (username, email, password_hash) VALUES ($1, $2, $3) RETURNING {USER_COLUMNS}"),
                &[&user.username, &user.email, &hash],
            )
            .await?
            .ok_or_else(|| ApiError::new(500, "insert returned nothing"))?;
            Ok((StatusCode::CREATED, user_json(&row, &sign(row.get(0)))))
        }
        .await,
    )
}

async fn login(body: Bytes) -> Response {
    respond(
        async {
            let UserBody { user } = parse::<UserBody<Login>>(&body)?;
            let row = query_opt(&format!("SELECT {USER_COLUMNS}, password_hash FROM users WHERE email = $1"), &[&user.email]).await?;
            match row {
                Some(row) if verify_password(user.password, row.get(5)).await => Ok((StatusCode::OK, user_json(&row, &sign(row.get(0))))),
                _ => Err(ApiError::field(401, "email or password", "is invalid")),
            }
        }
        .await,
    )
}

async fn current_user(headers: HeaderMap) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            let row = query_opt(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1"), &[&viewer.id])
                .await?
                .ok_or_else(|| ApiError::new(404, "not found"))?;
            Ok((StatusCode::OK, user_json(&row, &viewer.token)))
        }
        .await,
    )
}

async fn update_user(headers: HeaderMap, body: Bytes) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            let UserBody { user } = parse::<UserBody<UserUpdate>>(&body)?;
            let hash = match user.password {
                Some(p) => Some(hash_password(p).await),
                None => None,
            };
            let row = query_opt(
                &format!(
                    "UPDATE users SET email = COALESCE($2, email), username = COALESCE($3, username),
                        password_hash = COALESCE($4, password_hash), bio = COALESCE($5, bio), image = COALESCE($6, image)
                     WHERE id = $1 RETURNING {USER_COLUMNS}"
                ),
                &[&viewer.id, &user.email, &user.username, &hash, &user.bio, &user.image],
            )
            .await?
            .ok_or_else(|| ApiError::new(404, "not found"))?;
            Ok((StatusCode::OK, user_json(&row, &viewer.token)))
        }
        .await,
    )
}

// --------------------------------------------------------------- profiles

async fn fetch_profile(viewer: Option<i64>, username: &str) -> ApiResult {
    let row = query_opt(
        "SELECT u.username, u.bio, u.image,
                EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = $1 AND f.followee_id = u.id)
         FROM users u WHERE u.username = $2",
        &[&viewer, &username],
    )
    .await?;
    match row {
        Some(row) => Ok((
            StatusCode::OK,
            json!({"profile": {
                "username": row.get::<_, &str>(0),
                "bio": row.get::<_, &str>(1),
                "image": row.get::<_, Option<&str>>(2),
                "following": row.get::<_, bool>(3),
            }}),
        )),
        None => Err(ApiError::new(404, "profile not found")),
    }
}

async fn profile(headers: HeaderMap, Path(username): Path<String>) -> Response {
    respond(async { fetch_profile(viewer_id(&authenticate(&headers)?), &username).await }.await)
}

async fn follow_or_unfollow(headers: HeaderMap, username: String, add: bool) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            let statement = if add {
                "INSERT INTO follows (follower_id, followee_id)
                 SELECT $1, id FROM users WHERE username = $2 AND id <> $1 ON CONFLICT DO NOTHING"
            } else {
                "DELETE FROM follows f USING users u
                 WHERE f.follower_id = $1 AND f.followee_id = u.id AND u.username = $2"
            };
            execute(statement, &[&viewer.id, &username]).await?;
            fetch_profile(Some(viewer.id), &username).await
        }
        .await,
    )
}

async fn follow(headers: HeaderMap, Path(username): Path<String>) -> Response {
    follow_or_unfollow(headers, username, true).await
}

async fn unfollow(headers: HeaderMap, Path(username): Path<String>) -> Response {
    follow_or_unfollow(headers, username, false).await
}

// --------------------------------------------------------------- articles

#[derive(Deserialize)]
struct ArticleBody<T> {
    article: T,
}

#[derive(Deserialize)]
struct NewArticle {
    title: String,
    description: String,
    body: String,
    #[serde(rename = "tagList", default)]
    tag_list: Vec<String>,
}

#[derive(Deserialize)]
struct ArticleUpdate {
    title: Option<String>,
    description: Option<String>,
    body: Option<String>,
}

/// Columns for `article_json`. `$1` is the viewer's id (NULL when anonymous).
fn article_columns(with_body: bool) -> String {
    format!(
        "a.slug, a.title, a.description, {body},
         to_char(a.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),
         to_char(a.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),
         a.favorites_count, u.username, u.bio, u.image,
         COALESCE((SELECT json_agg(t.tag ORDER BY t.tag) FROM article_tags t WHERE t.article_id = a.id)::text, '[]'),
         EXISTS (SELECT 1 FROM favorites f WHERE f.article_id = a.id AND f.user_id = $1),
         EXISTS (SELECT 1 FROM follows fo WHERE fo.followee_id = a.author_id AND fo.follower_id = $1)",
        body = if with_body { "a.body" } else { "''" },
    )
}

fn article_json(row: &Row, with_body: bool) -> Json {
    let mut article = json!({
        "slug": row.get::<_, &str>(0),
        "title": row.get::<_, &str>(1),
        "description": row.get::<_, &str>(2),
        "tagList": serde_json::from_str::<Json>(row.get(10)).unwrap_or_else(|_| json!([])),
        "createdAt": row.get::<_, &str>(4),
        "updatedAt": row.get::<_, &str>(5),
        "favorited": row.get::<_, bool>(11),
        "favoritesCount": row.get::<_, i64>(6),
        "author": {
            "username": row.get::<_, &str>(7),
            "bio": row.get::<_, &str>(8),
            "image": row.get::<_, Option<&str>>(9),
            "following": row.get::<_, bool>(12),
        },
    });
    if with_body {
        article["body"] = json!(row.get::<_, &str>(3));
    }
    article
}

fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn page(query: &str) -> (i64, i64) {
    let get = |key, default: i64| param(query, key).and_then(|v| v.parse().ok()).unwrap_or(default);
    (get("limit", 20).clamp(1, 100), get("offset", 0).max(0))
}

/// WHERE clause with only the filters actually given (a generic
/// `$n IS NULL OR ...` clause would get a poor plan from Postgres), and the
/// values for its parameters, numbered from `$first`.
fn list_filter(query: &str, first: usize) -> (String, Vec<String>, bool) {
    let mut conditions = Vec::new();
    let mut params = Vec::new();
    let mut needs_users = false;
    let next = |value: String, params: &mut Vec<String>| {
        params.push(value);
        first + params.len() - 1
    };
    if let Some(tag) = param(query, "tag") {
        let n = next(tag, &mut params);
        conditions.push(format!("a.id IN (SELECT article_id FROM article_tags WHERE tag = ${n})"));
    }
    if let Some(author) = param(query, "author") {
        let n = next(author, &mut params);
        conditions.push(format!("u.username = ${n}"));
        needs_users = true;
    }
    if let Some(favorited) = param(query, "favorited") {
        let n = next(favorited, &mut params);
        conditions.push(format!(
            "a.id IN (SELECT f.article_id FROM favorites f JOIN users fu ON fu.id = f.user_id WHERE fu.username = ${n})"
        ));
    }
    let clause = if conditions.is_empty() { String::new() } else { format!("WHERE {}", conditions.join(" AND ")) };
    (clause, params, needs_users)
}

fn cache_get(key: &str) -> Option<Json> {
    let cache = CACHE.get().unwrap().read().unwrap();
    cache.get(key).filter(|(expires, _)| *expires > Instant::now()).map(|(_, v)| v.clone())
}

fn cache_set(key: String, value: Json, ttl: Duration) {
    CACHE.get().unwrap().write().unwrap().insert(key, (Instant::now() + ttl, value));
}

/// Total counts are the most expensive part of a list (a scan of every
/// matching row), so they come from the cache when a recent one exists.
async fn cached_count(key: String, sql: &str, params: &[Param<'_>]) -> Result<i64, ApiError> {
    if let Some(n) = cache_get(&key).and_then(|v| v.as_i64()) {
        return Ok(n);
    }
    let n: i64 = query_opt(sql, params).await?.map_or(0, |row| row.get(0));
    cache_set(key, json!(n), COUNT_TTL);
    Ok(n)
}

fn articles_json(rows: &[Row], count: i64) -> Json {
    json!({
        "articles": rows.iter().map(|row| article_json(row, false)).collect::<Vec<_>>(),
        "articlesCount": count,
    })
}

async fn list_articles(headers: HeaderMap, RawQuery(query): RawQuery) -> Response {
    respond(
        async {
            let viewer = viewer_id(&authenticate(&headers)?);
            let query = query.unwrap_or_default();
            let (limit, offset) = page(&query);
            let (filter, filter_params, _) = list_filter(&query, 2);
            let mut params: Vec<Param> = vec![&viewer];
            params.extend(filter_params.iter().map(|p| p as Param));
            let (lim, off) = (params.len() + 1, params.len() + 2);
            params.extend([&limit as Param, &offset]);
            let rows = query_all(
                &format!(
                    "SELECT {} FROM articles a JOIN users u ON u.id = a.author_id {filter}
                     ORDER BY a.created_at DESC LIMIT ${lim} OFFSET ${off}",
                    article_columns(false)
                ),
                &params,
            )
            .await?;
            let (filter, count_params, needs_users) = list_filter(&query, 1);
            let key = format!("count:{:?}:{:?}:{:?}", param(&query, "tag"), param(&query, "author"), param(&query, "favorited"));
            let join = if needs_users { "JOIN users u ON u.id = a.author_id" } else { "" };
            let count_params: Vec<Param> = count_params.iter().map(|p| p as Param).collect();
            let count = cached_count(key, &format!("SELECT count(*) FROM articles a {join} {filter}"), &count_params).await?;
            Ok((StatusCode::OK, articles_json(&rows, count)))
        }
        .await,
    )
}

async fn feed(headers: HeaderMap, RawQuery(query): RawQuery) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            let (limit, offset) = page(&query.unwrap_or_default());
            let followed = "a.author_id IN (SELECT followee_id FROM follows WHERE follower_id = $1)";
            let rows = query_all(
                &format!(
                    "SELECT {} FROM articles a JOIN users u ON u.id = a.author_id WHERE {followed}
                     ORDER BY a.created_at DESC LIMIT $2 OFFSET $3",
                    article_columns(false)
                ),
                &[&viewer.id, &limit, &offset],
            )
            .await?;
            let count = cached_count(
                format!("feed-count:{}", viewer.id),
                &format!("SELECT count(*) FROM articles a WHERE {followed}"),
                &[&viewer.id],
            )
            .await?;
            Ok((StatusCode::OK, articles_json(&rows, count)))
        }
        .await,
    )
}

async fn fetch_article(viewer: Option<i64>, slug: &str) -> Result<Option<Json>, ApiError> {
    let row = query_opt(
        &format!("SELECT {} FROM articles a JOIN users u ON u.id = a.author_id WHERE a.slug = $2", article_columns(true)),
        &[&viewer, &slug],
    )
    .await?;
    Ok(row.map(|row| article_json(&row, true)))
}

async fn article_response(viewer: Option<i64>, slug: &str, status: StatusCode) -> ApiResult {
    match fetch_article(viewer, slug).await? {
        Some(article) => Ok((status, json!({"article": article}))),
        None => Err(ApiError::new(404, "article not found")),
    }
}

async fn get_article(headers: HeaderMap, Path(slug): Path<String>) -> Response {
    respond(async { article_response(viewer_id(&authenticate(&headers)?), &slug, StatusCode::OK).await }.await)
}

async fn create_article(headers: HeaderMap, body: Bytes) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            let ArticleBody { article } = parse::<ArticleBody<NewArticle>>(&body)?;
            validate(&[("title", &article.title), ("description", &article.description), ("body", &article.body)])?;
            let hex: String = random_bytes::<4>().iter().map(|b| format!("{b:02x}")).collect();
            let slug = format!("{}-{hex}", slugify(&article.title));
            {
                let mut client = POOL.get().unwrap().get().await?;
                let tx = client.transaction().await?;
                let insert = tx
                    .prepare_cached("INSERT INTO articles (slug, title, description, body, author_id) VALUES ($1, $2, $3, $4, $5) RETURNING id")
                    .await?;
                let id: i64 = tx.query_one(&insert, &[&slug, &article.title, &article.description, &article.body, &viewer.id]).await?.get(0);
                if !article.tag_list.is_empty() {
                    let tags = tx
                        .prepare_cached(
                            "INSERT INTO article_tags (article_id, tag)
                             SELECT $1, t FROM json_array_elements_text($2::text::json) t ON CONFLICT DO NOTHING",
                        )
                        .await?;
                    tx.execute(&tags, &[&id, &serde_json::to_string(&article.tag_list).unwrap()]).await?;
                }
                tx.commit().await?;
            }
            article_response(Some(viewer.id), &slug, StatusCode::CREATED).await
        }
        .await,
    )
}

async fn update_article(headers: HeaderMap, Path(slug): Path<String>, body: Bytes) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            let ArticleBody { article } = parse::<ArticleBody<ArticleUpdate>>(&body)?;
            let updated = execute(
                "UPDATE articles SET title = COALESCE($3, title), description = COALESCE($4, description),
                    body = COALESCE($5, body), updated_at = now()
                 WHERE slug = $1 AND author_id = $2",
                &[&slug, &viewer.id, &article.title, &article.description, &article.body],
            )
            .await?;
            if updated == 0 {
                return Err(not_found_or_forbidden(&slug).await?);
            }
            article_response(Some(viewer.id), &slug, StatusCode::OK).await
        }
        .await,
    )
}

async fn delete_article(headers: HeaderMap, Path(slug): Path<String>) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            let deleted = execute("DELETE FROM articles WHERE slug = $1 AND author_id = $2", &[&slug, &viewer.id]).await?;
            if deleted == 0 {
                return Err(not_found_or_forbidden(&slug).await?);
            }
            Ok((StatusCode::OK, json!({})))
        }
        .await,
    )
}

/// After an owner-restricted change touched no rows: 404 or 403?
async fn not_found_or_forbidden(slug: &str) -> Result<ApiError, ApiError> {
    let found = query_opt("SELECT 1 FROM articles WHERE slug = $1", &[&slug]).await?;
    Ok(if found.is_none() { ApiError::new(404, "article not found") } else { ApiError::new(403, "not the author") })
}

async fn favorite_or_not(headers: HeaderMap, slug: String, add: bool) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            {
                let mut client = POOL.get().unwrap().get().await?;
                let tx = client.transaction().await?;
                let select = tx.prepare_cached("SELECT id FROM articles WHERE slug = $1").await?;
                let Some(row) = tx.query_opt(&select, &[&slug]).await? else {
                    return Err(ApiError::new(404, "article not found"));
                };
                let id: i64 = row.get(0);
                let statement = if add {
                    "INSERT INTO favorites (user_id, article_id) VALUES ($1, $2) ON CONFLICT DO NOTHING"
                } else {
                    "DELETE FROM favorites WHERE user_id = $1 AND article_id = $2"
                };
                let statement = tx.prepare_cached(statement).await?;
                if tx.execute(&statement, &[&viewer.id, &id]).await? > 0 {
                    let delta: i64 = if add { 1 } else { -1 };
                    let update = tx.prepare_cached("UPDATE articles SET favorites_count = favorites_count + $2 WHERE id = $1").await?;
                    tx.execute(&update, &[&id, &delta]).await?;
                }
                tx.commit().await?;
            }
            article_response(Some(viewer.id), &slug, StatusCode::OK).await
        }
        .await,
    )
}

async fn favorite(headers: HeaderMap, Path(slug): Path<String>) -> Response {
    favorite_or_not(headers, slug, true).await
}

async fn unfavorite(headers: HeaderMap, Path(slug): Path<String>) -> Response {
    favorite_or_not(headers, slug, false).await
}

// --------------------------------------------------------------- comments

#[derive(Deserialize)]
struct CommentBody {
    comment: NewComment,
}

#[derive(Deserialize)]
struct NewComment {
    body: String,
}

/// `$1` is the viewer's id.
const COMMENT_COLUMNS: &str = "c.id,
    to_char(c.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),
    to_char(c.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),
    c.body, u.username, u.bio, u.image,
    EXISTS (SELECT 1 FROM follows fo WHERE fo.followee_id = u.id AND fo.follower_id = $1)";

fn comment_json(row: &Row) -> Json {
    json!({
        "id": row.get::<_, i64>(0),
        "createdAt": row.get::<_, &str>(1),
        "updatedAt": row.get::<_, &str>(2),
        "body": row.get::<_, &str>(3),
        "author": {
            "username": row.get::<_, &str>(4),
            "bio": row.get::<_, &str>(5),
            "image": row.get::<_, Option<&str>>(6),
            "following": row.get::<_, bool>(7),
        },
    })
}

async fn comments(headers: HeaderMap, Path(slug): Path<String>) -> Response {
    respond(
        async {
            let viewer = viewer_id(&authenticate(&headers)?);
            let rows = query_all(
                &format!(
                    "SELECT {COMMENT_COLUMNS} FROM comments c JOIN users u ON u.id = c.author_id
                     WHERE c.article_id = (SELECT id FROM articles WHERE slug = $2) ORDER BY c.id DESC"
                ),
                &[&viewer, &slug],
            )
            .await?;
            Ok((StatusCode::OK, json!({"comments": rows.iter().map(comment_json).collect::<Vec<_>>()})))
        }
        .await,
    )
}

async fn add_comment(headers: HeaderMap, Path(slug): Path<String>, body: Bytes) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            let CommentBody { comment } = parse(&body)?;
            validate(&[("body", &comment.body)])?;
            let row = query_opt(
                &format!(
                    "WITH c AS (
                        INSERT INTO comments (article_id, author_id, body)
                        SELECT a.id, $1, $3 FROM articles a WHERE a.slug = $2
                        RETURNING *)
                     SELECT {COMMENT_COLUMNS} FROM c JOIN users u ON u.id = c.author_id"
                ),
                &[&viewer.id, &slug, &comment.body],
            )
            .await?;
            match row {
                Some(row) => Ok((StatusCode::CREATED, json!({"comment": comment_json(&row)}))),
                None => Err(ApiError::new(404, "article not found")),
            }
        }
        .await,
    )
}

async fn delete_comment(headers: HeaderMap, Path((slug, id)): Path<(String, String)>) -> Response {
    respond(
        async {
            let viewer = require(&headers)?;
            let id: i64 = id.parse().map_err(|_| ApiError::new(404, "comment not found"))?;
            let deleted = execute(
                "DELETE FROM comments c USING articles a
                 WHERE c.id = $1 AND c.article_id = a.id AND a.slug = $2 AND c.author_id = $3",
                &[&id, &slug, &viewer.id],
            )
            .await?;
            if deleted == 0 {
                return Err(ApiError::new(404, "comment not found"));
            }
            Ok((StatusCode::OK, json!({})))
        }
        .await,
    )
}

/// Aggregates every tag row, so the result is cached for everyone.
async fn tags() -> Response {
    respond(
        async {
            if let Some(cached) = cache_get("popular-tags") {
                return Ok((StatusCode::OK, cached));
            }
            let rows = query_all("SELECT tag FROM article_tags GROUP BY tag ORDER BY count(*) DESC, tag LIMIT 20", &[]).await?;
            let body = json!({"tags": rows.iter().map(|row| row.get::<_, &str>(0)).collect::<Vec<_>>()});
            cache_set("popular-tags".into(), body.clone(), TAGS_TTL);
            Ok((StatusCode::OK, body))
        }
        .await,
    )
}

// ---------------------------------------------------------------- helpers

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = match bytes[i] {
            b'%' if i + 2 < bytes.len() => hex(bytes[i + 1]).zip(hex(bytes[i + 2])).map(|(hi, lo)| hi << 4 | lo),
            _ => None,
        };
        match decoded {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn slugify(title: &str) -> String {
    let slug: String = title.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let slug = slug.split('-').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-");
    slug.chars().take(60).collect()
}
