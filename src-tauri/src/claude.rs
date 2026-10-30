//! Claude Code 集成：
//! - 全局模式：切换 ~/.claude/settings.json 里的代理变量（开启前记录原值，关闭时还原）
//! - 单窗口模式：新终端里用 `claude --settings <文件>` 启动，不改任何全局配置

use regex::Regex;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// 小写变量优先级更高（Node 的 proxy-from-env 先读小写），所以大小写都要设
pub const KEYS: [&str; 6] = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"];

pub fn settings_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CLAUDECHAIN_CLAUDE_SETTINGS").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap_or_default().join(".claude").join("settings.json")
}

fn proxy_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub fn is_enabled(port: u16) -> bool {
    let Ok(root) = read_root() else { return false };
    let env = root.get("env").and_then(Value::as_object);
    KEYS.iter().all(|k| env.and_then(|e| e.get(*k)).and_then(Value::as_str) == Some(proxy_url(port).as_str()))
}

pub fn enable(port: u16, state_path: &Path, backup_path: &Path) -> Result<(), String> {
    let root = read_root()?;
    if !state_path.exists() {
        let env = root.get("env").and_then(Value::as_object);
        let original: Map<String, Value> = KEYS
            .iter()
            .map(|k| (k.to_string(), env.and_then(|e| e.get(*k)).filter(|v| v.is_string()).cloned().unwrap_or(Value::Null)))
            .collect();
        write_atomic(state_path, &serde_json::to_vec_pretty(&original).unwrap(), true)?;
    }
    // 第一次修改前整份备份，永久保留
    let path = settings_path();
    if !backup_path.exists() && path.exists() {
        let _ = std::fs::copy(&path, backup_path);
    }
    let url = proxy_url(port);
    apply(&KEYS.iter().map(|k| (*k, Some(url.clone()))).collect::<Vec<_>>())
}

pub fn disable(port: u16, state_path: &Path) -> Result<(), String> {
    let root = read_root()?;
    let env = root.get("env").and_then(Value::as_object);
    let original: Option<Map<String, Value>> =
        std::fs::read(state_path).ok().and_then(|d| serde_json::from_slice(&d).ok());
    // 只还原仍指向本程序的变量；原本不存在的直接删除
    let url = proxy_url(port);
    let updates: Vec<(&str, Option<String>)> = KEYS
        .iter()
        .filter(|k| env.and_then(|e| e.get(**k)).and_then(Value::as_str) == Some(url.as_str()))
        .map(|k| (*k, original.as_ref().and_then(|o| o.get(*k)).and_then(Value::as_str).map(String::from)))
        .collect();
    if !updates.is_empty() {
        apply(&updates)?;
    }
    let _ = std::fs::remove_file(state_path);
    Ok(())
}

pub fn read_root() -> Result<Map<String, Value>, String> {
    let data = match std::fs::read(settings_path()) {
        Ok(d) if !d.iter().all(u8::is_ascii_whitespace) => d,
        _ => return Ok(Map::new()),
    };
    match serde_json::from_slice::<Value>(&data) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err("~/.claude/settings.json 不是有效的 JSON，为安全起见没有修改".into()),
    }
}

fn apply(updates: &[(&str, Option<String>)]) -> Result<(), String> {
    let path = settings_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut root = read_root()?;
    let mut env = root.get("env").and_then(Value::as_object).cloned().unwrap_or_default();
    for (k, v) in updates {
        match v {
            Some(v) => {
                env.insert(k.to_string(), Value::String(v.clone()));
            }
            None => {
                env.shift_remove(*k);
            }
        }
    }
    if env.is_empty() {
        root.shift_remove("env");
    } else {
        root.insert("env".into(), Value::Object(env));
    }
    let expected = Value::Object(root);

    // 优先原地替换以保留用户格式，并校验结构与预期完全一致；否则整体重写
    if let Some(patched) = patch_text(&text, updates) {
        if serde_json::from_str::<Value>(&patched).ok().as_ref() == Some(&expected) {
            return write_atomic(&path, patched.as_bytes(), false);
        }
    }
    let mut data = serde_json::to_vec_pretty(&expected).map_err(|e| e.to_string())?;
    data.push(b'\n');
    write_atomic(&path, &data, false)
}

