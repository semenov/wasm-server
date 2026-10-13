//! Adds and removes cluster workers by load (`--autoscale PROVIDER` on a
//! primary with `--cluster-listen`).
//!
//! - **Load** is the CPU the cluster's processes use (the primary's and
//!   every worker's) as a share of all their cores, sampled every 15 s.
//! - **Up:** at `--autoscale-high` (70%) or more for 2 minutes, a worker is
//!   added, up to `--autoscale-max`; not while a new one hasn't joined yet.
//! - **Down:** under `--autoscale-low` (30%) for 10 minutes, the newest
//!   worker it added is drained (no new requests, the running ones finish)
//!   and removed, down to `--autoscale-min`.
//! - After either, it waits 3 minutes before deciding again.
//!
//! Providers: `local` starts worker processes on this machine (for trying
//! it out and for tests); `digitalocean` creates droplets that download this
//! server's binary from the primary and join it, and deletes them.

use crate::cluster::{CpuGauge, Primary};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasmtime::{Result, bail, format_err};

const TICK: Duration = Duration::from_secs(15);
const UP_AFTER: Duration = Duration::from_secs(120);
const DOWN_AFTER: Duration = Duration::from_secs(600);
const COOL_DOWN: Duration = Duration::from_secs(180);
/// A worker that hasn't joined this long after being created is given up on.
const JOIN_WITHIN: Duration = Duration::from_secs(300);

pub struct AutoscaleOpts {
    pub provider: String,
    pub min: usize,
    pub max: usize,
    pub high: f64,
    pub low: f64,
    /// The primary's cluster address as workers reach it.
    pub join: String,
    pub token: String,
    pub data_dir: PathBuf,
    /// digitalocean: droplet size and region.
    pub size: String,
    pub region: String,
    /// Shorter waits, for tests: up after 2 samples, down after 4, no cool-down.
    pub fast: bool,
}

/// A worker the autoscaler created.
struct Made {
    /// The provider's handle: a process id, a droplet id.
    handle: String,
    /// The start of the worker's node id (its host name).
    name: String,
    created: Instant,
}

trait Provider: Send {
    fn create(&mut self, name: &str) -> Result<String>;
    fn destroy(&mut self, handle: &str) -> Result<()>;
}

// ---------------------------------------------------------------- local

/// Worker processes on this machine, on ports from 18100.
struct Local {
    join: String,
    token: String,
    dir: PathBuf,
    next_port: u16,
    children: Vec<(String, Child)>,
}

impl Provider for Local {
    fn create(&mut self, name: &str) -> Result<String> {
        let port = self.next_port;
        self.next_port += 1;
        let dir = self.dir.join(name);
        std::fs::create_dir_all(&dir)?;
        let log = std::fs::File::create(dir.join("worker.log"))?;
        let child = Command::new(std::env::current_exe()?)
            .args(["serve", "--join", &self.join, "--listen", &format!("127.0.0.1:{port}"), "--workers", "2", "--data-dir"])
            .arg(&dir)
            .env("WASM_SERVER_CLUSTER_TOKEN", &self.token)
            .env("HOSTNAME", name)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let handle = child.id().to_string();
        self.children.push((handle.clone(), child));
        Ok(handle)
    }

