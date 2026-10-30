//! 本地代理引擎：127.0.0.1:<本地端口> 同时接受 HTTP 代理和 SOCKS5，
//! 按规则把连接送往 住宅IP（前置代理 → 住宅IP → 目标）、前置代理 或 本地直连。

use crate::config::ProxyConfig;
use crate::socks;
use serde::Serialize;
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;

const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const HALF_CLOSE_WAIT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Route {
    /// 前置代理 → 住宅IP → 目标
    Residential,
    /// 前置代理 → 目标
    Front,
    /// 本机直连（仅本地/内网地址，或未启用前置代理时的非 Claude 流量）
    Direct,
}

/// 编译后的路由规则
pub struct Rules {
    pub cfg: ProxyConfig,
    domains: Vec<String>,
}

impl Rules {
    pub fn new(cfg: ProxyConfig) -> Self {
        let domains = cfg.domains();
        Self { cfg, domains }
    }

    pub fn route_for(&self, host: &str) -> Route {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if is_local(&host) {
            return Route::Direct;
        }
        let matched = self.domains.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")));
        if !self.cfg.split_mode || matched {
            Route::Residential
        } else if self.cfg.front_enabled {
            Route::Front
        } else {
            Route::Direct
        }
    }
}

fn is_local(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Ok(IpAddr::V6(ip)) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00,
        Err(_) => false,
    }
}

async fn tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    let s = TcpStream::connect((host, port)).await?;
    s.set_nodelay(true).ok();
    Ok(s)
}

/// 按路由建立到 host:port 的连接；住宅IP线路下目标域名由住宅代理远端解析
pub async fn dial(cfg: &ProxyConfig, route: Route, host: &str, port: u16) -> io::Result<TcpStream> {
    let fut = async {
        match route {
            Route::Direct => tcp(host, port).await,
            Route::Front if !cfg.front_enabled => tcp(host, port).await,
            Route::Front => {
                let mut s = tcp(&cfg.front_host, cfg.front_port).await.map_err(|e| ctx("连不上前置代理", e))?;
                socks::connect(&mut s, host, port, None).await.map_err(|e| ctx("前置代理", e))?;
                Ok(s)
            }
            Route::Residential => {
                let mut s = if cfg.front_enabled {
                    let mut s = tcp(&cfg.front_host, cfg.front_port).await.map_err(|e| ctx("连不上前置代理", e))?;
                    socks::connect(&mut s, &cfg.host, cfg.port, None)
                        .await
                        .map_err(|e| ctx("前置代理连住宅IP失败", e))?;
                    s
                } else {
                    tcp(&cfg.host, cfg.port).await.map_err(|e| ctx("连不上住宅IP", e))?
                };
                socks::connect(&mut s, host, port, Some((&cfg.username, &cfg.password)))
                    .await
                    .map_err(|e| ctx("住宅IP", e))?;
                Ok(s)
            }
        }
    };
    timeout(DIAL_TIMEOUT, fut)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "连接超时"))?
}

fn ctx(what: &str, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{what}：{}", friendly(&e)))
}

pub fn friendly(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => "连接被拒绝".into(),
        io::ErrorKind::TimedOut => "超时".into(),
        io::ErrorKind::ConnectionReset => "连接被重置".into(),
        io::ErrorKind::UnexpectedEof => "连接被对方关闭".into(),
        _ => e.to_string(),
    }
}

// ───────────────────────── 统计 ─────────────────────────

pub struct ConnEntry {
    host: String,
    route: Route,
    bytes: AtomicU64,
}

struct HostStat {
    route: Route,
    bytes: u64,
    active: u32,
    last_seen: Instant,
    error: Option<String>,
}

#[derive(Default)]
struct StatsInner {
    next_id: u64,
    active: HashMap<u64, Arc<ConnEntry>>,
    hosts: HashMap<String, HostStat>,
    closed: HashMap<Route, u64>,
}

