//! `wasm-server deploy` and `reload`: talk to a running server's
//! `/_platform/` endpoints over HTTP.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use std::path::Path;
use wasmtime::{Result, bail, format_err};

/// Sends one request and returns the status and body.
async fn send(url: &str, method: Method, path: &str, token: Option<&str>, body: Bytes) -> Result<(u16, String)> {
    let uri: hyper::Uri = url.parse().map_err(|e| format_err!("{url}: {e}"))?;
    if uri.scheme_str() != Some("http") {
        bail!("{url}: only http:// is supported (put TLS in a proxy in front)");
    }
    let host = uri.host().ok_or_else(|| format_err!("{url}: no host"))?;
    let port = uri.port_u16().unwrap_or(80);
    let stream = tokio::net::TcpStream::connect((host, port)).await.map_err(|e| format_err!("{host}:{port}: {e}"))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let mut req = Request::builder()
        .method(method)
        .uri(format!("{}{path}", uri.path().trim_end_matches('/')))
        .header("host", uri.authority().map(|a| a.as_str()).unwrap_or(host))
        .header("content-length", body.len());
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = sender.send_request(req.body(Full::new(body))?).await?;
    let status = resp.status().as_u16();
    let text = String::from_utf8_lossy(&resp.into_body().collect().await?.to_bytes()).into_owned();
    Ok((status, text))
}

fn run(url: &str, method: Method, path: &str, token: Option<&str>, body: Bytes) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let (status, text) = rt.block_on(send(url, method, path, token, body))?;
    print!("{text}");
    if status != 200 {
        bail!("server answered {status}");
    }
    Ok(())
}

/// Uploads a component for `app`; the server swaps it in without downtime.
pub fn deploy(url: &str, app: &str, file: &Path, token: Option<&str>) -> Result<()> {
    let bytes = std::fs::read(file).map_err(|e| format_err!("{}: {e}", file.display()))?;
    eprintln!("uploading {} ({} KiB) as {app}", file.display(), bytes.len() >> 10);
    run(url, Method::PUT, &format!("/_platform/apps/{app}"), token, bytes.into())
}

/// Lists an app's saved versions.
pub fn versions(url: &str, app: &str, token: Option<&str>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let (status, text) = rt.block_on(send(url, Method::GET, &format!("/_platform/apps/{app}/versions"), token, Bytes::new()))?;
    if status != 200 {
        print!("{text}");
        bail!("server answered {status}");
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    println!("{:<30} {:>9}  {}", "version", "KiB", "");
    for row in v["versions"].as_array().into_iter().flatten() {
        let current = if row["current"].as_bool() == Some(true) { "← running" } else { "" };
        println!("{:<30} {:>9}  {current}", row["id"].as_str().unwrap_or(""), row["bytes"].as_u64().unwrap_or(0) >> 10);
    }
    Ok(())
}

/// Switches an app back to its previous version, or to `to`.
pub fn rollback(url: &str, app: &str, to: Option<&str>, token: Option<&str>) -> Result<()> {
    let query = to.map(|id| format!("?to={id}")).unwrap_or_default();
    run(url, Method::POST, &format!("/_platform/apps/{app}/rollback{query}"), token, Bytes::new())
}

/// Asks the server to re-read its deployment.
pub fn reload(url: &str, token: Option<&str>) -> Result<()> {
    run(url, Method::POST, "/_platform/reload", token, Bytes::new())
}