fn patch_text(text: &str, updates: &[(&str, Option<String>)]) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let mut t = text.to_string();
    for (k, v) in updates {
        let v = v.as_ref()?;
        let re = Regex::new(&format!(r#"("{}"\s*:\s*)"(?:[^"\\]|\\.)*""#, regex::escape(k))).ok()?;
        let mut matches = re.captures_iter(&t);
        let m = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let whole = m.get(0)?.range();
        let replacement = format!("{}{}", &m[1], serde_json::to_string(v).ok()?);
        t.replace_range(whole, &replacement);
    }
    Some(t)
}

/// 原子写入：写临时文件再改名；软链接写到真实文件上，保留原权限
pub fn write_atomic(path: &Path, data: &[u8], private: bool) -> Result<(), String> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().ok_or("无效路径")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".{}.claudechain.tmp", target.file_name().and_then(|n| n.to_str()).unwrap_or("file")));
    std::fs::write(&tmp, data).map_err(|e| format!("写入失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if private { Some(0o600) } else { std::fs::metadata(&target).ok().map(|m| m.permissions().mode()) };
        if let Some(mode) = mode {
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("写入失败：{e}")
    })
}

pub fn write_env_file(path: &Path, port: u16) -> Result<(), String> {
    let env: Map<String, Value> = KEYS.iter().map(|k| (k.to_string(), Value::String(proxy_url(port)))).collect();
    let mut root = Map::new();
    root.insert("env".into(), Value::Object(env));
    write_atomic(path, &serde_json::to_vec_pretty(&root).unwrap(), false)
}

/// 终端里显示的链路信息
pub struct Banner {
    pub hop1: String,
    pub hop2: String,
    pub seen_ip: String,
}

/// 校验并规范化工作目录：支持 ~ 开头和相对路径（相对 base）；传入文件时取它所在的文件夹
pub fn normalize_dir(raw: &str, base: Option<&Path>) -> Result<PathBuf, String> {
    let raw = raw.trim().trim_matches('"');
    if raw.is_empty() {
        return Err("请先选择文件夹".into());
    }
    let mut p = match raw.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            dirs::home_dir().unwrap_or_default().join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(raw),
    };
    if p.is_relative() {
        let base = base.map(Path::to_path_buf).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
        p = base.join(p);
    }
    let mut p: PathBuf = p.components().collect();
    if p.is_file() {
        p = p.parent().map(Path::to_path_buf).unwrap_or(p);
    }
    if !p.is_dir() {
        return Err(format!("文件夹不存在：{}", p.display()));
    }
    Ok(p)
}

fn folder_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// 在新终端窗口里、指定文件夹下启动 Claude Code（仅该窗口走住宅IP）
pub fn launch(data_dir: &Path, env_file: &Path, work_dir: &Path, banner: &Banner) -> Result<(), String> {
    let scripts = data_dir.join("launch");
    std::fs::create_dir_all(&scripts).map_err(|e| e.to_string())?;
    clean_old_scripts(&scripts);
    // 每个文件夹一个脚本，连续启动不同文件夹时不会互相覆盖
    let name = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        work_dir.hash(&mut h);
        format!("claude-{:016x}", h.finish())
    };

    #[cfg(target_os = "macos")]
    {
        let script = scripts.join(format!("{name}.command"));
        write_script(&script, &unix_script("/bin/zsh", env_file, work_dir, banner))?;
        std::process::Command::new("open").arg(&script).spawn().map_err(|e| format!("无法打开终端：{e}"))?;
        Ok(())
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let script = scripts.join(format!("{name}.cmd"));
        std::fs::write(&script, windows_script(env_file, work_dir, banner)).map_err(|e| e.to_string())?;
        std::process::Command::new("cmd")
            .raw_arg(format!("/C start \"Claude Code\" \"{}\"", script.display()))
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW：不闪出中间的 cmd 窗口
            .spawn()
            .map_err(|e| format!("无法打开终端：{e}"))?;
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let script = scripts.join(format!("{name}.sh"));
        write_script(&script, &unix_script("/usr/bin/env bash", env_file, work_dir, banner))?;
        // 用交互式 shell 运行脚本，才能加载 ~/.bashrc / ~/.zshrc 里的 PATH（nvm、fnm 等）
        let shell = std::env::var("SHELL").ok().filter(|s| s.ends_with("/bash") || s.ends_with("/zsh")).unwrap_or("/bin/bash".into());
        let s = script.to_string_lossy().to_string();
        let candidates: [(&str, Vec<&str>); 8] = [
            ("gnome-terminal", vec!["--", &shell, "-i", &s]),
            ("x-terminal-emulator", vec!["-e", &shell, "-i", &s]),
            ("konsole", vec!["-e", &shell, "-i", &s]),
            ("xfce4-terminal", vec!["-x", &shell, "-i", &s]),
            ("kitty", vec![&shell, "-i", &s]),
            ("alacritty", vec!["-e", &shell, "-i", &s]),
            ("wezterm", vec!["start", "--", &shell, "-i", &s]),
            ("xterm", vec!["-e", &shell, "-i", &s]),
        ];
        for (bin, args) in candidates {
            if which(bin) && std::process::Command::new(bin).args(&args).current_dir(work_dir).spawn().is_ok() {
                return Ok(());
            }
        }
        Err("没有找到可用的终端（gnome-terminal / konsole / xterm 等）".into())
    }
}