#[derive(Default)]
pub struct Stats {
    inner: Mutex<StatsInner>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HostRow {
    pub host: String,
    pub route: Route,
    pub bytes: u64,
    pub active: u32,
    pub ago_secs: u64,
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct TrafficSnapshot {
    pub residential_bytes: u64,
    pub front_bytes: u64,
    pub direct_bytes: u64,
    pub active: usize,
    pub hosts: Vec<HostRow>,
}

pub struct ConnGuard {
    stats: Arc<Stats>,
    id: u64,
    entry: Arc<ConnEntry>,
}

impl ConnGuard {
    fn add(&self, n: usize) {
        self.entry.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let bytes = self.entry.bytes.load(Ordering::Relaxed);
        let mut s = self.stats.inner.lock().unwrap();
        s.active.remove(&self.id);
        *s.closed.entry(self.entry.route).or_default() += bytes;
        if let Some(h) = s.hosts.get_mut(&self.entry.host) {
            h.bytes += bytes;
            h.active = h.active.saturating_sub(1);
            h.last_seen = Instant::now();
        }
    }
}

impl Stats {
    fn open(self: &Arc<Self>, host: &str, route: Route) -> ConnGuard {
        let entry = Arc::new(ConnEntry { host: host.to_string(), route, bytes: AtomicU64::new(0) });
        let mut s = self.inner.lock().unwrap();
        s.next_id += 1;
        let id = s.next_id;
        s.active.insert(id, entry.clone());
        let h = s.hosts.entry(host.to_string()).or_insert(HostStat {
            route,
            bytes: 0,
            active: 0,
            last_seen: Instant::now(),
            error: None,
        });
        h.route = route;
        h.active += 1;
        h.last_seen = Instant::now();
        h.error = None;
        ConnGuard { stats: self.clone(), id, entry }
    }

    fn fail(&self, host: &str, route: Route, error: String) {
        let mut s = self.inner.lock().unwrap();
        let h = s.hosts.entry(host.to_string()).or_insert(HostStat {
            route,
            bytes: 0,
            active: 0,
            last_seen: Instant::now(),
            error: None,
        });
        h.route = route;
        h.last_seen = Instant::now();
        h.error = Some(error);
    }

    pub fn reset(&self) {
        let mut s = self.inner.lock().unwrap();
        s.hosts.clear();
        s.closed.clear();
    }

    pub fn snapshot(&self) -> TrafficSnapshot {
        let mut s = self.inner.lock().unwrap();
        let now = Instant::now();
        s.hosts.retain(|_, h| h.active > 0 || now.duration_since(h.last_seen) < Duration::from_secs(30 * 60));

        let mut live: HashMap<&str, u64> = HashMap::new();
        let mut per_route: HashMap<Route, u64> = s.closed.clone();
        for e in s.active.values() {
            let b = e.bytes.load(Ordering::Relaxed);
            *live.entry(e.host.as_str()).or_default() += b;
            *per_route.entry(e.route).or_default() += b;
        }
        let mut hosts: Vec<HostRow> = s
            .hosts
            .iter()
            .map(|(host, h)| HostRow {
                host: host.clone(),
                route: h.route,
                bytes: h.bytes + live.get(host.as_str()).copied().unwrap_or(0),
                active: h.active,
                ago_secs: now.duration_since(h.last_seen).as_secs(),
                error: h.error.clone(),
            })
            .collect();
        hosts.sort_by(|a, b| (b.active > 0).cmp(&(a.active > 0)).then(a.ago_secs.cmp(&b.ago_secs)).then(b.bytes.cmp(&a.bytes)));
        hosts.truncate(12);

        TrafficSnapshot {
            residential_bytes: per_route.get(&Route::Residential).copied().unwrap_or(0),
            front_bytes: per_route.get(&Route::Front).copied().unwrap_or(0),
            direct_bytes: per_route.get(&Route::Direct).copied().unwrap_or(0),
            active: s.active.len(),
            hosts,
        }
    }
}

// ───────────────────────── 引擎 ─────────────────────────

struct Running {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
pub struct Engine {
    running: Mutex<Option<Running>>,
    pub stats: Arc<Stats>,
}

impl Engine {
    pub async fn start(&self, cfg: ProxyConfig) -> Result<(), String> {
        // 等旧的监听彻底关闭再绑定，避免重启时端口还被自己占着
        let old = self.running.lock().unwrap().take();
        if let Some(r) = old {
            let _ = r.shutdown.send(true);
            r.task.abort();
            let _ = r.task.await;
        }
        let port = cfg.local_port;
        let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
            if e.kind() == io::ErrorKind::AddrInUse {
                format!("本地端口 {port} 已被其他程序占用，请在设置里换一个")
            } else {
                format!("无法监听 127.0.0.1:{port}：{e}")
            }
        })?;
        let (tx, rx) = watch::channel(false);
        self.stats.reset();
        let task = tokio::spawn(accept_loop(listener, Arc::new(Rules::new(cfg)), self.stats.clone(), rx));
        *self.running.lock().unwrap() = Some(Running { shutdown: tx, task });
        Ok(())
    }

    pub fn stop(&self) {
        if let Some(r) = self.running.lock().unwrap().take() {
            let _ = r.shutdown.send(true);
            r.task.abort();
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.lock().unwrap().is_some()
    }
}

async fn accept_loop(listener: TcpListener, rules: Arc<Rules>, stats: Arc<Stats>, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let Ok((sock, _)) = accepted else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                sock.set_nodelay(true).ok();
                let (rules, stats, mut sd) = (rules.clone(), stats.clone(), shutdown.clone());
                tokio::spawn(async move {
                    tokio::select! {
                        _ = handle(sock, &rules, &stats) => {}
                        _ = sd.changed() => {}
                    }
                });
            }
        }
    }
}

