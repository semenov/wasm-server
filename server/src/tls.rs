//! HTTPS with certificates from Let's Encrypt (`--https`).
//!
//! Every domain in the deployment (apps' and sites' `domains`) gets a
//! certificate through ACME's HTTP-01 challenge: Let's Encrypt asks for
//! `http://DOMAIN/.well-known/acme-challenge/TOKEN`, which the server's port
//! 80 answers; everything else on port 80 is redirected to https. A domain
//! added by a reload gets its certificate within a minute; certificates are
//! renewed 30 days before they expire. They live in `<data-dir>/certs`
//! (with the ACME account), so a restart reuses them.
//!
//! A failed request is retried after 5 minutes, then less often (up to every
//! 6 hours), which stays well inside Let's Encrypt's rate limits. Until a
//! domain has its certificate, TLS handshakes for it fail.

use crate::platform::Platform;
use arc_swap::ArcSwap;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use instant_acme::{Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder, OrderStatus, RetryPolicy};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wasmtime::{Result, format_err};
use wasmtime_wasi_http::io::TokioIo;

/// Renew when a certificate has less than this left.
const RENEW_BEFORE: u64 = 30 * 24 * 3600;
/// How often to look for new domains and certificates due for renewal.
const CHECK_EVERY: Duration = Duration::from_secs(60);

pub struct TlsOpts {
    /// ACME directory URL (Let's Encrypt's production one by default).
    pub directory: String,
    /// Contact for Let's Encrypt's expiry and policy emails.
    pub email: Option<String>,
    /// A root certificate for the ACME server's own TLS (a test CA such as
    /// Pebble's); the public roots otherwise.
    pub ca_file: Option<PathBuf>,
    /// Where port 80 is served: challenges and redirects.
    pub http_listen: SocketAddr,
    /// The HTTPS port, for redirects.
    pub https_port: u16,
    /// The server's own domain, certified besides the apps'.
    pub server_name: Option<String>,
}

struct Cert {
    key: Arc<CertifiedKey>,
    /// Unix seconds.
    not_after: u64,
}

/// The certificates by domain, and the challenges being answered.
pub struct Certs {
    certs: ArcSwap<HashMap<String, Arc<Cert>>>,
    /// HTTP-01 token → key authorization.
    challenges: Mutex<HashMap<String, String>>,
    dir: PathBuf,
}

impl std::fmt::Debug for Certs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Certs")
    }
}

impl ResolvesServerCert for Certs {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        self.certs.load().get(&name).map(|c| c.key.clone())
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Domains a public CA can certify: not IP addresses, not `localhost`.
fn certifiable(domain: &str) -> bool {
    domain.contains('.') && domain.parse::<std::net::IpAddr>().is_err() && !domain.ends_with(".localhost") && !domain.ends_with(".local")
}

fn load_cert(chain_pem: &[u8], key_pem: &[u8]) -> Result<Cert> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem).collect::<Result<_, _>>().map_err(|e| format_err!("certificate: {e}"))?;
    let first = chain.first().ok_or_else(|| format_err!("no certificate in the file"))?;
    let (_, parsed) = x509_parser::parse_x509_certificate(first).map_err(|e| format_err!("certificate: {e}"))?;
    let not_after = parsed.validity().not_after.timestamp().max(0) as u64;
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format_err!("private key: {e}"))?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key).map_err(|e| format_err!("private key: {e}"))?;
    Ok(Cert { key: Arc::new(CertifiedKey::new(chain, signing)), not_after })
}

impl Certs {
    fn paths(&self, domain: &str) -> (PathBuf, PathBuf) {
        (self.dir.join(format!("{domain}.crt")), self.dir.join(format!("{domain}.key")))
    }

    fn from_disk(&self, domain: &str) -> Option<Cert> {
        let (crt, key) = self.paths(domain);
        let (chain, key) = (std::fs::read(crt).ok()?, std::fs::read(key).ok()?);
        load_cert(&chain, &key).map_err(|e| eprintln!("https: {domain}: the stored certificate is unusable: {e}")).ok()
    }

