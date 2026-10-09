//! RealWorld "Conduit" API (https://realworld-docs.netlify.app): a Medium-like
//! blog backend, as a typical example of a JSON API over Postgres.
//!
//! Users and JWT auth (argon2id passwords), profiles and follows, articles
//! with tags, favorites and comments, a personal feed, and popular tags.
//! Configuration: `JWT_SECRET` (environment), database `conduit` (grant).

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use sha2::Sha256;
use std::io::Write as _;
use wasip2::http::types::{
    Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use wasip2::io::streams::StreamError;

wit_bindgen::generate!({
    path: "../../wit",
    world: "platform:sdk/imports",
});

use platform::sdk::cache;
use platform::sdk::sql::{self, Error as DbError, QueryResult, Transaction, Value};

const DB: &str = "conduit";
const TOKEN_TTL_SECS: u64 = 30 * 24 * 3600;
const MAX_BODY: usize = 1 << 20;
/// OWASP's minimum for argon2id: 19 MiB, 2 passes, 1 lane.
const ARGON2_M_KIB: u32 = 19 * 1024;
const ARGON2_T: u32 = 2;
/// How stale cached counts and popular tags may get.
const COUNT_TTL_MS: u64 = 10_000;
const TAGS_TTL_MS: u64 = 30_000;

// ---------------------------------------------------------------- routing

struct Conduit;

impl wasip2::exports::http::incoming_handler::Guest for Conduit {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let (status, body) = match route(&request) {
            Ok(response) => response,
            Err(e) => (e.status, e.body),
        };
        respond(out, status, &body);
    }
}

type Response = Result<(u16, Json), ApiError>;

