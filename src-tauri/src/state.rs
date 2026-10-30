use crate::claude::{self, Banner};
use crate::config::ProxyConfig;
use crate::probe::{self, Hop};
use crate::proxy::{Engine, Route, Rules, TrafficSnapshot};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum Health {
    Stopped,
    Starting,
    Ok,
    Failed(String),
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub health: Health,
    pub checking: bool,
    pub hop1: Option<Hop>,
    pub hop2: Option<Hop>,
    pub hop1_error: Option<String>,
    pub hop2_error: Option<String>,
    pub api_ms: Option<u64>,
    pub api_error: Option<String>,
    /// 漏网检查：不经代理直连 Claude 时 Claude 看到的 IP。None + direct_checked = 直连不通，不会漏网
    pub direct: Option<Hop>,
    pub direct_checked: bool,
    pub checked_at: Option<u64>,
}

impl Status {
    fn with(health: Health) -> Self {
        Self {
            health,
            checking: false,
            hop1: None,
            hop2: None,
            hop1_error: None,
            hop2_error: None,
            api_ms: None,
            api_error: None,
            direct: None,
            direct_checked: false,
            checked_at: None,
        }
    }
}

#[derive(Serialize)]
pub struct LaunchDirs {
    /// 当前选中的文件夹（最近一次启动的，没有就是主目录）
    pub current: String,
    pub recent: Vec<String>,
    pub home: String,
}