    fn destroy(&mut self, handle: &str) -> Result<()> {
        if let Some(i) = self.children.iter().position(|(h, _)| h == handle) {
            let (_, mut child) = self.children.remove(i);
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- DigitalOcean

/// Droplets through DigitalOcean's API (`DIGITALOCEAN_TOKEN`), with curl.
struct DigitalOcean {
    api_token: String,
    size: String,
    region: String,
    join: String,
    token: String,
}

// The API token goes to curl on its standard input, not its command line.
fn curl_json(args: &[&str], api_token: &str) -> Result<serde_json::Value> {
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-K", "-"]).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format_err!("can't run curl: {e}"))?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        let _ = writeln!(stdin, "header = \"Authorization: Bearer {api_token}\"");
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("curl: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if out.stdout.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn do_api(method: &str, path: &str, api_token: &str, body: Option<&serde_json::Value>) -> Result<serde_json::Value> {
    let url = format!("https://api.digitalocean.com/v2{path}");
    match body {
        Some(b) => {
            let file = std::env::temp_dir().join(format!("wasm-server-do-{}.json", std::process::id()));
            std::fs::write(&file, b.to_string())?;
            let data = format!("@{}", file.display());
            let r = curl_json(&["-X", method, "-H", "content-type: application/json", "--data-binary", &data, &url], api_token);
            let _ = std::fs::remove_file(&file);
            r
        }
        None => curl_json(&["-X", method, &url], api_token),
    }
}

/// The monthly price of a droplet size, in dollars.
fn do_price(api_token: &str, size: &str) -> Result<f64> {
    let sizes = do_api("GET", "/sizes?per_page=200", api_token, None)?;
    sizes["sizes"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["slug"] == size))
        .and_then(|s| s["price_monthly"].as_f64())
        .ok_or_else(|| format_err!("DigitalOcean has no droplet size {size:?}"))
}

impl Provider for DigitalOcean {
    fn create(&mut self, name: &str) -> Result<String> {
        // the worker: this server's binary from the primary, as a service
        // that joins it, reachable on the droplet's private address
        let user_data = format!(
            r#"#!/bin/sh
set -e
ip=$(curl -fsS http://169.254.169.254/metadata/v1/interfaces/private/0/ipv4/address)
curl -fsS -H "Authorization: Bearer {token}" {join}/cluster/binary -o /usr/local/bin/wasm-server
chmod +x /usr/local/bin/wasm-server
mkdir -p /var/lib/wasm-server
cat > /etc/systemd/system/wasm-server-worker.service <<UNIT
[Unit]
Description=wasm-server cluster worker
After=network-online.target
[Service]
Environment=WASM_SERVER_CLUSTER_TOKEN={token}
Environment=HOSTNAME={name}
ExecStart=/usr/local/bin/wasm-server serve --join {join} --listen 0.0.0.0:8080 --advertise http://$ip:8080 --data-dir /var/lib/wasm-server
Restart=always
[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now wasm-server-worker
"#,
            token = self.token,
            join = self.join,
            name = name
        );
        let body = serde_json::json!({ "name": name, "region": self.region, "size": self.size, "image": "ubuntu-24-04-x64", "user_data": user_data, "tags": ["wasm-server-worker"] });
        let r = do_api("POST", "/droplets", &self.api_token, Some(&body))?;
        r["droplet"]["id"].as_u64().map(|id| id.to_string()).ok_or_else(|| format_err!("DigitalOcean didn't create the droplet: {r}"))
    }

    fn destroy(&mut self, handle: &str) -> Result<()> {
        do_api("DELETE", &format!("/droplets/{handle}"), &self.api_token, None).map(|_| ())
    }
}

// ---------------------------------------------------------------- the loop

/// What the autoscaler is doing, for `/_platform/cluster`.
#[derive(Default, Clone, serde::Serialize)]
pub struct Status {
    pub provider: String,
    pub min: usize,
    pub max: usize,
    pub load: f64,
    pub workers: Vec<String>,
    pub last_action: String,
}

pub static STATUS: Mutex<Option<Status>> = Mutex::new(None);

pub fn start(primary: Arc<Primary>, opts: AutoscaleOpts) -> Result<()> {
    if opts.max < opts.min {
        bail!("--autoscale-max must be at least --autoscale-min");
    }
    let provider: Box<dyn Provider> = match opts.provider.as_str() {
        "local" => Box::new(Local { join: opts.join.clone(), token: opts.token.clone(), dir: opts.data_dir.join("local-workers"), next_port: 18100, children: vec![] }),
        "digitalocean" => {
            let api_token = std::env::var("DIGITALOCEAN_TOKEN").map_err(|_| format_err!("--autoscale digitalocean needs DIGITALOCEAN_TOKEN"))?;
            let price = do_price(&api_token, &opts.size)?;
            eprintln!(
                "autoscale: up to {} droplets {} in {} at ${price:.2}/month each (at most ${:.2}/month), billed by the hour while they run",
                opts.max,
                opts.size,
                opts.region,
                price * opts.max as f64
            );
            Box::new(DigitalOcean { api_token, size: opts.size.clone(), region: opts.region.clone(), join: opts.join.clone(), token: opts.token.clone() })
        }
        other => bail!("unknown --autoscale provider {other:?} (local or digitalocean)"),
    };
    eprintln!("autoscale: {} workers from {}, {}..{}, up at {:.0}% CPU, down under {:.0}%", opts.provider, opts.provider, opts.min, opts.max, opts.high * 100.0, opts.low * 100.0);
    std::thread::Builder::new().name("autoscale".into()).spawn(move || run(primary, opts, provider))?;
    Ok(())
}

fn run(primary: Arc<Primary>, opts: AutoscaleOpts, mut provider: Box<dyn Provider>) {
    let (tick, up_after, down_after, cool_down) = if opts.fast { (Duration::from_secs(2), Duration::from_secs(4), Duration::from_secs(8), Duration::ZERO) } else { (TICK, UP_AFTER, DOWN_AFTER, COOL_DOWN) };
    let gauge = CpuGauge::new(primary.local_cores);
    let mut samples: VecDeque<(Instant, f64)> = VecDeque::new();
    let mut made: Vec<Made> = vec![];
    let mut last_action = Instant::now() - cool_down;
    let mut counter = 0u32;
    let mut status = Status { provider: opts.provider.clone(), min: opts.min, max: opts.max, ..Default::default() };
    loop {
        std::thread::sleep(tick);
        // load across the cluster
        let local = gauge.sample();
        let (busy, cores) = {
            let nodes = primary.nodes.lock().unwrap();
            nodes.iter().fold((local * primary.local_cores as f64, primary.local_cores as f64), |(b, c), n| (b + n.cpu * n.cores as f64, c + n.cores as f64))
        };
        let load = if cores > 0.0 { busy / cores } else { 0.0 };
        let now = Instant::now();
        samples.push_back((now, load));
        while samples.front().is_some_and(|(t, _)| now.duration_since(*t) > down_after + tick) {
            samples.pop_front();
        }
        let span = |d: Duration| samples.iter().filter(|(t, _)| now.duration_since(*t) <= d).map(|(_, l)| *l).collect::<Vec<_>>();
        let covered = |d: Duration| samples.front().is_some_and(|(t, _)| now.duration_since(*t) + tick >= d);
        let joined = |m: &Made| primary.nodes.lock().unwrap().iter().any(|n| n.id.starts_with(&m.name));
        // workers that never joined are given up on
        let stuck: Vec<String> = made.iter().filter(|m| !joined(m) && m.created.elapsed() > JOIN_WITHIN).map(|m| m.handle.clone()).collect();
        for h in stuck {
            eprintln!("autoscale: a worker ({h}) didn't join in {JOIN_WITHIN:?}; removing it");
            let _ = provider.destroy(&h);
            made.retain(|m| m.handle != h);
        }
        let pending = made.iter().any(|m| !joined(m));
        let ready = last_action.elapsed() >= cool_down;
        // keep at least `min`
        if made.len() < opts.min && !pending {
            add(&mut *provider, &mut made, &mut counter, &mut status, "to keep the minimum");
            last_action = Instant::now();
        } else if ready && !pending && made.len() < opts.max && covered(up_after) && span(up_after).iter().all(|l| *l >= opts.high) {
            add(&mut *provider, &mut made, &mut counter, &mut status, &format!("load {:.0}% for {:?}", load * 100.0, up_after));
            last_action = Instant::now();
        } else if ready && made.len() > opts.min && covered(down_after) && span(down_after).iter().all(|l| *l < opts.low) {
            let m = made.pop().unwrap();
            status.last_action = format!("removing {} (load {:.0}% for {:?})", m.name, load * 100.0, down_after);
            eprintln!("autoscale: {}", status.last_action);
            drain_and_remove(&primary, &mut *provider, m);
            last_action = Instant::now();
            samples.clear();
        }
        status.load = load;
        status.workers = made.iter().map(|m| m.name.clone()).collect();
        *STATUS.lock().unwrap() = Some(status.clone());
    }
}

fn add(provider: &mut dyn Provider, made: &mut Vec<Made>, counter: &mut u32, status: &mut Status, why: &str) {
    *counter += 1;
    let name = format!("wasm-worker-{}-{}", std::process::id(), counter);
    match provider.create(&name) {
        Ok(handle) => {
            status.last_action = format!("added {name} ({why})");
            eprintln!("autoscale: {}", status.last_action);
            made.push(Made { handle, name, created: Instant::now() });
        }
        Err(e) => {
            status.last_action = format!("can't add a worker: {e}");
            eprintln!("autoscale: {}", status.last_action);
        }
    }
}

/// No new requests to it; once its running requests are done (at most a
/// minute), it goes.
fn drain_and_remove(primary: &Primary, provider: &mut dyn Provider, m: Made) {
    let id = {
        let mut nodes = primary.nodes.lock().unwrap();
        nodes.iter_mut().find(|n| n.id.starts_with(&m.name)).map(|n| {
            n.draining = true;
            n.id.clone()
        })
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while let Some(id) = &id {
        let busy = primary.nodes.lock().unwrap().iter().find(|n| &n.id == id).is_some_and(|n| n.sent.load(std::sync::atomic::Ordering::Relaxed) > 0);
        if !busy || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Err(e) = provider.destroy(&m.handle) {
        eprintln!("autoscale: removing {} failed: {e}", m.name);
    }
    if let Some(id) = id {
        primary.remove(&id);
    }
}