fn route(req: &IncomingRequest) -> Response {
    let path_and_query = req.path_with_query().unwrap_or_default();
    let (path, query) = path_and_query.split_once('?').unwrap_or((&path_and_query, ""));
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let Some(("api", rest)) = segments.split_first().map(|(a, b)| (*a, b)) else {
        return Err(ApiError::new(404, "not found"));
    };
    let viewer = authenticate(req)?;
    let method = req.method();

    match (&method, rest) {
        (Method::Post, ["users"]) => register(&read_json(req)?),
        (Method::Post, ["users", "login"]) => login(&read_json(req)?),
        (Method::Get, ["user"]) => current_user(require(viewer)?),
        (Method::Put, ["user"]) => update_user(require(viewer)?, &read_json(req)?),

        (Method::Get, ["profiles", username]) => profile(viewer, username),
        (Method::Post, ["profiles", username, "follow"]) => follow(require(viewer)?, username, true),
        (Method::Delete, ["profiles", username, "follow"]) => follow(require(viewer)?, username, false),

        (Method::Get, ["articles"]) => list_articles(viewer, query),
        (Method::Get, ["articles", "feed"]) => feed(require(viewer)?, query),
        (Method::Post, ["articles"]) => create_article(require(viewer)?, &read_json(req)?),
        (Method::Get, ["articles", slug]) => get_article(viewer, slug),
        (Method::Put, ["articles", slug]) => update_article(require(viewer)?, slug, &read_json(req)?),
        (Method::Delete, ["articles", slug]) => delete_article(require(viewer)?, slug),
        (Method::Post, ["articles", slug, "favorite"]) => favorite(require(viewer)?, slug, true),
        (Method::Delete, ["articles", slug, "favorite"]) => favorite(require(viewer)?, slug, false),
        (Method::Get, ["articles", slug, "comments"]) => comments(viewer, slug),
        (Method::Post, ["articles", slug, "comments"]) => add_comment(require(viewer)?, slug, &read_json(req)?),
        (Method::Delete, ["articles", slug, "comments", id]) => delete_comment(require(viewer)?, slug, id),

        (Method::Get, ["tags"]) => tags(),
        _ => Err(ApiError::new(404, "not found")),
    }
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

fn user_json(row: &[Value], token: &str) -> Json {
    json!({"user": {
        "email": text(&row[1]),
        "token": token,
        "username": text(&row[2]),
        "bio": text(&row[3]),
        "image": opt_text(&row[4]),
    }})
}

fn register(body: &Json) -> Response {
    let UserBody { user } = parse::<UserBody<NewUser>>(body)?;
    validate(&[("username", &user.username), ("email", &user.email), ("password", &user.password)])?;
    let hash = hash_password(&user.password);
    let r = sql::query(
        DB,
        &format!("INSERT INTO users (username, email, password_hash) VALUES ($1, $2, $3) RETURNING {USER_COLUMNS}"),
        &[s(&user.username), s(&user.email), s(&hash)],
    )?;
    let row = first(&r)?;
    Ok((201, user_json(row, &sign(int(&row[0]))?)))
}

fn login(body: &Json) -> Response {
    let UserBody { user } = parse::<UserBody<Login>>(body)?;
    let r = sql::query(
        DB,
        &format!("SELECT {USER_COLUMNS}, password_hash FROM users WHERE email = $1"),
        &[s(&user.email)],
    )?;
    match r.rows.first() {
        Some(row) if verify_password(&user.password, &text(&row[5])) => Ok((200, user_json(row, &sign(int(&row[0]))?))),
        _ => Err(ApiError::field(401, "email or password", "is invalid")),
    }
}

fn current_user(viewer: Viewer) -> Response {
    let r = sql::query(DB, &format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1"), &[Value::Int(viewer.id)])?;
    Ok((200, user_json(first(&r)?, &viewer.token)))
}

fn update_user(viewer: Viewer, body: &Json) -> Response {
    let UserBody { user } = parse::<UserBody<UserUpdate>>(body)?;
    let hash = user.password.as_deref().map(hash_password);
    let r = sql::query(
        DB,
        &format!(
            "UPDATE users SET email = COALESCE($2, email), username = COALESCE($3, username),
                password_hash = COALESCE($4, password_hash), bio = COALESCE($5, bio), image = COALESCE($6, image)
             WHERE id = $1 RETURNING {USER_COLUMNS}"
        ),
        &[
            Value::Int(viewer.id),
            opt(user.email.as_deref()),
            opt(user.username.as_deref()),
            opt(hash.as_deref()),
            opt(user.bio.as_deref()),
            opt(user.image.as_deref()),
        ],
    )?;
    Ok((200, user_json(first(&r)?, &viewer.token)))
}

// --------------------------------------------------------------- profiles

fn profile_json(row: &[Value]) -> Json {
    json!({
        "username": text(&row[0]),
        "bio": text(&row[1]),
        "image": opt_text(&row[2]),
        "following": boolean(&row[3]),
    })
}

fn profile(viewer: Option<Viewer>, username: &str) -> Response {
    let r = sql::query(
        DB,
        "SELECT u.username, u.bio, u.image,
                EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = $1 AND f.followee_id = u.id)
         FROM users u WHERE u.username = $2",
        &[viewer_id(&viewer), s(username)],
    )?;
    match r.rows.first() {
        Some(row) => Ok((200, json!({"profile": profile_json(row)}))),
        None => Err(ApiError::new(404, "profile not found")),
    }
}

fn follow(viewer: Viewer, username: &str, add: bool) -> Response {
    let statement = if add {
        "INSERT INTO follows (follower_id, followee_id)
         SELECT $1, id FROM users WHERE username = $2 AND id <> $1 ON CONFLICT DO NOTHING"
    } else {
        "DELETE FROM follows f USING users u
         WHERE f.follower_id = $1 AND f.followee_id = u.id AND u.username = $2"
    };
    sql::execute(DB, statement, &[Value::Int(viewer.id), s(username)])?;
    profile(Some(viewer), username)
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
/// Timestamps and the tag array are rendered by Postgres as text.
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

fn article_json(row: &[Value], with_body: bool) -> Json {
    let mut article = json!({
        "slug": text(&row[0]),
        "title": text(&row[1]),
        "description": text(&row[2]),
        "tagList": serde_json::from_str::<Json>(&text(&row[10])).unwrap_or_else(|_| json!([])),
        "createdAt": text(&row[4]),
        "updatedAt": text(&row[5]),
        "favorited": boolean(&row[11]),
        "favoritesCount": int(&row[6]),
        "author": {
            "username": text(&row[7]),
            "bio": text(&row[8]),
            "image": opt_text(&row[9]),
            "following": boolean(&row[12]),
        },
    });
    if with_body {
        article["body"] = json!(text(&row[3]));
    }
    article
}

/// WHERE clause with only the filters actually given (a generic
/// `$n IS NULL OR ...` clause would get a poor plan from Postgres), and the
/// values for its parameters, numbered from `$first`.
fn list_filter(query: &str, first: usize) -> (String, Vec<Value>, bool) {
    let mut conditions = Vec::new();
    let mut params = Vec::new();
    let mut needs_users = false;
    let next = |value: String, params: &mut Vec<Value>| {
        params.push(Value::Text(value));
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

fn list_articles(viewer: Option<Viewer>, query: &str) -> Response {
    let (limit, offset) = page(query);
    let (filter, filter_params, _) = list_filter(query, 2);
    let mut params = vec![viewer_id(&viewer)];
    params.extend(filter_params);
    let (lim, off) = (params.len() + 1, params.len() + 2);
    params.extend([Value::Int(limit), Value::Int(offset)]);
    let r = sql::query(
        DB,
        &format!(
            "SELECT {} FROM articles a JOIN users u ON u.id = a.author_id {filter}
             ORDER BY a.created_at DESC LIMIT ${lim} OFFSET ${off}",
            article_columns(false)
        ),
        &params,
    )?;
    let (filter, count_params, needs_users) = list_filter(query, 1);
    let key = format!("count:{:?}:{:?}:{:?}", param(query, "tag"), param(query, "author"), param(query, "favorited"));
    let count = cached_count(&key, || {
        let join = if needs_users { "JOIN users u ON u.id = a.author_id" } else { "" };
        sql::query(DB, &format!("SELECT count(*) FROM articles a {join} {filter}"), &count_params)
    })?;
    Ok((200, articles_json(&r, count)))
}

fn feed(viewer: Viewer, query: &str) -> Response {
    let (limit, offset) = page(query);
    let followed = "a.author_id IN (SELECT followee_id FROM follows WHERE follower_id = $1)";
    let r = sql::query(
        DB,
        &format!(
            "SELECT {} FROM articles a JOIN users u ON u.id = a.author_id WHERE {followed}
             ORDER BY a.created_at DESC LIMIT $2 OFFSET $3",
            article_columns(false)
        ),
        &[Value::Int(viewer.id), Value::Int(limit), Value::Int(offset)],
    )?;
    let count = cached_count(&format!("feed-count:{}", viewer.id), || {
        sql::query(DB, &format!("SELECT count(*) FROM articles a WHERE {followed}"), &[Value::Int(viewer.id)])
    })?;
    Ok((200, articles_json(&r, count)))
}

/// Total counts are the most expensive part of a list (a scan of every
/// matching row), so they come from the cache when a recent one exists.
fn cached_count(key: &str, count: impl FnOnce() -> Result<QueryResult, DbError>) -> Result<i64, ApiError> {
    if let Some(bytes) = cache::get(key) {
        if let Ok(bytes) = <[u8; 8]>::try_from(bytes.as_slice()) {
            return Ok(i64::from_le_bytes(bytes));
        }
    }
    let n = int(&first(&count()?)?[0]);
    let _ = cache::set(key, &n.to_le_bytes(), COUNT_TTL_MS);
    Ok(n)
}

fn articles_json(r: &QueryResult, count: i64) -> Json {
    json!({
        "articles": r.rows.iter().map(|row| article_json(row, false)).collect::<Vec<_>>(),
        "articlesCount": count,
    })
}

fn fetch_article(viewer: &Value, slug: &str) -> Result<Option<Json>, ApiError> {
    let r = sql::query(
        DB,
        &format!("SELECT {} FROM articles a JOIN users u ON u.id = a.author_id WHERE a.slug = $2", article_columns(true)),
        &[viewer.clone(), s(slug)],
    )?;
    Ok(r.rows.first().map(|row| article_json(row, true)))
}

fn get_article(viewer: Option<Viewer>, slug: &str) -> Response {
    match fetch_article(&viewer_id(&viewer), slug)? {
        Some(article) => Ok((200, json!({"article": article}))),
        None => Err(ApiError::new(404, "article not found")),
    }
}

fn create_article(viewer: Viewer, body: &Json) -> Response {
    let ArticleBody { article } = parse::<ArticleBody<NewArticle>>(body)?;
    validate(&[("title", &article.title), ("description", &article.description), ("body", &article.body)])?;
    let slug = format!("{}-{}", slugify(&article.title), random_hex(4));
    let tx = Transaction::begin(DB)?;
    let inserted = tx.query(
        "INSERT INTO articles (slug, title, description, body, author_id) VALUES ($1, $2, $3, $4, $5) RETURNING id",
        &[s(&slug), s(&article.title), s(&article.description), s(&article.body), Value::Int(viewer.id)],
    )?;
    if !article.tag_list.is_empty() {
        tx.execute(
            "INSERT INTO article_tags (article_id, tag)
             SELECT $1, t FROM json_array_elements_text($2::text::json) t ON CONFLICT DO NOTHING",
            &[first(&inserted)?[0].clone(), s(&serde_json::to_string(&article.tag_list).unwrap())],
        )?;
    }
    Transaction::commit(tx)?;
    let article = fetch_article(&Value::Int(viewer.id), &slug)?.ok_or_else(|| ApiError::new(500, "article vanished"))?;
    Ok((201, json!({"article": article})))
}

fn update_article(viewer: Viewer, slug: &str, body: &Json) -> Response {
    let ArticleBody { article } = parse::<ArticleBody<ArticleUpdate>>(body)?;
    let updated = sql::execute(
        DB,
        "UPDATE articles SET title = COALESCE($3, title), description = COALESCE($4, description),
            body = COALESCE($5, body), updated_at = now()
         WHERE slug = $1 AND author_id = $2",
        &[s(slug), Value::Int(viewer.id), opt(article.title.as_deref()), opt(article.description.as_deref()), opt(article.body.as_deref())],
    )?;
    if updated == 0 {
        return Err(not_found_or_forbidden(slug)?);
    }
    get_article(Some(viewer), slug)
}

fn delete_article(viewer: Viewer, slug: &str) -> Response {
    let deleted = sql::execute(DB, "DELETE FROM articles WHERE slug = $1 AND author_id = $2", &[s(slug), Value::Int(viewer.id)])?;
    if deleted == 0 {
        return Err(not_found_or_forbidden(slug)?);
    }
    Ok((200, json!({})))
}

/// After an owner-restricted change touched no rows: 404 or 403?
fn not_found_or_forbidden(slug: &str) -> Result<ApiError, ApiError> {
    let r = sql::query(DB, "SELECT 1 FROM articles WHERE slug = $1", &[s(slug)])?;
    Ok(if r.rows.is_empty() { ApiError::new(404, "article not found") } else { ApiError::new(403, "not the author") })
}

fn favorite(viewer: Viewer, slug: &str, add: bool) -> Response {
    let tx = Transaction::begin(DB)?;
    let found = tx.query("SELECT id FROM articles WHERE slug = $1", &[s(slug)])?;
    let Some(row) = found.rows.first() else {
        return Err(ApiError::new(404, "article not found"));
    };
    let ids = [Value::Int(viewer.id), row[0].clone()];
    let changed = if add {
        tx.execute("INSERT INTO favorites (user_id, article_id) VALUES ($1, $2) ON CONFLICT DO NOTHING", &ids)?
    } else {
        tx.execute("DELETE FROM favorites WHERE user_id = $1 AND article_id = $2", &ids)?
    };
    if changed > 0 {
        let delta = if add { 1 } else { -1 };
        tx.execute("UPDATE articles SET favorites_count = favorites_count + $2 WHERE id = $1", &[row[0].clone(), Value::Int(delta)])?;
    }
    Transaction::commit(tx)?;
    get_article(Some(viewer), slug)
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

fn comment_json(row: &[Value]) -> Json {
    json!({
        "id": int(&row[0]),
        "createdAt": text(&row[1]),
        "updatedAt": text(&row[2]),
        "body": text(&row[3]),
        "author": {
            "username": text(&row[4]),
            "bio": text(&row[5]),
            "image": opt_text(&row[6]),
            "following": boolean(&row[7]),
        },
    })
}

fn comments(viewer: Option<Viewer>, slug: &str) -> Response {
    let r = sql::query(
        DB,
        &format!(
            "SELECT {COMMENT_COLUMNS} FROM comments c JOIN users u ON u.id = c.author_id
             WHERE c.article_id = (SELECT id FROM articles WHERE slug = $2) ORDER BY c.id DESC"
        ),
        &[viewer_id(&viewer), s(slug)],
    )?;
    Ok((200, json!({"comments": r.rows.iter().map(|row| comment_json(row)).collect::<Vec<_>>()})))
}

fn add_comment(viewer: Viewer, slug: &str, body: &Json) -> Response {
    let CommentBody { comment } = parse(body)?;
    validate(&[("body", &comment.body)])?;
    let r = sql::query(
        DB,
        &format!(
            "WITH c AS (
                INSERT INTO comments (article_id, author_id, body)
                SELECT a.id, $1, $3 FROM articles a WHERE a.slug = $2
                RETURNING *)
             SELECT {COMMENT_COLUMNS} FROM c JOIN users u ON u.id = c.author_id"
        ),
        &[Value::Int(viewer.id), s(slug), s(&comment.body)],
    )?;
    match r.rows.first() {
        Some(row) => Ok((201, json!({"comment": comment_json(row)}))),
        None => Err(ApiError::new(404, "article not found")),
    }
}

fn delete_comment(viewer: Viewer, slug: &str, id: &str) -> Response {
    let id: i64 = id.parse().map_err(|_| ApiError::new(404, "comment not found"))?;
    let deleted = sql::execute(
        DB,
        "DELETE FROM comments c USING articles a
         WHERE c.id = $1 AND c.article_id = a.id AND a.slug = $2 AND c.author_id = $3",
        &[Value::Int(id), s(slug), Value::Int(viewer.id)],
    )?;
    if deleted == 0 {
        return Err(ApiError::new(404, "comment not found"));
    }
    Ok((200, json!({})))
}

/// Aggregates every tag row, so the result is cached for everyone.
fn tags() -> Response {
    if let Some(cached) = cache::get("popular-tags").and_then(|b| serde_json::from_slice(&b).ok()) {
        return Ok((200, cached));
    }
    let r = sql::query(DB, "SELECT tag FROM article_tags GROUP BY tag ORDER BY count(*) DESC, tag LIMIT 20", &[])?;
    let body = json!({"tags": r.rows.iter().map(|row| text(&row[0])).collect::<Vec<_>>()});
    let _ = cache::set("popular-tags", &serde_json::to_vec(&body).unwrap(), TAGS_TTL_MS);
    Ok((200, body))
}

// ------------------------------------------------------------------- auth

struct Viewer {
    id: i64,
    token: String,
}

/// Reads `Authorization: Token <jwt>` (or `Bearer`). A missing header is an
/// anonymous request; a bad token is an error.
fn authenticate(req: &IncomingRequest) -> Result<Option<Viewer>, ApiError> {
    let values = req.headers().get(&"authorization".to_string());
    let Some(header) = values.first() else {
        return Ok(None);
    };
    let header = std::str::from_utf8(header).unwrap_or("");
    let token = header.strip_prefix("Token ").or_else(|| header.strip_prefix("Bearer ")).unwrap_or("");
    match verify_token(token) {
        Some(id) => Ok(Some(Viewer { id, token: token.to_string() })),
        None => Err(ApiError::new(401, "invalid or expired token")),
    }
}

fn require(viewer: Option<Viewer>) -> Result<Viewer, ApiError> {
    viewer.ok_or_else(|| ApiError::new(401, "authentication required"))
}

fn viewer_id(viewer: &Option<Viewer>) -> Value {
    viewer.as_ref().map_or(Value::Null, |v| Value::Int(v.id))
}

fn jwt_secret() -> Result<Vec<u8>, ApiError> {
    match std::env::var("JWT_SECRET") {
        Ok(secret) if secret.len() >= 16 => Ok(secret.into_bytes()),
        _ => Err(ApiError::new(500, "JWT_SECRET is not configured")),
    }
}

fn hmac(secret: &[u8], data: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(data);
    mac
}

/// HS256 JWT with `sub` (user id) and `exp`.
fn sign(user_id: i64) -> Result<String, ApiError> {
    let header = B64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = B64.encode(json!({"sub": user_id, "exp": now() + TOKEN_TTL_SECS}).to_string());
    let signing_input = format!("{header}.{claims}");
    let signature = B64.encode(hmac(&jwt_secret()?, signing_input.as_bytes()).finalize().into_bytes());
    Ok(format!("{signing_input}.{signature}"))
}

fn verify_token(token: &str) -> Option<i64> {
    let (signing_input, signature) = token.rsplit_once('.')?;
    let signature = B64.decode(signature).ok()?;
    hmac(&jwt_secret().ok()?, signing_input.as_bytes()).verify_slice(&signature).ok()?;
    let (_, claims) = signing_input.split_once('.')?;
    let claims: Json = serde_json::from_slice(&B64.decode(claims).ok()?).ok()?;
    if claims["exp"].as_u64()? < now() {
        return None;
    }
    claims["sub"].as_i64()
}

fn now() -> u64 {
    wasip2::clocks::wall_clock::now().seconds
}

/// `argon2id$<m>$<t>$<salt>$<hash>`, all base64url.
fn hash_password(password: &str) -> String {
    let salt = wasip2::random::random::get_random_bytes(16);
    let hash = argon2_hash(password, &salt, ARGON2_M_KIB, ARGON2_T).expect("valid argon2 parameters");
    format!("argon2id${ARGON2_M_KIB}${ARGON2_T}${}${}", B64.encode(&salt), B64.encode(hash))
}

fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    let ["argon2id", m, t, salt, hash] = parts.as_slice() else {
        return false;
    };
    let (Ok(m), Ok(t), Ok(salt), Ok(expected)) = (m.parse(), t.parse(), B64.decode(salt), B64.decode(hash)) else {
        return false;
    };
    match argon2_hash(password, &salt, m, t) {
        // Constant-time comparison.
        Some(actual) => actual.len() == expected.len() && actual.iter().zip(&expected).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0,
        None => false,
    }
}

fn argon2_hash(password: &str, salt: &[u8], m_kib: u32, t: u32) -> Option<[u8; 32]> {
    let params = Params::new(m_kib, t, 1, Some(32)).ok()?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(password.as_bytes(), salt, &mut out).ok()?;
    Some(out)
}

// ---------------------------------------------------------------- helpers

struct ApiError {
    status: u16,
    body: Json,
}

impl ApiError {
    /// RealWorld's error shape: `{"errors": {"body": ["..."]}}`.
    fn new(status: u16, message: &str) -> Self {
        Self::field(status, "body", message)
    }

    fn field(status: u16, field: &str, message: &str) -> Self {
        Self { status, body: json!({"errors": {field: [message]}}) }
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        match e {
            // unique_violation, e.g. a taken username or email
            DbError::Query(m) if m.starts_with("23505") => ApiError::field(409, "username or email", "has already been taken"),
            DbError::Timeout => ApiError::new(504, "database timeout"),
            DbError::Unavailable(m) => ApiError::new(503, &m),
            DbError::NotAllowed(m) | DbError::Query(m) | DbError::Conversion(m) => ApiError::new(500, &m),
            DbError::TooManyRows(_) => ApiError::new(500, "result too large"),
        }
    }
}

fn read_json(req: &IncomingRequest) -> Result<Json, ApiError> {
    let body = req.consume().map_err(|_| ApiError::new(400, "body already read"))?;
    let stream = body.stream().map_err(|_| ApiError::new(400, "unreadable body"))?;
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_BODY {
                    return Err(ApiError::new(413, "body too large"));
                }
            }
            Err(StreamError::Closed) => break,
            Err(_) => return Err(ApiError::new(400, "error reading body")),
        }
    }
    drop(stream);
    IncomingBody::finish(body);
    serde_json::from_slice(&bytes).map_err(|e| ApiError::new(422, &format!("invalid JSON: {e}")))
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Json) -> Result<T, ApiError> {
    T::deserialize(body).map_err(|e| ApiError::new(422, &e.to_string()))
}

