const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const form = $('form');

let lastNoticeId = 0;
let busy = false;
let current = null;
let launchDir = null;
let dirs = { current: '', recent: [], home: '' };

const regionNames = (() => {
  try { return new Intl.DisplayNames(['zh-CN'], { type: 'region' }); } catch { return null; }
})();
const country = (code) => (code && regionNames?.of(code)) || code || '';
const place = (h) => [[country(h.country), h.city].filter(Boolean).join(' '), h.org].filter(Boolean).join(' · ');
const ms = (v) => (v == null ? '' : v < 1000 ? `${v} ms` : `${(v / 1000).toFixed(2)} s`);
const bytes = (b) => {
  if (b < 1024) return `${b} B`;
  const units = ['KB', 'MB', 'GB'];
  let i = -1;
  do { b /= 1024; i++; } while (b >= 1024 && i < units.length - 1);
  return `${b.toFixed(b < 10 ? 1 : 0)} ${units[i]}`;
};
const ago = (s) => (s < 5 ? '刚刚' : s < 60 ? `${s} 秒前` : s < 3600 ? `${Math.floor(s / 60)} 分钟前` : `${Math.floor(s / 3600)} 小时前`);
const ROUTE = { residential: '住宅IP', front: '前置', direct: '本地' };

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

function toast(text, error = false) {
  const t = $('toast');
  t.textContent = text;
  t.className = error ? 'toast err' : 'toast';
  t.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (t.hidden = true), error ? 8000 : 4000);
}

async function call(cmd, args = {}) {
  try {
    const r = await invoke(cmd, args);
    if (typeof r === 'string') toast(r);
    return r;
  } catch (e) {
    toast(String(e), true);
    throw e;
  }
}

// ───────────── 主界面 ─────────────

function setHop(id, hop, err, running, emptyMeta) {
  const li = $(id);
  li.classList.toggle('up', running && !!hop);
  li.classList.toggle('down', running && !hop && !!err);
  $(`${id}-ip`).textContent = hop ? hop.ip : running ? (err ? '不通' : '检测中…') : '—';
  $(`${id}-meta`).textContent = hop ? place(hop) || emptyMeta : running && err ? err : emptyMeta;
  $(`${id}-meta`).title = running && err ? err : '';
  $(`${id}-ms`).textContent = hop ? ms(hop.latencyMs) : '';
}

function render(s) {
  current = s;
  const st = s.status;
  const h = st.health;
  const ok = s.running && h.kind === 'ok';

  const [label, color] = !s.configured ? ['未配置住宅代理', '']
    : !s.running ? ['已停止', '']
    : h.kind === 'ok' ? ['链路正常', 'green']
    : h.kind === 'failed' ? ['链路异常', 'red']
    : ['正在连接…', 'amber'];
  $('state-text').textContent = st.checking && h.kind === 'ok' ? '正在检测…' : label;
  $('dot').className = `dot ${color}${st.checking || h.kind === 'starting' ? ' pulse' : ''}`;
  $('chain').checked = s.running;
  $('chain').disabled = !s.configured || busy;
  $('btn-check').disabled = !s.running || st.checking;

  $('hop1-title').textContent = s.frontEnabled ? '前置节点' : '本机直连';
  $('hop1-via').textContent = s.frontEnabled ? `经 ${s.frontLabel}` : '';
  setHop('hop1', st.hop1, st.hop1Error, s.running, s.frontEnabled ? 'FlClash 等本机代理的出口' : '你的真实出口');
  setHop('hop2', st.hop2, st.hop2Error, s.running, '住宅代理的出口');
  $('dest').classList.toggle('up', s.running && st.apiMs != null);
  $('api-ms').textContent = !s.running ? '—' : st.apiMs != null ? ms(st.apiMs) : st.apiError ? '不通' : '检测中…';
  $('api-ms').className = s.running && st.apiError ? 'bad' : 'muted';

  $('seen').hidden = !(ok && st.hop2);
  if (st.hop2) $('seen-ip').textContent = st.hop2.ip;
  const alertText = !s.configured ? '还没有填写住宅代理，点右上角的设置按钮填写。'
    : s.running && h.kind === 'failed' ? h.message : '';
  $('alert').hidden = !alertText;
  $('alert').textContent = alertText;

  $('btn-launch').disabled = !s.configured || busy;

  const t = s.traffic;
  $('totals').textContent = s.running ? `住宅IP ${bytes(t.residentialBytes)} · 前置 ${bytes(t.frontBytes)}` : '';
  $('hosts').replaceChildren(...t.hosts.map((row) => {
    const li = el('li', row.error ? 'err' : '');
    const meta = row.error ? '连接失败' : row.active ? `${bytes(row.bytes)} · 进行中` : `${bytes(row.bytes)} · ${ago(row.agoSecs)}`;
    li.append(el('span', `badge ${row.route}`, ROUTE[row.route]), el('span', 'host', row.host), el('span', row.active && !row.error ? 'meta live' : 'meta', meta));
    li.title = row.error || `${row.host} → ${ROUTE[row.route]}`;
    return li;
  }));
  $('hosts-empty').hidden = t.hosts.length > 0;

  renderLeak(s);

  $('global').checked = s.claudeGlobal;
  $('global').disabled = !s.claudeGlobal && !ok;
  $('data-dir').textContent = `数据目录：${s.dataDir}`;

  if (s.notice && s.notice.id !== lastNoticeId) {
    lastNoticeId = s.notice.id;
    toast(s.notice.text, s.notice.error);
  }
}

