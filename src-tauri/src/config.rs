use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_DOMAINS: &str =
    "anthropic.com, claude.ai, claude.com, claudeusercontent.com, datadoghq.com, sentry.io, statsig.com";

/// 用户配置，保存在 <数据目录>/ClaudeChain/config.json
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ProxyConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// 先经过本机的 FlClash / Clash 等代理再连住宅IP
    pub front_enabled: bool,
    pub front_host: String,
    pub front_port: u16,
    pub local_port: u16,
    /// 只有 residential_domains 里的域名走住宅IP，其余走前置代理
    pub split_mode: bool,
    pub residential_domains: String,
    pub auto_start: bool,
    pub launch_claude_on_open: bool,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 0,
            username: String::new(),
            password: String::new(),
            front_enabled: true,
            front_host: "127.0.0.1".into(),
            front_port: 7890,
            local_port: 7891,
            split_mode: true,
            residential_domains: DEFAULT_DOMAINS.into(),
            auto_start: true,
            launch_claude_on_open: false,
        }
    }
}

impl ProxyConfig {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        crate::claude::write_atomic(path, &data, true)
    }

    pub fn is_complete(&self) -> bool {
        !self.host.trim().is_empty() && self.port > 0
    }

    pub fn domains(&self) -> Vec<String> {
        self.residential_domains
            .to_lowercase()
            .split(|c: char| matches!(c, ',' | '，' | ';' | '；') || c.is_whitespace())
            .map(|d| d.trim_matches('.'))
            .filter(|d| d.contains('.') && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
            .map(String::from)
            .collect()
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.is_complete() {
            return Err("请填写住宅代理的地址和端口".into());
        }
        if self.local_port == 0 {
            return Err("本地端口无效".into());
        }
        if self.front_enabled {
            if self.front_host.trim().is_empty() || self.front_port == 0 {
                return Err("请填写前置代理的地址和端口".into());
            }
            if self.front_port == self.local_port && is_loopback(&self.front_host) {
                return Err("本地端口不能和前置代理端口相同".into());
            }
        }
        if self.split_mode && self.domains().is_empty() {
            return Err("分流模式下至少要有一个走住宅IP的域名".into());
        }
        Ok(())
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host.trim(), "127.0.0.1" | "localhost" | "::1")
}

#[derive(Debug, Serialize, PartialEq)]
pub struct LinkInfo {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

/// 解析代理链接：socks5://user:pass@host:port、socks5://<Base64>（二维码常见格式）、host:port:user:pass
pub fn parse_link(raw: &str) -> Option<LinkInfo> {
    let mut s = raw.trim().to_string();
    if let Some(i) = s.find("://") {
        s = s[i + 3..].to_string();
    }
    for sep in ['#', '?'] {
        if let Some(i) = s.find(sep) {
            s.truncate(i);
        }
    }
    let s = s.trim_end_matches('/').to_string();
    let colons = |t: &str| t.matches(':').count();

    let s = if !s.contains('@') && colons(&s) != 3 {
        match decode_base64(&s) {
            Some(d) if d.contains('@') || colons(&d) == 3 => d,
            _ => s,
        }
    } else {
        s
    };

    if let Some(at) = s.rfind('@') {
        let (host, port) = host_port(&s[at + 1..])?;
        let mut cred = s[..at].splitn(2, ':');
        let user = unescape(cred.next().unwrap_or(""));
        let pass = unescape(cred.next().unwrap_or(""));
        return Some(LinkInfo { host, port, username: user, password: pass });
    }

    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 4 && !parts[0].is_empty() {
        if let Ok(port) = parts[1].parse::<u16>() {
            if port > 0 {
                return Some(LinkInfo {
                    host: parts[0].into(),
                    port,
                    username: parts[2].into(),
                    password: parts[3].into(),
                });
            }
        }
    }
    let (host, port) = host_port(&s)?;
    Some(LinkInfo { host, port, username: String::new(), password: String::new() })
}

fn host_port(s: &str) -> Option<(String, u16)> {
    let c = s.rfind(':')?;
    let port: u16 = s[c + 1..].parse().ok().filter(|p| *p > 0)?;
    let host = s[..c].trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| (host.to_string(), port))
}

fn unescape(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn decode_base64(s: &str) -> Option<String> {
    let mut t: String = s.chars().map(|c| match c { '-' => '+', '_' => '/', c => c }).collect();
    while t.len() % 4 != 0 {
        t.push('=');
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(t).ok()?;
    String::from_utf8(bytes).ok().map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(host: &str, port: u16, u: &str, p: &str) -> Option<LinkInfo> {
        Some(LinkInfo { host: host.into(), port, username: u.into(), password: p.into() })
    }

    #[test]
    fn parses_links() {
        assert_eq!(
            parse_link("socks5://dGVzdHVzZXI6dGVzdHBhc3NAMjAzLjAuMTEzLjEwOjEwODA="),
            link("203.0.113.10", 1080, "testuser", "testpass")
        );
        assert_eq!(parse_link("socks5://u%40x:p@1.2.3.4:1080#日本"), link("1.2.3.4", 1080, "u@x", "p"));
        assert_eq!(parse_link(" 1.2.3.4:1080:user:pass "), link("1.2.3.4", 1080, "user", "pass"));
        assert_eq!(parse_link("proxy.example.com:8000"), link("proxy.example.com", 8000, "", ""));
        assert_eq!(parse_link("hello world"), None);
        assert_eq!(parse_link("1.2.3.4:99999"), None);
    }

    #[test]
    fn cleans_domains_and_validates() {
        let mut c = ProxyConfig { host: "1.2.3.4".into(), port: 1080, ..Default::default() };
        c.residential_domains = "anthropic.com，claude.ai; bad_domain!, .claude.com.".into();
        assert_eq!(c.domains(), vec!["anthropic.com", "claude.ai", "claude.com"]);
        assert!(c.validate().is_ok());
        c.local_port = 7890;
        assert!(c.validate().is_err());
        assert!(ProxyConfig::default().validate().is_err());
    }
}