fn validate(fields: &[(&str, &str)]) -> Result<(), ApiError> {
    match fields.iter().find(|(_, value)| value.trim().is_empty()) {
        Some((name, _)) => Err(ApiError::field(422, name, "can't be blank")),
        None => Ok(()),
    }
}

fn first(r: &QueryResult) -> Result<&[Value], ApiError> {
    r.rows.first().map(Vec::as_slice).ok_or_else(|| ApiError::new(404, "not found"))
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
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.split('-').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-");
    slug.chars().take(60).collect()
}

fn random_hex(bytes: u64) -> String {
    wasip2::random::random::get_random_bytes(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn s(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn opt(value: Option<&str>) -> Value {
    value.map_or(Value::Null, s)
}

fn text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        _ => String::new(),
    }
}

fn opt_text(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.clone()),
        _ => None,
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => 0,
    }
}

fn boolean(v: &Value) -> bool {
    matches!(v, Value::Boolean(true))
}

fn respond(out: ResponseOutparam, status: u16, body: &Json) {
    let headers = Fields::from_list(&[("content-type".into(), b"application/json".to_vec())]).unwrap();
    let resp = OutgoingResponse::new(headers);
    resp.set_status_code(status).unwrap();
    let body_handle = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let mut stream = body_handle.write().unwrap();
    stream.write_all(&serde_json::to_vec(body).unwrap()).unwrap();
    stream.flush().unwrap();
    drop(stream);
    OutgoingBody::finish(body_handle, None).unwrap();
}

wasip2::http::proxy::export!(Conduit);
