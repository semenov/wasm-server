//! Outgoing HTTP from apps (`wasi:http/outgoing-handler`).
//!
//! wasmtime's default sends every request on a new connection, to any
//! address: the host's loopback services, the admin endpoints and cloud
//! metadata included (an SSRF hole). This replaces it:
//!
//! - **Where:** an app reaches the public internet, or only the hosts in its
//!   `outbound` list (`api.example.com`, `*.example.com`; an empty list: no
//!   outbound at all). Loopback, private, link-local, metadata and other
//!   non-public addresses are always refused. The check runs after DNS, on
//!   every address the name has, and the connection goes to a checked
//!   address, so a name pointing inside is refused too.
//! - **Connections** are kept per worker and per host and reused
//!   (keep-alive): a new TLS connection costs a round trip or two (about
//!   190 ms to GitHub from here), a reused one nothing.
//! - **Timeouts:** connect 10 s, first byte 30 s, between bytes 30 s, or
//!   less if the guest sets `request-options`; the request's deadline covers
//!   everything.
//! - **Counted** per app: requests, bytes sent (billed as egress) and bytes
//!   received.

use crate::platform::Platform;
use crate::stats::Counter;
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::client::conn::http1::SendRequest;
use hyper::http::{self, Request, Response, uri::Scheme};
use hyper_util::rt::TokioIo;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(30);
const BETWEEN_BYTES_TIMEOUT: Duration = Duration::from_secs(30);
/// An idle connection older than this is closed rather than reused.
const IDLE_MAX: Duration = Duration::from_secs(60);
/// Idle connections kept per worker and host.
const IDLE_PER_HOST: usize = 16;

/// Which hosts an app may call.
#[derive(Debug, Default)]
pub enum Policy {
    /// Any public host.
    #[default]
    Public,
    /// Only these: `api.example.com`, or `*.example.com` for its subdomains.
    Only(Vec<String>),
}

impl Policy {
    /// From an app's `outbound` setting: none means any public host.
    pub fn new(list: Option<&[String]>) -> Policy {
        match list {
            None => Policy::Public,
            Some(hosts) => Policy::Only(hosts.iter().map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase()).collect()),
        }
    }

    fn allows(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        match self {
            Policy::Public => true,
            Policy::Only(hosts) => hosts.iter().any(|h| match h.strip_prefix("*.") {
                Some(base) => host.len() > base.len() + 1 && host.ends_with(base) && host.as_bytes()[host.len() - base.len() - 1] == b'.',
                None => *h == host,
            }),
        }
    }
}

/// Addresses on the public internet; everything an app must not reach is
/// outside: this machine, private networks, link-local (cloud metadata at
/// 169.254.169.254), carrier-grade NAT, multicast, reserved ranges.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            !(a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_multicast()
                || a.is_documentation()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
                || (o[0] == 198 && (o[1] & 0xfe) == 18)) // 198.18.0.0/15
        }
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = a.segments();
            // NAT64 (64:ff9b::/96) carries an IPv4 address
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                return is_public(IpAddr::V4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8)));
            }
            !(a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || a == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254)) // AWS metadata over IPv6
        }
    }
}

/// The `wasi:http` hooks of one instance.
pub struct Hooks {
    pub app: Arc<str>,
    pub policy: Arc<Policy>,
    pub platform: Arc<Platform>,
    pub worker: usize,
    pub app_id: usize,
}

/// Where the counts of one instance's outbound traffic go.
#[derive(Clone)]
struct Meter {
    platform: Arc<Platform>,
    worker: usize,
    app_id: usize,
}

impl Meter {
    // Counters belong to the worker thread; requests and bodies are driven on
    // that worker's (single-threaded) runtime.
    fn add(&self, c: Counter, n: u64) {
        self.platform.stats.counters(self.worker, self.app_id).add(c, n);
    }
}

type Outcome = Result<(Response<WasiBody>, Box<dyn Future<Output = Result<(), Error>> + Send>), Error>;