/// 启动脚本只在打开终端的那一刻有用，超过一天的删掉
fn clean_old_scripts(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let age = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if age.is_some_and(|a| a.as_secs() > 86_400) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(unix)]
fn write_script(path: &Path, content: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content).map_err(|e| e.to_string())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

#[cfg_attr(windows, allow(dead_code))]
fn unix_script(shebang: &str, env_file: &Path, work_dir: &Path, b: &Banner) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    format!(
        r#"#!{shebang}
# 由 ClaudeChain 生成：只有这个窗口里的 Claude Code 走住宅IP
clear
printf '\033]0;%s\007' {title}
printf '\033[2m本机 → %s → \033[0m\033[1;38;5;209m%s\033[0m\n' {hop1} {hop2}
printf '\033[32m▸ Claude 看到的 IP：%s\033[0m\n' {seen}
printf '\033[2m▸ 文件夹：%s\033[0m\n\n' {dir}
cd {dir} || {{ echo "进不去这个文件夹"; exec "$SHELL"; }}
if ! command -v claude >/dev/null 2>&1; then
  echo "没有找到 claude 命令，请先安装 Claude Code：https://docs.claude.com/claude-code"
  exec "$SHELL"
fi
exec claude --settings {env}
"#,
        title = q(&format!("Claude · {}", folder_name(work_dir))),
        hop1 = q(&b.hop1),
        hop2 = q(&b.hop2),
        seen = q(&b.seen_ip),
        dir = q(&work_dir.to_string_lossy()),
        env = q(&env_file.to_string_lossy()),
    )
}

