//! WebSockets held by the host (`platform:sdk/websocket`).
//!
//! The server does the upgrade and keeps the socket; the app runs only for
//! events: `on-connect` (before the 101, so it can refuse), `on-message`
//! (one at a time per connection, in order) and `on-close`. An idle
//! connection is a socket, a small buffer and a task, with no wasm instance.
//! Functions talk back through the hub: send to a connection, publish to a
//! channel, keep a little state per connection.

use crate::app::{App, RequestState};
use crate::bindings::websocket::{self as wit, Error, Message};
use crate::bindings::websocket_handler::ConnectInfo;
use crate::db::Pools;
use crate::platform::{self, Env, Platform};
use crate::stats::Counter;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, handshake::derive_accept_key};
use wasmtime::Result;
use wasmtime::component::{HasSelf, Linker};
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

/// Largest message in either direction.
pub const MAX_MESSAGE: usize = 64 << 10;
/// Messages waiting to be written to one client; past this it's dropped.
const SEND_BUFFER: usize = 256;
const MAX_STATE: usize = 4 << 10;
const PING_EVERY: Duration = Duration::from_secs(30);

enum Outgoing {
    Message(tungstenite::Message),
    Close(u16, String),
}

struct Conn {
    app: usize,
    tx: mpsc::Sender<Outgoing>,
    state: Mutex<Option<Vec<u8>>>,
    channels: Mutex<HashSet<String>>,
}

/// All open connections and channels on this server.
#[derive(Default)]
pub struct Hub {
    conns: Mutex<HashMap<String, Arc<Conn>>>,
    /// (app, channel) → connection ids.
    channels: Mutex<HashMap<(usize, String), HashSet<String>>>,
}

fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let random = || std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", random(), random())
}

fn to_tungstenite(msg: Message) -> tungstenite::Message {
    match msg {
        Message::Text(t) => tungstenite::Message::Text(t.into()),
        Message::Binary(b) => tungstenite::Message::Binary(b.into()),
    }
}

fn size(msg: &Message) -> usize {
    match msg {
        Message::Text(t) => t.len(),
        Message::Binary(b) => b.len(),
    }
}

impl Hub {
    fn register(&self, app: usize) -> (String, mpsc::Receiver<Outgoing>) {
        let (tx, rx) = mpsc::channel(SEND_BUFFER);
        let id = new_id();
        let conn = Conn { app, tx, state: Mutex::new(None), channels: Mutex::default() };
        self.conns.lock().unwrap().insert(id.clone(), Arc::new(conn));
        (id, rx)
    }

    fn remove(&self, id: &str) {
        self.leave_channels(id);
        self.conns.lock().unwrap().remove(id);
    }

    /// Takes a closing connection out of its channels but keeps its state,
    /// so `on-close` can still read it.
    fn leave_channels(&self, id: &str) {
        let Some(conn) = self.conns.lock().unwrap().get(id).cloned() else { return };
        let joined: Vec<String> = conn.channels.lock().unwrap().drain().collect();
        let mut channels = self.channels.lock().unwrap();
        for channel in joined {
            let key = (conn.app, channel);
            if let Some(members) = channels.get_mut(&key) {
                members.remove(id);
                if members.is_empty() {
                    channels.remove(&key);
                }
            }
        }
    }

    fn get_any(&self, id: &str) -> Option<Arc<Conn>> {
        self.conns.lock().unwrap().get(id).cloned()
    }

    fn get(&self, app: usize, id: &str) -> Result<Arc<Conn>, Error> {
        match self.conns.lock().unwrap().get(id) {
            Some(conn) if conn.app == app => Ok(conn.clone()),
            _ => Err(Error::NoSuchConnection),
        }
    }

    /// Queues `msg` for one connection; a client that can't keep up is
    /// disconnected rather than allowed to grow the server's memory.
    fn deliver(conn: &Conn, msg: tungstenite::Message) -> Result<(), Error> {
        match conn.tx.try_send(Outgoing::Message(msg)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                let _ = conn.tx.try_send(Outgoing::Close(1013, "too slow".into()));
                Err(Error::Backpressure)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::NoSuchConnection),
        }
    }

    /// Open connections per app, for billing.
    pub fn open_by_app(&self) -> HashMap<usize, u64> {
        let mut out = HashMap::new();
        for conn in self.conns.lock().unwrap().values() {
            *out.entry(conn.app).or_default() += 1;
        }
        out
    }
}

pub fn add_to_linker(linker: &mut Linker<RequestState>) -> Result<()> {
    wit::add_to_linker::<RequestState, HasSelf<RequestState>>(linker, |s| s)
}

