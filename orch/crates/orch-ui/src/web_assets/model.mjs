const FAILURE_RESULTS = new Set(["failed", "invalid"]);
const RESULT_VALUES = new Set(["none", "verified", "invalid", "failed", "unknown", "too-large"]);

export function historicalUnverified(row) {
  return row?.result === "invalid" && row?.parameters?.channelDiagnostic?.code === "capture_evidence_missing";
}

function needsAttention(row) {
  return FAILURE_RESULTS.has(row?.result) && !historicalUnverified(row);
}

function strictTime(value) {
  if (typeof value !== "string") return null;
  const match = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.\d+)?(Z|[+-]\d{2}:\d{2})$/.exec(value);
  if (!match) return null;
  const [, ys, ms, ds, hs, mins, ss, zone] = match;
  const year = Number(ys), month = Number(ms), day = Number(ds);
  const hour = Number(hs), minute = Number(mins), second = Number(ss);
  const days = new Date(Date.UTC(year, month, 0)).getUTCDate();
  if (month < 1 || month > 12 || day < 1 || day > days || hour > 23 || minute > 59 || second > 59) return null;
  if (zone !== "Z") {
    const zh = Number(zone.slice(1, 3)), zm = Number(zone.slice(4, 6));
    if (zh > 23 || zm > 59) return null;
  }
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

export function stamp(row) {
  return strictTime(row?.source_time) ?? strictTime(row?.started_at);
}

function memberKey(row) {
  return `${row.alias ?? "unknown"} / ${row.driver ?? "unknown"}`;
}

function rowMatches(row, options) {
  const task = row.task ?? {};
  if (options.query) {
    const needle = String(options.query).toLocaleLowerCase();
    const haystack = [row.id, row.summary, row.alias, row.driver, row.purpose, task.id]
      .filter(Boolean).join(" ").toLocaleLowerCase();
    if (!haystack.includes(needle)) return false;
  }
  if (options.member && memberKey(row) !== options.member) return false;
  if (options.result && row.result !== options.result) return false;
  if (options.task && task.id !== options.task) return false;
  if (options.taskState && task.state !== options.taskState) return false;
  if (options.purpose && row.purpose !== options.purpose) return false;
  if (options.time && options.time !== "all") {
    const widths = { "24h": 86400000, "7d": 604800000, "30d": 2592000000 };
    const width = widths[options.time];
    const time = stamp(row);
    if (!width || time === null || time < (options.now ?? Date.now()) - width) return false;
  }
  return true;
}

function cardFrom(key, kind, rows, group = null) {
  const sortedRows = [...rows].sort((a, b) => (stamp(b) ?? -Infinity) - (stamp(a) ?? -Infinity) || String(a.id).localeCompare(String(b.id)));
  const times = sortedRows.map(stamp).filter(value => value !== null);
  const versions = [];
  const seen = new Set();
  for (const row of sortedRows) {
    if (!row.task) continue;
    const version = `${row.task.attempt ?? ""}\u0000${row.task.head ?? row.head ?? ""}`;
    if (!seen.has(version)) {
      seen.add(version);
      versions.push({ attempt: row.task.attempt ?? null, head: row.task.head ?? row.head ?? null, state: row.task.state ?? "unknown" });
    }
  }
  const states = [...new Set(sortedRows.map(row => row.task?.state).filter(Boolean))];
  const latestTask = sortedRows.find(row => row.task)?.task ?? null;
  const summaryRow = sortedRows.find(row => typeof row.summary === "string" && row.summary.trim());
  return {
    id: key,
    kind,
    rows: sortedRows,
    group,
    versions,
    summary: summaryRow ? summaryRow.summary.trim() : null,
    task: latestTask,
    taskState: latestTask?.state ?? null,
    mixed: states.length > 1,
    failed: sortedRows.some(needsAttention),
    time: times.length ? Math.max(...times) : null,
    member: kind === "member" ? memberKey(sortedRows[0]) : null,
  };
}

function groupRows(rows, groups, view) {
  if (view === "calls") return rows.map(row => cardFrom(`call:${row.id}`, "call", [row]));
  if (view === "members") {
    const map = new Map();
    for (const row of rows) {
      const key = memberKey(row);
      if (!map.has(key)) map.set(key, []);
      map.get(key).push(row);
    }
    return [...map].map(([key, values]) => cardFrom(`member:${key}`, "member", values));
  }
  const groupMap = new Map((groups ?? []).map(group => [group.id, group]));
  const taskMap = new Map(), fusionMap = new Map(), standalone = [];
  for (const row of rows) {
    if (row.task?.round && row.task?.id) {
      const key = `${row.task.round}\u0000${row.task.id}`;
      if (!taskMap.has(key)) taskMap.set(key, []);
      taskMap.get(key).push(row);
    } else if (row.fusion_id) {
      if (!fusionMap.has(row.fusion_id)) fusionMap.set(row.fusion_id, []);
      fusionMap.get(row.fusion_id).push(row);
    } else {
      standalone.push(cardFrom(`call:${row.id}`, "call", [row]));
    }
  }
  return [
    ...[...taskMap].map(([key, values]) => cardFrom(`task:${key}`, "task", values)),
    ...[...fusionMap].map(([key, values]) => cardFrom(`fusion:${key}`, "fusion", values, groupMap.get(key) ?? { id: key, total: null, roster_complete: false })),
    ...standalone,
  ];
}