async fn handle(client: TcpStream, rules: &Rules, stats: &Arc<Stats>) -> io::Result<()> {
    let mut first = [0u8; 1];
    let n = timeout(Duration::from_secs(30), client.peek(&mut first))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    if n == 0 {
        return Ok(());
    }
    if first[0] == 0x05 {
        socks_inbound(client, rules, stats).await
    } else {
        http_inbound(client, rules, stats).await
    }
}

async fn socks_inbound(mut client: TcpStream, rules: &Rules, stats: &Arc<Stats>) -> io::Result<()> {
    let mut h = [0u8; 2];
    client.read_exact(&mut h).await?;
    let mut methods = vec![0u8; h[1] as usize];
    client.read_exact(&mut methods).await?;
    client.write_all(&[0x05, 0x00]).await?;

    let mut req = [0u8; 4];
    client.read_exact(&mut req).await?;
    if req[1] != 0x01 {
        client.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(());
    }
    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            client.read_exact(&mut a).await?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        0x03 => {
            let mut l = [0u8; 1];
            client.read_exact(&mut l).await?;
            let mut d = vec![0u8; l[0] as usize];
            client.read_exact(&mut d).await?;
            String::from_utf8_lossy(&d).into_owned()
        }
        0x04 => {
            let mut a = [0u8; 16];
            client.read_exact(&mut a).await?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => return Ok(()),
    };
    let mut p = [0u8; 2];
    client.read_exact(&mut p).await?;
    let port = u16::from_be_bytes(p);

    let route = rules.route_for(&host);
    match dial(&rules.cfg, route, &host, port).await {
        Ok(up) => {
            client.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
            relay(client, up, Vec::new(), stats.open(&host, route)).await
        }
        Err(e) => {
            stats.fail(&host, route, e.to_string());
            client.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
            Ok(())
        }
    }
}

async fn http_inbound(mut client: TcpStream, rules: &Rules, stats: &Arc<Stats>) -> io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let head_end = loop {
        let mut tmp = [0u8; 4096];
        let n = timeout(Duration::from_secs(30), client.read(&mut tmp))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return respond(&mut client, "431 Request Header Fields Too Large", "").await;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let leftover = buf[head_end..].to_vec();
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or("HTTP/1.1"));

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = split_host_port(target, 443) else {
            return respond(&mut client, "400 Bad Request", "无效的 CONNECT 目标").await;
        };
        let route = rules.route_for(&host);
        return match dial(&rules.cfg, route, &host, port).await {
            Ok(up) => {
                client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
                relay(client, up, leftover, stats.open(&host, route)).await
            }
            Err(e) => {
                stats.fail(&host, route, e.to_string());
                respond(&mut client, "502 Bad Gateway", &e.to_string()).await
            }
        };
    }

    // 普通 HTTP 代理请求：absolute-form → origin-form，每个连接只转发一个请求
    let lower = target.to_ascii_lowercase();
    let Some(rest) = lower.starts_with("http://").then(|| &target[7..]) else {
        return respond(&mut client, "400 Bad Request", "这是 ClaudeChain 代理端口，请把它配置为 HTTP/SOCKS5 代理使用").await;
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let Some((host, port)) = split_host_port(authority, 80) else {
        return respond(&mut client, "400 Bad Request", "无效的目标地址").await;
    };
    let mut out = format!("{method} {path} {version}\r\n");
    for line in lines.filter(|l| !l.is_empty()) {
        let l = line.to_ascii_lowercase();
        if ["proxy-connection:", "proxy-authorization:", "connection:", "keep-alive:"].iter().any(|p| l.starts_with(p)) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    let mut initial = out.into_bytes();
    initial.extend_from_slice(&leftover);

    let route = rules.route_for(&host);
    match dial(&rules.cfg, route, &host, port).await {
        Ok(up) => relay(client, up, initial, stats.open(&host, route)).await,
        Err(e) => {
            stats.fail(&host, route, e.to_string());
            respond(&mut client, "502 Bad Gateway", &e.to_string()).await
        }
    }
}

async fn respond(client: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(msg.as_bytes()).await
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn split_host_port(s: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']')?;
        let port = match rest[end + 1..].strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default_port,
        };
        return Some((rest[..end].to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), p.parse().ok()?)),
        _ if !s.is_empty() => Some((s.to_string(), default_port)),
        _ => None,
    }
}