#[derive(Serialize, Clone)]
pub struct Notice {
    pub id: u64,
    pub text: String,
    pub error: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiState {
    pub running: bool,
    pub configured: bool,
    pub claude_global: bool,
    pub front_enabled: bool,
    pub front_label: String,
    pub local_port: u16,
    pub split_mode: bool,
    pub status: Status,
    pub traffic: TrafficSnapshot,
    pub notice: Option<Notice>,
    pub data_dir: String,
}

pub struct AppState {
    pub dir: PathBuf,
    config: Mutex<ProxyConfig>,
    engine: Engine,
    status: Mutex<Status>,
    claude_global: AtomicBool,
    /// IP → (城市, 运营商)，每个 IP 只查一次
    details: Mutex<HashMap<String, (String, String)>>,
    check_lock: tokio::sync::Mutex<()>,
    monitor: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    last_launch: Mutex<Option<(Instant, PathBuf)>>,
    notice: Mutex<Option<Notice>>,
    /// 最近启动过 Claude Code 的文件夹，最新的在前
    recent_dirs: Mutex<Vec<String>>,
}

const MAX_RECENT: usize = 8;

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl AppState {
    pub fn new() -> Arc<Self> {
        let dir = dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("ClaudeChain");
        let _ = std::fs::create_dir_all(&dir);
        let config = ProxyConfig::load(&dir.join("config.json"));
        let recent: Vec<String> = std::fs::read(dir.join("recent.json"))
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default();
        Arc::new(Self {
            dir,
            config: Mutex::new(config),
            engine: Engine::default(),
            status: Mutex::new(Status::with(Health::Stopped)),
            claude_global: AtomicBool::new(false),
            details: Mutex::new(HashMap::new()),
            check_lock: tokio::sync::Mutex::new(()),
            monitor: Mutex::new(None),
            last_launch: Mutex::new(None),
            notice: Mutex::new(None),
            recent_dirs: Mutex::new(recent),
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn config(&self) -> ProxyConfig {
        self.config.lock().unwrap().clone()
    }

    pub fn notify(&self, text: impl Into<String>, error: bool) {
        let mut n = self.notice.lock().unwrap();
        let id = n.as_ref().map_or(1, |x| x.id + 1);
        *n = Some(Notice { id, text: text.into(), error });
    }

    fn health(&self) -> Health {
        self.status.lock().unwrap().health.clone()
    }

    pub fn ui_state(&self) -> UiState {
        let cfg = self.config();
        UiState {
            running: self.engine.is_running(),
            configured: cfg.is_complete(),
            claude_global: self.claude_global.load(SeqCst),
            front_enabled: cfg.front_enabled,
            front_label: format!("{}:{}", cfg.front_host, cfg.front_port),
            local_port: cfg.local_port,
            split_mode: cfg.split_mode,
            status: self.status.lock().unwrap().clone(),
            traffic: self.engine.stats.snapshot(),
            notice: self.notice.lock().unwrap().clone(),
            data_dir: self.dir.to_string_lossy().into_owned(),
        }
    }

    // ───────────── 生命周期 ─────────────

    /// `launch_dir`：命令行指定了文件夹时，链路就绪后在那里启动 Claude Code
    pub async fn bootstrap(self: &Arc<Self>, launch_dir: Option<PathBuf>) {
        let cfg = self.config();
        self.claude_global.store(claude::is_enabled(cfg.local_port), SeqCst);
        let wants_launch = launch_dir.is_some() || cfg.launch_claude_on_open;
        if (cfg.auto_start || launch_dir.is_some()) && cfg.is_complete() {
            match self.start_chain().await {
                Ok(()) if wants_launch => {
                    if let Err(e) = self.launch_claude(launch_dir).await {
                        self.notify(e, true);
                    }
                }
                Ok(()) => {}
                Err(e) => self.notify(e, true),
            }
        } else if self.claude_global.load(SeqCst) {
            // 链路没跑时，Claude Code 绝不能指向本程序的端口
            self.restore_claude(Some("链路未运行，已恢复 Claude Code 原来的代理设置"));
        }
    }

    pub fn shutdown(&self) {
        if self.claude_global.load(SeqCst) {
            let _ = claude::disable(self.config().local_port, &self.path("claude-original-env.json"));
        }
        self.engine.stop();
    }

    // ───────────── 链路 ─────────────

    pub async fn start_chain(self: &Arc<Self>) -> Result<(), String> {
        let cfg = self.config();
        cfg.validate()?;
        *self.status.lock().unwrap() = Status::with(Health::Starting);
        if let Err(e) = self.engine.start(cfg.clone()).await {
            *self.status.lock().unwrap() = Status::with(Health::Failed(e.clone()));
            if self.claude_global.load(SeqCst) {
                self.restore_claude(None);
            }
            return Err(e);
        }
        let _ = claude::write_env_file(&self.path("claude-env.json"), cfg.local_port);
        self.spawn_monitor();
        self.check().await;
        Ok(())
    }

    pub fn stop_chain(&self) {
        if self.claude_global.load(SeqCst) {
            self.restore_claude(Some("链路已停止，Claude Code 已恢复原来的代理设置"));
        }
        if let Some(h) = self.monitor.lock().unwrap().take() {
            h.abort();
        }
        self.engine.stop();
        *self.status.lock().unwrap() = Status::with(Health::Stopped);
    }

    fn spawn_monitor(self: &Arc<Self>) {
        let me = self.clone();
        let handle = tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                me.check().await;
            }
        });
        if let Some(old) = self.monitor.lock().unwrap().replace(handle) {
            old.abort();
        }
    }

    /// 并行检测：第1跳出口、第2跳（住宅IP）出口、Claude API 可达性
    pub async fn check(&self) {
        let _guard = self.check_lock.lock().await;
        if !self.engine.is_running() {
            return;
        }
        self.status.lock().unwrap().checking = true;
        let cfg = self.config();
        let api_route = Rules::new(cfg.clone()).route_for("api.anthropic.com");
        // 漏网检查：绕过所有代理直连 Claude。连得上说明没走代理的 Claude 流量会暴露真实 IP
        let direct_probe = async {
            tokio::time::timeout(Duration::from_secs(8), probe::direct_exposure(&cfg))
                .await
                .unwrap_or_else(|_| Err("超时".into()))
        };
        let (h1, h2, api, direct) = tokio::join!(
            probe::trace(&cfg, Route::Front),
            probe::trace(&cfg, Route::Residential),
            probe::claude_api(&cfg, api_route),
            direct_probe,
        );
        let direct_enriched = async {
            match direct {
                Ok(hop) => tokio::time::timeout(Duration::from_secs(8), self.enrich(&cfg, Route::Direct, Ok(hop.clone())))
                    .await
                    .unwrap_or(Ok(hop)),
                Err(e) => Err(e),
            }
        };
        let (h1, h2, direct) = tokio::join!(
            self.enrich(&cfg, Route::Front, h1),
            self.enrich(&cfg, Route::Residential, h2),
            direct_enriched,
        );

        let health = match (&h1, &h2, &api) {
            (Err(e), _, _) if cfg.front_enabled => {
                if port_open(&cfg.front_host, cfg.front_port).await {
                    Health::Failed(format!("第1跳（前置节点）不通：{e}。请检查 FlClash 当前节点"))
                } else {
                    Health::Failed(format!("前置代理 {}:{} 没有响应，请确认 FlClash 已开启", cfg.front_host, cfg.front_port))
                }
            }
            (Err(e), _, _) => Health::Failed(format!("本机网络不通：{e}")),
            (_, Err(e), _) => Health::Failed(format!("第2跳（住宅IP）不通：{e}。可能是代理到期、账号密码错误或前置节点异常")),
            (_, _, Err(e)) => Health::Failed(format!("住宅IP正常，但连不上 Claude API：{e}")),
            _ => Health::Ok,
        };

        if !self.engine.is_running() {
            return;
        }
        let mut s = self.status.lock().unwrap();
        s.health = health;
        s.checking = false;
        s.hop1_error = h1.as_ref().err().cloned();
        s.hop1 = h1.ok();
        s.hop2_error = h2.as_ref().err().cloned();
        s.hop2 = h2.ok();
        s.api_error = api.as_ref().err().cloned();
        s.api_ms = api.ok();
        s.direct = direct.ok();
        s.direct_checked = true;
        s.checked_at = Some(now_secs());
    }

    async fn enrich(&self, cfg: &ProxyConfig, route: Route, hop: Result<Hop, String>) -> Result<Hop, String> {
        let mut hop = hop?;
        let cached = self.details.lock().unwrap().get(&hop.ip).cloned();
        let (city, org) = match cached {
            Some(v) => v,
            // 沿同一线路查询，并核对 IP 一致才采用（避免前置代理分流到别的节点）
            None => match probe::ip_details(cfg, route).await {
                Some((ip, city, org)) if ip == hop.ip => {
                    self.details.lock().unwrap().insert(ip, (city.clone(), org.clone()));
                    (city, org)
                }
                _ => Default::default(),
            },
        };
        hop.city = city;
        hop.org = org;
        Ok(hop)
    }

    // ───────────── Claude Code ─────────────

    // ───────────── 文件夹 ─────────────

    pub fn launch_dirs(&self) -> LaunchDirs {
        let home = dirs::home_dir().unwrap_or_default().to_string_lossy().into_owned();
        let recent: Vec<String> = self.recent_dirs.lock().unwrap().iter().filter(|d| Path::new(d).is_dir()).cloned().collect();
        LaunchDirs { current: recent.first().cloned().unwrap_or_else(|| home.clone()), recent, home }
    }

    fn remember_dir(&self, dir: &Path) {
        let d = dir.to_string_lossy().into_owned();
        let mut recent = self.recent_dirs.lock().unwrap();
        recent.retain(|x| x != &d);
        recent.insert(0, d);
        recent.truncate(MAX_RECENT);
        let _ = std::fs::write(self.path("recent.json"), serde_json::to_vec_pretty(&*recent).unwrap_or_default());
    }

    pub fn forget_dir(&self, dir: &str) {
        let mut recent = self.recent_dirs.lock().unwrap();
        recent.retain(|x| x != dir);
        let _ = std::fs::write(self.path("recent.json"), serde_json::to_vec_pretty(&*recent).unwrap_or_default());
    }

    /// 确认链路正常后，在新终端里、指定文件夹下启动 Claude Code；链路不通就不启动，避免用错 IP。
    /// `dir` 为空时用最近一次的文件夹（没有就是主目录）
    pub async fn launch_claude(self: &Arc<Self>, dir: Option<PathBuf>) -> Result<String, String> {
        let dir = match dir {
            Some(d) => d,
            None => PathBuf::from(self.launch_dirs().current),
        };
        if !dir.is_dir() {
            return Err(format!("文件夹不存在：{}", dir.display()));
        }
        {
            // 防连点：同一个文件夹 3 秒内只启动一次
            let mut last = self.last_launch.lock().unwrap();
            if last.as_ref().is_some_and(|(t, d)| d == &dir && t.elapsed() < Duration::from_secs(3)) {
                return Ok("Claude Code 正在启动…".into());
            }
            *last = Some((Instant::now(), dir.clone()));
        }
        if !self.engine.is_running() {
            self.start_chain().await?;
        }
        let stale = {
            let s = self.status.lock().unwrap();
            s.health != Health::Ok || s.checked_at.map_or(true, |t| now_secs().saturating_sub(t) > 45)
        };
        if stale {
            self.check().await;
        }
        let s = self.status.lock().unwrap().clone();
        if s.health != Health::Ok {
            let why = match s.health {
                Health::Failed(m) => m,
                _ => "链路未就绪".into(),
            };
            return Err(format!("链路检测没通过，已取消启动 Claude Code：{why}"));
        }
        let cfg = self.config();
        let env_file = self.path("claude-env.json");
        claude::write_env_file(&env_file, cfg.local_port)?;
        claude::launch(&self.dir, &env_file, &dir, &banner(&cfg, &s))?;
        self.remember_dir(&dir);
        Ok(format!("已在新终端窗口启动 Claude Code：{}", dir.display()))
    }

    pub fn set_claude_global(&self, on: bool) -> Result<String, String> {
        let port = self.config().local_port;
        if on {
            if !self.engine.is_running() || self.health() != Health::Ok {
                return Err("请等链路检测通过后再开启".into());
            }
            claude::enable(port, &self.path("claude-original-env.json"), &self.path("claude-settings.backup.json"))?;
            self.claude_global.store(true, SeqCst);
            Ok("已开启：之后新开的 Claude Code（包括 IDE 插件）都走住宅IP".into())
        } else {
            claude::disable(port, &self.path("claude-original-env.json"))?;
            self.claude_global.store(false, SeqCst);
            Ok("已恢复 Claude Code 原来的代理设置".into())
        }
    }

    fn restore_claude(&self, note: Option<&str>) {
        match claude::disable(self.config().local_port, &self.path("claude-original-env.json")) {
            Ok(()) => {
                self.claude_global.store(false, SeqCst);
                if let Some(n) = note {
                    self.notify(n, false);
                }
            }
            Err(e) => self.notify(format!("恢复 settings.json 失败：{e}"), true),
        }
    }

    // ───────────── 设置 ─────────────

    pub async fn save_config(self: &Arc<Self>, new: ProxyConfig) -> Result<String, String> {
        new.validate()?;
        let old = self.config();
        if self.claude_global.load(SeqCst) && old.local_port != new.local_port {
            self.restore_claude(None);
        }
        new.save(&self.path("config.json"))?;
        *self.config.lock().unwrap() = new.clone();
        if self.engine.is_running() || (new.auto_start && !old.is_complete()) {
            self.start_chain().await?;
            Ok("已保存，链路已重启".into())
        } else {
            Ok("已保存".into())
        }
    }
}

async fn port_open(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

fn banner(cfg: &ProxyConfig, s: &Status) -> Banner {
    let place = |h: &Hop| {
        let mut p = country_name(&h.country);
        if !h.city.is_empty() {
            p = format!("{p} {}", h.city);
        }
        p
    };
    let hop1 = s.hop1.as_ref().map_or("—".into(), |h| {
        format!("{} {}（{}）", if cfg.front_enabled { "前置节点" } else { "本机直连" }, h.ip, place(h))
    });
    let (hop2, seen_ip) = s.hop2.as_ref().map_or(("住宅IP".into(), "—".into()), |h| {
        (format!("住宅IP {}（{}）", h.ip, place(h)), h.ip.clone())
    });
    Banner { hop1, hop2, seen_ip }
}

fn country_name(code: &str) -> String {
    let name = match code {
        "JP" => "日本",
        "US" => "美国",
        "HK" => "中国香港",
        "TW" => "中国台湾",
        "SG" => "新加坡",
        "KR" => "韩国",
        "GB" => "英国",
        "DE" => "德国",
        "FR" => "法国",
        "CA" => "加拿大",
        "AU" => "澳大利亚",
        "NL" => "荷兰",
        "CN" => "中国",
        "MY" => "马来西亚",
        "TH" => "泰国",
        "VN" => "越南",
        "PH" => "菲律宾",
        "IN" => "印度",
        "RU" => "俄罗斯",
        "BR" => "巴西",
        _ => return code.to_string(),
    };
    name.to_string()
}
