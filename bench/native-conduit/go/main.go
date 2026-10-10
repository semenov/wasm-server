// The RealWorld "Conduit" API as an ordinary Go server, for comparison with
// the wasm app in guests/conduit: the same routes, SQL, JWT and argon2id
// parameters, and an in-process cache with the same TTLs.
//
// Stack: net/http, pgx/v5 (pgxpool, cached prepared statements),
// golang.org/x/crypto/argon2.
//
// Environment: DATABASE_URL (libpq-style, e.g. ...?sslmode=verify-full&sslrootcert=ca.pem),
// JWT_SECRET (at least 16 bytes), LISTEN (default 127.0.0.1:8080),
// POOL_SIZE (default 32).
package main

import (
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
	"github.com/jackc/pgx/v5/pgxpool"
	"golang.org/x/crypto/argon2"
)

const (
	tokenTTL = 30 * 24 * time.Hour
	maxBody  = 1 << 20
	// OWASP's minimum for argon2id: 19 MiB, 2 passes, 1 lane.
	argon2MKiB = 19 * 1024
	argon2T    = 2
	// How stale cached counts and popular tags may get.
	countTTL = 10 * time.Second
	tagsTTL  = 30 * time.Second
)

var (
	db        *pgxpool.Pool
	jwtSecret []byte
	cache     = newTTLCache()
	b64       = base64.RawURLEncoding
)

func main() {
	jwtSecret = []byte(os.Getenv("JWT_SECRET"))
	if len(jwtSecret) < 16 {
		log.Fatal("JWT_SECRET must be at least 16 bytes")
	}
	cfg, err := pgxpool.ParseConfig(os.Getenv("DATABASE_URL"))
	if err != nil {
		log.Fatal(err)
	}
	cfg.MaxConns = int32(envInt("POOL_SIZE", 32))
	db, err = pgxpool.NewWithConfig(context.Background(), cfg)
	if err != nil {
		log.Fatal(err)
	}
	if err := db.Ping(context.Background()); err != nil {
		log.Fatal(err)
	}

	mux := http.NewServeMux()
	mux.HandleFunc("GET /hello", func(w http.ResponseWriter, r *http.Request) { io.WriteString(w, "Hello, world!\n") })
	route := func(pattern string, h func(*http.Request, *viewer) (int, any, error)) {
		mux.HandleFunc(pattern, func(w http.ResponseWriter, r *http.Request) { serve(w, r, h) })
	}
	route("POST /api/users", register)
	route("POST /api/users/login", login)
	route("GET /api/user", authed(currentUser))
	route("PUT /api/user", authed(updateUser))
	route("GET /api/profiles/{username}", profile)
	route("POST /api/profiles/{username}/follow", authed(func(r *http.Request, v *viewer) (int, any, error) { return follow(r, v, true) }))
	route("DELETE /api/profiles/{username}/follow", authed(func(r *http.Request, v *viewer) (int, any, error) { return follow(r, v, false) }))
	route("GET /api/articles", listArticles)
	route("GET /api/articles/feed", authed(feed))
	route("POST /api/articles", authed(createArticle))
	route("GET /api/articles/{slug}", getArticle)
	route("PUT /api/articles/{slug}", authed(updateArticle))
	route("DELETE /api/articles/{slug}", authed(deleteArticle))
	route("POST /api/articles/{slug}/favorite", authed(func(r *http.Request, v *viewer) (int, any, error) { return favorite(r, v, true) }))
	route("DELETE /api/articles/{slug}/favorite", authed(func(r *http.Request, v *viewer) (int, any, error) { return favorite(r, v, false) }))
	route("GET /api/articles/{slug}/comments", comments)
	route("POST /api/articles/{slug}/comments", authed(addComment))
	route("DELETE /api/articles/{slug}/comments/{id}", authed(deleteComment))
	route("GET /api/tags", tags)

	listen := os.Getenv("LISTEN")
	if listen == "" {
		listen = "127.0.0.1:8080"
	}
	log.Printf("listening on %s", listen)
	log.Fatal(http.ListenAndServe(listen, mux))
}

