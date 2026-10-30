// Windows 发布版不弹控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod claude;
mod config;
mod probe;
mod proxy;
mod socks;
mod state;

use config::{LinkInfo, ProxyConfig};
use state::{AppState, UiState};
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, State, WindowEvent};

type St<'a> = State<'a, Arc<AppState>>;

#[tauri::command]
fn get_state(st: St) -> UiState {
    st.ui_state()
}

#[tauri::command]
fn get_config(st: St) -> ProxyConfig {
    st.config()
}

#[tauri::command]
async fn save_config(st: St<'_>, config: ProxyConfig) -> Result<String, String> {
    st.inner().clone().save_config(config).await
}

#[tauri::command]
fn parse_link(link: String) -> Result<LinkInfo, String> {
    config::parse_link(&link).ok_or_else(|| "无法识别这个链接".into())
}

#[tauri::command]
async fn find_front_proxies(st: St<'_>) -> Result<Vec<String>, String> {
    Ok(probe::find_socks_proxies(st.config().local_port).await)
}

#[tauri::command]
async fn start_chain(st: St<'_>) -> Result<(), String> {
    st.inner().clone().start_chain().await
}

#[tauri::command]
fn stop_chain(st: St) {
    st.stop_chain();
}

#[tauri::command]
async fn check_now(st: St<'_>) -> Result<(), String> {
    st.check().await;
    Ok(())
}

#[tauri::command]
async fn launch_claude(st: St<'_>, dir: Option<String>) -> Result<String, String> {
    let dir = dir.map(|d| claude::normalize_dir(&d, None)).transpose()?;
    st.inner().clone().launch_claude(dir).await
}

#[tauri::command]
fn get_launch_dirs(st: St) -> state::LaunchDirs {
    st.launch_dirs()
}

/// 校验拖进来的路径：文件取所在文件夹
#[tauri::command]
fn choose_dir(path: String) -> Result<String, String> {
    claude::normalize_dir(&path, None).map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn forget_dir(st: St, dir: String) {
    st.forget_dir(&dir);
}

/// 系统原生的文件夹选择框
#[tauri::command]
async fn pick_folder(app: AppHandle, start: Option<String>) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = app.dialog().file().set_title("选择要打开的项目文件夹");
        if let Some(dir) = start.filter(|d| std::path::Path::new(d).is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(w) = app.get_webview_window("main") {
            dialog = dialog.set_parent(&w);
        }
        dialog.blocking_pick_folder().and_then(|p| p.into_path().ok()).map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten()
}

/// 命令行参数里的文件夹（claudechain ~/projects/foo），相对路径按 cwd 解析
fn dir_arg(args: &[String], cwd: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    args.iter().skip(1).filter(|a| !a.starts_with('-')).find_map(|a| claude::normalize_dir(a, cwd).ok())
}

#[tauri::command]
fn set_claude_global(st: St, on: bool) -> Result<String, String> {
    st.set_claude_global(on)
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 托盘菜单 / 命令行：启动 Claude Code，失败就把面板弹出来显示原因
fn launch_in(app: &AppHandle, dir: Option<std::path::PathBuf>) {
    let st = app.state::<Arc<AppState>>().inner().clone();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = st.launch_claude(dir).await {
            st.notify(e, true);
            show_main(&app);
        }
    });
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let launch = MenuItem::with_id(app, "launch", "在上次的文件夹启动 Claude Code", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 ClaudeChain", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&launch, &show, &PredefinedMenuItem::separator(app)?, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("ClaudeChain · Claude Code 住宅IP链路")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "launch" => launch_in(app, None),
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// 无界面模式：只跑链路并定时打印状态，适合服务器（claudechain --headless）
fn headless() {
    let rt = tokio::runtime::Runtime::new().expect("tokio");
    rt.block_on(async {
        let st = AppState::new();
        let cfg = st.config();
        if let Err(e) = st.start_chain().await {
            eprintln!("✗ 启动失败：{e}");
            std::process::exit(1);
        }
        println!("本地代理已启动：http://127.0.0.1:{}（HTTP 和 SOCKS5 均可），Ctrl+C 退出", cfg.local_port);
        loop {
            let s = st.ui_state().status;
            let hop = |name: &str, h: &Option<probe::Hop>, e: &Option<String>| match h {
                Some(h) => println!("  {name}  {:<15}  {} {} {}  {} ms", h.ip, h.country, h.city, h.org, h.latency_ms),
                None => println!("  {name}  不通：{}", e.as_deref().unwrap_or("—")),
            };
            match &s.health {
                state::Health::Ok => println!("✓ 链路正常"),
                state::Health::Failed(m) => println!("✗ {m}"),
                _ => println!("… 检测中"),
            }
            hop(if cfg.front_enabled { "第1跳 前置节点" } else { "第1跳 本机直连" }, &s.hop1, &s.hop1_error);
            hop("第2跳 住宅IP  ", &s.hop2, &s.hop2_error);
            match (s.api_ms, &s.api_error) {
                (Some(ms), _) => println!("  Claude API      {ms} ms"),
                (_, e) => println!("  Claude API      不通：{}", e.as_deref().unwrap_or("—")),
            }
            match (&s.direct, s.direct_checked) {
                (Some(h), true) => println!("  漏网检查        ⚠ 不经代理也能连上 Claude，会暴露 {} {} {}", h.ip, h.country, h.org),
                (None, true) => println!("  漏网检查        ✓ 不经代理连不上 Claude，不会漏网"),
                _ => {}
            }
            for h in st.ui_state().traffic.hosts.iter().take(8) {
                println!("    {:<12} {:?}  {} B", h.host, h.route, h.bytes);
            }
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => st.check().await,
            }
        }
        st.shutdown();
    });
}

fn main() {
    if std::env::args().any(|a| a == "--headless") {
        return headless();
    }
    let state = AppState::new();
    let setup_state = state.clone();
    let args: Vec<String> = std::env::args().collect();
    let cli_dir = dir_arg(&args, None);

    tauri::Builder::default()
        // 已在运行时再次打开：只把面板调出来；命令行带了文件夹就在那里启动
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            show_main(app);
            if let Some(dir) = dir_arg(&args, Some(std::path::Path::new(&cwd))) {
                launch_in(app, Some(dir));
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_config,
            save_config,
            parse_link,
            find_front_proxies,
            start_chain,
            stop_chain,
            check_now,
            launch_claude,
            get_launch_dirs,
            choose_dir,
            forget_dir,
            pick_folder,
            set_claude_global
        ])
        .setup(move |app| {
            build_tray(app)?;
            let st = setup_state.clone();
            tauri::async_runtime::spawn(async move { st.bootstrap(cli_dir).await });
            Ok(())
        })
        // 关窗口只是隐藏到托盘，链路继续工作
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("ClaudeChain 启动失败")
        .run(|app, event| match event {
            RunEvent::Exit => app.state::<Arc<AppState>>().shutdown(),
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => show_main(app),
            _ => {}
        });
}