    fn install(&self, domain: &str, cert: Cert) {
        let mut map = (**self.certs.load()).clone();
        map.insert(domain.to_string(), Arc::new(cert));
        self.certs.store(Arc::new(map));
    }

    /// The answer to an HTTP-01 challenge, if one is open.
    pub fn challenge(&self, token: &str) -> Option<String> {
        self.challenges.lock().unwrap().get(token).cloned()
    }
}

/// Starts certificate management and the port 80 server; the returned
/// config serves HTTPS with whatever certificates there are at the moment.
pub fn start(platform: Arc<Platform>, opts: TlsOpts) -> Result<Arc<rustls::ServerConfig>> {
    let dir = platform.data_dir.join("certs");
    std::fs::create_dir_all(&dir).map_err(|e| format_err!("{}: {e}", dir.display()))?;
    let certs = Arc::new(Certs { certs: ArcSwap::default(), challenges: Mutex::default(), dir });
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(certs.clone());
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let http = std::net::TcpListener::bind(opts.http_listen).map_err(|e| format_err!("binding {} for https: {e}", opts.http_listen))?;
    http.set_nonblocking(true)?;
    eprintln!("https: certificates from {}; port 80 on http://{}", opts.directory, opts.http_listen);
    let (c, port) = (certs.clone(), opts.https_port);
    std::thread::Builder::new().name("http-80".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(serve_port_80(http, c, port));
    })?;
    std::thread::Builder::new().name("acme".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(manage(platform, certs, opts));
    })?;
    Ok(Arc::new(config))
}

/// Port 80: ACME challenges, and a redirect to https for everything else.
async fn serve_port_80(listener: std::net::TcpListener, certs: Arc<Certs>, https_port: u16) {
    let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let certs = certs.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                let certs = certs.clone();
                async move { Ok::<_, std::convert::Infallible>(port_80(&certs, https_port, req)) }
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
        });
    }
}

fn port_80(certs: &Certs, https_port: u16, req: Request<Incoming>) -> Response<String> {
    let path = req.uri().path();
    if let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
        return match certs.challenge(token) {
            Some(answer) => Response::new(answer),
            None => Response::builder().status(StatusCode::NOT_FOUND).body(String::new()).unwrap(),
        };
    }
    let host = req.headers().get(hyper::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let host = host.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map_or(host, |(h, _)| h);
    let port = if https_port == 443 { String::new() } else { format!(":{https_port}") };
    let target = format!("https://{host}{port}{}", req.uri().path_and_query().map_or("/", |p| p.as_str()));
    Response::builder().status(StatusCode::PERMANENT_REDIRECT).header(hyper::header::LOCATION, target).body(String::new()).unwrap()
}

