//! 健康检查：分别沿 第1跳（前置代理）和 第2跳（住宅IP）发 HTTPS 请求，拿到各自的出口 IP。

use crate::config::ProxyConfig;
use crate::proxy::{dial, friendly, Route};
use serde::Serialize;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio_rustls::rustls::{self, pki_types::ServerName};
use tokio_rustls::TlsConnector;

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Hop {
    pub ip: String,
    pub country: String,
    pub city: String,
    pub org: String,
    pub latency_ms: u64,
}

fn tls() -> TlsConnector {
    static CONNECTOR: OnceLock<TlsConnector> = OnceLock::new();
    CONNECTOR
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("TLS 配置")
                .with_root_certificates(roots)
                .with_no_client_auth();
            TlsConnector::from(Arc::new(config))
        })
        .clone()
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 沿指定线路发一个 HTTPS 请求（Connection: close）
pub async fn https(cfg: &ProxyConfig, route: Route, method: &str, host: &str, path: &str) -> Result<Response, String> {
    let fut = async {
        let tcp = dial(cfg, route, host, 443).await.map_err(|e| e.to_string())?;
        let name = ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
        let mut s = tls().connect(name, tcp).await.map_err(|e| format!("TLS 握手失败：{}", friendly(&e)))?;
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Mozilla/5.0 ClaudeChain/1.0\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.map_err(|e| friendly(&e))?;

        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = s.read(&mut tmp).await.map_err(|e| friendly(&e))?;
            if n == 0 {
                return Err("响应不完整".to_string());
            }
            buf.extend_from_slice(&tmp[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or("响应格式错误")?;
        let mut body = buf[head_end..].to_vec();
        if method == "HEAD" || status == 204 || status == 304 {
            return Ok(Response { status, body: Vec::new() });
        }
        let length = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok());
        let chunked = head.lines().any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked"));
        loop {
            if length.is_some_and(|n| body.len() >= n) || (chunked && body.windows(5).any(|w| w == b"0\r\n\r\n")) {
                break;
            }
            match s.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&tmp[..n]),
                Err(_) if !body.is_empty() => break,
                Err(e) => return Err(friendly(&e)),
            }
        }
        if chunked {
            body = dechunk(&body);
        }
        Ok(Response { status, body })
    };
    timeout(Duration::from_secs(20), fut).await.map_err(|_| "超时".to_string())?
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(i) = data.windows(2).position(|w| w == b"\r\n") {
        let size_str = String::from_utf8_lossy(&data[..i]);
        let Ok(size) = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16) else { break };
        if size == 0 || data.len() < i + 2 + size {
            break;
        }
        out.extend_from_slice(&data[i + 2..i + 2 + size]);
        data = &data[(i + 4 + size).min(data.len())..];
    }
    out
}

/// Cloudflare trace：出口 IP + 国家，体积小、无频率限制。
/// 第1跳用 IPv4 地址 1.1.1.1：前置节点连住宅IP走的就是 IPv4，住宅代理看到的是这个 IP；
/// 第2跳用双栈域名，和 Claude API 一样，测出的才是 Claude 实际看到的 IP。
pub async fn trace(cfg: &ProxyConfig, route: Route) -> Result<Hop, String> {
    let host = if route == Route::Residential { "www.cloudflare.com" } else { "1.1.1.1" };
    trace_host(cfg, route, host).await
}

async fn trace_host(cfg: &ProxyConfig, route: Route, host: &str) -> Result<Hop, String> {
    let t0 = Instant::now();
    let r = https(cfg, route, "GET", host, "/cdn-cgi/trace").await?;
    let text = String::from_utf8_lossy(&r.body);
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap_or("").to_string();
    let ip = get("ip");
    if ip.is_empty() {
        return Err(format!("检测接口返回异常（HTTP {}）", r.status));
    }
    Ok(Hop { ip, country: get("loc"), latency_ms: t0.elapsed().as_millis() as u64, ..Default::default() })
}

/// ipinfo：运营商和城市（每个 IP 只查一次）。返回 (ip, city, org)
pub async fn ip_details(cfg: &ProxyConfig, route: Route) -> Option<(String, String, String)> {
    let r = https(cfg, route, "GET", "ipinfo.io", "/json").await.ok()?;
    let v: serde_json::Value = serde_json::from_slice(&r.body).ok()?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let org = s("org");
    // "AS2497 Internet Initiative Japan Inc." → "Internet Initiative Japan Inc."
    let org = match org.split_once(' ') {
        Some((asn, rest)) if asn.starts_with("AS") => rest.to_string(),
        _ => org,
    };
    Some((s("ip"), s("city"), org))
}