#[cfg_attr(not(windows), allow(dead_code))]
fn windows_script(env_file: &Path, work_dir: &Path, b: &Banner) -> String {
    // 引号外的 echo/title 文本要转义特殊字符；引号内只有 % 需要转义
    let e = |s: &str| {
        s.chars()
            .flat_map(|c| match c {
                '^' | '&' | '|' | '<' | '>' | '(' | ')' => vec!['^', c],
                '%' => vec!['%', '%'],
                c => vec![c],
            })
            .collect::<String>()
    };
    let pct = |s: &str| s.replace('%', "%%");
    let dir = work_dir.display().to_string();
    [
        "@echo off".to_string(),
        "chcp 65001 >nul".into(),
        format!("title Claude - {}", e(&folder_name(work_dir))),
        format!("echo 本机 -^> {} -^> {}", e(&b.hop1), e(&b.hop2)),
        format!("echo Claude 看到的 IP：{}", e(&b.seen_ip)),
        format!("echo 文件夹：{}", e(&dir)),
        "echo.".into(),
        format!("cd /d \"{}\" || (echo 进不去这个文件夹 & pause & exit /b 1)", pct(&dir)),
        "where claude >nul 2>nul || (echo 没有找到 claude 命令，请先安装 Claude Code & pause & exit /b 1)".into(),
        format!("claude --settings \"{}\"", pct(&env_file.display().to_string())),
        String::new(),
    ]
    .join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // 测试共用环境变量，串行执行
    static LOCK: Mutex<()> = Mutex::new(());

    fn setup(name: &str, content: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("claudechain-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        std::fs::write(&settings, content).unwrap();
        std::env::set_var("CLAUDECHAIN_CLAUDE_SETTINGS", &settings);
        (settings, dir.join("state.json"), dir.join("backup.json"))
    }

    const REAL: &str = "{\n  \"env\": {\n    \"HTTP_PROXY\": \"http://127.0.0.1:7890\",\n    \"HTTPS_PROXY\": \"http://127.0.0.1:7890\",\n    \"ALL_PROXY\": \"http://127.0.0.1:7890\",\n    \"http_proxy\": \"http://127.0.0.1:7890\",\n    \"https_proxy\": \"http://127.0.0.1:7890\",\n    \"all_proxy\": \"http://127.0.0.1:7890\",\n    \"CLAUDE_CODE_SCROLL_SPEED\": \"0.25\"\n  },\n  \"model\": \"opus\",\n  \"hooks\": {\"x\": [{\"command\": \"echo \\\"HTTP_PROXY\\\": hi\"}]}\n}\n";

    #[test]
    fn roundtrip_preserves_bytes() {
        let _g = LOCK.lock().unwrap();
        let (settings, state, backup) = setup("roundtrip", REAL);
        assert!(!is_enabled(7891));
        enable(7891, &state, &backup).unwrap();
        assert!(is_enabled(7891));
        let after = std::fs::read_to_string(&settings).unwrap();
        let changed = REAL.lines().zip(after.lines()).filter(|(a, b)| a != b).count();
        assert_eq!((REAL.lines().count(), changed), (after.lines().count(), 6), "只改 6 行");
        enable(7891, &state, &backup).unwrap(); // 重复开启不覆盖原值记录
        disable(7891, &state).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), REAL);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), REAL);
    }

    #[test]
    fn no_env_and_user_edits() {
        let _g = LOCK.lock().unwrap();
        let (settings, state, backup) = setup("noenv", "{\n  \"model\": \"opus\"\n}\n");
        enable(7891, &state, &backup).unwrap();
        assert!(is_enabled(7891));
        // 开启期间用户手动改了一项
        let mut root = read_root().unwrap();
        root["env"]["HTTPS_PROXY"] = Value::String("http://10.0.0.1:1".into());
        std::fs::write(&settings, serde_json::to_vec(&root).unwrap()).unwrap();
        disable(7891, &state).unwrap();
        let root = read_root().unwrap();
        assert_eq!(root["model"], "opus");
        assert_eq!(root["env"].as_object().unwrap().len(), 1, "只保留用户手改的那一项");
        assert_eq!(root["env"]["HTTPS_PROXY"], "http://10.0.0.1:1");
    }

    #[test]
    fn refuses_invalid_json() {
        let _g = LOCK.lock().unwrap();
        let (settings, state, backup) = setup("invalid", "{ broken");
        assert!(enable(7891, &state, &backup).is_err());
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{ broken");
    }

    #[test]
    fn scripts_quote_safely() {
        let b = Banner { hop1: "前置节点 1.2.3.4（日本）".into(), hop2: "住宅IP 5.6.7.8 it's".into(), seen_ip: "5.6.7.8".into() };
        let s = unix_script("/bin/zsh", Path::new("/tmp/a b/env.json"), Path::new("/tmp/my 'proj'"), &b);
        assert!(s.contains(r"'住宅IP 5.6.7.8 it'\''s'") && s.contains("--settings '/tmp/a b/env.json'"));
        assert!(s.contains(r"cd '/tmp/my '\''proj'\''' ||"), "{s}");
        assert!(s.contains(r"'Claude · my '\''proj'\'''"), "{s}");
        let w = windows_script(
            Path::new(r"C:\Users\a b\env.json"),
            Path::new(r"D:\code\R&D (100%)"),
            &Banner { hop1: "a&b>(c)%".into(), ..b },
        );
        assert!(w.contains("a^&b^>^(c^)%%") && w.contains(r#"--settings "C:\Users\a b\env.json""#), "{w}");
        assert!(w.contains(r#"cd /d "D:\code\R&D (100%%)" || (echo"#), "{w}");
        // 在 Windows 上标题是文件夹名，其他平台测试时是整个路径，结尾都一样
        assert!(w.lines().any(|l| l.starts_with("title Claude - ") && l.ends_with("R^&D ^(100%%^)")), "{w}");
        assert!(w.contains("请先安装 Claude Code & pause & exit /b 1)"), "括号里的 & 不能被转义");
    }

    #[test]
    fn normalizes_dirs() {
        let tmp = std::env::temp_dir().join(format!("claudechain-dir-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("sub").join("file.txt"), "x").unwrap();
        let want: PathBuf = tmp.join("sub").components().collect();
        assert_eq!(normalize_dir("sub", Some(&tmp)).unwrap(), want);
        assert_eq!(normalize_dir("./sub/.", Some(&tmp)).unwrap(), want);
        assert_eq!(normalize_dir(&tmp.join("sub").join("file.txt").to_string_lossy(), None).unwrap(), want, "文件取所在文件夹");
        assert_eq!(normalize_dir("~", None).unwrap(), dirs::home_dir().unwrap().components().collect::<PathBuf>());
        assert!(normalize_dir("nope-not-here", Some(&tmp)).is_err());
        assert!(normalize_dir("  ", None).is_err());
    }
}
