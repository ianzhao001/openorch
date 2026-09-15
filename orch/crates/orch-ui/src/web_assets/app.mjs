import { marked } from "/assets/marked.mjs";
import * as model from "/assets/model.mjs";

const capability = document.querySelector('meta[name="orch-capability"]').content;
const $ = id => document.getElementById(id);
const ui = {
  project: $("project-select"), form: $("project-form"), path: $("project-path"), search: $("search"),
  result: $("result-filter"), member: $("member-filter"), time: $("time-filter"), task: $("task-filter"),
  taskState: $("task-state-filter"), purpose: $("purpose-filter"), windowSummary: $("window-summary"), cards: $("cards"),
  taskCards: $("task-cards"), otherCards: $("unassociated-cards"), taskCount: $("task-count"),
  otherCount: $("unassociated-count"), load: $("load-more"), notice: $("notice"), status: $("read-status"),
  dot: $("status-dot"), drawer: $("drawer"), scrim: $("scrim"), drawerTitle: $("drawer-title"),
  drawerSummary: $("drawer-summary"), memberList: $("member-list"), reader: $("reader"),
  readerTitle: $("reader-title"), answer: $("answer"), answerState: $("answer-state"),
  openReader: $("open-reader"), copyAnswer: $("copy-answer"), theme: $("theme-select"), lang: $("lang-toggle"),
};

const TEXT = {
  en: { readonly:"Read-only observation",project:"Project",addProject:"Add project",theme:"Theme",projectPath:"Exact Git root",register:"Register",tasks:"Tasks",task:"Task",taskState:"Task state",purpose:"Purpose",calls:"All calls",members:"Members",search:"Search",result:"Answer",member:"Member",time:"Time",refresh:"Refresh",linked:"EXPLICITLY LINKED",taskOverview:"Task overview",unlinked:"NOT LINKED TO A TASK",unassociated:"Unassociated calls",loadMore:"Load 30 more",details:"DETAILS",readAnswer:"Read verified answer",back:"Back",verifiedAnswer:"VERIFIED ANSWER",copy:"Copy text",empty:"No observations match these filters",noSummary:"No captured summary",loading:"Loading…",updated:"Updated",failed:"Refresh failed · showing previous snapshot",callsLabel:"calls",versions:"versions",partial:"roster incomplete",moved:"Selected item is outside the current page",missing:"Selected item is no longer available",window:"cards in current window",outside:"outside",hiddenFailures:"hidden failures",evidence:"evidence read",nativeUnknown:"native unknown",nativeEnded:"native ended" },
  zh: { readonly:"本机只读观察",project:"项目",addProject:"添加项目",theme:"主题",projectPath:"精确 Git 根目录",register:"登记",tasks:"任务",task:"任务",taskState:"任务状态",purpose:"用途",calls:"全部调用",members:"成员",search:"搜索",result:"答卷",member:"成员",time:"时间",refresh:"刷新",linked:"明确关联",taskOverview:"任务总览",unlinked:"未关联任务",unassociated:"未关联调用",loadMore:"再加载 30 张",details:"详情",readAnswer:"阅读已验证答卷",back:"返回",verifiedAnswer:"已验证答卷",copy:"复制文本",empty:"没有符合筛选条件的记录",noSummary:"未捕获摘要",loading:"读取中…",updated:"已更新",failed:"刷新失败 · 正在显示旧快照",callsLabel:"次调用",versions:"个版本",partial:"名单不完整",moved:"所选记录已移出当前页面",missing:"所选记录已不可用",window:"张当前窗口卡片",outside:"窗口外",hiddenFailures:"个隐藏异常",evidence:"证据读取",nativeUnknown:"原生终态未知",nativeEnded:"原生已结束" },
};

let prefs = model.preferences(document.cookie, navigator.language);
let state = null;
let projects = [];
let view = "home";
let selectedCard = null;
let lastFocus = null;
let lastReaderFocus = null;
let lastAnswerText = "";
const limits = { home: 30, calls: 30, members: 30 };

