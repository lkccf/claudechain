# ClaudeChain

让 Claude Code 走住宅IP的链式代理，支持 macOS / Windows / Ubuntu（Rust + Tauri 2）。

```
本机 ──▶ ① 前置节点（FlClash 等） ──▶ ② 住宅IP ──▶ Claude
                                     └─ Claude 只看得到这一跳
```

- **两跳都看得见**：面板上分别显示第 1 跳和第 2 跳的出口 IP、位置、运营商和延迟
- **一键启动**：点「启动 Claude Code」后先检测链路，通过了才在新终端里启动；链路不通就不启动，不会用错 IP 出去。默认不会自动弹出终端
- **分流**：默认只有 Claude 相关域名走住宅IP，Claude Code 执行的 npm、git、brew 等命令照常走前置代理，节省住宅流量
- **漏网检查**：检测这台机器不经代理能不能连上 Claude，能连上的话会显示 Claude 会看到哪个 IP
- **不改系统网络**：不碰系统代理、不碰 FlClash、不用 TUN。代理只在 `127.0.0.1:7891` 上监听，同时支持 HTTP 和 SOCKS5

## 使用

1. 打开 ClaudeChain，点右上角 ⚙，把代理链接粘进去后点「识别」。支持 `socks5://user:pass@host:port`、二维码里的 Base64 格式和 `host:port:user:pass`
2. 「前置代理」填本机 FlClash 的混合端口（默认 `127.0.0.1:7890`），也可以点「自动查找」
3. 保存。以后打开 App 会自动连接链路
4. 选好项目文件夹，点「启动 Claude Code」。选文件夹有三种方式：点「选择…」、点最近用过的文件夹，或者把文件夹拖进窗口。拖进来只会选中，不会自动启动

关掉窗口只是隐藏到托盘，链路会继续工作；再次打开 App 只会把面板调出来。想打开 App 就自动启动 Claude Code，可以在 设置 › 启动 里勾选。

| 启动方式 | 作用范围 | 是否修改配置 |
|---|---|---|
| 「启动 Claude Code」按钮 | 只有这个终端窗口 | 不修改（用 `claude --settings` 覆盖） |
| 设置 › 高级 › 全局走住宅IP | 所有 Claude Code 会话，包括 IDE 插件 | 修改 `~/.claude/settings.json`；关闭开关、停止链路或退出 App 时自动还原 |

在终端里也可以直接用，跟 `code .` 一样：`claudechain ~/projects/foo` 或 `claudechain .`。App 没开的话会先连接链路，再在这个文件夹里启动 Claude Code。macOS 上的可执行文件在 `/Applications/ClaudeChain.app/Contents/MacOS/claudechain`。

没有桌面的服务器上可以用无界面模式：`claudechain --headless`。它会启动链路并每 30 秒打印一次检测结果，Claude Code 里设置 `HTTPS_PROXY=http://127.0.0.1:7891` 即可。

## 在虚拟机里使用：怎么做到不漏网

### 1. ClaudeChain 装在虚拟机里

装在和 Claude Code 同一台系统里。虚拟机里的 `127.0.0.1:7890` 并不是宿主机上的 FlClash，所以：

- 在**宿主机**的 FlClash 里打开「允许局域网连接（Allow LAN）」
- 在虚拟机里的 ClaudeChain 设置中点「自动查找」，或者手动填宿主机地址。常见的宿主机地址：
  - Parallels：`10.211.55.2`
  - UTM / Apple 虚拟化：`192.168.64.1`
  - VirtualBox NAT：`10.0.2.2`
  - VMware：虚拟网段的 `.1`

### 2. 哪些情况会漏网

| 情况 | 会不会漏 | 说明 |
|---|---|---|
| 用 ClaudeChain 启动的 Claude Code | 不会 | 全部请求都经本地代理；域名在住宅代理那边解析，本机不查 DNS |
| 在虚拟机里直接敲 `claude`，没开全局模式 | **会** | 它用的是自己的代理设置，或者根本没设代理 |
| 其他程序直连 Claude（浏览器打开 claude.ai 等） | **会** | 没走代理的流量从虚拟机的 NAT 出去，Claude 看到的是你的真实 IP |
| 分流模式下的非 Claude 域名 | 不算漏 | 走前置节点（FlClash）出去，不会用真实 IP |

面板上的「漏网检查」会实际测一遍：显示「可直连 Claude」，说明上面加粗的两种情况确实可能发生。

### 3. 彻底杜绝：在虚拟机里封掉直连（可选）

让虚拟机只能连宿主机上的前置代理，其他直连一律不通。这样就算有程序没走代理，也发不出去。**这只影响虚拟机，宿主机的网络不受影响**。但开启后，虚拟机里所有要上网的程序都得走代理。

先把下面的 `HOST` 和 `PORT` 换成你的前置代理地址。

**Ubuntu（ufw）**

```bash
HOST=192.168.64.1 PORT=7890
sudo ufw default deny incoming
sudo ufw default deny outgoing
sudo ufw allow out to $HOST port $PORT proto tcp   # 只放行前置代理
sudo ufw allow out 67/udp                          # DHCP 续租
sudo ufw enable
# 撤销：sudo ufw default allow outgoing
```

**Windows（以管理员身份运行 PowerShell）**

```powershell
$HostIP = "192.168.64.1"; $Port = 7890
New-NetFirewallRule -DisplayName "ClaudeChain 前置代理" -Direction Outbound -Action Allow -Protocol TCP -RemoteAddress $HostIP -RemotePort $Port
Set-NetFirewallProfile -Profile Domain,Private,Public -DefaultOutboundAction Block
# 撤销：
# Set-NetFirewallProfile -Profile Domain,Private,Public -DefaultOutboundAction Allow
# Remove-NetFirewallRule -DisplayName "ClaudeChain 前置代理"
```

设置好以后，「漏网检查」应该变成 ✓「无法直连 Claude」。如果没变，说明系统里还有别的放行规则，Windows 上比较常见。

更彻底的办法是把虚拟机网卡改成「仅主机（Host-only）」模式：虚拟机根本没有通往外网的路由，只能连到宿主机，前置代理填宿主机在仅主机网段上的地址即可。

### 4. 再开「全局走住宅IP」

在虚拟机里打开 设置 › 高级 › Claude Code 全局走住宅IP。这样直接敲 `claude` 也会走住宅IP，不用每次都从 App 启动。

## 构建

```bash
npm install
npx tauri build           # 当前平台的安装包
cargo test --manifest-path src-tauri/Cargo.toml
```

三个平台的安装包由 GitHub Actions 构建（见 `.github/workflows/build.yml`），推送 `v*` 标签就会生成草稿 Release。Linux 构建依赖 `libwebkit2gtk-4.1-dev` 和 `libayatana-appindicator3-dev`。

## 数据文件

| 平台 | 位置 |
|---|---|
| macOS | `~/Library/Application Support/ClaudeChain/` |
| Windows | `%APPDATA%\ClaudeChain\` |
| Linux | `~/.local/share/ClaudeChain/` |

目录里有这些文件：

- `config.json`：配置，包含代理密码，文件权限为 600
- `claude-env.json`：单窗口启动时给 `--settings` 用的配置
- `recent.json`：最近打开过的文件夹
- `launch/`：启动终端用的临时脚本，超过一天自动清理
- `claude-original-env.json`：开启全局模式前记录的原代理变量
- `claude-settings.backup.json`：第一次修改 settings.json 前留的完整备份
