"use strict";

const $ = (sel) => document.querySelector(sel);

// Build DOM nodes without innerHTML: every value ends up as text.
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k in el && typeof v !== "string") el[k] = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

async function api(path, body) {
  const opts = body === undefined ? {} : {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-iDevice-Fleet": "1" },
    body: JSON.stringify(body),
  };
  const r = await fetch(path, opts);
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data.error || `Request failed (${r.status})`);
  return data;
}

const fmtSize = (n) => n == null ? "" : n >= 1e9 ? (n / 1e9).toFixed(2) + " GB" : (n / 1e6).toFixed(0) + " MB";
const fmtDate = (s) => s ? new Date(s).toLocaleString() : "";
const label = (d) => d.name || d.display_name || d.product_type || d.key;
const elapsed = (j) => {
  if (!j.started) return "";
  const s = Math.round(((j.ended || Date.now() / 1000) - j.started));
  return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
};

let state = null;
const selected = new Set();
const openLogs = new Map(); // job id -> {since, lines}
let lastDevSig = "";
let lastBackupsAt = 0;
let backups = [];

// ---------- tabs ----------
for (const t of document.querySelectorAll(".tab")) {
  t.addEventListener("click", () => {
    for (const o of document.querySelectorAll(".tab")) {
      const on = o === t;
      o.setAttribute("aria-selected", String(on));
      $("#tab-" + o.dataset.tab).hidden = !on;
    }
    if (t.dataset.tab === "backups") loadBackups();
  });
}

// ---------- rendering ----------
function renderTools() {
  $("#ver").textContent = `v${state.version} · ${state.platform}`;
  $("#tools").replaceChildren(...Object.entries(state.tools).map(([name, path]) =>
    h("span", { class: "tool" + (path ? "" : " missing"), title: path || "Not found" }, name)));
  const missing = Object.entries(state.tools).filter(([, p]) => !p).map(([n]) => n);
  const banner = $("#banner");
  const msgs = [];
  if (missing.length) msgs.push(`Not found: ${missing.join(", ")}. See the README for how to install them, or start with --tools-dir.`);
  if (state.scanner_error) msgs.push(`Device scan problem: ${state.scanner_error}`);
  banner.textContent = msgs.join(" ");
  banner.hidden = !msgs.length;
  $("#libDir").textContent = state.library_dir;
  $("#bakDir").textContent = state.backup_dir;
}

function jobFor(dev) {
  return state.jobs.find((j) => (j.status === "running" || j.status === "queued") && j.device_key === dev.key);
}

function act(text, fn, opts = {}) {
  return h("button", { class: "btn small", type: "button", disabled: !!opts.disabled, title: opts.title, onclick: fn }, text);
}

async function run(path, body) {
  try { await api(path, body); await poll(); }
  catch (e) { alert(e.message); }
}