function chronological(a, b) {
  if (a.time === null && b.time !== null) return 1;
  if (b.time === null && a.time !== null) return -1;
  if (a.time !== b.time) return (b.time ?? 0) - (a.time ?? 0);
  return String(a.rows[0]?.id ?? a.id).localeCompare(String(b.rows[0]?.id ?? b.id));
}

export function project(rows = [], groups = [], options = {}) {
  const filtered = rows.filter(row => rowMatches(row, options));
  const all = groupRows(filtered, groups, options.view ?? "home").sort(chronological);
  const limit = Math.max(1, Number(options.limit ?? 30));
  const offset = Math.max(0, Number(options.offset ?? 0));
  const selected = all.slice(offset, offset + limit);
  const cards = [...selected].sort((a, b) => Number(b.failed) - Number(a.failed) || chronological(a, b));
  const visibleIds = new Set(selected.map(card => card.id));
  const hiddenFailures = all.filter(card => card.failed && !visibleIds.has(card.id)).length;
  return {
    cards,
    sections: {
      tasks: cards.filter(card => card.kind === "task"),
      unassociated: cards.filter(card => card.kind !== "task"),
    },
    total: all.length,
    outsideWindow: Math.max(0, all.length - selected.length),
    hiddenFailures,
    offset,
    limit,
  };
}

export function initialState(server, project) {
  return { server, project, epoch: 0, lastSeq: 0, snapshotSeq: 0, detailSeq: 0, generation: 0, snapshot: null, selected: null, answer: null, detailPending: false, detailError: null, refreshError: null, lastSuccessAt: null };
}

export function beginRefresh(state) {
  const snapshotSeq = state.snapshotSeq + 1;
  return { ...state, lastSeq: snapshotSeq, snapshotSeq, refreshError: null };
}

export function isCurrentResponse(state, envelope, epoch, sequence) {
  return envelope?.serverInstanceId === state.server && envelope?.projectId === state.project && epoch === state.epoch && sequence === state.snapshotSeq && Number(envelope.snapshotGeneration) >= state.generation;
}

export function applySnapshot(state, envelope, epoch, sequence) {
  if (!isCurrentResponse(state, envelope, epoch, sequence)) return state;
  if (Number(envelope.snapshotGeneration) === state.generation && state.snapshot !== null) return state;
  const selected = envelope.data?.rows?.find(row => row.id === state.selected);
  const stillVerified = !state.selected || selected?.result === "verified";
  return {
    ...state,
    generation: Number(envelope.snapshotGeneration),
    snapshot: envelope.data,
    refreshError: null,
    lastSuccessAt: Date.now(),
    answer: stillVerified ? state.answer : null,
    detailPending: stillVerified ? state.detailPending : false,
    detailError: stillVerified ? state.detailError : (selected ? "answer_changed" : "not_found"),
    detailSeq: stillVerified ? state.detailSeq : state.detailSeq + 1,
  };
}

export function select(state, id) {
  return { ...state, selected: id, answer: null, detailSeq: state.detailSeq + 1, detailPending: false, detailError: null };
}

export function beginDetail(state, id) {
  return { ...state, selected: id, detailSeq: state.detailSeq + 1, answer: null, detailPending: true, detailError: null };
}

export function applyDetail(state, envelope, epoch, sequence, selectedId) {
  const current = state.selected === selectedId && epoch === state.epoch && sequence === state.detailSeq;
  const identity = envelope?.serverInstanceId === state.server && envelope?.projectId === state.project && envelope?.data?.row?.id === selectedId;
  if (!current) return state;
  if (!identity) return { ...state, answer: null, detailPending: false, detailError: "invalid_response" };
  const selected = state.snapshot?.rows?.find(row => row.id === selectedId);
  if (selected?.result !== "verified" || envelope.data.row.result !== "verified" || envelope.data.text == null) {
    return { ...state, answer: null, detailPending: false, detailError: envelope.data?.row?.result ?? "not_found" };
  }
  return { ...state, generation: Math.max(state.generation, Number(envelope.snapshotGeneration)), answer: envelope.data.text, detailPending: false, detailError: null, refreshError: null };
}

export function failDetail(state, code, epoch, sequence, selectedId) {
  if (state.selected !== selectedId || state.epoch !== epoch || state.detailSeq !== sequence) return state;
  return { ...state, answer: null, detailPending: false, detailError: code };
}

