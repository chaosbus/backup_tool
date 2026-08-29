const invoke = window.__TAURI__.core.invoke;
const { Channel } = window.__TAURI__.core;

const $ = (sel) => document.querySelector(sel);

const state = {
  apps: [],
  running: false,
  appProg: new Map(),
  selectedAppId: null,
  // Runtime backup selection per app id; apps absent from the map default to
  // their `enabled` config value. Survives list re-renders.
  userChecked: new Map(),
};

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function fmtBytes(n) {
  if (!n) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

function renderApps() {
  const list = $("#app-list");
  list.innerHTML = "";
  if (!state.apps.length) {
    list.innerHTML = '<div class="empty">暂无应用。点击 [+ 添加应用] 添加。</div>';
  }
  for (const app of state.apps) {
    const li = document.createElement("li");
    li.className = "app-item";
    if (state.selectedAppId === app.id) li.classList.add("selected");

    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = state.userChecked.has(app.id)
      ? state.userChecked.get(app.id)
      : app.enabled;
    cb.disabled = state.running || !app.has_paths;
    cb.dataset.id = app.id;
    cb.addEventListener("change", () => {
      state.userChecked.set(app.id, cb.checked);
      updateSelectAll();
    });

    const name = document.createElement("span");
    name.className = "app-name";
    name.textContent = app.name;

    const meta = document.createElement("span");
    meta.className = "app-meta";
    meta.textContent = app.last_backup ? `上次: ${app.last_backup}` : "从未备份";

    const badge = document.createElement("span");
    if (app.has_missing) {
      badge.className = "badge missing";
      badge.textContent = "路径缺失";
    } else if (!app.has_paths) {
      badge.className = "badge nopath";
      badge.textContent = "无路径";
    }

    li.append(cb, name, meta, badge);
    li.addEventListener("click", (e) => {
      if (e.target.tagName === "INPUT" || e.target.tagName === "BUTTON") return;
      selectApp(app.id);
    });
    li.addEventListener("dblclick", () => openEdit(app.id));
    list.appendChild(li);
  }
  updateSelectAll();
  updateSelectionUi();
}

function updateSelectAll() {
  const boxes = [...document.querySelectorAll("#app-list input[type=checkbox]")];
  const checked = boxes.filter((b) => b.checked).length;
  const selAll = $("#select-all");
  selAll.checked = boxes.length > 0 && checked === boxes.length;
  selAll.indeterminate = checked > 0 && checked < boxes.length;
  $("#apps-count").textContent = `(${checked} / ${boxes.length} 已选)`;
  $("#btn-backup").disabled = state.running || checked === 0;
}

function updateSelectionUi() {
  const has = state.selectedAppId != null
    && state.apps.some((a) => a.id === state.selectedAppId);
  if (!has) state.selectedAppId = null;
  $("#btn-edit-app").disabled = !has;
  $("#btn-delete-app").disabled = !has;
}

function selectApp(id) {
  state.selectedAppId = id;
  renderApps();
}

function selectedIds() {
  return [...document.querySelectorAll("#app-list input[type=checkbox]:checked")]
    .map((b) => b.dataset.id);
}

function renderHistory(entries) {
  const list = $("#history-list");
  list.innerHTML = "";
  if (!entries.length) {
    list.innerHTML = '<div class="empty">暂无备份记录</div>';
  }
  for (const e of entries) {
    const li = document.createElement("li");
    li.className = "history-item";
    const f = document.createElement("span");
    f.className = "fname";
    f.textContent = e.file;
    const m = document.createElement("span");
    m.className = "hmeta";
    m.textContent = `${fmtBytes(e.size)} · ${e.status}`;
    li.append(f, m);
    list.appendChild(li);
  }
  $("#history-count").textContent = `${entries.length} 条记录`;
}

function log(msg, level) {
  const div = $("#log");
  const line = document.createElement("div");
  const time = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  const cls = level === "warn" ? "lwarn" : level === "error" ? "lerr" : level === "ok" ? "lok" : "";
  const stamp = document.createElement("span");
  stamp.className = "ln";
  stamp.textContent = time;
  const message = document.createElement("span");
  message.textContent = msg;
  line.append(stamp, message);
  if (cls) line.className = cls;
  div.appendChild(line);
  while (div.childNodes.length > 400) div.removeChild(div.firstChild);
  div.scrollTop = div.scrollHeight;
}

function resetProgress() {
  state.appProg.clear();
  $("#overall-bar").style.width = "0%";
  $("#overall-text").textContent = "—";
  $("#app-progress").innerHTML = "";
}

function renderAppRow(appId, data) {
  let row = state.appProg.get(appId);
  if (!row) {
    row = document.createElement("div");
    row.className = "app-prog-row";
    row.innerHTML = `
      <span class="name"></span>
      <div class="bar"><div class="bar-fill"></div></div>
      <span class="st">等待中</span>`;
    $("#app-progress").appendChild(row);
    state.appProg.set(appId, row);
  }
  const name = row.querySelector(".name");
  const fill = row.querySelector(".bar-fill");
  const st = row.querySelector(".st");
  name.textContent = appId;
  if (data) {
    const pct = data.bytes_total > 0
      ? Math.round((data.bytes_done / data.bytes_total) * 100)
      : data.files_total > 0
        ? Math.round((data.files_done / data.files_total) * 100)
        : 0;
    fill.style.width = `${pct}%`;
    st.textContent = `${fmtBytes(data.bytes_done)} / ${fmtBytes(data.bytes_total)}`;
  }
  row.dataset.app = appId;
}

function markAppDone(appId, result, detail) {
  const row = state.appProg.get(appId);
  if (row) {
    row.classList.add(result);
    if (result === "ok") row.classList.remove("indeterminate");
    row.querySelector(".st").textContent = detail;
    row.querySelector(".bar-fill").style.width = result === "ok" ? "100%" : "0%";
  }
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

function handleUiEvent(ev) {
  switch (ev.type) {
    case "app_started":
      log(`开始备份 ${ev.app_id}`, "info");
      renderAppRow(ev.app_id, null);
      break;
    case "scan_done":
      renderAppRow(ev.app_id, { bytes_done: 0, bytes_total: ev.bytes_total, files_done: 0, files_total: ev.files_total });
      log(`扫描完成 ${ev.app_id}: ${ev.files_total} 文件`, "info");
      break;
    case "app_progress": {
      const row = state.appProg.get(ev.app_id);
      if (row) row.classList.remove("indeterminate");
      renderAppRow(ev.app_id, ev);
      break;
    }
    case "overall_progress":
      setOverall(ev);
      break;
    case "app_finished":
      markAppDone(ev.app_id, ev.result, ev.detail);
      if (ev.result === "ok") log(`✓ ${ev.app_id} 完成: ${fmtBytes(ev.size)}`, "ok");
      else if (ev.result === "skipped") log(`– ${ev.app_id} 跳过: ${ev.detail}`, "warn");
      else if (ev.result === "failed") log(`✗ ${ev.app_id} 失败: ${ev.detail}`, "error");
      else log(`× ${ev.app_id} 已取消`, "warn");
      break;
    case "log":
      log(ev.msg, ev.level);
      break;
  }
}

function setOverall(ev) {
  const pct = ev.bytes_total > 0
    ? Math.round((ev.bytes_done / ev.bytes_total) * 100)
    : ev.apps_total > 0 ? Math.round((ev.apps_done / ev.apps_total) * 100) : 0;
  $("#overall-bar").style.width = `${pct}%`;
  $("#overall-text").textContent =
    `应用 ${ev.apps_done}/${ev.apps_total} · ${fmtBytes(ev.bytes_done)}/${fmtBytes(ev.bytes_total)}`;
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async function loadApps() {
  try {
    state.apps = await invoke("get_apps");
    renderApps();
  } catch (e) {
    log(`加载应用失败: ${e}`, "error");
  }
}

async function loadHistory() {
  try {
    const entries = await invoke("get_history", { app: null });
    renderHistory(entries);
  } catch (e) {
    log(`加载历史失败: ${e}`, "error");
  }
}

async function loadSettings() {
  try {
    const s = await invoke("get_settings");
    $("#footer").textContent = `备份位置: ${s.dest}   格式: ${s.format}   保留: ${s.retention}`;
  } catch (e) {
    $("#footer").textContent = `备份位置: 加载失败 (${e})`;
  }
}

async function runBackup(ids) {
  if (state.running) return;
  state.running = true;
  $("#btn-backup").disabled = true;
  $("#btn-backup-all").disabled = true;
  $("#btn-stop").disabled = false;
  resetProgress();
  log(`备份开始: ${ids.length ? ids.join(", ") : "全部应用"}`, "info");

  const channel = new Channel();
  channel.onmessage = (ev) => handleUiEvent(ev);

  try {
    const report = await invoke("run_backup", { selected: ids, channel });
    log(`备份完成: 成功 ${report.ok} · 失败 ${report.failed} · 跳过 ${report.skipped} · 取消 ${report.cancelled}`, "ok");
    log(`保存位置: ${report.dest}`, "info");
    loadHistory();
    loadApps();
  } catch (e) {
    log(`备份出错: ${e}`, "error");
  } finally {
    state.running = false;
    $("#btn-backup").disabled = false;
    $("#btn-backup-all").disabled = false;
    $("#btn-stop").disabled = true;
  }
}

// ---------------------------------------------------------------------------
// Path rows
// ---------------------------------------------------------------------------

function attachPathPreview(input, preview) {
  let timer = null;
  const update = () => {
    clearTimeout(timer);
    const raw = input.value.trim();
    if (!raw) {
      preview.textContent = "";
      preview.className = "preview";
      return;
    }
    timer = setTimeout(async () => {
      try {
        const r = await invoke("resolve_path", { raw });
        preview.textContent = r.note || (r.state === "ok" ? `→ ${r.resolved}  ✓ 存在` : `→ ${r.resolved}  (不存在)`);
        preview.className = `preview ${r.state}`;
      } catch (e) {
        preview.textContent = `解析失败: ${e}`;
        preview.className = "preview undefined";
      }
    }, 200);
  };
  input.addEventListener("input", update);
  return update;
}

async function pickDirectory(input, preview, refreshPreview) {
  try {
    const dir = await invoke("pick_directory");
    if (dir) {
      input.value = dir;
      refreshPreview();
    }
  } catch (e) {
    preview.textContent = `选择目录失败: ${e}`;
    preview.className = "preview undefined";
  }
}

function makePathRow(value = "") {
  const row = document.createElement("div");
  row.className = "path-row";
  const inputs = document.createElement("div");
  inputs.className = "path-inputs";
  const input = document.createElement("input");
  input.type = "text";
  input.value = value;
  input.placeholder = "$HOME/.config/foo  或  %APPDATA%\\foo";
  const preview = document.createElement("div");
  preview.className = "preview";
  const refreshPreview = attachPathPreview(input, preview);
  const pick = document.createElement("button");
  pick.className = "pick-btn";
  pick.textContent = "选择";
  pick.title = "选择目录";
  pick.addEventListener("click", () => pickDirectory(input, preview, refreshPreview));
  const del = document.createElement("button");
  del.className = "icon-btn";
  del.textContent = "−";
  del.title = "移除路径";
  del.addEventListener("click", () => row.remove());
  inputs.append(input, preview);
  row.append(inputs, pick, del);
  return { row, input };
}

function makeExcludeRow(value = "", placeholder = "Cache/**  *.log") {
  const row = document.createElement("div");
  row.className = "path-row";
  const inputs = document.createElement("div");
  inputs.className = "path-inputs";
  const input = document.createElement("input");
  input.type = "text";
  input.value = value;
  input.placeholder = placeholder;
  const del = document.createElement("button");
  del.className = "icon-btn";
  del.textContent = "−";
  del.title = "移除排除";
  del.addEventListener("click", () => row.remove());
  inputs.appendChild(input);
  row.append(inputs, del);
  return { row, input };
}

// ---------------------------------------------------------------------------
// App add / edit drawer
// ---------------------------------------------------------------------------

let editingId = null;

function openAppDrawer() {
  $("#app-drawer").classList.remove("hidden");
}

function closeAppDrawer() {
  $("#app-drawer").classList.add("hidden");
}

function clearAppError() {
  const box = $("#app-form-error");
  box.textContent = "";
  box.classList.add("hidden");
}

function showAppError(errors) {
  const box = $("#app-form-error");
  box.textContent = errors.join("\n");
  box.classList.remove("hidden");
}

function openAdd() {
  clearAppError();
  editingId = null;
  $("#app-drawer-title").textContent = "添加应用";
  $("#f-name").value = "";
  $("#f-enabled").checked = true;
  $("#f-compress").checked = true;
  $("#app-drawer-delete").classList.add("hidden");
  const pl = $("#path-list");
  pl.innerHTML = "";
  pl.appendChild(makePathRow().row);
  const el = $("#exclude-list");
  el.innerHTML = "";
  openAppDrawer();
}

async function openEdit(id) {
  clearAppError();
  let detail;
  try {
    detail = await invoke("get_app", { id });
  } catch (e) {
    log(`加载应用失败: ${e}`, "error");
    return;
  }
  editingId = detail.id;
  $("#app-drawer-title").textContent = `编辑应用 · ${detail.name}`;
  $("#f-name").value = detail.name;
  $("#f-enabled").checked = detail.enabled;
  $("#f-compress").checked = detail.compress;
  $("#app-drawer-delete").classList.remove("hidden");
  const pl = $("#path-list");
  pl.innerHTML = "";
  for (const p of detail.paths) pl.appendChild(makePathRow(p).row);
  if (!detail.paths.length) pl.appendChild(makePathRow().row);
  const el = $("#exclude-list");
  el.innerHTML = "";
  for (const e of detail.excludes) el.appendChild(makeExcludeRow(e).row);
  openAppDrawer();
}

function collectAppForm() {
  const paths = [...$("#path-list").querySelectorAll("input[type=text]")]
    .map((i) => i.value.trim()).filter(Boolean);
  const excludes = [...$("#exclude-list").querySelectorAll("input[type=text]")]
    .map((i) => i.value.trim()).filter(Boolean);
  return {
    id: editingId || "",
    name: $("#f-name").value.trim(),
    enabled: $("#f-enabled").checked,
    compress: $("#f-compress").checked,
    paths,
    excludes,
  };
}

function validateAppForm(input) {
  const errors = [];
  if (!input.name) errors.push("名称不能为空");
  if (!input.paths.length) errors.push("至少需要填写一个备份路径");
  return errors;
}

async function saveAppForm() {
  clearAppError();
  const input = collectAppForm();
  const errors = validateAppForm(input);
  if (errors.length) {
    showAppError(errors);
    return;
  }
  try {
    await invoke("save_app", { input });
    log(`已保存应用: ${input.name}`, "ok");
    closeAppDrawer();
    loadApps();
    loadHistory();
    loadSettings();
  } catch (e) {
    showAppError([`保存失败: ${e}`]);
  }
}

async function deleteAppForm() {
  if (!editingId) return;
  if (!confirm(`确定删除应用「${editingId}」？配置文件将被修改。`)) return;
  try {
    await invoke("remove_app", { id: editingId });
    log(`已删除应用: ${editingId}`, "ok");
    if (state.selectedAppId === editingId) state.selectedAppId = null;
    closeAppDrawer();
    loadApps();
    loadHistory();
  } catch (e) {
    log(`删除失败: ${e}`, "error");
  }
}

async function deleteSelectedApp() {
  const id = state.selectedAppId;
  const app = state.apps.find((a) => a.id === id);
  if (!app) return;
  if (!confirm(`确定删除应用「${app.name}」？配置文件将被修改。`)) return;
  try {
    await invoke("remove_app", { id });
    log(`已删除应用: ${app.name}`, "ok");
    state.selectedAppId = null;
    loadApps();
    loadHistory();
  } catch (e) {
    log(`删除失败: ${e}`, "error");
  }
}

// ---------------------------------------------------------------------------
// Settings drawer
// ---------------------------------------------------------------------------

function openSettings() {
  clearSettingsError();
  $("#settings-drawer").classList.remove("hidden");
  loadSettingsForm();
}

function closeSettings() {
  $("#settings-drawer").classList.add("hidden");
}

function clearSettingsError() {
  const box = $("#settings-error");
  box.textContent = "";
  box.classList.add("hidden");
}

function showSettingsError(errors) {
  const box = $("#settings-error");
  box.textContent = errors.join("\n");
  box.classList.remove("hidden");
}

async function loadSettingsForm() {
  try {
    const s = await invoke("get_settings");
    fillSettingsForm(s);
  } catch (e) {
    showSettingsError([`加载设置失败: ${e}`]);
  }
}

function fillSettingsForm(s) {
  $("#s-dest").value = s.destRaw || s.dest;
  $("#s-format").value = s.format;
  $("#s-parallel").value = s.parallel;
  $("#s-retention").value = s.retention;
  $("#s-cleanup").value = s.cleanup;
  $("#s-config-path").textContent = s.configPath;
  const el = $("#s-exclude-list");
  el.innerHTML = "";
  for (const e of s.excludes) el.appendChild(makeExcludeRow(e, "**/*.tmp").row);
}

function collectSettings() {
  return {
    dest: $("#s-dest").value.trim(),
    format: $("#s-format").value,
    parallel: Number($("#s-parallel").value),
    retention: Number($("#s-retention").value),
    cleanup: $("#s-cleanup").value,
    excludes: [...$("#s-exclude-list").querySelectorAll("input[type=text]")]
      .map((i) => i.value.trim()).filter(Boolean),
  };
}

function validateSettings(input) {
  const errors = [];
  if (!input.dest) errors.push("备份位置不能为空");
  if (!["zip", "tar.gz", "dir"].includes(input.format)) errors.push("备份格式无效");
  if (!Number.isInteger(input.parallel) || input.parallel < 1) errors.push("并行备份数必须大于 0");
  if (!Number.isInteger(input.retention) || input.retention < 0) errors.push("保留份数不能为负数");
  if (!["after_each", "at_end"].includes(input.cleanup)) errors.push("清理时机无效");
  return errors;
}

async function saveSettingsForm() {
  clearSettingsError();
  const input = collectSettings();
  const errors = validateSettings(input);
  if (errors.length) {
    showSettingsError(errors);
    return;
  }
  try {
    await invoke("save_settings", { input });
    log("设置已保存", "ok");
    closeSettings();
    loadSettings();
    loadApps();
    loadHistory();
  } catch (e) {
    showSettingsError([`保存失败: ${e}`]);
  }
}

async function restoreDefaults() {
  clearSettingsError();
  try {
    const s = await invoke("default_settings");
    fillSettingsForm(s);
    log("已填入默认设置，点击保存后生效", "info");
  } catch (e) {
    showSettingsError([`恢复默认失败: ${e}`]);
  }
}

async function reloadSettingsFromDisk() {
  clearSettingsError();
  try {
    const s = await invoke("reload_settings");
    fillSettingsForm(s);
    log("已重新加载配置文件", "ok");
    loadSettings();
  } catch (e) {
    showSettingsError([`重新加载失败: ${e}`]);
  }
}

// ---------------------------------------------------------------------------
// Restore wizard (placeholder; full restore is P2)
// ---------------------------------------------------------------------------

function closeRestore() {
  $("#restore-modal").classList.add("hidden");
}

async function openRestore() {
  $("#restore-modal").classList.remove("hidden");
  const list = $("#restore-list");
  list.innerHTML = '<div class="empty">加载中…</div>';
  try {
    const entries = await invoke("get_history", { app: null });
    renderRestoreList(entries);
  } catch (e) {
    list.innerHTML = `<div class="empty">加载备份历史失败: ${e}</div>`;
  }
}

function renderRestoreList(entries) {
  const list = $("#restore-list");
  list.innerHTML = "";
  if (!entries.length) {
    list.innerHTML = '<div class="empty">暂无备份记录</div>';
    return;
  }
  for (const e of entries) {
    const li = document.createElement("li");
    li.className = "history-item";
    const f = document.createElement("span");
    f.className = "fname";
    f.textContent = `${e.app_id} · ${e.file}`;
    const m = document.createElement("span");
    m.className = "hmeta";
    m.textContent = `${fmtBytes(e.size)} · ${e.started_at || e.status}`;
    li.append(f, m);
    list.appendChild(li);
  }
}

// ---------------------------------------------------------------------------
// Wire up
// ---------------------------------------------------------------------------

$("#btn-backup").addEventListener("click", () => {
  const ids = selectedIds();
  if (!ids.length) {
    log("请先勾选要备份的应用", "warn");
    return;
  }
  runBackup(ids);
});
$("#btn-backup-all").addEventListener("click", () => runBackup([]));
$("#btn-stop").addEventListener("click", () => {
  invoke("cancel_backup");
  log("已请求停止，等待当前文件完成后中止", "warn");
});
$("#btn-restore").addEventListener("click", openRestore);
$("#restore-close").addEventListener("click", closeRestore);
$("#restore-modal").addEventListener("click", (e) => {
  if (e.target === $("#restore-modal")) closeRestore();
});
$("#btn-settings").addEventListener("click", openSettings);
$("#btn-exit").addEventListener("click", () => {
  invoke("exit_app");
});
$("#select-all").addEventListener("change", (e) => {
  document.querySelectorAll("#app-list input[type=checkbox]").forEach((b) => {
    if (!b.disabled) {
      b.checked = e.target.checked;
      state.userChecked.set(b.dataset.id, b.checked);
    }
  });
  updateSelectAll();
});

$("#btn-add-app").addEventListener("click", openAdd);
$("#btn-edit-app").addEventListener("click", () => {
  if (state.selectedAppId) openEdit(state.selectedAppId);
});
$("#btn-delete-app").addEventListener("click", deleteSelectedApp);
$("#app-drawer-close").addEventListener("click", closeAppDrawer);
$("#app-drawer-cancel").addEventListener("click", closeAppDrawer);
$("#app-drawer-save").addEventListener("click", saveAppForm);
$("#app-drawer-delete").addEventListener("click", deleteAppForm);
$("#add-path").addEventListener("click", () => $("#path-list").appendChild(makePathRow().row));
$("#add-exclude").addEventListener("click", () => $("#exclude-list").appendChild(makeExcludeRow().row));
$("#app-drawer").addEventListener("click", (e) => {
  if (e.target === $("#app-drawer")) closeAppDrawer();
});

$("#settings-close").addEventListener("click", closeSettings);
$("#s-cancel").addEventListener("click", closeSettings);
$("#s-save").addEventListener("click", saveSettingsForm);
$("#s-restore-defaults").addEventListener("click", restoreDefaults);
$("#s-reload").addEventListener("click", reloadSettingsFromDisk);
$("#s-add-exclude").addEventListener("click", () => $("#s-exclude-list").appendChild(makeExcludeRow("", "**/*.tmp").row));
$("#s-pick-dest").addEventListener("click", async () => {
  try {
    const dir = await invoke("pick_directory");
    if (dir) $("#s-dest").value = dir;
  } catch (e) {
    log(`选择目录失败: ${e}`, "error");
  }
});
$("#settings-drawer").addEventListener("click", (e) => {
  if (e.target === $("#settings-drawer")) closeSettings();
});

loadApps();
loadHistory();
loadSettings();
