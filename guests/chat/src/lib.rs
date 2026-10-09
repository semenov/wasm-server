//! Chat rooms over WebSockets held by the host.
//!
//! The function has no HTTP handler and keeps nothing between events: the
//! host holds the connections, each event runs in a fresh instance, the
//! room is a channel, who-is-who lives in per-connection state, history in
//! SQLite, and the rate limit in the cache.
//!
//! Client → server: `{"text": "..."}`
//! Server → client: `{"type": "history" | "message" | "presence" | "error", ...}`

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

wit_bindgen::generate!({
    path: "../../wit",
    world: "platform:sdk/websocket-app",
    generate_all,
});

use exports::platform::sdk::websocket_handler::{ConnectInfo, Guest};
use platform::sdk::cache;
use platform::sdk::sql::{self, Value};
use platform::sdk::websocket::{self as ws, Message};

const DB: &str = "chat";
const HISTORY: i64 = 50;
const MAX_TEXT: usize = 500;
/// Messages per second per connection.
const RATE_LIMIT: i64 = 5;

/// Kept with the connection by the host.
#[derive(Serialize, Deserialize)]
struct Member {
    name: String,
    room: String,
}

struct Chat;

impl Guest for Chat {
    fn on_connect(info: ConnectInfo) -> Result<(), String> {
        let name = clean(&param(&info.query, "name").unwrap_or_default(), 24);
        let room = clean(&param(&info.query, "room").unwrap_or_else(|| "general".into()), 32).to_lowercase();
        if name.is_empty() || room.is_empty() {
            return Err("name and room are required".into());
        }
        let member = Member { name: name.clone(), room: room.clone() };
        ws::set_state(&info.conn, &serde_json::to_vec(&member).unwrap()).map_err(|e| format!("{e:?}"))?;
        ws::subscribe(&info.conn, &channel(&room)).map_err(|e| format!("{e:?}"))?;

        let history = sql::query(
            DB,
            "SELECT name, text, at FROM (SELECT id, name, text, at FROM messages WHERE room = $1 ORDER BY id DESC LIMIT $2) ORDER BY id",
            &[Value::Text(room.clone()), Value::Int(HISTORY)],
        )
        .map_err(|e| format!("{e:?}"))?;
        let messages: Vec<Json> = history
            .rows
            .iter()
            .map(|r| json!({"name": text(&r[0]), "text": text(&r[1]), "at": int(&r[2])}))
            .collect();
        send(&info.conn, json!({"type": "history", "room": room, "messages": messages}));
        presence(&room, &name, "joined");
        Ok(())
    }

    fn on_message(conn: String, msg: Message) {
        let Some(member) = member(&conn) else { return };
        let Message::Text(raw) = msg else {
            return send(&conn, json!({"type": "error", "text": "text messages only"}));
        };
        let second = now_ms() / 1000;
        if cache::incr(&format!("rate:{conn}:{second}"), 1, 2_000) > RATE_LIMIT {
            return send(&conn, json!({"type": "error", "text": "slow down"}));
        }
        let Ok(Incoming { text: body }) = serde_json::from_str(&raw) else {
            return send(&conn, json!({"type": "error", "text": "expected {\"text\": \"...\"}"}));
        };
        let body = body.trim();
        if body.is_empty() {
            return;
        }
        let body: String = body.chars().take(MAX_TEXT).collect();
        let at = now_ms();
        if let Err(e) = sql::execute(
            DB,
            "INSERT INTO messages (room, name, text, at) VALUES ($1, $2, $3, $4)",
            &[Value::Text(member.room.clone()), Value::Text(member.name.clone()), Value::Text(body.clone()), Value::Int(at)],
        ) {
            return send(&conn, json!({"type": "error", "text": format!("not saved: {e:?}")}));
        }
        let out = json!({"type": "message", "name": member.name, "text": body, "at": at});
        let _ = ws::publish(&channel(&member.room), &Message::Text(out.to_string()));
    }

    fn on_close(conn: String, _code: u16) {
        // The host already took the connection out of its channels.
        if let Some(member) = member(&conn) {
            presence(&member.room, &member.name, "left");
        }
    }
}

#[derive(Deserialize)]
struct Incoming {
    text: String,
}

fn channel(room: &str) -> String {
    format!("room:{room}")
}

fn member(conn: &str) -> Option<Member> {
    serde_json::from_slice(&ws::get_state(conn)?).ok()
}

fn presence(room: &str, name: &str, action: &str) {
    let online = ws::subscribers(&channel(room));
    let msg = json!({"type": "presence", "name": name, "action": action, "online": online});
    let _ = ws::publish(&channel(room), &Message::Text(msg.to_string()));
}

fn send(conn: &str, msg: Json) {
    let _ = ws::send(conn, &Message::Text(msg.to_string()));
}

/// Printable characters only, trimmed and shortened.
fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(max).collect()
}

fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(hi), Some(lo)) => {
                out.push(hi << 4 | lo);
                i += 3;
            }
            (b'+', _, _) => {
                out.push(b' ');
                i += 1;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        _ => String::new(),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => 0,
    }
}

fn now_ms() -> i64 {
    let now = wasip2::clocks::wall_clock::now();
    now.seconds as i64 * 1000 + (now.nanoseconds / 1_000_000) as i64
}

export!(Chat);