export function cancelDetail(state, code = null) {
  return { ...state, answer: null, detailSeq: state.detailSeq + 1, detailPending: false, detailError: code };
}

export function failRefresh(state, code, lastSuccessAt = state.lastSuccessAt) {
  return { ...state, refreshError: code, lastSuccessAt };
}

function cookieValue(cookies, key) {
  for (const pair of String(cookies ?? "").split(";")) {
    const [name, ...parts] = pair.trim().split("=");
    if (name === key) {
      try { return decodeURIComponent(parts.join("=")); }
      catch { return null; }
    }
  }
  return null;
}

export function preferences(cookies, browserLocale = "en") {
  const rawTheme = cookieValue(cookies, "orch_theme");
  const rawLang = cookieValue(cookies, "orch_lang");
  const theme = ["system", "light", "dark"].includes(rawTheme) ? rawTheme : "system";
  const fallbackLang = String(browserLocale).toLowerCase().startsWith("zh") ? "zh" : "en";
  const lang = ["zh", "en"].includes(rawLang) ? rawLang : fallbackLang;
  return { theme, lang };
}

export function writePreference(documentLike, key, value) {
  const allowed = key === "theme" ? ["system", "light", "dark"] : key === "lang" ? ["zh", "en"] : [];
  if (!allowed.includes(value)) return false;
  try {
    documentLike.cookie = `orch_${key}=${encodeURIComponent(value)}; Path=/; Max-Age=31536000; SameSite=Strict`;
    return true;
  } catch {
    return false;
  }
}

export function safeHref(value) {
  if (typeof value !== "string" || /[\u0000-\u001f\u007f]|%0[0-9a-f]|&#/i.test(value)) return null;
  try {
    const url = new URL(value);
    return url.protocol === "http:" || url.protocol === "https:" ? url.href : null;
  } catch {
    return null;
  }
}

export function resultValues() {
  return [...RESULT_VALUES];
}

// Role edits are independent of discovery refreshes and observation snapshots.
export function fusionRemoveRole(config, id) {
  const next = structuredClone(config);
  next.roles = next.roles.filter(role => role.id !== id);
  for (const group of next.combinations) {
    group.members = group.members.filter(member => member !== id);
    group.disabled = group.disabled.filter(member => member !== id);
    if (group.synthesizer === id) group.synthesizer = null;
  }
  return next;
}
export function fusionMoveMember(config, groupId, roleId, delta) {
  const next = structuredClone(config), group = next.combinations.find(g => g.id === groupId);
  if (!group || ![-1, 1].includes(delta)) return next;
  const index = group.members.indexOf(roleId), target = index + delta;
  if (index < 0 || target < 0 || target >= group.members.length) return next;
  [group.members[index], group.members[target]] = [group.members[target], group.members[index]];
  return next;
}
export function fusionTuple(role, row) {
  const native = row?.native?.current ?? {};
  return Object.fromEntries(["provider", "model", "effort", "mode"].map(key => [key, role?.fixed?.[key] ?? native[key] ?? null]));
}
export function fusionDraftError(config) {
  if (!config) return "loading";
  const bytes = text => new TextEncoder().encode(text ?? "").length;
  if (config.roles.some(role => !role.name.trim() || !role.harness || bytes(role.name) > 256 || bytes(role.instructions) > 16384)) return "roleIncomplete";
  return null;
}
export function fusionReady(config, groupId, rows) {
  const group = config?.combinations.find(g => g.id === groupId);
  if (!group) return "chooseCombination";
  const active = group.members.filter(id => !group.disabled.includes(id));
  if (active.length < 2 || active.length > 5) return "memberCount";
  if (!group.synthesizer) return "chooseSynthesis";
  for (const id of [...active, group.synthesizer]) {
    const role = config.roles.find(r => r.id === id), row = rows.find(r => r.id === role?.harness);
    if (!row || !row.enabled || row.availability !== "supported") return "clientUnavailable";
    const tuple = fusionTuple(role, row);
    if (["pi", "zcode", "dsh"].includes(row.driver) && ["provider", "model", "effort"].some(key => !tuple[key])) return "requiredNativePin";
    if (row.driver === "smartclaw" && Object.values(tuple).some(value => value !== null)) return "unsupportedPin";
    if (row.driver === "cursor" && (tuple.provider || tuple.effort || (tuple.mode && !["ask","plan"].includes(tuple.mode)))) return "unsupportedPin";
    if (["claude","codebuddy"].includes(row.driver) && (tuple.provider || (tuple.mode && tuple.mode!=="plan"))) return "unsupportedPin";
    if (row.driver === "codex" && (tuple.provider || (tuple.mode && tuple.mode!=="fast_mode"))) return "unsupportedPin";
    if (["opencode","mimo"].includes(row.driver) && tuple.mode && tuple.mode!=="pure") return "unsupportedPin";
    if (["pi","zcode"].includes(row.driver) && tuple.mode) return "unsupportedPin";
  }
  return null;
}