function renderLeak(s) {
  const st = s.status;
  $('leak-card').hidden = !(s.running && st.directChecked);
  if ($('leak-card').hidden) return;
  const badge = $('leak-badge');
  const text = $('leak-text');
  if (st.direct) {
    badge.className = 'pill warn';
    badge.textContent = '可直连 Claude';
    const where = place(st.direct);
    text.replaceChildren(
      '这台机器不经代理也能连上 Claude，Claude 会看到 ', el('b', null, st.direct.ip), where ? `（${where}）` : '',
      '。如果有 Claude 流量没走代理，就会暴露这个 IP。用本程序启动的 Claude Code 全部走住宅IP，不受影响；要彻底杜绝，可以在虚拟机里打开防火墙防漏网（见使用说明）。',
    );
  } else {
    badge.className = 'pill ok';
    badge.textContent = '无法直连 Claude';
    text.textContent = '这台机器不经代理连不上 Claude，没走代理的 Claude 流量发不出去，不会漏网。';
  }
}

async function refresh() {
  try { render(await invoke('get_state')); } catch { /* 下一轮再试 */ }
}

$('chain').addEventListener('change', async (e) => {
  busy = true;
  try { await call(e.target.checked ? 'start_chain' : 'stop_chain'); } catch { /* 已提示 */ }
  busy = false;
  refresh();
});

$('btn-check').addEventListener('click', async () => {
  $('btn-check').disabled = true;
  await call('check_now').catch(() => {});
  refresh();
});

// ───────────── 启动文件夹 ─────────────

const baseName = (p) => p.replace(/[\\/]+$/, '').split(/[\\/]/).pop() || p;
const tildify = (p) => (dirs.home && (p === dirs.home || p.startsWith(dirs.home + '/') || p.startsWith(dirs.home + '\\')) ? '~' + p.slice(dirs.home.length) : p);

function renderDirs() {
  $('folder-name').textContent = launchDir ? baseName(launchDir) : '—';
  $('folder-path').textContent = launchDir ? tildify(launchDir) : '';
  $('folder').title = launchDir || '';
  $('recent').replaceChildren(...dirs.recent.map((d) => {
    const chip = el('span', d === launchDir ? 'dir-chip on' : 'dir-chip');
    const pick = el('button', null, baseName(d));
    pick.type = 'button';
    pick.title = d;
    pick.addEventListener('click', () => { launchDir = d; renderDirs(); });
    const x = el('button', 'x', '×');
    x.type = 'button';
    x.title = '从最近列表移除';
    x.addEventListener('click', async () => { await invoke('forget_dir', { dir: d }); loadDirs(); });
    chip.append(pick, x);
    return chip;
  }));
}

async function loadDirs() {
  try {
    dirs = await invoke('get_launch_dirs');
    if (!launchDir) launchDir = dirs.current;
    renderDirs();
  } catch { /* 下次再试 */ }
}

$('btn-pick').addEventListener('click', async () => {
  const picked = await invoke('pick_folder', { start: launchDir }).catch(() => null);
  if (picked) { launchDir = picked; renderDirs(); }
});

$('btn-launch').addEventListener('click', async () => {
  busy = true;
  $('btn-launch').disabled = true;
  $('launch-text').textContent = '正在检测链路…';
  try { await call('launch_claude', { dir: launchDir }); } catch { /* 已提示 */ }
  $('launch-text').textContent = '启动 Claude Code';
  busy = false;
  loadDirs();
  refresh();
});