// ---------------------------------------------------------------- serving

type apiError struct {
	status int
	field  string
	msg    string
}

func (e *apiError) Error() string { return e.msg }

func errStatus(status int, msg string) error { return &apiError{status, "body", msg} }

type viewer struct {
	id    int64
	token string
}

func serve(w http.ResponseWriter, r *http.Request, h func(*http.Request, *viewer) (int, any, error)) {
	status, body, err := func() (int, any, error) {
		v, err := authenticate(r)
		if err != nil {
			return 0, nil, err
		}
		return h(r, v)
	}()
	if err != nil {
		var ae *apiError
		if !errors.As(err, &ae) {
			ae = dbError(err)
		}
		status, body = ae.status, map[string]any{"errors": map[string][]string{ae.field: {ae.msg}}}
	}
	w.Header().Set("content-type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(body)
}

func dbError(err error) *apiError {
	var pg *pgconn.PgError
	if errors.As(err, &pg) && pg.Code == "23505" {
		return &apiError{409, "username or email", "has already been taken"}
	}
	return &apiError{500, "body", err.Error()}
}

func authed(h func(*http.Request, *viewer) (int, any, error)) func(*http.Request, *viewer) (int, any, error) {
	return func(r *http.Request, v *viewer) (int, any, error) {
		if v == nil {
			return 0, nil, errStatus(401, "authentication required")
		}
		return h(r, v)
	}
}

func readJSON(r *http.Request, into any) error {
	data, err := io.ReadAll(io.LimitReader(r.Body, maxBody+1))
	if err != nil {
		return errStatus(400, "error reading body")
	}
	if len(data) > maxBody {
		return errStatus(413, "body too large")
	}
	if err := json.Unmarshal(data, into); err != nil {
		return errStatus(422, "invalid JSON: "+err.Error())
	}
	return nil
}

func validate(fields ...[2]string) error {
	for _, f := range fields {
		if strings.TrimSpace(f[1]) == "" {
			return &apiError{422, f[0], "can't be blank"}
		}
	}
	return nil
}

func viewerID(v *viewer) any {
	if v == nil {
		return nil
	}
	return v.id
}

// ------------------------------------------------------------------ users

const userColumns = "id, email, username, bio, image"

type userRow struct {
	id                    int64
	email, username, bio string
	image                 *string
}

func (u *userRow) scan(row pgx.Row, extra ...any) error {
	return row.Scan(append([]any{&u.id, &u.email, &u.username, &u.bio, &u.image}, extra...)...)
}

func userJSON(u *userRow, token string) map[string]any {
	return map[string]any{"user": map[string]any{
		"email": u.email, "token": token, "username": u.username, "bio": u.bio, "image": u.image,
	}}
}

func register(r *http.Request, _ *viewer) (int, any, error) {
	var body struct {
		User struct{ Username, Email, Password string }
	}
	if err := readJSON(r, &body); err != nil {
		return 0, nil, err
	}
	u := body.User
	if err := validate([2]string{"username", u.Username}, [2]string{"email", u.Email}, [2]string{"password", u.Password}); err != nil {
		return 0, nil, err
	}
	var row userRow
	err := row.scan(db.QueryRow(r.Context(),
		"INSERT INTO users (username, email, password_hash) VALUES ($1, $2, $3) RETURNING "+userColumns,
		u.Username, u.Email, hashPassword(u.Password)))
	if err != nil {
		return 0, nil, err
	}
	return 201, userJSON(&row, sign(row.id)), nil
}

func login(r *http.Request, _ *viewer) (int, any, error) {
	var body struct{ User struct{ Email, Password string } }
	if err := readJSON(r, &body); err != nil {
		return 0, nil, err
	}
	var row userRow
	var hash string
	err := row.scan(db.QueryRow(r.Context(), "SELECT "+userColumns+", password_hash FROM users WHERE email = $1", body.User.Email), &hash)
	if err != nil && !errors.Is(err, pgx.ErrNoRows) {
		return 0, nil, err
	}
	if err != nil || !verifyPassword(body.User.Password, hash) {
		return 0, nil, &apiError{401, "email or password", "is invalid"}
	}
	return 200, userJSON(&row, sign(row.id)), nil
}

func currentUser(r *http.Request, v *viewer) (int, any, error) {
	var row userRow
	if err := row.scan(db.QueryRow(r.Context(), "SELECT "+userColumns+" FROM users WHERE id = $1", v.id)); err != nil {
		return 0, nil, notFound(err, "not found")
	}
	return 200, userJSON(&row, v.token), nil
}

func updateUser(r *http.Request, v *viewer) (int, any, error) {
	var body struct {
		User struct{ Email, Username, Password, Bio, Image *string }
	}
	if err := readJSON(r, &body); err != nil {
		return 0, nil, err
	}
	u := body.User
	var hash *string
	if u.Password != nil {
		h := hashPassword(*u.Password)
		hash = &h
	}
	var row userRow
	err := row.scan(db.QueryRow(r.Context(),
		`UPDATE users SET email = COALESCE($2, email), username = COALESCE($3, username),
		    password_hash = COALESCE($4, password_hash), bio = COALESCE($5, bio), image = COALESCE($6, image)
		 WHERE id = $1 RETURNING `+userColumns,
		v.id, u.Email, u.Username, hash, u.Bio, u.Image))
	if err != nil {
		return 0, nil, notFound(err, "not found")
	}
	return 200, userJSON(&row, v.token), nil
}

func notFound(err error, msg string) error {
	if errors.Is(err, pgx.ErrNoRows) {
		return errStatus(404, msg)
	}
	return err
}

// --------------------------------------------------------------- profiles

type profileJSON struct {
	Username  string  `json:"username"`
	Bio       string  `json:"bio"`
	Image     *string `json:"image"`
	Following bool    `json:"following"`
}

func profile(r *http.Request, v *viewer) (int, any, error) {
	return fetchProfile(r.Context(), v, r.PathValue("username"))
}

func fetchProfile(ctx context.Context, v *viewer, username string) (int, any, error) {
	var p profileJSON
	err := db.QueryRow(ctx,
		`SELECT u.username, u.bio, u.image,
		        EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = $1 AND f.followee_id = u.id)
		 FROM users u WHERE u.username = $2`, viewerID(v), username).Scan(&p.Username, &p.Bio, &p.Image, &p.Following)
	if err != nil {
		return 0, nil, notFound(err, "profile not found")
	}
	return 200, map[string]any{"profile": p}, nil
}

func follow(r *http.Request, v *viewer, add bool) (int, any, error) {
	statement := `DELETE FROM follows f USING users u
		WHERE f.follower_id = $1 AND f.followee_id = u.id AND u.username = $2`
	if add {
		statement = `INSERT INTO follows (follower_id, followee_id)
			SELECT $1, id FROM users WHERE username = $2 AND id <> $1 ON CONFLICT DO NOTHING`
	}
	username := r.PathValue("username")
	if _, err := db.Exec(r.Context(), statement, v.id, username); err != nil {
		return 0, nil, err
	}
	return fetchProfile(r.Context(), v, username)
}

// --------------------------------------------------------------- articles

// Columns for scanArticle. $1 is the viewer's id (NULL when anonymous).
func articleColumns(withBody bool) string {
	body := "''"
	if withBody {
		body = "a.body"
	}
	return `a.slug, a.title, a.description, ` + body + `,
	    to_char(a.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
	    to_char(a.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
	    a.favorites_count, u.username, u.bio, u.image,
	    COALESCE((SELECT json_agg(t.tag ORDER BY t.tag) FROM article_tags t WHERE t.article_id = a.id)::text, '[]'),
	    EXISTS (SELECT 1 FROM favorites f WHERE f.article_id = a.id AND f.user_id = $1),
	    EXISTS (SELECT 1 FROM follows fo WHERE fo.followee_id = a.author_id AND fo.follower_id = $1)`
}

type articleJSON struct {
	Slug           string          `json:"slug"`
	Title          string          `json:"title"`
	Description    string          `json:"description"`
	Body           *string         `json:"body,omitempty"`
	TagList        json.RawMessage `json:"tagList"`
	CreatedAt      string          `json:"createdAt"`
	UpdatedAt      string          `json:"updatedAt"`
	Favorited      bool            `json:"favorited"`
	FavoritesCount int64           `json:"favoritesCount"`
	Author         profileJSON     `json:"author"`
}

func scanArticle(row pgx.Row, withBody bool) (*articleJSON, error) {
	var a articleJSON
	var body, tagList string
	err := row.Scan(&a.Slug, &a.Title, &a.Description, &body, &a.CreatedAt, &a.UpdatedAt, &a.FavoritesCount,
		&a.Author.Username, &a.Author.Bio, &a.Author.Image, &tagList, &a.Favorited, &a.Author.Following)
	if err != nil {
		return nil, err
	}
	a.TagList = json.RawMessage(tagList)
	if withBody {
		a.Body = &body
	}
	return &a, nil
}

// listFilter builds a WHERE clause with only the filters actually given (a
// generic `$n IS NULL OR ...` clause gets a poor plan), with parameters
// numbered from `first`.
func listFilter(q map[string][]string, first int) (clause string, params []any, needsUsers bool) {
	var conditions []string
	next := func(value string) int {
		params = append(params, value)
		return first + len(params) - 1
	}
	if tag := param(q, "tag"); tag != nil {
		conditions = append(conditions, fmt.Sprintf("a.id IN (SELECT article_id FROM article_tags WHERE tag = $%d)", next(*tag)))
	}
	if author := param(q, "author"); author != nil {
		conditions = append(conditions, fmt.Sprintf("u.username = $%d", next(*author)))
		needsUsers = true
	}
	if fav := param(q, "favorited"); fav != nil {
		conditions = append(conditions, fmt.Sprintf(
			"a.id IN (SELECT f.article_id FROM favorites f JOIN users fu ON fu.id = f.user_id WHERE fu.username = $%d)", next(*fav)))
	}
	if len(conditions) > 0 {
		clause = "WHERE " + strings.Join(conditions, " AND ")
	}
	return
}

func param(q map[string][]string, key string) *string {
	if v, ok := q[key]; ok && len(v) > 0 {
		return &v[0]
	}
	return nil
}

func page(q map[string][]string) (int64, int64) {
	get := func(key string, def int64) int64 {
		if v := param(q, key); v != nil {
			if n, err := strconv.ParseInt(*v, 10, 64); err == nil {
				return n
			}
		}
		return def
	}
	return min(max(get("limit", 20), 1), 100), max(get("offset", 0), 0)
}

func listArticles(r *http.Request, v *viewer) (int, any, error) {
	q := r.URL.Query()
	limit, offset := page(q)
	filter, filterParams, _ := listFilter(q, 2)
	params := append([]any{viewerID(v)}, filterParams...)
	sql := fmt.Sprintf(`SELECT %s FROM articles a JOIN users u ON u.id = a.author_id %s
		ORDER BY a.created_at DESC LIMIT $%d OFFSET $%d`, articleColumns(false), filter, len(params)+1, len(params)+2)
	articles, err := queryArticles(r.Context(), sql, append(params, limit, offset)...)
	if err != nil {
		return 0, nil, err
	}
	filter, countParams, needsUsers := listFilter(q, 1)
	key := fmt.Sprintf("count:%v:%v:%v", deref(param(q, "tag")), deref(param(q, "author")), deref(param(q, "favorited")))
	count, err := cachedCount(r.Context(), key, func() (string, []any) {
		join := ""
		if needsUsers {
			join = "JOIN users u ON u.id = a.author_id"
		}
		return fmt.Sprintf("SELECT count(*) FROM articles a %s %s", join, filter), countParams
	})
	if err != nil {
		return 0, nil, err
	}
	return 200, map[string]any{"articles": articles, "articlesCount": count}, nil
}

func deref(s *string) string {
	if s == nil {
		return "<none>"
	}
	return *s
}

func feed(r *http.Request, v *viewer) (int, any, error) {
	limit, offset := page(r.URL.Query())
	const followed = "a.author_id IN (SELECT followee_id FROM follows WHERE follower_id = $1)"
	articles, err := queryArticles(r.Context(), fmt.Sprintf(`SELECT %s FROM articles a JOIN users u ON u.id = a.author_id
		WHERE %s ORDER BY a.created_at DESC LIMIT $2 OFFSET $3`, articleColumns(false), followed), v.id, limit, offset)
	if err != nil {
		return 0, nil, err
	}
	count, err := cachedCount(r.Context(), fmt.Sprintf("feed-count:%d", v.id), func() (string, []any) {
		return "SELECT count(*) FROM articles a WHERE " + followed, []any{v.id}
	})
	if err != nil {
		return 0, nil, err
	}
	return 200, map[string]any{"articles": articles, "articlesCount": count}, nil
}

func queryArticles(ctx context.Context, sql string, args ...any) ([]*articleJSON, error) {
	rows, err := db.Query(ctx, sql, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	articles := []*articleJSON{}
	for rows.Next() {
		a, err := scanArticle(rows, false)
		if err != nil {
			return nil, err
		}
		articles = append(articles, a)
	}
	return articles, rows.Err()
}

// cachedCount: total counts are the most expensive part of a list, so they
// come from the cache when a recent one exists.
func cachedCount(ctx context.Context, key string, query func() (string, []any)) (int64, error) {
	if v, ok := cache.get(key); ok {
		return v.(int64), nil
	}
	sql, args := query()
	var n int64
	if err := db.QueryRow(ctx, sql, args...).Scan(&n); err != nil {
		return 0, err
	}
	cache.set(key, n, countTTL)
	return n, nil
}

func fetchArticle(ctx context.Context, viewerID any, slug string) (*articleJSON, error) {
	a, err := scanArticle(db.QueryRow(ctx, "SELECT "+articleColumns(true)+
		" FROM articles a JOIN users u ON u.id = a.author_id WHERE a.slug = $2", viewerID, slug), true)
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, nil
	}
	return a, err
}

func getArticle(r *http.Request, v *viewer) (int, any, error) {
	return articleResponse(r.Context(), v, r.PathValue("slug"), 200)
}

func articleResponse(ctx context.Context, v *viewer, slug string, status int) (int, any, error) {
	a, err := fetchArticle(ctx, viewerID(v), slug)
	if err != nil {
		return 0, nil, err
	}
	if a == nil {
		return 0, nil, errStatus(404, "article not found")
	}
	return status, map[string]any{"article": a}, nil
}

func createArticle(r *http.Request, v *viewer) (int, any, error) {
	var body struct {
		Article struct {
			Title, Description, Body string
			TagList                  []string `json:"tagList"`
		}
	}
	if err := readJSON(r, &body); err != nil {
		return 0, nil, err
	}
	a := body.Article
	if err := validate([2]string{"title", a.Title}, [2]string{"description", a.Description}, [2]string{"body", a.Body}); err != nil {
		return 0, nil, err
	}
	slug := slugify(a.Title) + "-" + randomHex(4)
	ctx := r.Context()
	err := pgx.BeginFunc(ctx, db, func(tx pgx.Tx) error {
		var id int64
		err := tx.QueryRow(ctx, "INSERT INTO articles (slug, title, description, body, author_id) VALUES ($1, $2, $3, $4, $5) RETURNING id",
			slug, a.Title, a.Description, a.Body, v.id).Scan(&id)
		if err != nil || len(a.TagList) == 0 {
			return err
		}
		tagList, _ := json.Marshal(a.TagList)
		_, err = tx.Exec(ctx, `INSERT INTO article_tags (article_id, tag)
			SELECT $1, t FROM json_array_elements_text($2::text::json) t ON CONFLICT DO NOTHING`, id, string(tagList))
		return err
	})
	if err != nil {
		return 0, nil, err
	}
	return articleResponse(ctx, v, slug, 201)
}

func updateArticle(r *http.Request, v *viewer) (int, any, error) {
	var body struct{ Article struct{ Title, Description, Body *string } }
	if err := readJSON(r, &body); err != nil {
		return 0, nil, err
	}
	a, slug := body.Article, r.PathValue("slug")
	tag, err := db.Exec(r.Context(), `UPDATE articles SET title = COALESCE($3, title), description = COALESCE($4, description),
		    body = COALESCE($5, body), updated_at = now()
		 WHERE slug = $1 AND author_id = $2`, slug, v.id, a.Title, a.Description, a.Body)
	if err != nil {
		return 0, nil, err
	}
	if tag.RowsAffected() == 0 {
		return 0, nil, notFoundOrForbidden(r.Context(), slug)
	}
	return articleResponse(r.Context(), v, slug, 200)
}

func deleteArticle(r *http.Request, v *viewer) (int, any, error) {
	slug := r.PathValue("slug")
	tag, err := db.Exec(r.Context(), "DELETE FROM articles WHERE slug = $1 AND author_id = $2", slug, v.id)
	if err != nil {
		return 0, nil, err
	}
	if tag.RowsAffected() == 0 {
		return 0, nil, notFoundOrForbidden(r.Context(), slug)
	}
	return 200, map[string]any{}, nil
}

// notFoundOrForbidden: after an owner-restricted change touched no rows,
// is it 404 or 403?
func notFoundOrForbidden(ctx context.Context, slug string) error {
	var one int
	err := db.QueryRow(ctx, "SELECT 1 FROM articles WHERE slug = $1", slug).Scan(&one)
	if errors.Is(err, pgx.ErrNoRows) {
		return errStatus(404, "article not found")
	} else if err != nil {
		return err
	}
	return errStatus(403, "not the author")
}

func favorite(r *http.Request, v *viewer, add bool) (int, any, error) {
	ctx, slug := r.Context(), r.PathValue("slug")
	err := pgx.BeginFunc(ctx, db, func(tx pgx.Tx) error {
		var id int64
		if err := tx.QueryRow(ctx, "SELECT id FROM articles WHERE slug = $1", slug).Scan(&id); err != nil {
			return notFound(err, "article not found")
		}
		statement := "DELETE FROM favorites WHERE user_id = $1 AND article_id = $2"
		delta := -1
		if add {
			statement = "INSERT INTO favorites (user_id, article_id) VALUES ($1, $2) ON CONFLICT DO NOTHING"
			delta = 1
		}
		tag, err := tx.Exec(ctx, statement, v.id, id)
		if err != nil || tag.RowsAffected() == 0 {
			return err
		}
		_, err = tx.Exec(ctx, "UPDATE articles SET favorites_count = favorites_count + $2 WHERE id = $1", id, delta)
		return err
	})
	if err != nil {
		return 0, nil, err
	}
	return articleResponse(ctx, v, slug, 200)
}

// --------------------------------------------------------------- comments

// $1 is the viewer's id.
const commentColumns = `c.id,
    to_char(c.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    to_char(c.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    c.body, u.username, u.bio, u.image,
    EXISTS (SELECT 1 FROM follows fo WHERE fo.followee_id = u.id AND fo.follower_id = $1)`

type commentJSON struct {
	ID        int64       `json:"id"`
	CreatedAt string      `json:"createdAt"`
	UpdatedAt string      `json:"updatedAt"`
	Body      string      `json:"body"`
	Author    profileJSON `json:"author"`
}

func scanComment(row pgx.Row) (*commentJSON, error) {
	var c commentJSON
	err := row.Scan(&c.ID, &c.CreatedAt, &c.UpdatedAt, &c.Body, &c.Author.Username, &c.Author.Bio, &c.Author.Image, &c.Author.Following)
	return &c, err
}

func comments(r *http.Request, v *viewer) (int, any, error) {
	rows, err := db.Query(r.Context(), "SELECT "+commentColumns+` FROM comments c JOIN users u ON u.id = c.author_id
		WHERE c.article_id = (SELECT id FROM articles WHERE slug = $2) ORDER BY c.id DESC`, viewerID(v), r.PathValue("slug"))
	if err != nil {
		return 0, nil, err
	}
	defer rows.Close()
	list := []*commentJSON{}
	for rows.Next() {
		c, err := scanComment(rows)
		if err != nil {
			return 0, nil, err
		}
		list = append(list, c)
	}
	if err := rows.Err(); err != nil {
		return 0, nil, err
	}
	return 200, map[string]any{"comments": list}, nil
}

func addComment(r *http.Request, v *viewer) (int, any, error) {
	var body struct{ Comment struct{ Body string } }
	if err := readJSON(r, &body); err != nil {
		return 0, nil, err
	}
	if err := validate([2]string{"body", body.Comment.Body}); err != nil {
		return 0, nil, err
	}
	c, err := scanComment(db.QueryRow(r.Context(), `WITH c AS (
		    INSERT INTO comments (article_id, author_id, body)
		    SELECT a.id, $1, $3 FROM articles a WHERE a.slug = $2
		    RETURNING *)
		 SELECT `+commentColumns+` FROM c JOIN users u ON u.id = c.author_id`, v.id, r.PathValue("slug"), body.Comment.Body))
	if err != nil {
		return 0, nil, notFound(err, "article not found")
	}
	return 201, map[string]any{"comment": c}, nil
}

func deleteComment(r *http.Request, v *viewer) (int, any, error) {
	id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
	if err != nil {
		return 0, nil, errStatus(404, "comment not found")
	}
	tag, err := db.Exec(r.Context(), `DELETE FROM comments c USING articles a
		WHERE c.id = $1 AND c.article_id = a.id AND a.slug = $2 AND c.author_id = $3`, id, r.PathValue("slug"), v.id)
	if err != nil {
		return 0, nil, err
	}
	if tag.RowsAffected() == 0 {
		return 0, nil, errStatus(404, "comment not found")
	}
	return 200, map[string]any{}, nil
}

// tags aggregates every tag row, so the result is cached for everyone.
func tags(r *http.Request, _ *viewer) (int, any, error) {
	if v, ok := cache.get("popular-tags"); ok {
		return 200, v, nil
	}
	rows, err := db.Query(r.Context(), "SELECT tag FROM article_tags GROUP BY tag ORDER BY count(*) DESC, tag LIMIT 20")
	if err != nil {
		return 0, nil, err
	}
	list, err := pgx.CollectRows(rows, pgx.RowTo[string])
	if err != nil {
		return 0, nil, err
	}
	body := map[string]any{"tags": list}
	cache.set("popular-tags", body, tagsTTL)
	return 200, body, nil
}

// ------------------------------------------------------------------- auth

// authenticate reads `Authorization: Token <jwt>` (or Bearer). A missing
// header is an anonymous request; a bad token is an error.
func authenticate(r *http.Request) (*viewer, error) {
	header := r.Header.Get("authorization")
	if header == "" {
		return nil, nil
	}
	token, ok := strings.CutPrefix(header, "Token ")
	if !ok {
		token, _ = strings.CutPrefix(header, "Bearer ")
	}
	id, ok := verifyToken(token)
	if !ok {
		return nil, errStatus(401, "invalid or expired token")
	}
	return &viewer{id, token}, nil
}

func mac(data string) []byte {
	m := hmac.New(sha256.New, jwtSecret)
	m.Write([]byte(data))
	return m.Sum(nil)
}

// sign makes an HS256 JWT with `sub` (user id) and `exp`.
func sign(userID int64) string {
	header := b64.EncodeToString([]byte(`{"alg":"HS256","typ":"JWT"}`))
	claims, _ := json.Marshal(map[string]int64{"sub": userID, "exp": time.Now().Add(tokenTTL).Unix()})
	input := header + "." + b64.EncodeToString(claims)
	return input + "." + b64.EncodeToString(mac(input))
}

func verifyToken(token string) (int64, bool) {
	i := strings.LastIndexByte(token, '.')
	if i < 0 {
		return 0, false
	}
	input := token[:i]
	signature, err := b64.DecodeString(token[i+1:])
	if err != nil || !hmac.Equal(signature, mac(input)) {
		return 0, false
	}
	_, encoded, ok := strings.Cut(input, ".")
	if !ok {
		return 0, false
	}
	raw, err := b64.DecodeString(encoded)
	if err != nil {
		return 0, false
	}
	var claims struct{ Sub, Exp *int64 }
	if json.Unmarshal(raw, &claims) != nil || claims.Sub == nil || claims.Exp == nil || *claims.Exp < time.Now().Unix() {
		return 0, false
	}
	return *claims.Sub, true
}

// hashPassword: `argon2id$<m>$<t>$<salt>$<hash>`, all base64url.
func hashPassword(password string) string {
	salt := make([]byte, 16)
	rand.Read(salt)
	hash := argon2.IDKey([]byte(password), salt, argon2T, argon2MKiB, 1, 32)
	return fmt.Sprintf("argon2id$%d$%d$%s$%s", argon2MKiB, argon2T, b64.EncodeToString(salt), b64.EncodeToString(hash))
}

func verifyPassword(password, stored string) bool {
	parts := strings.Split(stored, "$")
	if len(parts) != 5 || parts[0] != "argon2id" {
		return false
	}
	m, err1 := strconv.ParseUint(parts[1], 10, 32)
	t, err2 := strconv.ParseUint(parts[2], 10, 32)
	salt, err3 := b64.DecodeString(parts[3])
	expected, err4 := b64.DecodeString(parts[4])
	if err := errors.Join(err1, err2, err3, err4); err != nil {
		return false
	}
	actual := argon2.IDKey([]byte(password), salt, uint32(t), uint32(m), 1, uint32(len(expected)))
	return subtle.ConstantTimeCompare(actual, expected) == 1
}

// ---------------------------------------------------------------- helpers

func slugify(title string) string {
	var parts []string
	for _, part := range strings.FieldsFunc(strings.ToLower(title), func(c rune) bool {
		return !unicode.IsLetter(c) && !unicode.IsNumber(c)
	}) {
		parts = append(parts, part)
	}
	slug := []rune(strings.Join(parts, "-"))
	return string(slug[:min(len(slug), 60)])
}

func randomHex(n int) string {
	b := make([]byte, n)
	rand.Read(b)
	return hex.EncodeToString(b)
}

func envInt(key string, def int) int {
	if n, err := strconv.Atoi(os.Getenv(key)); err == nil {
		return n
	}
	return def
}

// ttlCache is an in-process cache with per-entry expiry, shared by all
// goroutines (like the wasm host's per-app cache).
type ttlCache struct {
	mu      sync.RWMutex
	entries map[string]ttlEntry
}

type ttlEntry struct {
	value   any
	expires time.Time
}

func newTTLCache() *ttlCache { return &ttlCache{entries: map[string]ttlEntry{}} }

func (c *ttlCache) get(key string) (any, bool) {
	c.mu.RLock()
	e, ok := c.entries[key]
	c.mu.RUnlock()
	if !ok || time.Now().After(e.expires) {
		return nil, false
	}
	return e.value, true
}

func (c *ttlCache) set(key string, value any, ttl time.Duration) {
	c.mu.Lock()
	c.entries[key] = ttlEntry{value, time.Now().Add(ttl)}
	c.mu.Unlock()
}