function renderDevices() {
  const devs = state.devices;
  const sig = JSON.stringify([devs.map(({ last_seen, ...rest }) => rest), state.jobs.filter((j) => j.status === "running" || j.status === "queued").map((j) => [j.id, j.device_key, j.stage, j.progress]), [...selected]]);
  if (sig === lastDevSig) return;
  lastDevSig = sig;
  for (const k of [...selected]) if (!devs.some((d) => d.key === k)) selected.delete(k);

  const rows = devs.map((d) => {
    const job = jobFor(d);
    const normal = d.mode === "Normal";
    const rec = d.mode === "Recovery" || d.mode === "DFU";
    let status;
    if (job) status = h("div", {}, h("span", { class: "pill running" }, job.stage || job.status), job.progress != null ? ` ${Math.round(job.progress)}%` : "");
    else if (normal && !d.paired) status = h("span", { class: "pill no", title: d.error || "" }, "Not trusted");
    else if (normal) status = h("span", { class: "pill yes" }, d.activation_state || "Paired");
    else status = h("span", { class: "sub" }, d.ecid ? "Ready to restore" : "Waiting for ECID");
    const busy = !!job;
    return h("tr", {},
      h("td", { class: "ck" }, h("input", { type: "checkbox", checked: selected.has(d.key), disabled: !d.ecid || busy, "aria-label": "Select " + label(d),
        onchange: (e) => { e.target.checked ? selected.add(d.key) : selected.delete(d.key); lastDevSig = ""; renderDevices(); } })),
      h("td", {}, h("div", { class: "name" }, label(d)), h("div", { class: "sub" }, [d.product_type, d.model].filter(Boolean).join(" · "))),
      h("td", {}, h("span", { class: "pill " + d.mode }, d.mode)),
      h("td", {}, d.os_version || ""),
      h("td", { class: "mono" }, d.serial || ""),
      h("td", { class: "mono" }, d.ecid ? "0x" + d.ecid : ""),
      h("td", {}, status),
      h("td", {}, h("div", { class: "acts" },
        normal && !d.paired && act("Pair", () => run("/api/pair", { key: d.key }), { disabled: busy }),
        normal && act("Back up", () => run("/api/backup", { key: d.key }), { disabled: busy || !d.paired, title: d.paired ? "Full backup to the backup folder" : "Pair first" }),
        normal && act("Encryption…", () => openEnc(d), { disabled: busy || !d.paired }),
        normal && act("Restore backup…", () => openBakRestore(d), { disabled: busy || !d.paired }),
        normal && act("Recovery mode", () => run("/api/recovery/enter", { key: d.key }), { disabled: busy }),
        rec && act("Exit recovery", () => run("/api/recovery/exit", { key: d.key }), { disabled: busy }),
        d.ecid && act("Restore firmware…", () => openRestore([d.key]), { disabled: busy }),
      )),
    );
  });
  $("#devRows").replaceChildren(...(rows.length ? rows : [h("tr", {}, h("td", { colspan: 8, class: "empty" },
    "No devices found. Connect an iPhone or iPad by USB. Devices in recovery or DFU mode show up here too."))]));
  $("#restoreSel").disabled = selected.size === 0;
  $("#restoreSel").textContent = selected.size ? `Restore selected (${selected.size})` : "Restore selected";
  const all = devs.filter((d) => d.ecid && !jobFor(d));
  $("#selAll").checked = all.length > 0 && all.every((d) => selected.has(d.key));

  const idents = [...new Set(devs.map((d) => d.product_type).filter(Boolean))];
  $("#identList").replaceChildren(...idents.map((i) => h("option", { value: i })));
  if (!$("#ident").value && idents.length) $("#ident").placeholder = idents[0];
}

$("#selAll").addEventListener("change", (e) => {
  for (const d of state.devices) if (d.ecid && !jobFor(d)) e.target.checked ? selected.add(d.key) : selected.delete(d.key);
  lastDevSig = ""; renderDevices();
});

function renderJobs() {
  const list = state.jobs;
  if (!list.length) {
    $("#jobList").replaceChildren(h("p", { class: "hint" }, "Nothing running. Backups, restores and downloads appear here."));
    return;
  }
  $("#jobList").replaceChildren(...list.map((j) => {
    const active = j.status === "running" || j.status === "queued";
    const log = openLogs.get(j.id);
    const bar = h("i");
    bar.style.width = (j.progress != null ? j.progress : (j.status === "done" ? 100 : 0)) + "%";
    return h("div", { class: "job " + j.status },
      h("div", {},
        h("div", { class: "title" }, `#${j.id} ${j.title}`),
        h("div", { class: "meta" }, [j.stage, j.progress != null ? Math.round(j.progress) + "%" : null, elapsed(j)].filter(Boolean).join(" · ")),
        j.error && h("div", { class: "meta err" }, j.error)),
      h("div", { class: "right" },
        h("span", { class: "pill " + j.status }, j.status),
        h("button", { class: "btn small", type: "button", onclick: () => toggleLog(j.id) }, log ? "Hide log" : "Log"),
        active && h("button", { class: "btn small", type: "button", onclick: () => run(`/api/jobs/${j.id}/cancel`, {}) }, j.kind === "download" ? "Pause" : "Cancel")),
      h("div", { class: "progress full" }, bar),
      log && h("pre", { class: "log full" }, log.lines.join("\n") || "…"),
    );
  }));
  for (const pre of document.querySelectorAll(".log")) pre.scrollTop = pre.scrollHeight;
}

async function toggleLog(id) {
  if (openLogs.has(id)) openLogs.delete(id);
  else { openLogs.set(id, { since: 0, lines: [] }); await fetchLog(id); }
  renderJobs();
}

async function fetchLog(id) {
  const log = openLogs.get(id);
  if (!log) return;
  try {
    const j = await api(`/api/jobs/${id}?since=${log.since}`);
    log.lines.push(...j.log);
    if (log.lines.length > 4000) log.lines.splice(0, log.lines.length - 4000);
    log.since = j.log_total;
  } catch { openLogs.delete(id); }
}