function t(key) { return TEXT[prefs.lang][key] ?? key; }
function applyLanguage() {
  document.documentElement.lang = prefs.lang === "zh" ? "zh-CN" : "en";
  document.querySelectorAll("[data-i18n]").forEach(node => { node.textContent = t(node.dataset.i18n); });
  ui.lang.textContent = prefs.lang === "zh" ? "EN" : "中";
  ui.search.placeholder = prefs.lang === "zh" ? "搜索 ID、摘要或成员…" : "Search ID, summary, member…";
  if (state?.lastSuccessAt) setStatus(!state.refreshError, state.refreshError ? t("failed") : `${t("updated")} · ${formatTime(state.lastSuccessAt)} · ${t("evidence")} ${state.snapshot?.read_at ?? "—"}`);
  render();
}
function applyTheme() {
  const resolved = prefs.theme === "system" ? (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light") : prefs.theme;
  document.documentElement.dataset.theme = resolved;
  ui.theme.value = prefs.theme;
}

async function request(path, init = {}) {
  const headers = new Headers(init.headers ?? {});
  headers.set("X-Orch-Capability", capability);
  const response = await fetch(path, { ...init, headers, cache: "no-store" });
  const envelope = await response.json();
  if (!response.ok) throw Object.assign(new Error(envelope.error?.code ?? "read_failed"), { envelope });
  return envelope;
}

function formatTime(time) {
  return time === null ? "—" : new Intl.DateTimeFormat(prefs.lang === "zh" ? "zh-CN" : "en", { dateStyle:"medium", timeStyle:"short" }).format(new Date(time));
}
function element(name, className, text) {
  const node = document.createElement(name);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}
function pill(text, kind = "") { return element("span", `pill ${kind}`.trim(), text); }

function cardTitle(card) {
  if (card.kind === "task") return `${card.task?.round ?? "?"} / ${card.task?.id ?? "?"}`;
  if (card.kind === "fusion") return `Fusion · ${card.group?.id ?? card.id.replace("fusion:", "")}`;
  if (card.kind === "member") return card.member;
  return card.rows[0]?.purpose || card.rows[0]?.id || "—";
}
function createCard(card) {
  const button = element("button", `card${card.failed ? " failed" : ""}`);
  button.type = "button";
  button.dataset.cardId = card.id;
  const top = element("div", "card-top");
  const first = card.rows[0] ?? {};
  top.append(element("h3", "", cardTitle(card)), pill(card.taskState ?? "task unknown"));
  const summary = element("p", "card-summary", card.summary ?? t("noSummary"));
  const meta = element("div", "card-meta");
  const verified = card.rows.filter(row => row.result === "verified").length;
  meta.append(pill(`${card.rows.length} ${t("callsLabel")}`), pill(`${verified} verified`, verified ? "verified" : ""));
  meta.append(pill(`phase · ${first.phase ?? "unknown"}`), pill(`answer · ${first.result ?? "unknown"}`, first.result));
  const nativeEnded = first.native_status?.managedScopeTerminated === true || first.native_status?.terminalSeen === true || first.native_status?.state === "answered";
  meta.append(pill(nativeEnded ? t("nativeEnded") : t("nativeUnknown")));
  if (card.failed) meta.append(pill("needs attention", "failed"));
  if (card.mixed) meta.append(pill("mixed versions"));
  if (card.group && !card.group.roster_complete) meta.append(pill(t("partial")));
  const foot = element("div", "card-foot");
  foot.append(element("span", "", formatTime(card.time)), element("span", "", card.versions.length ? `${card.versions.length} ${t("versions")}` : card.rows[0]?.result ?? "none"));
  button.append(top, summary, meta, foot);
  button.addEventListener("click", () => openDrawer(card, button));
  return button;
}

function filters() {
  return { view, limit: limits[view], query: ui.search.value.trim(), result: ui.result.value, member: ui.member.value, time: ui.time.value, task: ui.task.value, taskState: ui.taskState.value, purpose: ui.purpose.value };
}
function render() {
  if (!state?.snapshot) return;
  const projection = model.project(state.snapshot.rows, state.snapshot.groups, filters());
  const home = view === "home";
  $("task-section").hidden = !home;
  $("unassociated-section").querySelector("h2").textContent = home ? t("unassociated") : view === "calls" ? t("calls") : t("members");
  ui.taskCards.replaceChildren(...projection.sections.tasks.map(createCard));
  const other = home ? projection.sections.unassociated : projection.cards;
  ui.otherCards.replaceChildren(...other.map(createCard));
  if (!projection.cards.length) ui.otherCards.append(element("div", "empty", t("empty")));
  ui.taskCount.textContent = String(projection.sections.tasks.length);
  ui.otherCount.textContent = String(other.length);
  ui.windowSummary.textContent = `${projection.cards.length} ${t("window")} · ${projection.outsideWindow} ${t("outside")} · ${projection.hiddenFailures} ${t("hiddenFailures")}`;
  ui.load.hidden = projection.cards.length >= projection.total;
  ui.cards.setAttribute("aria-busy", "false");
  if (selectedCard) {
    const fresh = projection.cards.find(card => card.id === selectedCard.id);
    const invocationExists = state.snapshot.rows.some(row => row.id === state.selected);
    if (fresh) {
      selectedCard = fresh;
      if (ui.drawer.classList.contains("open")) {
        populateDrawer(fresh);
        lastFocus = [...document.querySelectorAll(".card")].find(card => card.dataset.cardId === fresh.id) ?? lastFocus;
      }
      if (!invocationExists) { showNotice(t("missing")); clearAnswer(t("missing")); }
      else if (!state.refreshError) hideNotice();
    } else {
      showNotice(invocationExists ? t("moved") : t("missing"));
      if (!invocationExists) clearAnswer(t("missing"));
    }
  } else if (!state.refreshError) hideNotice();
}

function populateMembers() {
  const current = ui.member.value;
  const values = [...new Set((state?.snapshot?.rows ?? []).map(row => `${row.alias ?? "unknown"} / ${row.driver ?? "unknown"}`))].sort();
  ui.member.replaceChildren(new Option("All", ""), ...values.map(value => new Option(value, value)));
  if (values.includes(current)) ui.member.value = current;
  populateSelect(ui.task, [...new Set((state?.snapshot?.rows ?? []).map(row => row.task?.id).filter(Boolean))].sort());
  populateSelect(ui.taskState, [...new Set((state?.snapshot?.rows ?? []).map(row => row.task?.state).filter(Boolean))].sort());
  populateSelect(ui.purpose, [...new Set((state?.snapshot?.rows ?? []).map(row => row.purpose).filter(Boolean))].sort());
}
function populateSelect(select, values) { const current = select.value; select.replaceChildren(new Option("All", ""), ...values.map(value => new Option(value, value))); if (values.includes(current)) select.value = current; }
function populateProjects() {
  ui.project.replaceChildren(...projects.map(project => new Option(project.name || project.id, project.id)));
  if (state?.project) ui.project.value = state.project;
}
function showNotice(text) { ui.notice.textContent = text; ui.notice.hidden = false; }
function hideNotice() { ui.notice.hidden = true; }
function setStatus(ok, text) { ui.status.textContent = text; ui.dot.className = ok ? "ok" : "error"; }
function setBackgroundInert(value) { for (const node of document.querySelectorAll(".topbar,.views,main")) node.inert = value; }
function clearAnswer(status = "") { lastAnswerText = ""; ui.answer.replaceChildren(); ui.answerState.textContent = status; ui.copyAnswer.disabled = true; }

async function refresh() {
  if (!state) return;
  const previousSuccess = state.lastSuccessAt;
  state = model.beginRefresh(state);
  clearAnswer(ui.reader.classList.contains("open") ? t("loading") : "");
  const server = state.server, project = state.project, epoch = state.epoch, seq = state.lastSeq, scroll = ui.reader.scrollTop;
  try {
    const envelope = await request(`/api/v1/projects/${encodeURIComponent(project)}/snapshot`);
    const next = model.applySnapshot(state, envelope, epoch, seq);
    if (next === state) return;
    state = next;
    populateMembers(); render();
    ui.reader.scrollTop = scroll;
    setStatus(true, `${t("updated")} · ${formatTime(state.lastSuccessAt)} · ${t("evidence")} ${state.snapshot?.read_at ?? "—"}`);
  } catch (error) {
    const current = state.server === server && state.project === project && state.epoch === epoch && state.lastSeq === seq;
    if (!current || (error.envelope && !model.isCurrentResponse(state, error.envelope, epoch, seq))) return;
    state = model.failRefresh(state, error.message, previousSuccess);
    showNotice(t("failed"));
    setStatus(false, t("failed"));
  }
}

function openDrawer(card, trigger) {
  selectedCard = card; lastFocus = trigger;
  const initial = card.rows.find(row => row.result === "verified") ?? card.rows[0];
  state = model.select(state, initial?.id ?? null);
  clearAnswer("");
  populateDrawer(card);
  setBackgroundInert(true);
  document.body.classList.add("drawer-open");
  ui.drawer.inert = false;
  ui.drawer.classList.add("open"); ui.drawer.setAttribute("aria-hidden", "false"); ui.scrim.hidden = false;
  history.pushState({ drawer: card.id }, "");
  ui.drawer.focus();
}
function populateDrawer(card) {
  ui.drawerTitle.textContent = cardTitle(card);
  ui.drawerSummary.textContent = card.summary ?? t("noSummary");
  ui.memberList.replaceChildren(...card.rows.map(row => {
    const button = element("button", "member");
    button.type = "button"; button.dataset.rowId = row.id;
    const nativeEnded = row.native_status?.managedScopeTerminated === true || row.native_status?.terminalSeen === true || row.native_status?.state === "answered";
    button.append(element("strong", "", `${row.alias ?? "unknown"} / ${row.driver ?? "unknown"}`), pill(row.result, row.result), element("small", "", row.id), element("small", "", `${row.phase} · ${nativeEnded ? t("nativeEnded") : t("nativeUnknown")} · ${formatTime(model.stamp(row))}`));
    button.addEventListener("click", () => { state = model.select(state, row.id); clearAnswer(""); ui.openReader.disabled = row.result !== "verified"; if (row.result === "verified") loadDetail(row.id); else ui.answerState.textContent = row.result; });
    return button;
  }));
  const selected = card.rows.find(row => row.id === state.selected);
  ui.openReader.disabled = selected?.result !== "verified";
}
function closeDrawer(restore = true) {
  ui.drawer.classList.remove("open"); ui.drawer.setAttribute("aria-hidden", "true"); ui.scrim.hidden = true;
  ui.drawer.inert = true; document.body.classList.remove("drawer-open"); setBackgroundInert(false);
  if (restore) lastFocus?.focus();
}
async function loadDetail(id) {
  state = model.beginDetail(state, id);
  const server = state.server, project = state.project, epoch = state.epoch, seq = state.lastSeq;
  clearAnswer(t("loading"));
  try {
    const envelope = await request(`/api/v1/projects/${encodeURIComponent(project)}/detail?id=${encodeURIComponent(id)}`);
    const next = model.applyDetail(state, envelope, epoch, seq, id);
    if (next === state || state.server !== server || state.project !== project) return;
    state = next;
    if (state.answer === null) { clearAnswer(envelope.data?.row?.result ?? t("missing")); return; }
    lastAnswerText = state.answer;
    ui.copyAnswer.disabled = false;
    ui.readerTitle.textContent = id;
    ui.answer.replaceChildren(safeMarkdown(state.answer));
    ui.answerState.textContent = "";
  } catch (error) {
    const current = state.server === server && state.project === project && state.epoch === epoch && state.lastSeq === seq && state.selected === id;
    if (!current || (error.envelope && !model.isCurrentResponse(state, error.envelope, epoch, seq))) return;
    clearAnswer(t("missing"));
    ui.answerState.textContent = t("missing");
  }
}
function openReader() {
  const id = state?.selected ?? selectedCard?.rows[0]?.id;
  if (!id || ui.openReader.disabled) return;
  lastReaderFocus = document.activeElement;
  ui.drawer.inert = true; ui.reader.inert = false;
  ui.reader.classList.add("open"); ui.reader.setAttribute("aria-hidden", "false");
  history.pushState({ reader: id }, ""); ui.reader.focus(); loadDetail(id);
}
function closeReader(restore = true) {
  ui.reader.classList.remove("open"); ui.reader.setAttribute("aria-hidden", "true");
  ui.reader.inert = true; ui.drawer.inert = false;
  if (restore) (lastReaderFocus?.isConnected ? lastReaderFocus : ui.openReader).focus();
}

const ALLOWED = new Set(["P","BR","H1","H2","H3","H4","H5","H6","UL","OL","LI","BLOCKQUOTE","PRE","CODE","STRONG","EM","DEL","TABLE","THEAD","TBODY","TR","TH","TD","A","HR"]);
const DENIED_SUBTREES = new Set(["SCRIPT","STYLE","IFRAME","OBJECT","EMBED","SVG","MATH","FORM","VIDEO","AUDIO","SOURCE","TRACK","CANVAS","TEMPLATE"]);
function sanitizeNode(source, target) {
  if (source.nodeType === Node.TEXT_NODE) { target.append(document.createTextNode(source.nodeValue ?? "")); return; }
  if (source.nodeType !== Node.ELEMENT_NODE) return;
  const tag = source.tagName.toUpperCase();
  if (tag === "IMG") { target.append(document.createTextNode(source.getAttribute("alt") ?? "")); return; }
  if (DENIED_SUBTREES.has(tag)) return;
  if (!ALLOWED.has(tag)) { for (const child of source.childNodes) sanitizeNode(child, target); return; }
  if (tag === "A" && !model.safeHref(source.getAttribute("href"))) { for (const child of source.childNodes) sanitizeNode(child, target); return; }
  const clean = document.createElement(tag.toLowerCase());
  if (tag === "A") {
    const href = model.safeHref(source.getAttribute("href"));
    clean.setAttribute("href", href); clean.setAttribute("target", "_blank"); clean.setAttribute("rel", "noopener noreferrer");
  }
  for (const child of source.childNodes) sanitizeNode(child, clean);
  target.append(clean);
}
function safeMarkdown(text) {
  const parsed = new DOMParser().parseFromString(marked.parse(text, { gfm: true, breaks: false }), "text/html");
  const fragment = document.createDocumentFragment();
  for (const child of parsed.body.childNodes) sanitizeNode(child, fragment);
  return fragment;
}

// Export the exact production sanitizer for the explicit browser attack corpus.
// The returned fragment is detached; callers still decide where to place it.
export function sanitizeMarkdown(text) { return safeMarkdown(text); }

async function bootstrap() {
  applyTheme(); applyLanguage();
  try {
    const envelope = await request("/api/v1/projects");
    projects = envelope.data ?? [];
    if (!projects.length) throw new Error("not_found");
    state = model.initialState(envelope.serverInstanceId, projects[0].id);
    populateProjects(); await refresh();
  } catch (error) { setStatus(false, error.message); showNotice(error.message); }
}

document.querySelectorAll(".view").forEach(button => button.addEventListener("click", () => { document.querySelectorAll(".view").forEach(v => v.classList.toggle("active", v === button)); view = button.dataset.view; render(); }));
[ui.search, ui.result, ui.member, ui.time, ui.task, ui.taskState, ui.purpose].forEach(control => control.addEventListener("input", render));
$("refresh").addEventListener("click", refresh);
ui.load.addEventListener("click", () => { limits[view] += 30; render(); });
$("register-toggle").addEventListener("click", () => { ui.form.hidden = !ui.form.hidden; if (!ui.form.hidden) ui.path.focus(); });
ui.form.addEventListener("submit", async event => { event.preventDefault(); const prior = state; try { const envelope = await request("/api/v1/projects", { method:"POST", headers:{"Content-Type":"application/json"}, body:JSON.stringify({root:ui.path.value}) }); const list = await request("/api/v1/projects"); if (state !== prior || list.serverInstanceId !== prior.server) return; projects = list.data; clearAnswer(""); state = { ...model.initialState(list.serverInstanceId, envelope.data.id), epoch:prior.epoch+1 }; selectedCard = null; populateProjects(); ui.form.hidden = true; await refresh(); } catch (error) { if (state === prior) showNotice(error.message); } });
ui.project.addEventListener("change", async () => { const epoch = state.epoch + 1; closeReader(false); closeDrawer(false); clearAnswer(""); state = { ...model.initialState(state.server, ui.project.value), epoch }; selectedCard = null; await refresh(); });
ui.theme.addEventListener("change", () => { prefs.theme = ui.theme.value; model.writePreference(document, "theme", prefs.theme); applyTheme(); });
ui.lang.addEventListener("click", () => { prefs.lang = prefs.lang === "zh" ? "en" : "zh"; model.writePreference(document, "lang", prefs.lang); applyLanguage(); });
$("drawer-close").addEventListener("click", () => closeDrawer()); ui.scrim.addEventListener("click", () => closeDrawer());
ui.openReader.addEventListener("click", openReader); $("reader-back").addEventListener("click", () => closeReader());
ui.copyAnswer.addEventListener("click", async () => { if (!lastAnswerText) return; try { await navigator.clipboard.writeText(lastAnswerText); } catch { ui.answerState.textContent = "Copy unavailable"; } });
addEventListener("keydown", event => { if (event.key === "Escape") { if (ui.reader.classList.contains("open")) closeReader(); else if (ui.drawer.classList.contains("open")) closeDrawer(); } });
addEventListener("popstate", () => { if (ui.reader.classList.contains("open")) closeReader(false); else closeDrawer(false); });
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => { if (prefs.theme === "system") applyTheme(); });
setInterval(refresh, 2000);
bootstrap();