impl WasiHttpHooks for Hooks {
    fn send_request(
        &mut self,
        request: Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> Box<dyn Future<Output = Outcome> + Send> {
        drop(fut);
        let app = self.app.clone();
        let policy = self.policy.clone();
        let meter = Meter { platform: self.platform.clone(), worker: self.worker, app_id: self.app_id };
        Box::new(async move {
            let res = send(&app, &policy, &meter, request, options).await?;
            Ok((res, Box::new(async { Ok(()) }) as Box<dyn Future<Output = Result<(), Error>> + Send>))
        })
    }
}

async fn send(app: &str, policy: &Policy, meter: &Meter, mut req: Request<WasiBody>, options: Option<RequestOptions>) -> Result<Response<WasiBody>, Error> {
    let uri = req.uri();
    let tls = uri.scheme() == Some(&Scheme::HTTPS);
    let host = uri.host().ok_or(Error::HttpRequestUriInvalid)?.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    if !policy.allows(&host) {
        eprintln!("{app}: outbound request to {host} refused: not in the app's `outbound` list");
        return Err(Error::HttpRequestDenied);
    }
    let pick = |t: Option<Duration>, default: Duration| t.map_or(default, |t| t.min(default));
    let connect_timeout = pick(options.and_then(|o| o.connect_timeout), CONNECT_TIMEOUT);
    let first_byte = pick(options.and_then(|o| o.first_byte_timeout), FIRST_BYTE_TIMEOUT);
    let between_bytes = pick(options.and_then(|o| o.between_bytes_timeout), BETWEEN_BYTES_TIMEOUT).max(Duration::from_millis(1));

    // the request line has only the path: the host is in the Host header
    let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_string();
    *req.uri_mut() = http::Uri::builder().path_and_query(path).build().map_err(|_| Error::HttpRequestUriInvalid)?;
    meter.add(Counter::OutboundRequests, 1);
    let req = req.map(|body| Counted { inner: body, meter: meter.clone(), counter: Counter::OutboundBytesSent }.boxed_unsync());

    let key = format!("{}://{host}:{port}", if tls { "https" } else { "http" });
    let mut sender = match idle_take(&key) {
        Some(s) => s,
        None => connect(app, &host, port, tls, connect_timeout).await?,
    };
    let res = match tokio::time::timeout(first_byte, sender.send_request(req)).await {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => return Err(Error::from(e)),
        Err(_) => return Err(Error::ConnectionReadTimeout),
    };
    let mut timeout = tokio::time::interval(between_bytes);
    timeout.reset();
    let (parts, incoming) = res.into_parts();
    let mut body = ResponseBody { incoming: Some(incoming), timeout, meter: meter.clone(), sender: Some(sender), key };
    // no body at all (204, HEAD): the connection is free now
    if body.incoming.as_ref().is_some_and(|i| i.is_end_stream()) {
        body.incoming = None;
        body.finished();
    }
    Ok(Response::from_parts(parts, body.boxed_unsync()))
}

/// A new connection to a checked address, with TLS when asked.
async fn connect(app: &str, host: &str, port: u16, tls: bool, timeout: Duration) -> Result<SendRequest<WasiBody>, Error> {
    let lookup = tokio::time::timeout(timeout, tokio::net::lookup_host((host, port)));
    let addrs: Vec<SocketAddr> = match lookup.await {
        Ok(Ok(a)) => a.collect(),
        Ok(Err(e)) => return Err(Error::DnsError { rcode: Some(e.to_string()), info_code: None }),
        Err(_) => return Err(Error::DnsTimeout),
    };
    if addrs.is_empty() {
        return Err(Error::DestinationNotFound);
    }
    // every address must be public: a name with one inside is refused
    if let Some(bad) = addrs.iter().find(|a| !is_public(a.ip())) {
        eprintln!("{app}: outbound request to {host} refused: it resolves to {}, which isn't a public address", bad.ip());
        return Err(Error::DestinationIpProhibited);
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = None;
    let mut stream = None;
    for addr in &addrs {
        match tokio::time::timeout_at(deadline, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                stream = Some(s);
                break;
            }
            Ok(Err(e)) => last = Some(e),
            Err(_) => return Err(Error::ConnectionTimeout),
        }
    }
    let Some(stream) = stream else {
        return Err(last.map_or(Error::ConnectionRefused, Error::Connect));
    };
    let _ = stream.set_nodelay(true);
    if tls {
        let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| Error::TlsProtocolError)?;
        let connector = tokio_rustls::TlsConnector::from(tls_config());
        let stream = match tokio::time::timeout_at(deadline, connector.connect(name, stream)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(Error::Tls(e)),
            Err(_) => return Err(Error::ConnectionTimeout),
        };
        handshake(stream, deadline).await
    } else {
        handshake(stream, deadline).await
    }
}