/// 漏网检查：不经任何代理能否连上 Claude；连得上就返回 Claude 会看到的 IP
pub async fn direct_exposure(cfg: &ProxyConfig) -> Result<Hop, String> {
    match trace_host(cfg, Route::Direct, "claude.ai").await {
        Ok(h) => Ok(h),
        Err(e) => {
            // claude.ai 不通但 API 通，同样算漏网通道；再查一下真实出口
            claude_api(cfg, Route::Direct).await.map_err(|_| e)?;
            let ip = https(cfg, Route::Direct, "GET", "api-ipv4.ip.sb", "/ip")
                .await
                .map(|r| String::from_utf8_lossy(&r.body).trim().to_string())
                .unwrap_or_else(|_| "未知".into());
            Ok(Hop { ip, ..Default::default() })
        }
    }
}

/// Claude API 可达性：任何 HTTP 响应都算通，返回耗时
pub async fn claude_api(cfg: &ProxyConfig, route: Route) -> Result<u64, String> {
    let t0 = Instant::now();
    https(cfg, route, "HEAD", "api.anthropic.com", "/").await?;
    Ok(t0.elapsed().as_millis() as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dechunks() {
        assert_eq!(super::dechunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"), b"hello world");
    }
}

/// 在本机和宿主机常见地址上查找可用的 SOCKS5 代理（虚拟机里用来找宿主机的 FlClash）
pub async fn find_socks_proxies(exclude_local_port: u16) -> Vec<String> {
    let mut hosts: Vec<String> = vec!["127.0.0.1".into()];
    // 本机主网卡所在 /24 的 .1 和 .2：VMware/Hyper-V/KVM 的宿主机或网关通常在这里
    if let Some(std::net::IpAddr::V4(ip)) = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("1.1.1.1:80").map(|_| s))
        .and_then(|s| s.local_addr())
        .ok()
        .map(|a| a.ip())
    {
        let [a, b, c, _] = ip.octets();
        hosts.push(format!("{a}.{b}.{c}.1"));
        hosts.push(format!("{a}.{b}.{c}.2"));
    }
    // VirtualBox NAT、Parallels、UTM / Apple 虚拟化的宿主机地址
    for h in ["10.0.2.2", "10.211.55.2", "192.168.64.1"] {
        hosts.push(h.into());
    }
    hosts.dedup();
    let mut tasks = Vec::new();
    for host in &hosts {
        for port in [7890u16, 7897, 7898, 10808, 1080] {
            if host == "127.0.0.1" && port == exclude_local_port {
                continue;
            }
            let host = host.clone();
            tasks.push(tokio::spawn(async move { is_socks5(&host, port).await.then(|| format!("{host}:{port}")) }));
        }
    }
    let mut found = Vec::new();
    for t in tasks {
        if let Ok(Some(addr)) = t.await {
            if !found.contains(&addr) {
                found.push(addr);
            }
        }
    }
    found
}

async fn is_socks5(host: &str, port: u16) -> bool {
    let probe = async {
        let mut s = tokio::net::TcpStream::connect((host, port)).await.ok()?;
        s.write_all(&[0x05, 0x01, 0x00]).await.ok()?;
        let mut r = [0u8; 2];
        s.read_exact(&mut r).await.ok()?;
        (r[0] == 0x05 && r[1] == 0x00).then_some(())
    };
    matches!(timeout(Duration::from_millis(1200), probe).await, Ok(Some(())))
}

/// 联网测试：cargo test live_ -- --ignored --nocapture（使用已保存的配置）
#[cfg(test)]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn live_ip_details() {
        let dir = dirs::data_dir().unwrap().join("ClaudeChain");
        let cfg = ProxyConfig::load(&dir.join("config.json"));
        for route in [Route::Front, Route::Residential] {
            match https(&cfg, route, "GET", "ipinfo.io", "/json").await {
                Ok(r) => println!("{route:?} HTTP {} body={}", r.status, String::from_utf8_lossy(&r.body)),
                Err(e) => println!("{route:?} 失败 {e}"),
            }
            println!("{route:?} 1.1.1.1 trace = {:?}", trace_host(&cfg, route, "1.1.1.1").await.map(|h| h.ip));
        }
    }
}