/// Keeps every domain of the deployment certified.
async fn manage(platform: Arc<Platform>, certs: Arc<Certs>, opts: TlsOpts) {
    let mut account: Option<Account> = None;
    // domain → (when to try again, how long to wait after the next failure)
    let mut backoff: HashMap<String, (u64, u64)> = HashMap::new();
    loop {
        let deployment = &platform.current().deployment;
        let mut domains: Vec<String> = deployment.apps.values().flat_map(|a| a.domains.iter()).chain(deployment.sites.values().flat_map(|s| s.domains.iter())).map(|d| d.to_ascii_lowercase()).collect();
        domains.extend(opts.server_name.clone());
        domains.sort();
        domains.dedup();
        for domain in domains.iter().filter(|d| certifiable(d)) {
            let due = |c: &Cert| c.not_after <= now() + RENEW_BEFORE;
            if certs.certs.load().get(domain).is_some_and(|c| !due(c)) {
                continue;
            }
            if let Some(cert) = certs.from_disk(domain) {
                if !due(&cert) {
                    certs.install(domain, cert);
                    continue;
                }
                certs.install(domain, cert); // still good meanwhile
            }
            if backoff.get(domain).is_some_and(|(at, _)| now() < *at) {
                continue;
            }
            if account.is_none() {
                match open_account(&certs.dir, &opts).await {
                    Ok(a) => account = Some(a),
                    Err(e) => {
                        eprintln!("https: can't use the ACME account at {}: {e}", opts.directory);
                        break;
                    }
                }
            }
            eprintln!("https: {domain}: requesting a certificate");
            match issue(account.as_ref().unwrap(), &certs, domain).await {
                Ok(()) => {
                    backoff.remove(domain);
                    eprintln!("https: {domain}: certificate installed");
                }
                Err(e) => {
                    let wait = backoff.get(domain).map_or(300, |(_, w)| (*w * 2).min(6 * 3600));
                    backoff.insert(domain.clone(), (now() + wait, wait));
                    eprintln!("https: {domain}: no certificate: {e} (next try in {} min)", wait / 60);
                }
            }
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

/// The ACME account from `certs/account-*.json`, or a new one.
async fn open_account(dir: &Path, opts: &TlsOpts) -> Result<Account> {
    let builder = || -> Result<_> {
        Ok(match &opts.ca_file {
            Some(ca) => Account::builder_with_root(ca)?,
            None => Account::builder()?,
        })
    };
    // one account per ACME server
    let tag: String = opts.directory.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let file = dir.join(format!("account-{tag}.json"));
    if let Ok(text) = std::fs::read_to_string(&file) {
        let creds: AccountCredentials = serde_json::from_str(&text)?;
        return Ok(builder()?.from_credentials(creds).await?);
    }
    let contact = opts.email.as_ref().map(|e| format!("mailto:{e}"));
    let contacts: Vec<&str> = contact.iter().map(|c| c.as_str()).collect();
    let (account, creds) = builder()?
        .create(&NewAccount { contact: &contacts, terms_of_service_agreed: true, only_return_existing: false }, opts.directory.clone(), None)
        .await?;
    write_private(&file, serde_json::to_string_pretty(&creds)?.as_bytes())?;
    Ok(account)
}

/// One certificate through HTTP-01, written to disk and installed.
async fn issue(account: &Account, certs: &Certs, domain: &str) -> Result<()> {
    let ids = [Identifier::Dns(domain.to_string())];
    let mut order = account.new_order(&NewOrder::new(&ids)).await?;
    let mut tokens = vec![];
    let mut auths = order.authorizations();
    while let Some(auth) = auths.next().await {
        let mut auth = auth?;
        if auth.status == AuthorizationStatus::Valid {
            continue;
        }
        let mut challenge = auth.challenge(ChallengeType::Http01).ok_or_else(|| format_err!("the CA offers no HTTP-01 challenge"))?;
        let answer = challenge.key_authorization().as_str().to_string();
        certs.challenges.lock().unwrap().insert(challenge.token.clone(), answer);
        tokens.push(challenge.token.clone());
        challenge.set_ready().await?;
    }
    let ready = order.poll_ready(&RetryPolicy::new().timeout(Duration::from_secs(90))).await;
    {
        let mut open = certs.challenges.lock().unwrap();
        for t in &tokens {
            open.remove(t);
        }
    }
    let status = ready?;
    if status != OrderStatus::Ready {
        let why = order.state().error.as_ref().map(|p| p.to_string()).unwrap_or_else(|| format!("{status:?}"));
        return Err(format_err!("the CA didn't validate {domain} (is port 80 reachable at this name?): {why}"));
    }
    let key_pem = order.finalize().await?;
    let chain_pem = order.poll_certificate(&RetryPolicy::new().timeout(Duration::from_secs(90))).await?;
    let cert = load_cert(chain_pem.as_bytes(), key_pem.as_bytes())?;
    let (crt, key) = certs.paths(domain);
    write_private(&key, key_pem.as_bytes())?;
    std::fs::write(&crt, chain_pem.as_bytes())?;
    certs.install(domain, cert);
    Ok(())
}

/// Writes a secret (a key, account credentials) readable by the owner only.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `--acme-directory`: a URL, or `staging` / `production` for Let's Encrypt's.
pub fn directory_url(arg: Option<&str>) -> String {
    match arg {
        None | Some("production") => LetsEncrypt::Production.url().to_string(),
        Some("staging") => LetsEncrypt::Staging.url().to_string(),
        Some(url) => url.to_string(),
    }
}