function renderLibrary() {
  const rows = state.library.map((e) => h("tr", {},
    h("td", { class: "mono" }, e.file),
    h("td", {}, e.error ? h("span", { class: "err" }, e.error) : e.version || ""),
    h("td", { class: "mono" }, e.build || ""),
    h("td", { title: (e.product_types || []).join(", ") }, (e.product_types || []).length ? `${e.product_types.length} models` : ""),
    h("td", {}, fmtSize(e.size)),
    h("td", {}, e.signed == null ? h("span", { class: "sub", title: "Look up a model above to check" }, "unknown") : h("span", { class: "pill " + (e.signed ? "yes" : "no") }, e.signed ? "signed" : "not signed")),
  ));
  $("#libRows").replaceChildren(...(rows.length ? rows : [h("tr", {}, h("td", { colspan: 6, class: "empty" }, "No firmware yet. Look up a model above and download its signed firmware."))]));
}

async function loadBackups() {
  lastBackupsAt = Date.now();
  try { backups = await api("/api/backups"); } catch { backups = []; }
  const rows = backups.map((b) => h("tr", {},
    h("td", {}, h("div", { class: "name" }, b.device_name || b.folder), h("div", { class: "sub mono" }, b.folder)),
    h("td", {}, b.product_type || ""), h("td", {}, b.os_version || ""), h("td", { class: "mono" }, b.serial || ""),
    h("td", {}, fmtDate(b.date)),
    h("td", {}, b.encrypted == null ? "" : h("span", { class: "pill " + (b.encrypted ? "yes" : "no") }, b.encrypted ? "yes" : "no")),
    h("td", {}, h("span", { class: "pill " + (b.complete ? "yes" : "no") }, b.complete ? "yes" : "no")),
  ));
  $("#bakRows").replaceChildren(...(rows.length ? rows : [h("tr", {}, h("td", { colspan: 7, class: "empty" }, "No backups yet."))]));
}

// ---------- firmware lookup ----------
$("#lookupForm").addEventListener("submit", async (e) => {
  e.preventDefault();
  const id = $("#ident").value.trim();
  $("#lookupMsg").textContent = "Looking up…";
  $("#lookupMsg").className = "msg";
  try {
    const data = await api("/api/catalog?identifier=" + encodeURIComponent(id));
    $("#lookupMsg").textContent = `${data.name || data.identifier}: ${data.firmwares.filter((f) => f.signed).length} signed of ${data.firmwares.length} versions.`;
    const fws = data.firmwares.slice(0, 40);
    $("#catalogRows").replaceChildren(...fws.map((f) => h("tr", {},
      h("td", {}, f.version), h("td", { class: "mono" }, f.buildid), h("td", {}, f.releasedate ? new Date(f.releasedate).toLocaleDateString() : ""),
      h("td", {}, fmtSize(f.filesize)),
      h("td", {}, h("span", { class: "pill " + (f.signed ? "yes" : "no") }, f.signed ? "signed" : "not signed")),
      h("td", {}, f.in_library ? h("span", { class: "sub" }, "In library") :
        h("button", { class: "btn small" + (f.signed ? " primary" : ""), type: "button", title: f.signed ? "" : "Apple no longer signs this version; it can't be restored",
          onclick: () => run("/api/firmware/download", { url: f.url, sha256: f.sha256sum, sha1: f.sha1sum }) }, "Download")),
    )));
    $("#catalogWrap").hidden = false;
    await poll();
  } catch (err) {
    $("#lookupMsg").textContent = err.message;
    $("#lookupMsg").className = "msg err";
  }
});

