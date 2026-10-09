// Minimal RealWorld "Conduit" frontend: plain JS, hash routing, talks to the
// wasm API on the same origin under /api.

const view = document.getElementById("view");
const navLinks = document.getElementById("nav-links");
const PAGE_SIZE = 10;

let user = null;
try {
  user = JSON.parse(localStorage.getItem("conduit-user") || "null");
} catch {
  user = null;
}

function saveUser(u) {
  user = u;
  try {
    if (u) localStorage.setItem("conduit-user", JSON.stringify(u));
    else localStorage.removeItem("conduit-user");
  } catch {
    // Storage unavailable (private mode): stay logged in for this page only.
  }
}

async function api(method, path, body) {
  const headers = { "content-type": "application/json" };
  if (user) headers.authorization = `Token ${user.token}`;
  const resp = await fetch(`/api${path}`, { method, headers, body: body && JSON.stringify(body) });
  const data = await resp.json().catch(() => ({}));
  if (resp.status === 401 && user) {
    saveUser(null);
    renderNav();
  }
  if (!resp.ok) {
    const errors = data.errors || { error: [resp.statusText] };
    throw new Error(Object.entries(errors).map(([k, v]) => `${k} ${v.join(", ")}`).join("; "));
  }
  return data;
}

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function date(s) {
  return new Date(s).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

function renderNav() {
  const route = location.hash || "#/";
  const link = (href, label) => `<a href="${href}" class="${route === href ? "active" : ""}">${label}</a>`;
  navLinks.innerHTML = user
    ? link("#/", "Home") + link("#/editor", "New article") + link(`#/profile/${esc(user.username)}`, esc(user.username)) +
      `<a href="#/" id="logout">Sign out</a>`
    : link("#/", "Home") + link("#/login", "Sign in") + link("#/register", "Sign up");
  const logout = document.getElementById("logout");
  if (logout) logout.onclick = () => { saveUser(null); renderNav(); };
}

function previewHtml(a) {
  return `<div class="preview">
    <div class="meta">
      <span><a href="#/profile/${esc(a.author.username)}">${esc(a.author.username)}</a> · ${date(a.createdAt)}</span>
      <button data-fav="${esc(a.slug)}" data-on="${a.favorited}">${a.favorited ? "♥" : "♡"} ${a.favoritesCount}</button>
    </div>
    <a href="#/article/${esc(a.slug)}"><h2>${esc(a.title)}</h2></a>
    <p>${esc(a.description)}</p>
    ${a.tagList.map((t) => `<a class="tag" href="#/?tag=${encodeURIComponent(t)}">${esc(t)}</a>`).join("")}
  </div>`;
}

function wireFavorites(root, reload) {
  root.querySelectorAll("[data-fav]").forEach((b) => {
    b.onclick = async () => {
      if (!user) return (location.hash = "#/login");
      b.disabled = true;
      await api(b.dataset.on === "true" ? "DELETE" : "POST", `/articles/${b.dataset.fav}/favorite`).catch(() => {});
      reload();
    };
  });
}

async function home(params) {
  const tag = params.get("tag");
  const feed = params.get("feed") === "1" && user;
  const page = Number(params.get("page") || 1);
  const tabs = [
    user && `<a href="#/?feed=1" class="${feed ? "active" : ""}">Your feed</a>`,
    `<a href="#/" class="${!feed && !tag ? "active" : ""}">Global feed</a>`,
    tag && `<a class="active">#${esc(tag)}</a>`,
  ].filter(Boolean).join("");
  view.innerHTML = `<div class="layout"><div><div class="tabs">${tabs}</div><div id="list" class="muted">Loading…</div></div>
    <aside class="sidebar"><h3>Popular tags</h3><div id="tags" class="muted">Loading…</div></aside></div>`;

  const query = new URLSearchParams({ limit: PAGE_SIZE, offset: (page - 1) * PAGE_SIZE });
  if (tag) query.set("tag", tag);
  const load = async () => {
    const list = document.getElementById("list");
    try {
      const data = await api("GET", feed ? `/articles/feed?${query}` : `/articles?${query}`);
      const pages = Math.min(Math.ceil(data.articlesCount / PAGE_SIZE), 20);
      const base = feed ? "#/?feed=1&" : tag ? `#/?tag=${encodeURIComponent(tag)}&` : "#/?";
      list.innerHTML = (data.articles.map(previewHtml).join("") || `<p class="muted">No articles here yet.</p>`) +
        `<div class="pager">${Array.from({ length: pages }, (_, i) =>
          `<a href="${base}page=${i + 1}" class="${i + 1 === page ? "active" : ""}">${i + 1}</a>`).join("")}</div>
        <p class="muted">${data.articlesCount.toLocaleString()} articles</p>`;
      wireFavorites(list, load);
    } catch (e) {
      list.innerHTML = `<p class="error">${esc(e.message)}</p>`;
    }
  };
  load();
  api("GET", "/tags").then((d) => {
    document.getElementById("tags").innerHTML = d.tags.map((t) => `<a class="tag" href="#/?tag=${encodeURIComponent(t)}">${esc(t)}</a>`).join("");
  }).catch(() => {});
}

async function article(slug) {
  view.innerHTML = `<p class="muted">Loading…</p>`;
  try {
    const [{ article: a }, { comments }] = await Promise.all([api("GET", `/articles/${slug}`), api("GET", `/articles/${slug}/comments`)]);
    const mine = user && user.username === a.author.username;
    view.innerHTML = `<h1>${esc(a.title)}</h1>
      <div class="meta"><span><a href="#/profile/${esc(a.author.username)}">${esc(a.author.username)}</a> · ${date(a.createdAt)}</span>
        <span>${mine ? `<button id="delete">Delete article</button>` : ""}
        <button data-fav="${esc(a.slug)}" data-on="${a.favorited}">${a.favorited ? "♥ Unfavorite" : "♡ Favorite"} (${a.favoritesCount})</button></span></div>
      <div class="article-body">${esc(a.body)}</div>
      ${a.tagList.map((t) => `<span class="tag">${esc(t)}</span>`).join("")}
      <h3>Comments</h3>
      ${user ? `<form id="comment-form" style="margin:0;max-width:none"><textarea name="body" placeholder="Write a comment…" required style="min-height:80px"></textarea>
        <div><button class="primary">Post comment</button></div></form>` : `<p><a href="#/login">Sign in</a> to comment.</p>`}
      <div id="comments">${comments.map((c) => `<div class="comment"><p>${esc(c.body)}</p>
        <div class="meta"><span><a href="#/profile/${esc(c.author.username)}">${esc(c.author.username)}</a> · ${date(c.createdAt)}</span></div></div>`).join("")}</div>`;
    wireFavorites(view, () => article(slug));
    const del = document.getElementById("delete");
    if (del) del.onclick = async () => { await api("DELETE", `/articles/${slug}`); location.hash = "#/"; };
    const form = document.getElementById("comment-form");
    if (form) form.onsubmit = async (e) => {
      e.preventDefault();
      await api("POST", `/articles/${slug}/comments`, { comment: { body: form.body.value } });
      article(slug);
    };
  } catch (e) {
    view.innerHTML = `<p class="error">${esc(e.message)}</p>`;
  }
}

function authForm(kind) {
  const register = kind === "register";
  view.innerHTML = `<form id="auth"><h1>${register ? "Sign up" : "Sign in"}</h1>
    <p><a href="#/${register ? "login" : "register"}">${register ? "Have an account?" : "Need an account?"}</a></p>
    <p id="auth-error" class="error"></p>
    ${register ? `<input name="username" placeholder="Username" required>` : ""}
    <input name="email" type="email" placeholder="Email" required>
    <input name="password" type="password" placeholder="Password" required>
    <button class="primary">${register ? "Sign up" : "Sign in"}</button></form>`;
  const form = document.getElementById("auth");
  form.onsubmit = async (e) => {
    e.preventDefault();
    const fields = { email: form.email.value, password: form.password.value };
    if (register) fields.username = form.username.value;
    try {
      const data = await api("POST", register ? "/users" : "/users/login", { user: fields });
      saveUser(data.user);
      location.hash = "#/";
    } catch (err) {
      document.getElementById("auth-error").textContent = err.message;
    }
  };
}

function editor() {
  if (!user) return (location.hash = "#/login");
  view.innerHTML = `<form id="editor"><h1>New article</h1><p id="editor-error" class="error"></p>
    <input name="title" placeholder="Title" required>
    <input name="description" placeholder="What's this article about?" required>
    <textarea name="body" placeholder="Write your article" required></textarea>
    <input name="tags" placeholder="Tags, separated by spaces">
    <button class="primary">Publish</button></form>`;
  const form = document.getElementById("editor");
  form.onsubmit = async (e) => {
    e.preventDefault();
    try {
      const { article: a } = await api("POST", "/articles", { article: {
        title: form.title.value, description: form.description.value, body: form.body.value,
        tagList: form.tags.value.split(/\s+/).filter(Boolean),
      } });
      location.hash = `#/article/${a.slug}`;
    } catch (err) {
      document.getElementById("editor-error").textContent = err.message;
    }
  };
}

async function profile(username) {
  view.innerHTML = `<p class="muted">Loading…</p>`;
  try {
    const [{ profile: p }, list] = await Promise.all([
      api("GET", `/profiles/${username}`),
      api("GET", `/articles?author=${encodeURIComponent(username)}&limit=${PAGE_SIZE}`),
    ]);
    const self = user && user.username === p.username;
    view.innerHTML = `<h1>${esc(p.username)}</h1><p class="muted">${esc(p.bio)}</p>
      ${user && !self ? `<button id="follow">${p.following ? "Unfollow" : "Follow"} ${esc(p.username)}</button>` : ""}
      <div class="tabs" style="margin-top:24px"><a class="active">Articles (${list.articlesCount})</a></div>
      <div id="list">${list.articles.map(previewHtml).join("") || `<p class="muted">No articles yet.</p>`}</div>`;
    wireFavorites(view, () => profile(username));
    const follow = document.getElementById("follow");
    if (follow) follow.onclick = async () => {
      await api(p.following ? "DELETE" : "POST", `/profiles/${username}/follow`);
      profile(username);
    };
  } catch (e) {
    view.innerHTML = `<p class="error">${esc(e.message)}</p>`;
  }
}

function route() {
  renderNav();
  const [path, query] = (location.hash.slice(1) || "/").split("?");
  const params = new URLSearchParams(query || "");
  const parts = path.split("/").filter(Boolean).map(decodeURIComponent);
  window.scrollTo(0, 0);
  if (parts[0] === "article") return article(parts[1]);
  if (parts[0] === "profile") return profile(parts[1]);
  if (parts[0] === "login") return authForm("login");
  if (parts[0] === "register") return authForm("register");
  if (parts[0] === "editor") return editor();
  return home(params);
}

window.addEventListener("hashchange", route);
route();
