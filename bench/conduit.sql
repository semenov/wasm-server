-- RealWorld "Conduit" schema and seed data for the `conduit` guest app.
-- Runs as a Postgres init script; creates its own database.
CREATE DATABASE conduit;
\connect conduit

CREATE TABLE users (
    id            bigserial PRIMARY KEY,
    username      text NOT NULL UNIQUE,
    email         text NOT NULL UNIQUE,
    password_hash text NOT NULL,
    bio           text NOT NULL DEFAULT '',
    image         text
);

CREATE TABLE follows (
    follower_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    followee_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    PRIMARY KEY (follower_id, followee_id)
);
CREATE INDEX follows_followee ON follows (followee_id);

CREATE TABLE articles (
    id              bigserial   PRIMARY KEY,
    slug            text        NOT NULL UNIQUE,
    title           text        NOT NULL,
    description     text        NOT NULL,
    body            text        NOT NULL,
    author_id       bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    favorites_count bigint      NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX articles_created ON articles (created_at DESC);
CREATE INDEX articles_author_created ON articles (author_id, created_at DESC);

CREATE TABLE article_tags (
    article_id bigint NOT NULL REFERENCES articles (id) ON DELETE CASCADE,
    tag        text   NOT NULL,
    PRIMARY KEY (article_id, tag)
);
CREATE INDEX article_tags_tag ON article_tags (tag, article_id);

CREATE TABLE favorites (
    user_id    bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    article_id bigint NOT NULL REFERENCES articles (id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, article_id)
);
CREATE INDEX favorites_article ON favorites (article_id);

CREATE TABLE comments (
    id         bigserial   PRIMARY KEY,
    article_id bigint      NOT NULL REFERENCES articles (id) ON DELETE CASCADE,
    author_id  bigint      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    body       text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX comments_article ON comments (article_id, id);
CREATE INDEX comments_author ON comments (author_id);

-- Seed: 10,000 users (they can't log in; load tests register their own),
-- 20 follows each, 50,000 articles (article-N, newest first), 2 of 50 tags
-- per article, 4 favorites and 3 comments per article.
INSERT INTO users (id, username, email, password_hash, bio)
SELECT i, 'user' || i, 'user' || i || '@example.com', 'seed-no-login', 'Bio of user ' || i
FROM generate_series(1, 10000) AS i;
SELECT setval('users_id_seq', 10000);

INSERT INTO follows
SELECT u, 1 + (u * 7919 + k * 104729) % 10000
FROM generate_series(1, 10000) AS u, generate_series(1, 20) AS k
WHERE 1 + (u * 7919 + k * 104729) % 10000 <> u
ON CONFLICT DO NOTHING;

INSERT INTO articles (id, slug, title, description, body, author_id, created_at, updated_at)
SELECT i, 'article-' || i, 'Article ' || i, 'Description of article ' || i,
       repeat('Lorem ipsum dolor sit amet, consectetur adipiscing elit. ', 10 + i % 30),
       1 + (i * 31) % 10000, now() - i * interval '1 minute', now() - i * interval '1 minute'
FROM generate_series(1, 50000) AS i;
SELECT setval('articles_id_seq', 50000);

INSERT INTO article_tags
SELECT i, 'tag' || (i % 50) FROM generate_series(1, 50000) AS i
UNION
SELECT i, 'tag' || ((i * 7 + 3) % 50) FROM generate_series(1, 50000) AS i;

INSERT INTO favorites
SELECT 1 + (k * 7907 + a * 13) % 10000, a
FROM generate_series(1, 50000) AS a, generate_series(1, 4) AS k
ON CONFLICT DO NOTHING;
UPDATE articles a SET favorites_count = f.n
FROM (SELECT article_id, count(*) AS n FROM favorites GROUP BY article_id) f
WHERE f.article_id = a.id;

INSERT INTO comments (article_id, author_id, body)
SELECT a, 1 + (a * 13 + k * 101) % 10000, 'Comment ' || k || ' on article ' || a
FROM generate_series(1, 50000) AS a, generate_series(1, 3) AS k;

ANALYZE;