// ---------- restore dialog ----------
let restoreKeys = [];
function openRestore(keys) {
  restoreKeys = keys;
  const rows = keys.map((k) => {
    const d = state.devices.find((x) => x.key === k);
    if (!d) return null;
    const fits = state.library.filter((e) => (e.product_types || []).includes(d.product_type))
      .sort((a, b) => (b.version || "").localeCompare(a.version || "", undefined, { numeric: true }));
    const any = d.product_type ? fits : state.library.filter((e) => !e.error);
    const sel = h("select", { "data-key": k, "aria-label": "Firmware for " + label(d) },
      any.length ? any.map((e) => h("option", { value: e.file }, `${e.version} (${e.build})${e.signed === false ? " - not signed" : ""} · ${e.file}`))
        : h("option", { value: "" }, `No firmware for ${d.product_type || "this model"} in the library`));
    return h("tr", {}, h("td", {}, label(d)), h("td", { class: "mono" }, d.product_type || "?"), h("td", {}, sel));
  }).filter(Boolean);
  $("#restoreRows").replaceChildren(...rows);
  $("#confirmWord").value = "";
  $("#restoreMsg").textContent = "";
  syncConfirm();
  $("#restoreDlg").showModal();
}
function syncConfirm() {
  const erase = document.querySelector('input[name="rtype"]:checked').value === "erase";
  $("#confirmNeed").textContent = erase ? "ERASE" : "UPDATE";
}
for (const r of document.querySelectorAll('input[name="rtype"]')) r.addEventListener("change", syncConfirm);
$("#restoreSel").addEventListener("click", () => openRestore([...selected]));
$("#restoreGo").addEventListener("click", async () => {
  const targets = [...document.querySelectorAll("#restoreRows select")].map((s) => ({ key: s.dataset.key, ipsw: s.value }));
  if (targets.some((t) => !t.ipsw)) { $("#restoreMsg").textContent = "Every device needs firmware from the library. Download it on the Firmware tab first."; return; }
  const erase = document.querySelector('input[name="rtype"]:checked').value === "erase";
  try {
    await api("/api/restore", { targets, erase, confirm: $("#confirmWord").value });
    $("#restoreDlg").close();
    selected.clear(); lastDevSig = "";
    await poll();
  } catch (e) { $("#restoreMsg").textContent = e.message; }
});

// ---------- backup restore / encryption ----------
let bakDevice = null;
async function openBakRestore(d) {
  bakDevice = d;
  await loadBackups();
  $("#bakTarget").textContent = `Target: ${label(d)} (${d.product_type || ""}, serial ${d.serial || "?"})`;
  const sorted = [...backups].sort((a, b) => (b.serial === d.serial) - (a.serial === d.serial) || (b.date || "").localeCompare(a.date || ""));
  $("#bakPick").replaceChildren(...(sorted.length ? sorted.map((b) => h("option", { value: b.folder },
    `${b.device_name || b.folder} · ${b.serial || ""} · ${fmtDate(b.date)}${b.encrypted ? " · encrypted" : ""}${b.serial === d.serial ? " · this device" : ""}`))
    : [h("option", { value: "" }, "No backups found")]));
  $("#bakPw").value = ""; $("#bakMsg").textContent = "";
  $("#bakRestoreDlg").showModal();
}
$("#bakGo").addEventListener("click", async () => {
  try {
    await api("/api/backup-restore", { key: bakDevice.key, backup: $("#bakPick").value, password: $("#bakPw").value });
    $("#bakPw").value = "";
    $("#bakRestoreDlg").close(); await poll();
  } catch (e) { $("#bakMsg").textContent = e.message; }
});

let encDevice = null;
function openEnc(d) {
  encDevice = d;
  $("#encTarget").textContent = `${label(d)} (serial ${d.serial || "?"})`;
  $("#encPw").value = ""; $("#encMsg").textContent = "";
  $("#encDlg").showModal();
}
$("#encGo").addEventListener("click", async () => {
  const enable = document.querySelector('input[name="enc"]:checked').value === "on";
  try {
    await api("/api/encryption", { key: encDevice.key, enable, password: $("#encPw").value });
    $("#encPw").value = "";
    $("#encDlg").close(); await poll();
  } catch (e) { $("#encMsg").textContent = e.message; }
});

// ---------- misc ----------
$("#refresh").addEventListener("click", () => run("/api/refresh", {}));
$("#clearJobs").addEventListener("click", async () => {
  for (const id of [...openLogs.keys()]) if (!state.jobs.some((j) => j.id === id && (j.status === "running" || j.status === "queued"))) openLogs.delete(id);
  await run("/api/jobs/clear", {});
});

let prevJobStatus = new Map();
async function poll() {
  try {
    state = await api("/api/state");
  } catch (e) {
    $("#banner").textContent = "Lost connection to the iDevice Fleet server. Is it still running?";
    $("#banner").hidden = false;
    return;
  }
  renderTools();
  renderDevices();
  for (const id of openLogs.keys()) await fetchLog(id);
  renderJobs();
  renderLibrary();
  // Refresh the backup list when a backup job finishes.
  const finishedBackup = state.jobs.some((j) => j.kind === "backup" && j.status !== "running" && prevJobStatus.get(j.id) === "running");
  prevJobStatus = new Map(state.jobs.map((j) => [j.id, j.status]));
  if (finishedBackup && !$("#tab-backups").hidden) loadBackups();
}

poll();
setInterval(poll, 1500);