/// 双向转发并实时计数；一侧结束后给另一侧有限的收尾时间
async fn relay(client: TcpStream, upstream: TcpStream, initial: Vec<u8>, guard: ConnGuard) -> io::Result<()> {
    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = upstream.into_split();
    if !initial.is_empty() {
        uw.write_all(&initial).await?;
        guard.add(initial.len());
    }
    let g = &guard;
    let up = async {
        let mut b = vec![0u8; 32 * 1024];
        loop {
            let n = timeout(IDLE_TIMEOUT, cr.read(&mut b)).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
            if n == 0 {
                break;
            }
            uw.write_all(&b[..n]).await?;
            g.add(n);
        }
        uw.shutdown().await.ok();
        io::Result::Ok(())
    };
    let down = async {
        let mut b = vec![0u8; 32 * 1024];
        loop {
            let n = timeout(IDLE_TIMEOUT, ur.read(&mut b)).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
            if n == 0 {
                break;
            }
            cw.write_all(&b[..n]).await?;
            g.add(n);
        }
        cw.shutdown().await.ok();
        io::Result::Ok(())
    };
    tokio::pin!(up, down);
    tokio::select! {
        r = &mut up => if r.is_ok() { let _ = timeout(HALF_CLOSE_WAIT, &mut down).await; },
        r = &mut down => if r.is_ok() { let _ = timeout(HALF_CLOSE_WAIT, &mut up).await; },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(split: bool, front: bool) -> Rules {
        Rules::new(ProxyConfig { host: "1.2.3.4".into(), port: 1080, split_mode: split, front_enabled: front, ..Default::default() })
    }

    #[test]
    fn routes() {
        let r = rules(true, true);
        assert_eq!(r.route_for("api.anthropic.com"), Route::Residential);
        assert_eq!(r.route_for("claude.ai"), Route::Residential);
        assert_eq!(r.route_for("API.Anthropic.com."), Route::Residential);
        assert_eq!(r.route_for("notanthropic.com"), Route::Front);
        assert_eq!(r.route_for("registry.npmjs.org"), Route::Front);
        assert_eq!(r.route_for("localhost"), Route::Direct);
        assert_eq!(r.route_for("127.0.0.1"), Route::Direct);
        assert_eq!(r.route_for("192.168.1.5"), Route::Direct);
        assert_eq!(r.route_for("::1"), Route::Direct);
        assert_eq!(rules(false, true).route_for("registry.npmjs.org"), Route::Residential);
        assert_eq!(rules(true, false).route_for("registry.npmjs.org"), Route::Direct);
        assert_eq!(rules(true, false).route_for("api.anthropic.com"), Route::Residential);
    }

    #[test]
    fn host_ports() {
        assert_eq!(split_host_port("a.com:8443", 443), Some(("a.com".into(), 8443)));
        assert_eq!(split_host_port("a.com", 80), Some(("a.com".into(), 80)));
        assert_eq!(split_host_port("[::1]:9", 80), Some(("::1".into(), 9)));
        assert_eq!(split_host_port("a.com:x", 80), None);
    }
}