impl wit::Host for RequestState {
    fn send(&mut self, conn: String, msg: Message) -> Result<Result<(), Error>> {
        if size(&msg) > MAX_MESSAGE {
            return Ok(Err(Error::TooLarge(size(&msg) as u64)));
        }
        let hub = &self.env.platform.hub;
        let result = hub.get(self.app_id, &conn).and_then(|c| {
            let bytes = size(&msg) as u64;
            Hub::deliver(&c, to_tungstenite(msg))?;
            let counters = self.counters();
            counters.add(Counter::WsMessagesOut, 1);
            counters.add(Counter::EgressBytes, bytes);
            Ok(())
        });
        Ok(result)
    }

    fn close(&mut self, conn: String, code: u16, reason: String) -> Result<()> {
        if let Ok(c) = self.env.platform.hub.get(self.app_id, &conn) {
            let _ = c.tx.try_send(Outgoing::Close(code, reason.chars().take(120).collect()));
        }
        Ok(())
    }

    fn subscribe(&mut self, conn: String, channel: String) -> Result<Result<(), Error>> {
        let hub = &self.env.platform.hub;
        let c = match hub.get(self.app_id, &conn) {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        c.channels.lock().unwrap().insert(channel.clone());
        hub.channels.lock().unwrap().entry((self.app_id, channel)).or_default().insert(conn);
        Ok(Ok(()))
    }

    fn unsubscribe(&mut self, conn: String, channel: String) -> Result<()> {
        let hub = &self.env.platform.hub;
        if let Ok(c) = hub.get(self.app_id, &conn) {
            c.channels.lock().unwrap().remove(&channel);
        }
        let mut channels = hub.channels.lock().unwrap();
        let key = (self.app_id, channel);
        if let Some(members) = channels.get_mut(&key) {
            members.remove(&conn);
            if members.is_empty() {
                channels.remove(&key);
            }
        }
        Ok(())
    }

    fn publish(&mut self, channel: String, msg: Message) -> Result<Result<u32, Error>> {
        if size(&msg) > MAX_MESSAGE {
            return Ok(Err(Error::TooLarge(size(&msg) as u64)));
        }
        let hub = &self.env.platform.hub;
        let members: Vec<String> = hub.channels.lock().unwrap().get(&(self.app_id, channel)).map(|m| m.iter().cloned().collect()).unwrap_or_default();
        let msg = to_tungstenite(msg);
        let bytes = msg.len() as u64;
        let mut delivered = 0u32;
        for id in members {
            if let Ok(conn) = hub.get(self.app_id, &id) {
                if Hub::deliver(&conn, msg.clone()).is_ok() {
                    delivered += 1;
                }
            }
        }
        let counters = self.counters();
        counters.add(Counter::WsMessagesOut, delivered as u64);
        counters.add(Counter::EgressBytes, bytes * delivered as u64);
        Ok(Ok(delivered))
    }

    fn subscribers(&mut self, channel: String) -> Result<u32> {
        let channels = self.env.platform.hub.channels.lock().unwrap();
        Ok(channels.get(&(self.app_id, channel)).map_or(0, |m| m.len() as u32))
    }

    fn set_state(&mut self, conn: String, state: Vec<u8>) -> Result<Result<(), Error>> {
        if state.len() > MAX_STATE {
            return Ok(Err(Error::TooLarge(state.len() as u64)));
        }
        Ok(self.env.platform.hub.get(self.app_id, &conn).map(|c| *c.state.lock().unwrap() = Some(state)))
    }

    fn get_state(&mut self, conn: String) -> Result<Option<Vec<u8>>> {
        Ok(self.env.platform.hub.get(self.app_id, &conn).ok().and_then(|c| c.state.lock().unwrap().clone()))
    }
}

pub fn is_upgrade(req: &Request<Incoming>) -> bool {
    let has = |name, value: &str| {
        req.headers().get_all(name).iter().any(|v| v.to_str().is_ok_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(value))))
    };
    has(hyper::header::CONNECTION, "upgrade") && has(hyper::header::UPGRADE, "websocket")
}