async fn handshake<T>(io: T, deadline: tokio::time::Instant) -> Result<SendRequest<WasiBody>, Error>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let (sender, conn) = match tokio::time::timeout_at(deadline, hyper::client::conn::http1::handshake(TokioIo::new(io))).await {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return Err(Error::from(e)),
        Err(_) => return Err(Error::ConnectionTimeout),
    };
    // the connection lives as long as a sender for it does
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.into() };
            let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(config)
        })
        .clone()
}

// ---- idle connections, per worker thread

thread_local! {
    static IDLE: RefCell<HashMap<String, Vec<(SendRequest<WasiBody>, Instant)>>> = RefCell::new(HashMap::new());
}

fn idle_take(key: &str) -> Option<SendRequest<WasiBody>> {
    IDLE.with(|idle| {
        let mut idle = idle.borrow_mut();
        let list = idle.get_mut(key)?;
        while let Some((sender, since)) = list.pop() {
            if sender.is_ready() && !sender.is_closed() && since.elapsed() < IDLE_MAX {
                return Some(sender);
            }
        }
        None
    })
}

fn idle_put(key: String, sender: SendRequest<WasiBody>) {
    if !sender.is_ready() || sender.is_closed() {
        return;
    }
    IDLE.with(|idle| {
        let mut idle = idle.borrow_mut();
        let list = idle.entry(key).or_default();
        list.retain(|(s, since)| !s.is_closed() && since.elapsed() < IDLE_MAX);
        if list.len() < IDLE_PER_HOST {
            list.push((sender, Instant::now()));
        }
    })
}

// ---- bodies

/// A request body that counts its bytes.
struct Counted {
    inner: WasiBody,
    meter: Meter,
    counter: Counter,
}

impl Body for Counted {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled {
            if let Some(data) = frame.data_ref() {
                self.meter.add(self.counter, data.len() as u64);
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A response body: counts its bytes, times out between them, and gives
/// the connection back for reuse once it has been read to the end.
struct ResponseBody {
    incoming: Option<hyper::body::Incoming>,
    timeout: tokio::time::Interval,
    meter: Meter,
    sender: Option<SendRequest<WasiBody>>,
    key: String,
}

impl ResponseBody {
    fn finished(&mut self) {
        if let Some(sender) = self.sender.take() {
            idle_put(std::mem::take(&mut self.key), sender);
        }
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        let Some(incoming) = self.incoming.as_mut() else {
            return Poll::Ready(None);
        };
        match Pin::new(incoming).poll_frame(cx) {
            Poll::Ready(None) => {
                self.incoming = None;
                self.finished();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                self.sender = None;
                Poll::Ready(Some(Err(if e.is_timeout() { Error::HttpResponseTimeout } else { Error::from(e) })))
            }
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.meter.add(Counter::OutboundBytesReceived, data.len() as u64);
                }
                self.timeout.reset();
                if self.incoming.as_ref().is_some_and(|i| i.is_end_stream()) {
                    self.incoming = None;
                    self.finished();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Pending => {
                if self.timeout.poll_tick(cx).is_ready() {
                    self.sender = None;
                    return Poll::Ready(Some(Err(Error::ConnectionReadTimeout)));
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.incoming.as_ref().is_none_or(|i| i.is_end_stream())
    }

    fn size_hint(&self) -> SizeHint {
        self.incoming.as_ref().map_or(SizeHint::with_exact(0), |i| i.size_hint())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        for ip in ["8.8.8.8", "140.82.112.3", "2606:4700:4700::1111", "::ffff:1.1.1.1"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "255.255.255.255",
            "224.0.0.1", "198.18.0.1", "::1", "::", "fe80::1", "fd00::1", "::ffff:127.0.0.1", "64:ff9b::a9fe:a9fe", "fd00:ec2::254",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn policies() {
        assert!(Policy::new(None).allows("anything.example"));
        let p = Policy::new(Some(&["api.stripe.com".into(), "*.openai.com".into()]));
        assert!(p.allows("api.stripe.com") && p.allows("API.Stripe.com.") && p.allows("api.openai.com"));
        assert!(!p.allows("stripe.com") && !p.allows("openai.com") && !p.allows("evilopenai.com") && !p.allows("api.stripe.com.evil.io"));
        assert!(!Policy::new(Some(&[])).allows("api.stripe.com"));
    }
}
