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
  en: { readonly:"Local harness workspace",fusion:"Fusion",project:"Project",addProject:"Add project",theme:"Theme",projectPath:"Exact Git root",register:"Register",tasks:"Tasks",task:"Task",taskState:"Task state",purpose:"Purpose",calls:"All calls",members:"Members",search:"Search",result:"Answer",member:"Member",time:"Time",refresh:"Refresh",linked:"EXPLICITLY LINKED",taskOverview:"Task overview",unlinked:"NOT LINKED TO A TASK",unassociated:"Unassociated calls",loadMore:"Load 30 more",details:"DETAILS",readAnswer:"Read verified answer",back:"Back",verifiedAnswer:"VERIFIED ANSWER",copy:"Copy text",empty:"No observations match these filters",noSummary:"No captured summary",loading:"Loading…",updated:"Updated",failed:"Refresh failed · showing previous snapshot",callsLabel:"calls",versions:"versions",partial:"roster incomplete",moved:"Selected item is outside the current page",missing:"Selected item is no longer available",answer_changed:"Answer verification changed · reopen after refresh",not_found:"Selected answer is no longer available",read_failed:"Answer could not be revalidated",busy:"Reader is busy · try again",request_timeout:"Server timed out while reading the answer",forbidden:"Reader authorization expired · reload the page",invalid_response:"Reader returned an invalid response",client_timeout:"Answer read exceeded 12 seconds",window:"cards in current window",outside:"outside",hiddenFailures:"hidden failures",evidence:"evidence read",nativeUnknown:"native unknown",nativeEnded:"native ended" },
  zh: { readonly:"本机调用与观察",fusion:"Fusion",project:"项目",addProject:"添加项目",theme:"主题",projectPath:"精确 Git 根目录",register:"登记",tasks:"任务",task:"任务",taskState:"任务状态",purpose:"用途",calls:"全部调用",members:"成员",search:"搜索",result:"答卷",member:"成员",time:"时间",refresh:"刷新",linked:"明确关联",taskOverview:"任务总览",unlinked:"未关联任务",unassociated:"未关联调用",loadMore:"再加载 30 张",details:"详情",readAnswer:"阅读已验证答卷",back:"返回",verifiedAnswer:"已验证答卷",copy:"复制文本",empty:"没有符合筛选条件的记录",noSummary:"未捕获摘要",loading:"读取中…",updated:"已更新",failed:"刷新失败 · 正在显示旧快照",callsLabel:"次调用",versions:"个版本",partial:"名单不完整",moved:"所选记录已移出当前页面",missing:"所选记录已不可用",answer_changed:"答卷验证状态已变化，请刷新后重新打开",not_found:"所选答卷已不可用",read_failed:"答卷重新验证失败",busy:"阅读器正忙，请稍后重试",request_timeout:"服务端读取答卷超时",forbidden:"阅读权限已失效，请重新加载页面",invalid_response:"阅读器返回了无效响应",client_timeout:"答卷读取超过 12 秒",window:"张当前窗口卡片",outside:"窗口外",hiddenFailures:"个隐藏异常",evidence:"证据读取",nativeUnknown:"原生终态未知",nativeEnded:"原生已结束" },
};