/// Handles an upgrade request: asks the app (`on-connect`), then switches
/// protocols and runs the connection in the background.
pub async fn accept(platform: Arc<Platform>, worker: usize, pools: Arc<Pools>, app: Arc<App>, mut req: Request<Incoming>) -> Response<HyperOutgoingBody> {
    let reply = |status: StatusCode, text: &str| {
        let mut resp = Response::new(crate::server::body(Bytes::from(text.to_string())));
        *resp.status_mut() = status;
        resp
    };
    let Some(key) = req.headers().get("sec-websocket-key").map(|k| derive_accept_key(k.as_bytes())) else {
        return reply(StatusCode::BAD_REQUEST, "missing Sec-WebSocket-Key\n");
    };
    if req.headers().get("sec-websocket-version").is_none_or(|v| v != "13") {
        return reply(StatusCode::BAD_REQUEST, "unsupported WebSocket version\n");
    }

    let (id, rx) = platform.hub.register(app.id);
    let info = ConnectInfo {
        conn: id.clone(),
        path: req.uri().path().to_string(),
        query: req.uri().query().unwrap_or("").to_string(),
        headers: req
            .headers()
            .iter()
            .take(64)
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect(),
    };
    let counters = platform.stats.counters(worker, app.id);
    counters.request();
    counters.add(Counter::WsEvents, 1);
    let env = Env::new(platform.clone(), worker, pools.clone());
    match platform::ws_connect(env, &app, info).await {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            platform.hub.remove(&id);
            return reply(StatusCode::FORBIDDEN, &format!("{reason}\n"));
        }
        Err(e) => {
            platform.hub.remove(&id);
            counters.error();
            eprintln!("{}: on-connect: {e:?}", app.name);
            return reply(StatusCode::INTERNAL_SERVER_ERROR, "guest error\n");
        }
    }

    let upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        match upgrade.await {
            Ok(upgraded) => run(platform, worker, pools, app.name.clone(), id, rx, upgraded).await,
            Err(_) => platform.hub.remove(&id),
        }
    });
    let mut resp = reply(StatusCode::SWITCHING_PROTOCOLS, "");
    let h = resp.headers_mut();
    h.insert(hyper::header::CONNECTION, "Upgrade".parse().unwrap());
    h.insert(hyper::header::UPGRADE, "websocket".parse().unwrap());
    h.insert("sec-websocket-accept", key.parse().unwrap());
    resp
}

async fn run(
    platform: Arc<Platform>,
    worker: usize,
    pools: Arc<Pools>,
    app_name: String,
    id: String,
    mut rx: mpsc::Receiver<Outgoing>,
    upgraded: hyper::upgrade::Upgraded,
) {
    // The app is looked up for every event, so a deploy takes effect on open
    // connections without reconnecting.
    let current_app = || platform.current().routes.app(&app_name).cloned();
    let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
    let ws = WebSocketStream::from_raw_socket(hyper_util::rt::TokioIo::new(upgraded), Role::Server, Some(config)).await;
    let (mut sink, mut stream) = ws.split();

    // Writer: messages from functions, plus keep-alive pings.
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(Outgoing::Message(msg)) => {
                        if sink.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Some(Outgoing::Close(code, reason)) => {
                        let frame = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
                        let _ = sink.send(tungstenite::Message::Close(Some(frame))).await;
                        break;
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    if sink.send(tungstenite::Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Reader: one `on-message` at a time, so a connection's messages are
    // handled in order.
    let mut code = 1005; // no status received
    while let Some(frame) = stream.next().await {
        let Some(app) = current_app() else {
            // The app was removed from the deployment.
            let _ = platform.hub.get_any(&id).map(|c| c.tx.try_send(Outgoing::Close(1001, "app removed".into())));
            break;
        };
        let counters = platform.stats.counters(worker, app.id);
        let msg = match frame {
            Ok(tungstenite::Message::Text(t)) => Message::Text(t.to_string()),
            Ok(tungstenite::Message::Binary(b)) => Message::Binary(b.to_vec()),
            Ok(tungstenite::Message::Close(frame)) => {
                code = frame.map_or(1005, |f| u16::from(f.code));
                break;
            }
            Ok(_) => continue, // ping/pong: answered by tungstenite
            Err(_) => {
                code = 1006; // abnormal closure
                break;
            }
        };
        counters.request();
        counters.add(Counter::WsEvents, 1);
        counters.add(Counter::WsMessagesIn, 1);
        if platform.billing.is_blocked(app.id) {
            let _ = platform.hub.get(app.id, &id).map(|c| c.tx.try_send(Outgoing::Close(1008, "out of credits".into())));
            continue;
        }
        let env = Env::new(platform.clone(), worker, pools.clone());
        if let Err(e) = platform::ws_message(env, &app, id.clone(), msg).await {
            counters.error();
            eprintln!("{}: on-message: {e:?}", app.name);
        }
    }

    platform.hub.leave_channels(&id);
    writer.abort();
    if let Some(app) = current_app() {
        let counters = platform.stats.counters(worker, app.id);
        counters.request();
        counters.add(Counter::WsEvents, 1);
        let env = Env::new(platform.clone(), worker, pools);
        if let Err(e) = platform::ws_close(env, &app, id.clone(), code).await {
            counters.error();
            eprintln!("{}: on-close: {e:?}", app.name);
        }
    }
    platform.hub.remove(&id);
}