// 把文件夹拖进窗口：只选中，不自动启动
(() => {
  const listen = window.__TAURI__.event?.listen;
  if (!listen) return;
  listen('tauri://drag-enter', () => { if (!$('home').hidden) $('drop').hidden = false; });
  listen('tauri://drag-leave', () => { $('drop').hidden = true; });
  listen('tauri://drag-drop', async (e) => {
    $('drop').hidden = true;
    const path = e.payload?.paths?.[0];
    if (!path || !$('settings').hidden) return;
    try {
      launchDir = await invoke('choose_dir', { path });
      renderDirs();
      toast(`已选中 ${tildify(launchDir)}，点「启动 Claude Code」打开`);
    } catch (err) {
      toast(String(err), true);
    }
  });
})();

// ───────────── 设置 ─────────────

function fillForm(cfg) {
  for (const input of form.elements) {
    if (!input.name || !(input.name in cfg)) continue;
    if (input.type === 'checkbox') input.checked = cfg[input.name];
    else input.value = cfg[input.name] || (input.type === 'number' ? '' : '');
  }
  syncForm();
}

function readForm() {
  const cfg = {};
  for (const input of form.elements) {
    if (!input.name) continue;
    cfg[input.name] = input.type === 'checkbox' ? input.checked : input.type === 'number' ? Number(input.value) || 0 : input.value.trim();
  }
  return cfg;
}

function syncForm() {
  const front = form.elements.frontEnabled.checked;
  for (const n of ['frontHost', 'frontPort']) form.elements[n].disabled = !front;
  const split = form.elements.splitMode.checked;
  form.elements.residentialDomains.disabled = !split;
  $('split-note').textContent = split
    ? `其余流量（npm、git、brew 等）${front ? '走前置代理' : '本机直连'}，节省住宅IP流量。`
    : 'Claude Code 的全部流量（包括它执行的命令）都走住宅IP。';
}

function showSettings(on) {
  $('home').hidden = on;
  $('settings').hidden = !on;
  window.scrollTo(0, 0);
}

$('btn-settings').addEventListener('click', async () => {
  if (!$('settings').hidden) return showSettings(false);
  fillForm(await invoke('get_config'));
  showSettings(true);
});
$('btn-back').addEventListener('click', () => showSettings(false));
form.addEventListener('change', syncForm);

$('btn-parse').addEventListener('click', async () => {
  const link = $('link').value.trim();
  if (!link) return;
  try {
    const r = await invoke('parse_link', { link });
    form.elements.host.value = r.host;
    form.elements.port.value = r.port;
    form.elements.username.value = r.username;
    form.elements.password.value = r.password;
    $('link').value = '';
    toast(`已识别 ${r.host}:${r.port}，点「保存」生效`);
  } catch (e) {
    toast(String(e), true);
  }
});
$('link').addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); $('btn-parse').click(); } });

$('btn-find').addEventListener('click', async () => {
  const out = $('find-result');
  $('btn-find').disabled = true;
  out.textContent = '正在查找本机和宿主机上的代理…';
  try {
    const found = await invoke('find_front_proxies');
    if (!found.length) {
      out.textContent = '没找到。虚拟机里请确认宿主机 FlClash 已打开「允许局域网连接」。';
    } else {
      out.replaceChildren(...found.map((addr) => {
        const chip = el('button', 'chip', addr);
        chip.type = 'button';
        chip.title = '使用这个前置代理';
        chip.addEventListener('click', () => {
          const i = addr.lastIndexOf(':');
          form.elements.frontEnabled.checked = true;
          form.elements.frontHost.value = addr.slice(0, i);
          form.elements.frontPort.value = addr.slice(i + 1);
          syncForm();
          toast(`已填入 ${addr}，点「保存」生效`);
        });
        return chip;
      }));
    }
  } catch (e) {
    out.textContent = String(e);
  }
  $('btn-find').disabled = false;
});

$('btn-eye').addEventListener('click', () => {
  const pw = form.elements.password;
  pw.type = pw.type === 'password' ? 'text' : 'password';
  $('btn-eye').textContent = pw.type === 'password' ? '显示' : '隐藏';
});

form.addEventListener('submit', async (e) => {
  e.preventDefault();
  try {
    await call('save_config', { config: readForm() });
    showSettings(false);
  } catch { /* 已提示 */ }
  refresh();
});

$('global').addEventListener('change', async (e) => {
  try { await call('set_claude_global', { on: e.target.checked }); } catch { e.target.checked = !e.target.checked; }
  refresh();
});

refresh();
loadDirs();
setInterval(refresh, 1000);