let prefs = model.preferences(document.cookie, navigator.language);
let state = null;
let projects = [];
let view = new URL(location.href).searchParams.get("view") === "fusion" ? "fusion" : "home";
let selectedCard = null;
let lastFocus = null;
let lastReaderFocus = null;
let lastAnswerText = "";
let refreshPending = null;
let detailController = null;
let detailDeadline = null;
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
  let envelope;
  try { envelope = await response.json(); }
  catch { throw Object.assign(new Error("invalid_response"), { httpStatus:response.status }); }
  if (!envelope || typeof envelope !== "object" || Array.isArray(envelope)) throw Object.assign(new Error("invalid_response"), { httpStatus:response.status });
  const shaped = typeof envelope.serverInstanceId === "string" && Number.isFinite(Number(envelope.snapshotGeneration)) && (Object.hasOwn(envelope, "data") || Object.hasOwn(envelope, "error"));
  if (!shaped) throw Object.assign(new Error("invalid_response"), { envelope, httpStatus:response.status });
  if (!response.ok) throw Object.assign(new Error(typeof envelope.error?.code === "string" ? envelope.error.code : "invalid_response"), { envelope, httpStatus:response.status });
  return envelope;
}
export function createDetailDeadline(controller, options = {}) {
  const timeoutMs = options.timeoutMs ?? 12000;
  const schedule = options.schedule ?? setTimeout;
  const cancel = options.cancel ?? clearTimeout;
  let timedOut = false, handle = schedule(() => { timedOut = true; controller.abort(); }, timeoutMs);
  return {
    get timedOut() { return timedOut; },
    cancel() { if (handle !== null) { cancel(handle); handle = null; } },
  };
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
function diagnosticText(diagnostic) {
  if (!diagnostic) return null;
  const fields = [
    diagnostic.failureClass && `class=${diagnostic.failureClass}`,
    diagnostic.stage && `stage=${diagnostic.stage}`,
    diagnostic.durationSecs !== undefined && `elapsed=${diagnostic.durationSecs}s`,
    diagnostic.deadlineSecs !== undefined && `deadline=${diagnostic.deadlineSecs}s`,
    diagnostic.exitCode !== undefined && `exit=${diagnostic.exitCode}`,
    diagnostic.terminalStatus && `terminal=${diagnostic.terminalStatus}`,
    diagnostic.observedModel && `model=${diagnostic.observedModel}`,
    diagnostic.stdoutOverflow !== undefined && `stdoutOverflow=${diagnostic.stdoutOverflow}`,
    diagnostic.stderrOverflow !== undefined && `stderrOverflow=${diagnostic.stderrOverflow}`,
  ].filter(Boolean);
  const label = diagnostic.code === "capture_evidence_missing"
    ? (prefs.lang === "zh" ? "历史记录缺少捕获闭合证据" : "Historical record lacks capture-closure evidence")
    : (diagnostic.code ?? "diagnostic");
  return `${label}${fields.length ? ` · ${fields.join(" · ")}` : ""}${diagnostic.reason ? ` · ${diagnostic.reason}` : ""}`;
}

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
  const fusion = view === "fusion";
  $("fusion-workspace").hidden = !fusion;
  for (const id of ["observation-filters","cards","window-summary"]) $(id).hidden = fusion;
  document.querySelectorAll(".view").forEach(button => button.classList.toggle("active", button.dataset.view === view));
  if (fusion) { const key=state ? `${state.server}|${state.project}|${prefs.lang}` : null; if(key!==fusionRenderedKey)renderFusion(); return; }
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
function detailMessage(code) { return t(code === "missing" ? "not_found" : code || "read_failed"); }
function abortDetailTransport() {
  detailDeadline?.cancel();
  detailDeadline = null;
  if (detailController) detailController.abort();
  detailController = null;
}

async function refresh() {
  if (!state) return;
  const refreshKey = `${state.server}|${state.project}|${state.epoch}`;
  if (refreshPending === refreshKey) return;
  refreshPending = refreshKey;
  const previousSuccess = state.lastSuccessAt;
  state = model.beginRefresh(state);
  const server = state.server, project = state.project, epoch = state.epoch, seq = state.snapshotSeq, scroll = ui.reader.scrollTop;
  try {
    const envelope = await request(`/api/v1/projects/${encodeURIComponent(project)}/snapshot`);
    const next = model.applySnapshot(state, envelope, epoch, seq);
    if (next === state) return;
    state = next;
    populateMembers(); render();
    ui.reader.scrollTop = scroll;
    if (ui.reader.classList.contains("open") && !state.answer && !state.detailPending && state.detailError) {
      abortDetailTransport();
      clearAnswer(detailMessage(state.detailError));
    }
    setStatus(true, `${t("updated")} · ${formatTime(state.lastSuccessAt)} · ${t("evidence")} ${state.snapshot?.read_at ?? "—"}`);
  } catch (error) {
    const current = state.server === server && state.project === project && state.epoch === epoch && state.snapshotSeq === seq;
    if (!current) return;
    state = model.failRefresh(state, error.message, previousSuccess);
    showNotice(t("failed"));
    setStatus(false, t("failed"));
  } finally {
    if (refreshPending === refreshKey) refreshPending = null;
  }
}

function openDrawer(card, trigger) {
  abortDetailTransport(); selectedCard = card; lastFocus = trigger;
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
    const diagnostic = diagnosticText(row.parameters?.channelDiagnostic);
    if (diagnostic) button.append(element("small", model.historicalUnverified(row) ? "notice neutral" : "notice", diagnostic));
    button.addEventListener("click", () => { abortDetailTransport(); state = model.select(state, row.id); clearAnswer(row.result === "verified" ? "" : row.result); ui.openReader.disabled = row.result !== "verified"; });
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
  abortDetailTransport();
  state = model.beginDetail(state, id);
  const server = state.server, project = state.project, epoch = state.epoch, seq = state.detailSeq;
  const controller = new AbortController();
  const deadline = createDetailDeadline(controller);
  detailController = controller;
  detailDeadline = deadline;
  ui.readerTitle.textContent = id;
  clearAnswer(t("loading"));
  try {
    const envelope = await request(`/api/v1/projects/${encodeURIComponent(project)}/detail?id=${encodeURIComponent(id)}`, { signal:controller.signal });
    const next = model.applyDetail(state, envelope, epoch, seq, id);
    if (next === state || state.server !== server || state.project !== project) return;
    state = next;
    if (state.answer === null) { clearAnswer(detailMessage(state.detailError ?? envelope.data?.row?.result)); return; }
    lastAnswerText = state.answer;
    ui.copyAnswer.disabled = false;
    ui.readerTitle.textContent = id;
    ui.answer.replaceChildren(safeMarkdown(state.answer));
    ui.answerState.textContent = "";
  } catch (error) {
    const owned = state.server === server && state.project === project && state.epoch === epoch && state.detailSeq === seq && state.selected === id;
    if (!owned || (error.name === "AbortError" && !deadline.timedOut)) return;
    const code = deadline.timedOut ? "client_timeout" : error.message;
    state = model.failDetail(state, code, epoch, seq, id);
    clearAnswer(detailMessage(code));
  } finally {
    if (detailController === controller) {
      deadline.cancel();
      detailDeadline = null;
      detailController = null;
    }
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
  abortDetailTransport();
  if (state) state = model.cancelDetail(state);
  clearAnswer("");
  ui.readerTitle.textContent = "—";
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

const FUSION_TEXT = {
  en: {eyebrow:"EXPLICIT CONSULTATION",title:"Roles and Fusion",refreshNative:"Refresh native settings",reload:"Reload saved roles",discard:"Discard edits and reload",save:"Save roles",library:"Role library",addRole:"Add role",roleSettings:"Role settings",deleteRole:"Delete role",chooseRole:"Add or choose a role.",name:"Name",harness:"Native client",instructions:"Perspective and instructions",followHint:"Blank fields follow exposed native settings. Explicit IDs are kept unchanged. Consultation modes apply to each client.",provider:"Provider",model:"Model",effort:"Effort / variant",mode:"Native mode / profile",useConfigured:"Use captured project model pins",sources:"Native configuration sources",composition:"Saved combination",newCombination:"New combination",deleteCombination:"Delete combination",combination:"Combination",combinationName:"Combination name",addMember:"Add consultation role",add:"Add",synthesizer:"Synthesis role",combinationHint:"The checked 2–5 roles consult in parallel. The synthesis role is called separately once.",question:"Consultation question",start:"Start one Fusion",history:"Fusion runs",choose:"Choose…",loading:"Loading…",emptyRoles:"No saved roles yet.",emptyRuns:"No Fusion runs yet.",newRole:"New role",newGroup:"New combination",unsaved:"Unsaved role changes",saved:"Configuration revision",roleIncomplete:"Complete role names and clients; names and instructions must fit their limits.",chooseCombination:"Choose a saved combination.",memberCount:"Enable 2–5 consultation roles.",chooseSynthesis:"Choose a synthesis role.",clientUnavailable:"A selected client is disabled, unsupported or not installed.",requiredNativePin:"This channel requires provider, model and effort. Fill any default the client does not expose.",unsupportedPin:"This client does not support one of these separate parameter fields or modes.",saveFirst:"Save role changes before starting.",questionRequired:"Enter a question (up to 128 KiB).",ready:"Native settings will be scanned again before launch. There is no automatic retry.",remove:"Remove",up:"Move up",down:"Move down",scanning:"Scanning native settings; no models are called…",nativeUnknown:"Native default not exposed",follow:"Follow native",available:"available",catalog:"catalog",'current-only':"current settings only",'auth-required':"login required",unknown:"unknown",unsupported:"unsupported",supported:"supported",unavailable:"unavailable",preparing:"Reading current native configuration",consulting:"Consulting in parallel",synthesizing:"Synthesizing once",completed:"Completed",failed:"Failed",hold:"HOLD · inspect the unfinished run; it will not restart",verified:"Verified answer",pending:"Not started",running:"Running",skipped:"Skipped",'timed-out':"Timed out",'too-large':"Answer retained; too large for this reader",invalid:"Artifact changed or could not be verified",synthesis:"Synthesis",requested:"Requested parameters",captured:"Captured configuration revision",copy:"Copy answer",copied:"Copied",revision_conflict:"The saved configuration changed. Your edits are retained; reload to reconcile.",project_has_unclosed_run:"This project has an unfinished run. Inspect its status before starting another.",prompt_contains_secret:"Remove credentials from the question or role instructions; configure them in the native client.",fusion_unavailable:"Fusion is temporarily unavailable. Existing inputs and results are retained.",request_id_conflict:"This request ID already belongs to different input.",combination_not_ready:"The combination is incomplete.",combination_not_found:"This combination changed or was deleted. Reload saved roles.",invalid_fusion_config:"Check required fields and role references.",invalid_fusion_request:"Check the question and request identity.",fusion_input_limit:"A field exceeds the supported size limit.",native_discovery_failed:"Native settings could not be refreshed.",retryStatus:"Checking the previously submitted request…",cursorHint:"Cursor accepts a complete native model ID and ask/plan mode; separate provider/effort fields are unsupported.",smartHint:"This channel has no per-call model parameter override; leave these fields blank.",pinHint:"This channel requires provider/model/effort; defaults it does not expose need an explicit value."},
  zh: {eyebrow:"手动发起咨询",title:"角色与 Fusion",refreshNative:"刷新原生配置",reload:"重载已保存配置",discard:"放弃更改并重载",save:"保存角色配置",library:"角色库",addRole:"添加角色",roleSettings:"角色设置",deleteRole:"删除角色",chooseRole:"添加或选择一个角色。",name:"名称",harness:"原生客户端",instructions:"视角与说明",followHint:"留空表示跟随客户端已暴露的原生设置；固定 ID 原样保留。各客户端另应用咨询模式。",provider:"Provider",model:"模型",effort:"推理强度 / variant",mode:"原生模式 / profile",useConfigured:"采用项目别名的模型参数",sources:"原生配置来源",composition:"已保存组合",newCombination:"新建组合",deleteCombination:"删除组合",combination:"组合",combinationName:"组合名称",addMember:"添加咨询角色",add:"添加",synthesizer:"合成角色",combinationHint:"勾选的 2–5 个角色并行咨询，随后单独调用合成角色一次。",question:"咨询问题",start:"开始一次 Fusion",history:"Fusion 记录",choose:"请选择…",loading:"读取中…",emptyRoles:"尚未保存角色。",emptyRuns:"尚无 Fusion 记录。",newRole:"新角色",newGroup:"新组合",unsaved:"角色配置尚未保存",saved:"配置版本",roleIncomplete:"请填写角色名称和客户端，并检查名称、说明的长度。",chooseCombination:"请选择一个组合。",memberCount:"请启用 2–5 个咨询角色。",chooseSynthesis:"请选择合成角色。",clientUnavailable:"所选客户端未安装、已停用或不支持咨询。",requiredNativePin:"此通道需要 provider、model 和 effort；未暴露的默认值需显式填写。",unsupportedPin:"此客户端不支持部分独立参数或所选模式，请核对或清空这些字段。",saveFirst:"请先保存角色配置。",questionRequired:"请输入问题，最多 128 KiB。",ready:"启动前会重新扫描原生配置；失败不会自动重试。",remove:"移除",up:"上移",down:"下移",scanning:"正在扫描原生配置，不调用模型…",nativeUnknown:"原生未暴露默认值",follow:"跟随原生",available:"可用",catalog:"有模型目录",'current-only':"仅当前设置",'auth-required':"需要登录",unknown:"未知",unsupported:"不支持",supported:"支持咨询",unavailable:"不可用",preparing:"正在读取最新原生配置",consulting:"正在并行咨询",synthesizing:"正在进行一次合成",completed:"已完成",failed:"失败",hold:"HOLD · 请检查未结束的运行，不会自动重启",verified:"已验证答卷",pending:"未启动",running:"运行中",skipped:"已跳过",'timed-out':"超时",'too-large':"答卷已保留，超出本阅读器大小限制",invalid:"产物已改变或无法验证",synthesis:"合成",requested:"本次请求参数",captured:"本次配置版本",copy:"复制答卷",copied:"已复制",revision_conflict:"已保存配置发生变化。草稿已保留，请重载后核对。",project_has_unclosed_run:"当前项目有未结束的运行，请先检查其状态。",prompt_contains_secret:"请移除问题或角色说明中的凭据，并在原生客户端中配置。",fusion_unavailable:"Fusion 暂不可用，已有输入与结果仍保留。",request_id_conflict:"此请求 ID 已用于其他输入。",combination_not_ready:"组合尚未配置完整。",combination_not_found:"组合已变化或删除，请重载已保存配置。",invalid_fusion_config:"请检查必填字段与角色引用。",invalid_fusion_request:"请检查问题和请求标识。",fusion_input_limit:"某个字段超出了大小限制。",native_discovery_failed:"暂时无法刷新原生配置。",retryStatus:"正在读取上次提交的请求状态…",cursorHint:"Cursor 支持完整原生模型 ID 及 ask/plan 模式，不支持独立 provider、effort 字段。",smartHint:"当前通道不提供每次调用的模型覆盖，请将参数留空。",pinHint:"此通道需要 provider/model/effort；未暴露的默认值请显式填写。"}
};
const fusionStates = new Map();
let fusionRenderedKey = null;
function ft(key) { return FUSION_TEXT[prefs.lang]?.[key] ?? key; }
function fusionState() {
  if (!state) return null;
  const key = `${state.server}|${state.project}`;
  if (!fusionStates.has(key)) fusionStates.set(key, {server:state.server,project:state.project,config:null,native:null,role:null,group:null,dirty:false,editSeq:0,configSeq:0,runSeq:0,loading:false,saving:false,starting:false,scanBusy:false,question:"",runs:[],run:null,runId:null,pending:null,error:null,scanError:null,runError:null,initialized:false});
  return fusionStates.get(key);
}
function fusionCurrent(f) { return state?.server === f.server && state?.project === f.project; }
function fusionRows(f) { return f.native?.harnesses ?? []; }
function fusionRole(f) { return f.config?.roles.find(role => role.id === f.role); }
function fusionGroup(f) { return f.config?.combinations.find(group => group.id === f.group); }
async function fusionRequest(f, suffix, init = {}) {
  const envelope = await request(`/api/v1/projects/${encodeURIComponent(f.project)}/fusion/${suffix}`, init);
  if (envelope.serverInstanceId !== f.server || envelope.projectId !== f.project) throw Error("stale_project");
  return envelope.data;
}
const fusionPost = body => ({method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(body)});
function fusionOptions(node, choices, value, placeholder = true) {
  const options = placeholder ? [new Option(ft("choose"), "")] : [];
  for (const [id, label, disabled] of choices) { const option = new Option(label, id); option.disabled = Boolean(disabled); options.push(option); }
  if (value && !choices.some(choice => choice[0] === value)) options.push(new Option(`${value} · ${ft("unavailable")}`, value));
  node.replaceChildren(...options); node.value = value ?? "";
}
function fusionValue(id, value) { const node = $(id), next = value ?? ""; if (node.value !== next) node.value = next; }
function fusionRedraw(f) { if (view === "fusion" && fusionCurrent(f)) renderFusion(); }
function fusionEdited(f) { f.dirty = true; f.editSeq += 1; f.error = null; renderFusionControls(f); renderFusionRoles(f); }
async function fusionLoadConfig(f) {
  const seq = ++f.configSeq; f.loading = true; f.error = null; fusionRedraw(f);
  try { const config = await fusionRequest(f, "config"); if (seq !== f.configSeq) return; f.config = config; f.dirty = false; if (!config.roles.some(r => r.id === f.role)) f.role = config.roles[0]?.id ?? null; if (!config.combinations.some(g => g.id === f.group)) f.group = config.combinations[0]?.id ?? null; }
  catch (error) { if (seq === f.configSeq) f.error = error.message; }
  finally { if (seq === f.configSeq) { f.loading = false; fusionRedraw(f); } }
}
async function fusionScan(f, refresh = false) {
  if (f.scanRequest) return; f.scanRequest = true; let changed=false;
  if (refresh) { f.scanBusy = true; f.scanError = null; fusionRedraw(f); }
  try { const data = await fusionRequest(f, "discovery", refresh ? fusionPost({}) : {}); f.scanBusy = data.scanning; f.scanError = data.error; if (data.snapshot && (!data.scanning || !f.native)) { f.native = data.snapshot; changed=true; } }
  catch (error) { f.scanBusy = false; f.scanError = error.message; }
  finally { f.scanRequest = false; if(fusionCurrent(f)&&view==="fusion"){renderFusionControls(f);if(changed)renderFusionRoleForm(f);} }
}
async function fusionHistory(f) {
  if (f.historyRequest) return; f.historyRequest = true;
  try { f.runs = await fusionRequest(f, "runs"); if (!f.runId && f.runs.length) { f.runId = f.runs[0].id; await fusionReadRun(f); } }
  catch (error) { f.runError = error.message; }
  finally { f.historyRequest = false; if (fusionCurrent(f) && view === "fusion") renderFusionHistory(f); }
}
async function fusionReadRun(f) {
  if (!f.runId || f.readRequest) return;
  const id = f.runId, seq = ++f.runSeq; f.readRequest = true;
  try { const run = await fusionRequest(f, `runs/${encodeURIComponent(id)}`); if (f.runId === id && seq === f.runSeq) { f.run = run; f.runError = null; f.pending = null; } }
  catch (error) { if (f.runId === id && seq === f.runSeq) { f.runError = error.message; if(f.run) for(const member of [...f.run.members,f.run.synthesis]) { member.answer=null; member.answerStatus="invalid"; } } }
  finally { f.readRequest = false; if (fusionCurrent(f) && view === "fusion") { renderFusionResults(f); renderFusionControls(f); } }
}
async function fusionSave(f) {
  if (!f.config || f.saving || model.fusionDraftError(f.config)) return;
  const seq = f.editSeq, config = structuredClone(f.config); f.saving = true; f.error = null; renderFusionControls(f);
  try { const saved = await fusionRequest(f, "config", fusionPost({expectedRevision:config.revision,config})); if (seq === f.editSeq) { f.config = saved; f.dirty = false; } else f.config.revision = saved.revision; }
  catch (error) { f.error = error.message; }
  finally { f.saving = false; fusionRedraw(f); }
}
function renderFusionRoles(f) {
  const nodes = (f.config?.roles ?? []).map(role => { const button = element("button", role.id === f.role ? "quiet active" : "quiet"); button.type = "button"; button.dataset.roleId = role.id; button.disabled = f.loading; button.append(element("strong", "", role.name || ft("newRole")), element("small", "", role.harness || ft("choose"))); button.addEventListener("click", () => { f.role = role.id; renderFusion(); }); return button; });
  $("fusion-role-list").replaceChildren(...nodes);
  if (!nodes.length) $("fusion-role-list").append(element("p", "fusion-hint", ft("emptyRoles")));
}
function renderFusionRoleForm(f) {
  const role = fusionRole(f), row = fusionRows(f).find(row => row.id === role?.harness);
  $("fusion-role-fields").hidden = !role; $("fusion-role-empty").hidden = Boolean(role); $("fusion-delete-role").disabled = !role || f.loading;
  if (!role) return;
  fusionValue("fusion-role-name", role.name); fusionValue("fusion-role-instructions", role.instructions);
  fusionOptions($("fusion-role-harness"), fusionRows(f).map(row => [row.id, `${row.alias ?? row.driver} · ${row.driver} · ${ft(row.availability !== "supported" ? row.availability : row.native.status)}`, !row.enabled || row.availability !== "supported"]), role.harness);
  for (const key of ["provider","model","effort","mode"]) { const input = $(`fusion-role-${key}`); fusionValue(input.id, role.fixed?.[key]); input.placeholder = row?.native?.current?.[key] ? `${ft("follow")}: ${row.native.current[key]}` : ft("nativeUnknown"); input.disabled = f.loading; }
  for (const id of ["fusion-role-name","fusion-role-instructions","fusion-role-harness"]) $(id).disabled = f.loading;
  const models = row?.native?.models ?? [], current = model.fusionTuple(role,row);
  $("fusion-provider-options").replaceChildren(...[...new Set(models.map(m => m.provider).concat(row?.native?.current?.provider).filter(Boolean))].map(v => new Option(v,v)));
  const selectedModels = models.filter(m => !current.provider || !m.provider || m.provider === current.provider);
  $("fusion-model-options").replaceChildren(...selectedModels.map(m => { const id = row?.driver === "mimo" && m.provider ? `${m.provider}/${m.id}` : m.id; return new Option(m.name || id,id); }));
  const selected = models.find(m => (m.id === current.model || `${m.provider}/${m.id}` === current.model) && (!current.provider || m.provider === current.provider));
  const efforts = [...new Set((selected?.efforts ?? []).concat(row?.native?.current?.effort).filter(Boolean))]; $("fusion-effort-options").replaceChildren(...efforts.map(v => new Option(v,v)));
  const hint = row?.driver === "cursor" ? ft("cursorHint") : row?.driver === "smartclaw" ? ft("smartHint") : ["pi","dsh","zcode"].includes(row?.driver) ? ft("pinHint") : "";
  $("fusion-role-native").textContent = row ? `${ft(row.availability)} · ${ft(row.native.status)}${row.native.current.model ? ` · ${row.native.current.model}` : ""}${hint ? ` — ${hint}` : ""}` : ft("clientUnavailable");
  $("fusion-use-configured").disabled = f.loading || !row?.alias || !["provider","model","effort"].some(k => row.configured?.[k]);
  $("fusion-role-sources").replaceChildren(...(row?.sources ?? []).map(source => element("li", "", `${source.kind}: ${source.path}`)));
}
function renderFusionComposition(f) {
  const focusId = document.activeElement?.id, group = fusionGroup(f), roles = f.config?.roles ?? [];
  fusionOptions($("fusion-combination"), (f.config?.combinations ?? []).map(g => [g.id,g.name]), f.group);
  fusionValue("fusion-combination-name",group?.name);
  $("fusion-combination-name").disabled = !group || f.loading; $("fusion-delete-combination").disabled = !group || f.loading;
  fusionOptions($("fusion-add-member"),roles.filter(role => !group?.members.includes(role.id)).map(role => [role.id,role.name]),$("fusion-add-member").value);
  $("fusion-add-member-button").disabled = !group || f.loading || !roles.some(role => !group.members.includes(role.id));
  fusionOptions($("fusion-synthesizer"),roles.map(role => [role.id,role.name]),group?.synthesizer); $("fusion-synthesizer").disabled = !group || f.loading;
  $("fusion-members").replaceChildren(...(group?.members ?? []).map((id,index) => {
    const role = roles.find(r => r.id === id), row = element("div","fusion-member"), label = element("label"), check = element("input"); check.type="checkbox"; check.id=`fusion-enabled-${id}`; check.checked=!group.disabled.includes(id); check.disabled=f.loading;
    check.addEventListener("change",()=>{group.disabled = check.checked ? group.disabled.filter(v=>v!==id) : [...group.disabled,id];fusionEdited(f);});
    label.append(check,document.createTextNode(role?.name ?? id));row.append(label);
    for (const [delta,key] of [[-1,"up"],[1,"down"]]) { const b=element("button","quiet",delta<0?"↑":"↓");b.type="button";b.id=`fusion-${key}-${id}`;b.setAttribute("aria-label",`${ft(key)} ${role?.name ?? id}`);b.disabled=f.loading || index+delta<0 || index+delta>=group.members.length;b.addEventListener("click",()=>{f.config=model.fusionMoveMember(f.config,group.id,id,delta);fusionEdited(f);renderFusionComposition(f);});row.append(b); }
    const remove=element("button","quiet",ft("remove"));remove.type="button";remove.disabled=f.loading;remove.addEventListener("click",()=>{group.members=group.members.filter(v=>v!==id);group.disabled=group.disabled.filter(v=>v!==id);fusionEdited(f);renderFusionComposition(f);});row.append(remove);return row;
  }));
  if (focusId && document.activeElement === document.body) $(focusId)?.focus();
}
function renderFusionControls(f) {
  const invalid=model.fusionDraftError(f.config), readiness=model.fusionReady(f.config,f.group,fusionRows(f));
  $("fusion-note").textContent = f.error ? ft(f.error) : f.loading ? ft("loading") : invalid ? ft(invalid) : f.dirty ? ft("unsaved") : `${ft("saved")} ${f.config?.revision ?? 0}`;
  $("fusion-note").classList.toggle("fusion-note-neutral", !f.error && !invalid);
  $("fusion-save").disabled=f.loading || f.saving || !f.dirty || Boolean(invalid);$("fusion-reload").disabled=f.loading || f.saving;$("fusion-reload").textContent=ft(f.dirty?"discard":"reload");
  $("fusion-add-role").disabled=!f.config || f.loading || f.config.roles.length>=32;$("fusion-add-combination").disabled=!f.config || f.loading || f.config.combinations.length>=16;
  $("fusion-native-refresh").disabled=f.scanBusy;
  const running=f.runs.some(run=>["preparing","consulting","synthesizing"].includes(run.phase)) || ["preparing","consulting","synthesizing","hold"].includes(f.run?.phase);
  const questionBad=!f.question.trim() || new TextEncoder().encode(f.question).length>131072;
  $("fusion-start").textContent=f.pending?(prefs.lang==="zh"?"重试原请求（同一 ID）":"Retry original request (same ID)"):ft("start");
  $("fusion-question").disabled=f.starting||Boolean(f.pending);
  $("fusion-start").disabled=f.loading || f.saving || f.starting || running || f.dirty || Boolean(invalid) || Boolean(readiness) || questionBad;
  $("fusion-ready").textContent=ft(f.dirty?"saveFirst":readiness ?? (questionBad?"questionRequired":"ready"));
  $("fusion-native-status").textContent=f.scanBusy?ft("scanning"):f.scanError?ft(f.scanError):f.native?`${f.native.observed_at} · ${fusionRows(f).filter(r=>r.enabled&&r.availability==="supported").length} ${ft("supported")}`:ft("loading");
}
function renderFusionResults(f) {
  const oldScroll=new Map([...$("fusion-results").children].map(node=>[node.dataset.resultKey,node.querySelector(".markdown")?.scrollTop??0]));
  const focusId=document.activeElement?.id;
  $("fusion-run-status").textContent=f.runError?ft(f.runError):f.run?`${ft(f.run.phase)} · ${ft("captured")} ${f.run.configRevision} · ${f.run.id}${f.run.reason?` · ${ft(f.run.reason)}`:""}`:"";
  const nodes=[];
  if (f.run) for (const [member,synthesis] of [...f.run.members.map(m=>[m,false]),[f.run.synthesis,true]]) {
    const card=element("section",`fusion-result${synthesis?" synthesis":""}`);card.dataset.resultKey=`${synthesis?"synthesis":"member"}-${member.roleId}`;card.append(element("h3","",`${member.name}${synthesis?` · ${ft("synthesis")}`:""} · ${ft(member.status)}`));
    card.append(element("p","fusion-hint",`${member.harness} · ${ft("requested")}: ${Object.entries(member.tuple).filter(([,v])=>v!==null).map(([k,v])=>`${k}=${v}`).join(" · ") || ft("nativeUnknown")}`));
    card.append(element("p","fusion-hint",`${synthesis ? (prefs.lang === "zh" ? "合成波" : "Synthesis wave") : (prefs.lang === "zh" ? "成员波" : "Member wave")} · ${prefs.lang === "zh" ? "硬上限" : "hard ceiling"}=900s`));
    if (member.reason && !member.channelDiagnostic) card.append(element("p","fusion-hint",member.reason));
    const diagnostic=diagnosticText(member.channelDiagnostic);if(diagnostic)card.append(element("p","notice",diagnostic));
    if (member.answerStatus && member.answerStatus!=="verified") card.append(element("p","notice",ft(member.answerStatus)));
    if (member.answerStatus==="verified" && member.answer) {const body=element("article","markdown");body.append(safeMarkdown(member.answer));card.append(body);const copy=element("button","quiet",ft("copy"));copy.type="button";copy.id=`fusion-copy-${card.dataset.resultKey}`;copy.addEventListener("click",async()=>{try{await navigator.clipboard.writeText(member.answer);copy.textContent=ft("copied");}catch{copy.textContent="Copy unavailable";}});card.append(copy);}
    nodes.push(card);
  }
  $("fusion-results").replaceChildren(...nodes);
  for(const node of nodes){const body=node.querySelector(".markdown");if(body)body.scrollTop=oldScroll.get(node.dataset.resultKey)??0;}
  if(focusId&&document.activeElement===document.body)$(focusId)?.focus({preventScroll:true});
}
function renderFusionHistory(f) {
  $("fusion-history").replaceChildren(...f.runs.map(run=>{const b=element("button","quiet",`${ft(run.phase)} · ${run.question} · ${run.createdAt}`);b.type="button";b.dataset.runId=run.id;b.addEventListener("click",()=>{f.runId=run.id;f.run=null;f.runSeq+=1;renderFusionResults(f);fusionReadRun(f);});return b;}));
  if (!f.runs.length) $("fusion-history").append(element("p","fusion-hint",ft("emptyRuns")));
}
function renderFusion() {
  const f=fusionState();if(!f)return;
  fusionRenderedKey=`${f.server}|${f.project}|${prefs.lang}`;
  document.querySelectorAll("[data-f-i18n]").forEach(node=>{node.textContent=ft(node.dataset.fI18n);});
  if(!f.initialized){f.initialized=true;fusionLoadConfig(f);fusionScan(f,true);fusionHistory(f);}
  renderFusionRoles(f);renderFusionRoleForm(f);renderFusionComposition(f);renderFusionControls(f);fusionValue("fusion-question",f.question);renderFusionResults(f);renderFusionHistory(f);
}
async function fusionStart(f) {
  if(f.starting || f.dirty || model.fusionReady(f.config,f.group,fusionRows(f)))return;
  f.starting=true;f.runError=null;renderFusionControls(f);
  if(!f.pending){f.pending={requestId:crypto.randomUUID(),combinationId:f.group,question:f.question};f.pendingRevision=f.config.revision;}
  try {const init=fusionPost(f.pending);init.headers["X-Orch-Config-Revision"]=String(f.pendingRevision);const run=await fusionRequest(f,"runs",init);f.run=run;f.runId=run.id;f.pending=null;await fusionHistory(f);}
  catch(error){f.runError=error.message;if(["revision_conflict","project_has_unclosed_run","combination_not_ready","combination_not_found","invalid_fusion_request","prompt_contains_secret"].includes(error.message))f.pending=null;else if(f.pending){f.runId=f.pending.requestId;await fusionReadRun(f);}}
  finally{f.starting=false;fusionRedraw(f);}
}
$("fusion-native-refresh").addEventListener("click",()=>fusionScan(fusionState(),true));$("fusion-reload").addEventListener("click",()=>fusionLoadConfig(fusionState()));$("fusion-save").addEventListener("click",()=>fusionSave(fusionState()));
$("fusion-add-role").addEventListener("click",()=>{const f=fusionState();if(!f.config)return;const role={id:crypto.randomUUID(),name:ft("newRole"),instructions:"",harness:"",fixed:{provider:null,model:null,effort:null,mode:null}};f.config.roles.push(role);f.role=role.id;fusionEdited(f);renderFusion();$("fusion-role-name").focus();});
$("fusion-delete-role").addEventListener("click",()=>{const f=fusionState();if(!f.role)return;f.config=model.fusionRemoveRole(f.config,f.role);f.role=f.config.roles[0]?.id??null;fusionEdited(f);renderFusion();});
for(const key of ["name","instructions","provider","model","effort","mode"]) $(`fusion-role-${key}`).addEventListener("input",event=>{const f=fusionState(),role=fusionRole(f);if(!role)return;if(["name","instructions"].includes(key))role[key]=event.target.value;else role.fixed[key]=event.target.value===""?null:event.target.value;fusionEdited(f);if(key==="name")renderFusionComposition(f);});
for(const key of ["provider","model"]) $(`fusion-role-${key}`).addEventListener("change",()=>renderFusionRoleForm(fusionState()));
$("fusion-role-harness").addEventListener("change",event=>{const f=fusionState(),role=fusionRole(f);if(!role)return;role.harness=event.target.value;role.fixed={provider:null,model:null,effort:null,mode:null};fusionEdited(f);renderFusionRoleForm(f);});
$("fusion-use-configured").addEventListener("click",()=>{const f=fusionState(),role=fusionRole(f),row=fusionRows(f).find(r=>r.id===role?.harness);if(!role||!row)return;for(const key of ["provider","model","effort"])role.fixed[key]=row.configured?.[key]??null;fusionEdited(f);renderFusionRoleForm(f);});
$("fusion-add-combination").addEventListener("click",()=>{const f=fusionState();if(!f.config)return;const group={id:crypto.randomUUID(),name:ft("newGroup"),members:[],disabled:[],synthesizer:null};f.config.combinations.push(group);f.group=group.id;fusionEdited(f);renderFusionComposition(f);});
$("fusion-delete-combination").addEventListener("click",()=>{const f=fusionState();f.config.combinations=f.config.combinations.filter(g=>g.id!==f.group);f.group=f.config.combinations[0]?.id??null;fusionEdited(f);renderFusionComposition(f);});
$("fusion-combination").addEventListener("change",event=>{const f=fusionState();f.group=event.target.value;renderFusionComposition(f);renderFusionControls(f);});
$("fusion-combination-name").addEventListener("input",event=>{const f=fusionState(),group=fusionGroup(f);if(group){group.name=event.target.value;fusionEdited(f);}});
$("fusion-add-member-button").addEventListener("click",()=>{const f=fusionState(),group=fusionGroup(f),id=$("fusion-add-member").value;if(group&&id&&!group.members.includes(id)){group.members.push(id);fusionEdited(f);renderFusionComposition(f);}});
$("fusion-synthesizer").addEventListener("change",event=>{const f=fusionState(),group=fusionGroup(f);if(group){group.synthesizer=event.target.value||null;fusionEdited(f);}});
$("fusion-question").addEventListener("input",event=>{const f=fusionState();f.question=event.target.value;renderFusionControls(f);});$("fusion-start").addEventListener("click",()=>fusionStart(fusionState()));
setInterval(()=>{if(view!=="fusion"||!state)return;const f=fusionState();if(f.scanBusy)fusionScan(f);if(f.runId&&(["preparing","consulting","synthesizing"].includes(f.run?.phase)||f.pending))fusionReadRun(f);if(f.runs.some(r=>["preparing","consulting","synthesizing"].includes(r.phase)))fusionHistory(f);},2000);


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
ui.form.addEventListener("submit", async event => { event.preventDefault(); const prior = state; try { const envelope = await request("/api/v1/projects", { method:"POST", headers:{"Content-Type":"application/json"}, body:JSON.stringify({root:ui.path.value}) }); const list = await request("/api/v1/projects"); if (state !== prior || list.serverInstanceId !== prior.server) return; abortDetailTransport(); projects = list.data; clearAnswer(""); state = { ...model.initialState(list.serverInstanceId, envelope.data.id), epoch:prior.epoch+1 }; selectedCard = null; populateProjects(); ui.form.hidden = true; render(); await refresh(); } catch (error) { if (state === prior) showNotice(error.message); } });
ui.project.addEventListener("change", async () => { const epoch = state.epoch + 1; closeReader(false); closeDrawer(false); abortDetailTransport(); clearAnswer(""); state = { ...model.initialState(state.server, ui.project.value), epoch }; selectedCard = null; render(); await refresh(); });
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
