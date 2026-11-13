// Главное окно Mihomo Tray.
"use strict";

const state = {
  status: null,
  page: "overview",
  rules: [],
  groupsForRules: [],
  defaultGroup: "Proxy",
  ruleFilter: { q: "", type: "all", act: "all" },
  selected: new Set(),
  groups: [],
  openGroups: new Set(),
  hosts: [],
  sitesFilter: { seg: "all", q: "", scope: "suffix" },
  settings: null,
  paths: null,
};

const KIND_LABEL = {
  "DOMAIN-SUFFIX": "Сайт",
  "DOMAIN": "Адрес",
  "DOMAIN-KEYWORD": "Слово",
  "PROCESS-NAME": "Программа",
  "IP-CIDR": "IP-сеть",
  "IP-CIDR6": "IP-сеть",
};
const KIND_HINT = {
  "DOMAIN-SUFFIX": "Сайт и все его поддомены",
  "DOMAIN": "Только этот адрес, без поддоменов",
  "DOMAIN-KEYWORD": "Любой адрес, содержащий слово",
  "PROCESS-NAME": "Весь трафик программы",
  "IP-CIDR": "Диапазон IP-адресов",
  "IP-CIDR6": "Диапазон IPv6-адресов",
};
const ruleKey = (r) => `${r.kind},${r.value.toLowerCase()}`;
const ruleCat = (r) => (r.kind === "PROCESS-NAME" ? "process" : r.kind.startsWith("IP-CIDR") ? "ip" : "domain");

// ================================================================= инициализация
function fillIcons(root = document) {
  $$("[data-icon]", root).forEach((el) => {
    if (!el.firstChild) el.innerHTML = icon(el.dataset.icon);
  });
}

function go(page) {
  state.page = page;
  $$("#nav button").forEach((b) => b.classList.toggle("on", b.dataset.page === page));
  $$(".page").forEach((p) => p.classList.toggle("on", p.id === "page-" + page));
  if (page === "nodes") loadGroups();
  if (page === "rules") loadRules();
  if (page === "subs") loadSubs();
  if (page === "sites") { loadRules(true); loadHosts(); }
  if (page === "settings") loadSettings();
}

async function init() {
  fillIcons();
  $$("#nav button").forEach((b) => b.addEventListener("click", () => go(b.dataset.page)));

  listen("status", renderStatus);
  listen("toast", (t) => toast(t.text, t.kind));
  listen("navigate", handleNavigate);
  listen("smart", () => loadSmart());

  try { renderStatus(await invoke("get_status")); } catch {}
  // окно создано заново (после выгрузки из памяти) — открыть страницу, которую просили
  invoke("take_nav").then((p) => { if (p) handleNavigate(p); }).catch(() => {});
  loadRules(true);
  loadSmart();

  bindOverview();
  bindRules();
  bindSites();
  bindNodes();
  bindSettings();
  bindExtras();
  bindSubs();

  // фоновые обновления, пока окно видно
  setInterval(() => {
    if (document.visibilityState !== "visible") return;
    if (state.page === "sites") loadHosts();
  }, 2000);
  setInterval(() => {
    if (document.visibilityState !== "visible") return;
    if (state.page !== "sites") loadHosts(true);
    if (state.page === "nodes") loadGroups(true);
    if (state.page === "nodes" || state.page === "overview") loadSmart();
    if (state.page === "settings") loadExtras();
  }, 6000);

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeModal();
  });
}

function handleNavigate(target) {
  const [page, action] = String(target).split(":");
  go(page || "overview");
  if (action === "add-domain") openRuleModal({ tab: "domain" });
  if (action === "add-process") openRuleModal({ tab: "process" });
  if (action === "broken") setSitesSeg("broken");
}

// ================================================================= обзор
function bindOverview() {
  $("#powerBtn").addEventListener("click", () => {
    const st = state.status;
    if (!st || st.busy) return;
    const running = st.api_ok || st.service === "running";
    if (st.service === "pending") return;
    call("service_action", { action: running ? "stop" : "start" });
  });
  $("#restartBtn").addEventListener("click", () => call("service_action", { action: "restart" }));
  $("#pingBtn").addEventListener("click", async (e) => {
    const b = e.currentTarget;
    b.disabled = true;
    try {
      await invoke("test_group", { group: state.status?.main_group || "Proxy" });
      toast("Пинг обновлён", "ok", 1800);
    } catch (err) {
      toast(String(err), "error");
    } finally { b.disabled = false; }
  });
  $$("#modeSeg button").forEach((b) =>
    b.addEventListener("click", async () => {
      await call("set_mode", { mode: b.dataset.mode });
      $$("#modeSeg button").forEach((x) => x.classList.toggle("on", x === b));
    })
  );
  $$(".qa").forEach((b) =>
    b.addEventListener("click", () => {
      const q = b.dataset.q;
      if (q === "domain") openRuleModal({ tab: "domain" });
      if (q === "process") openRuleModal({ tab: "process" });
      if (q === "broken") { go("sites"); setSitesSeg("broken"); }
      if (q === "panel") invoke("open_target", { what: "panel" });
    })
  );
}

const MODE_TEXT = {
  rule: "Сайты из правил идут через VPN, остальные — напрямую",
  global: "Весь трафик идёт через VPN",
  direct: "Весь трафик идёт напрямую, без VPN",
};

function renderStatus(st) {
  state.status = st;
  const busy = st.busy;
  const power = $("#powerBtn");
  power.className = "power " + st.health + (busy ? " busy" : "");

  const title = $("#stateTitle");
  let t, sub;
  if (busy === "start") { t = "Включение…"; sub = "Запускаем службу Mihomo"; }
  else if (busy === "stop") { t = "Выключение…"; sub = "Останавливаем службу Mihomo"; }
  else if (busy === "restart") { t = "Перезапуск…"; sub = "Перезапускаем службу Mihomo"; }
  else if (st.health === "ok") { t = st.mode === "direct" ? "Напрямую" : "Защищено"; sub = MODE_TEXT[st.mode] || st.reason; }
  else if (st.health === "warn") { t = "Есть проблема"; sub = st.reason; }
  else { t = "VPN выключен"; sub = st.reason === "VPN выключен" ? "Нажмите кнопку, чтобы включить" : st.reason; }
  title.textContent = t;
  title.className = "state " + (busy ? "" : st.health);
  $("#stateReason").textContent = sub;
  const cn = $("#coreNote");
  cn.classList.toggle("hidden", !st.core_note);
  cn.textContent = st.core_note || "";

  const chain = $("#chain");
  chain.innerHTML = "";
  if (st.api_ok && st.chain && st.chain.length) {
    st.chain.forEach((c, i) => {
      if (i) chain.append(h("i", {}, "›"));
      chain.append(h("span", { title: c }, c));
    });
  }

  const ping = $("#pingChip");
  const dc = delayClass(st.delay);
  ping.className = "chip " + (st.api_ok ? ({ good: "ok", mid: "warn", bad: "bad", none: "" }[dc]) : "off");
  ping.lastChild.textContent = st.api_ok ? "Пинг: " + delayText(st.delay) : "Пинг: —";
  $("#pingBtn").disabled = !st.api_ok;
  $("#restartBtn").classList.toggle("hidden", !(st.api_ok || st.service === "running"));
  $("#restartBtn").disabled = !!busy;

  $$("#modeSeg button").forEach((b) => {
    b.classList.toggle("on", b.dataset.mode === st.mode);
    b.disabled = !st.api_ok;
  });

  $("#downSpeed").textContent = fmtSpeed(st.down_speed);
  $("#upSpeed").textContent = fmtSpeed(st.up_speed);
  if (st.history_down && st.history_down.length > 1) sparkline($("#spark"), st.history_down, st.history_up);
  $("#tDown").textContent = st.api_ok ? fmtBytes(st.down_total) : "—";
  $("#tUp").textContent = st.api_ok ? fmtBytes(st.up_total) : "—";
  $("#tConns").textContent = st.api_ok ? st.conns : "—";
  $("#tVersion").textContent = st.version || "—";

  $("#sideDot").className = "pulse " + st.health;
  $("#sideTitle").textContent = busy ? t : st.health === "ok" ? (st.mode === "direct" ? "Напрямую" : "Подключено") : st.health === "warn" ? "Есть проблема" : "Выключено";
  $("#sideSub").textContent = st.api_ok && st.node ? `${st.node} · ${delayText(st.delay)}` : st.reason;
  renderSmartLine();
  const gl = $("#gameLine");
  gl.classList.toggle("hidden", !st.game);
  if (st.game) { gl.innerHTML = icon("cpu"); gl.append(h("span", { class: "ellipsis" }, `Игровой режим: ${st.game} — VPN не мешает игре`)); }
}

// ================================================================= умный выбор
async function loadSmart() {
  try { state.smart = await invoke("get_smart"); } catch { return; }
  renderSmartLine();
  if (state.page === "nodes" && !$(".modal-back")) renderGroups();
}

function renderSmartLine() {
  const el = $("#smartLine");
  if (!el) return;
  const sm = state.smart;
  const show = sm && sm.enabled && state.status?.api_ok && (sm.reason || sm.speed_progress);
  el.classList.toggle("hidden", !show);
  if (!show) return;
  el.innerHTML = icon("zap");
  const sp = sm.speed_progress;
  el.append(h("span", { class: "ellipsis" }, sp ? `Тест скорости нод: ${sp[0] + 1} из ${sp[1]}…` : "Умный выбор: " + sm.reason));
}

const mbit = (bps) => (bps * 8 / 1e6).toFixed(bps * 8 / 1e6 >= 10 ? 0 : 1) + " Мбит/с";

function smartCard(g) {
  const sm = state.smart;
  const scores = sm.nodes.filter((n) => n.score != null).map((n) => n.score);
  const best = scores.length ? Math.min(...scores) : null;
  const sp = sm.speed_progress;

  const speedBtn = h("button", { class: "btn sm", disabled: !!sp, onclick: async () => {
    try { await invoke("smart_speed_test"); toast("Тест скорости запущен — ваш трафик он не трогает", "info", 3500); loadSmart(); }
    catch (e) { toast(String(e), "error"); }
  } }, iconEl("download"), sp ? "Идёт тест…" : "Тест скорости");
  const roundBtn = h("button", { class: "btn sm", onclick: () => {
    invoke("smart_round"); toast("Проверяю все ноды…", "info", 1500); setTimeout(loadSmart, 5000);
  } }, iconEl("refresh"), "Проверить");

  const head = h("div", { class: "smart-head" },
    h("div", { class: "grow", style: { minWidth: 0 } },
      h("div", { class: "row" },
        h("span", { class: "name" }, g.name),
        h("span", { class: "chip smart", html: icon("zap") }, "Умный выбор"),
        sm.pinned ? h("span", { class: "chip violet" }, "закреплено вручную") : null),
      h("div", { class: "smart-reason ellipsis", title: sm.reason }, sm.reason || "Собираю статистику — первые результаты появятся через минуту")),
    speedBtn, roundBtn);

  const parts = [head];
  if (sp) {
    const pct = Math.round((sp[0] / Math.max(1, sp[1])) * 100);
    parts.push(h("div", { class: "progress" }, h("div", { class: "bar", style: { width: pct + "%" } }),
      h("span", {}, `Тест скорости: ${sp[2] || "подготовка"} (${sp[0] + 1} из ${sp[1]})`)));
  }
  if (sm.error) parts.push(h("div", { class: "smart-err" }, sm.error));

  const table = h("div", { class: "smart-table" },
    h("div", { class: "st-head" }, h("div", {}, "#"), h("div", {}, "Нода"), h("div", { class: "r" }, "Пинг"),
      h("div", { class: "r" }, "Разброс"), h("div", { class: "r" }, "Потери"), h("div", { class: "r" }, "Скорость"),
      h("div", {}, "Качество"), h("div", {})));
  sm.nodes.forEach((n, i) => {
    const cur = n.name === sm.current;
    const pinned = n.name === sm.pinned;
    const q = n.score != null && best != null ? Math.max(4, Math.round((best / n.score) * 100)) : 0;
    const qc = n.score == null ? "bad" : q >= 80 ? "good" : q >= 50 ? "mid" : "bad";
    const ping = n.median != null ? n.median + " мс" : n.samples ? "нет ответа" : "…";
    table.append(h("div", { class: "st-row" + (cur ? " cur" : "") + (n.dead ? " dead" : "") },
      h("div", { class: "dim num" }, i + 1),
      h("div", { class: "row ellipsis", style: { gap: "8px" } }, h("span", { class: "ellipsis", title: n.name }, n.name),
        cur ? h("span", { class: "chip ok" }, "сейчас") : null, pinned ? h("span", { class: "chip violet", html: icon("check") }, "закреплена") : null),
      h("div", { class: "r num delay " + delayClass(n.median ?? (n.samples ? 0 : null)) }, ping),
      h("div", { class: "r num muted" }, n.jitter != null ? "±" + n.jitter : "—"),
      h("div", { class: "r num " + (n.loss > 0.2 ? "bad-t" : n.loss > 0 ? "mid-t" : "muted") }, n.samples ? Math.round(n.loss * 100) + "%" : "—"),
      h("div", { class: "r num " + (n.speed_error && !n.speed_error.startsWith("пропущена") ? "bad-t" : "muted"), title: n.speed_error || "" },
        n.speed ? mbit(n.speed) : n.speed_error ? (n.speed_error.startsWith("пропущена") ? "пропущена" : "ошибка") : "—"),
      h("div", { class: "qbar " + qc, title: n.score != null ? "Оценка: " + n.score.toFixed(0) + " (меньше — лучше)" : "Нода не отвечает" }, h("i", { style: { width: q + "%" } })),
      h("div", { class: "r" }, h("button", { class: "btn sm ghost", title: pinned ? "Вернуть автоматический выбор" : "Всегда использовать эту ноду (пока она отвечает)",
        onclick: async () => {
          await call("smart_pin", { node: pinned ? "" : n.name });
          toast(pinned ? "Закрепление снято — снова выбираю сам" : `${n.name} закреплена`, "ok", 2200);
          setTimeout(loadSmart, 1500);
        } }, pinned ? "Открепить" : "Закрепить"))));
  });
  parts.push(table);

  if (sm.history.length) {
    parts.push(h("div", { class: "smart-hist" }, h("b", {}, "Последние решения"),
      ...sm.history.slice(0, 4).map(([t, text]) => h("div", {}, h("span", { class: "dim num" }, new Date(t * 1000).toLocaleTimeString("ru-RU", { hour: "2-digit", minute: "2-digit" })), " ", text))));
  }
  parts.push(h("div", { class: "hint", style: { marginTop: "10px" } },
    "Оценка учитывает медиану пинга, его разброс, потери за последние 10 минут и скорость. Нода, которая перестала отвечать, заменяется в течение ~30 секунд; ради небольшого выигрыша узел не меняется чаще раза в 5 минут."));
  return h("div", { class: "card group smart-card" }, ...parts);
}

// ================================================================= подписки
function bindSubs() {
  $("#addSubBtn").addEventListener("click", () => openSubModal());
  $$("#subModeSeg button").forEach((b) => b.addEventListener("click", async () => {
    const subs = state.subs?.subs || [];
    if (b.dataset.m === "all") return subAction("activate", "*", {}, "Используются все подписки");
    const first = subs.find((x) => x.active) || subs[0];
    if (first) subAction("activate", first.name, {}, `Активна только «${first.name}»`);
  }));
}

async function loadSubs() {
  try { state.subs = await invoke("subs_list"); } catch (e) { toast(String(e), "error"); return; }
  renderSubs();
}

function maskUrl(u) {
  try {
    const x = new URL(u);
    const tail = (x.pathname + x.search).slice(-4);
    return `${x.protocol}//${x.host}/••••••${tail}`;
  } catch { return "••••••"; }
}

function renderSubs() {
  const v = state.subs;
  const box = $("#subsList");
  box.innerHTML = "";
  if (v.error) toast(v.error, "error");
  $("#navSubCount").textContent = v.subs.length > 1 ? v.subs.length : "";
  const one = v.mode !== "all" && v.mode !== "custom";
  $$("#subModeSeg button").forEach((b) => {
    b.classList.toggle("on", (b.dataset.m === "all" && v.mode === "all") || (b.dataset.m === "one" && one));
    b.disabled = v.subs.length < 2;
  });
  $("#subModeHint").textContent = v.mode === "custom" ? "сейчас группы настроены вручную по-разному" : v.subs.length < 2 ? "переключение появится, когда подписок будет больше одной" : "";
  if (!v.subs.length) {
    box.append(emptyState("layers", "Подписок пока нет", "Нажмите «Добавить подписку» и вставьте ссылку от провайдера"));
    return;
  }
  for (const s of v.subs) {
    let shown = false;
    const urlEl = h("span", { class: "mono ellipsis sub-url", title: "Ссылка скрыта — это ваш личный ключ" }, maskUrl(s.url));
    const eye = h("button", { class: "btn icon ghost", title: "Показать ссылку", html: icon("eye"), onclick: () => {
      shown = !shown; urlEl.textContent = shown ? s.url : maskUrl(s.url);
    } });
    const copy = h("button", { class: "btn icon ghost", title: "Скопировать ссылку", html: icon("copy"), onclick: async () => {
      try { await navigator.clipboard.writeText(s.url); toast("Ссылка скопирована", "ok", 1500); } catch { toast("Не удалось скопировать", "error"); }
    } });
    const upd = s.updated_at ? new Date(s.updated_at) : null;
    const meta = [
      s.nodes != null ? `${s.nodes} ${plural(s.nodes, "нода", "ноды", "нод")}` : "ноды ещё не загружены",
      upd && !isNaN(upd) && upd.getFullYear() > 2000 ? "обновлена " + upd.toLocaleString("ru-RU", { day: "2-digit", month: "2-digit", hour: "2-digit", minute: "2-digit" }) : null,
      s.kind && s.kind !== "http" ? "тип: " + s.kind : null,
    ].filter(Boolean).join(" · ");
    const card = h("div", { class: "card sub-card" + (s.active ? " active" : "") },
      h("div", { class: "sub-ic", html: icon("layers") }),
      h("div", { class: "grow", style: { minWidth: 0 } },
        h("div", { class: "row" }, h("b", { class: "sub-name" }, s.name),
          s.active ? h("span", { class: "chip ok" }, h("span", { class: "dot" }), "используется") : h("span", { class: "chip off" }, "не используется")),
        h("div", { class: "row", style: { gap: "4px", marginTop: "4px" } }, urlEl, eye, copy),
        h("div", { class: "hint" }, meta)),
      h("div", { class: "sub-ops" },
        one && !s.active ? h("button", { class: "btn sm primary", onclick: () => subAction("activate", s.name, {}, `Активна «${s.name}»`) }, "Сделать активной") : null,
        h("button", { class: "btn sm", title: "Скачать список нод заново", onclick: (e) => subAction("refresh", s.name, {}, "Подписка обновлена", e.currentTarget) }, iconEl("refresh"), "Обновить"),
        h("button", { class: "btn sm", onclick: () => openSubModal(s) }, iconEl("edit"), "Ссылка"),
        h("button", { class: "btn sm danger", onclick: async () => {
          const last = v.subs.length === 1;
          if (!(await confirmModal("Удалить подписку?", last
            ? `«${s.name}» — последняя подписка. После удаления ноды пропадут и трафик из групп пойдёт напрямую, пока не добавите новую.`
            : `Ноды «${s.name}» пропадут из всех групп. Копия конфига сохранится в резервных копиях.`, "Удалить", true))) return;
          subAction("remove", s.name, {}, "Подписка удалена");
        } }, iconEl("trash"))));
    box.append(card);
  }
}

async function subAction(action, name, extra = {}, okText = "", btn = null) {
  if (btn) btn.disabled = true;
  try {
    const r = await invoke("subs_action", { action, name, ...extra });
    if (showApply(r.apply, okText)) { await loadSubs(); loadGroups(true); loadSmart(); }
    return r;
  } catch (e) { toast(String(e), "error"); }
  finally { if (btn) btn.disabled = false; }
}

function openSubModal(existing = null) {
  const isEdit = !!existing;
  const url = h("input", { class: "input mono", autofocus: true, placeholder: "https://… ссылка подписки от провайдера", value: isEdit ? existing.url : "" });
  const name = h("input", { class: "input", placeholder: "необязательно — возьмётся из адреса", disabled: isEdit, value: isEdit ? existing.name : "" });
  const act = h("input", { type: "checkbox" });
  const hasSubs = (state.subs?.subs || []).length > 0;
  act.checked = !hasSubs;
  const paste = h("button", { class: "btn sm", onclick: async () => {
    try { url.value = (await navigator.clipboard.readText()).trim(); url.focus(); } catch { toast("Не удалось прочитать буфер — вставьте Ctrl+V", "warn"); }
  } }, iconEl("download"), "Вставить");
  const go = h("button", { class: "btn primary", onclick: async () => {
    if (!/^https?:\/\/\S+$/.test(url.value.trim())) { toast("Нужна ссылка вида https://…", "warn"); return; }
    go.disabled = true;
    go.lastChild.textContent = isEdit ? "Сохраняю…" : "Добавляю и загружаю ноды…";
    const r = isEdit
      ? await subAction("set_url", existing.name, { url: url.value.trim() }, "Ссылка изменена")
      : await subAction("add", name.value.trim(), { url: url.value.trim(), activate: act.checked }, "Подписка добавлена");
    if (r && r.apply.ok) closeModal();
    else { go.disabled = false; go.lastChild.textContent = isEdit ? "Сохранить" : "Добавить"; }
  } }, h("span", {}, isEdit ? "Сохранить" : "Добавить"));
  openModal(h("div", { class: "modal" },
    h("h2", {}, isEdit ? `Ссылка подписки «${existing.name}»` : "Новая подписка"),
    h("div", { class: "sub" }, isEdit ? "После сохранения ноды скачаются заново." : "Ссылку даёт ваш VPN-провайдер (часто называется «подписка для Clash / Mihomo»)."),
    h("div", { class: "field" }, h("div", { class: "row" }, h("label", { class: "grow", style: { fontSize: "12.5px", fontWeight: 600, color: "var(--muted)" } }, "Ссылка"), paste), url),
    isEdit ? null : h("div", { class: "field" }, h("label", {}, "Название"), name),
    isEdit || !hasSubs ? null : h("label", { class: "row", style: { fontSize: "13px", cursor: "pointer" } }, h("span", { class: "cb" }, act, h("span")),
      "Сделать активной (использовать только её, остальные отключить)"),
    h("div", { class: "foot" }, h("button", { class: "btn ghost", onclick: closeModal }, "Отмена"), go)));
  modalEl.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); go.click(); } });
}

// ================================================================= ноды
function bindNodes() {
  $("#nodeSearch").addEventListener("input", renderGroups);
}

async function loadGroups(silent) {
  try {
    state.groups = await invoke("get_groups");
  } catch (e) {
    state.groups = [];
    if (!silent) $("#groups").innerHTML = "";
    if (!silent) $("#groups").append(emptyState("wifi", "Mihomo не отвечает", "Включите VPN, чтобы увидеть ноды"));
    return;
  }
  if (!state.openGroups.size && state.status?.main_group) state.openGroups.add(state.status.main_group);
  renderGroups();
}

function renderGroups() {
  const box = $("#groups");
  const q = $("#nodeSearch").value.trim().toLowerCase();
  box.innerHTML = "";
  const sm = state.smart;
  const smartOn = sm && sm.enabled;
  const groups = state.groups.filter((g) => !q || g.name.toLowerCase().includes(q) || g.nodes.some((n) => n.name.toLowerCase().includes(q)));
  if (smartOn) groups.sort((a, b) => (b.name === sm.group) - (a.name === sm.group));
  if (!groups.length) { box.append(emptyState("server", "Ничего не найдено", "")); return; }
  for (const g of groups) {
    if (smartOn && g.name === sm.group && !q) { box.append(smartCard(g)); continue; }
    const open = q ? true : state.openGroups.has(g.name);
    const auto = !g.selectable;
    const nowNode = g.nodes.find((n) => n.name === g.now);
    const head = h("div", { class: "group-head" },
      h("div", { class: "grow ellipsis" },
        h("div", { class: "row" }, h("span", { class: "name" }, g.name),
          h("span", { class: "chip " + (auto ? "blue" : "violet") }, auto ? "авто · " + typeLabel(g.kind) : "ручной выбор"),
          h("span", { class: "dim", style: { fontSize: "12px" } }, g.nodes.length + " " + plural(g.nodes.length, "нода", "ноды", "нод"))),
        h("div", { class: "now ellipsis" }, "Сейчас: ", h("b", {}, g.now || "—"), nowNode ? h("span", { class: "delay " + delayClass(nowNode.delay), style: { marginLeft: "8px" } }, delayText(nowNode.delay)) : null)),
      h("button", { class: "btn sm", title: "Проверить пинг всех нод группы", onclick: async (e) => {
        e.stopPropagation();
        const b = e.currentTarget; b.disabled = true; b.lastChild.textContent = "Проверяем…";
        try { await invoke("test_group", { group: g.name }); } catch (err) { toast(String(err), "error"); }
        await loadGroups(true);
      } }, iconEl("gauge"), h("span", {}, "Пинг")),
      iconEl("down", "caret"));
    const nodes = h("div", { class: "nodes" });
    for (const n of g.nodes) {
      if (q && !g.name.toLowerCase().includes(q) && !n.name.toLowerCase().includes(q)) continue;
      nodes.append(h("button", {
        class: "node" + (n.name === g.now ? " on" : "") + (auto ? " ro" : ""),
        title: auto ? "Группа выбирает ноду автоматически" : "Выбрать " + n.name,
        onclick: async () => {
          if (auto || n.name === g.now) return;
          await call("select_node", { group: g.name, node: n.name });
          toast(`${g.name} → ${n.name}`, "ok", 1800);
          loadGroups(true);
        },
      }, h("span", { class: "nm ellipsis" }, n.name),
         h("span", { class: "meta" }, h("span", {}, typeLabel(n.kind)), h("span", { class: "delay " + delayClass(n.delay) }, delayText(n.delay)))));
    }
    const card = h("div", { class: "card group" + (open ? " open" : "") }, head, h("div", { class: "group-body" }, nodes));
    head.addEventListener("click", () => {
      card.classList.toggle("open");
      card.classList.contains("open") ? state.openGroups.add(g.name) : state.openGroups.delete(g.name);
    });
    box.append(card);
  }
}

function typeLabel(t) {
  return { Selector: "выбор", URLTest: "по пингу", Fallback: "резерв", LoadBalance: "балансировка", Direct: "напрямую", Reject: "блок", Vless: "VLESS", Vmess: "VMess", Trojan: "Trojan", Shadowsocks: "SS", Hysteria2: "Hysteria2", Tuic: "TUIC", WireGuard: "WireGuard" }[t] || t || "";
}

function emptyState(ic, title, text) {
  return h("div", { class: "empty", html: icon(ic) }, h("b", {}, title), text);
}

// ================================================================= правила
function bindRules() {
  $("#ruleSearch").addEventListener("input", (e) => { state.ruleFilter.q = e.target.value.trim().toLowerCase(); renderRules(); });
  segBind("#ruleTypeSeg", "f", (v) => { state.ruleFilter.type = v; renderRules(); });
  segBind("#ruleActSeg", "a", (v) => { state.ruleFilter.act = v; renderRules(); });
  $("#addRuleBtn").addEventListener("click", () => openRuleModal({ tab: "domain" }));
  $("#importBtn").addEventListener("click", openImportModal);
  $("#exportBtn").addEventListener("click", async () => {
    try {
      const path = await pickSaveFile("mihomo-rules.txt");
      if (!path) return;
      const n = await invoke("export_rules", { path });
      toast(`Экспортировано ${n} ${plural(n, "правило", "правила", "правил")}`, "ok");
    } catch (e) { toast(String(e), "error"); }
  });
  $("#ruleAll").addEventListener("change", (e) => {
    const vis = filteredRules();
    if (e.target.checked) vis.forEach((r) => state.selected.add(ruleKey(r)));
    else vis.forEach((r) => state.selected.delete(ruleKey(r)));
    renderRules();
  });
  $("#bulkClear").addEventListener("click", () => { state.selected.clear(); renderRules(); });
  $("#bulkDelete").addEventListener("click", async () => {
    const keys = [...state.selected];
    if (!(await confirmModal("Удалить правила?", `Будет удалено ${keys.length} ${plural(keys.length, "правило", "правила", "правил")}. Копия конфига сохранится в резервных копиях.`, "Удалить", true))) return;
    const res = await call("delete_rules", { keys });
    if (showApply(res, "Правила удалены")) { state.selected.clear(); loadRules(); }
  });
  $("#bulkApply").addEventListener("click", async () => {
    const action = $("#bulkAction").value;
    const list = state.rules.filter((r) => state.selected.has(ruleKey(r)));
    // одним изменением конфига: один бэкап и одна перезагрузка Mihomo вместо N
    const items = list
      .filter((r) => r.action !== action)
      .map((r) => ({ key: ruleKey(r), item: { kind: r.kind, value: r.value, action } }));
    if (!items.length) { toast("Изменений нет", "info"); return; }
    try {
      const res = await invoke("update_rules", { items });
      showApply(res, "Действие изменено");
    } catch (e) { toast(String(e), "error"); }
    loadRules();
  });
}

function segBind(sel, attr, cb) {
  $$(sel + " button").forEach((b) =>
    b.addEventListener("click", () => {
      $$(sel + " button").forEach((x) => x.classList.toggle("on", x === b));
      cb(b.dataset[attr]);
    })
  );
}

async function loadRules(silent) {
  try {
    const rv = await invoke("get_rules");
    if (rv.error && !silent) toast(rv.error, "error");
    state.rules = rv.rules || [];
    state.groupsForRules = rv.groups && rv.groups.length ? rv.groups : ["Proxy"];
    state.defaultGroup = rv.default_group || state.groupsForRules[0];
    const keys = new Set(state.rules.map(ruleKey));
    [...state.selected].forEach((k) => keys.has(k) || state.selected.delete(k));
    $("#navRuleCount").textContent = state.rules.length || "";
    renderRules();
    if (state.page === "sites") renderHosts();
  } catch (e) { if (!silent) toast(String(e), "error"); }
}

function filteredRules() {
  const f = state.ruleFilter;
  return state.rules.filter((r) =>
    (f.type === "all" || ruleCat(r) === f.type) &&
    (f.act === "all" || actionKind(r.action) === f.act) &&
    (!f.q || r.value.toLowerCase().includes(f.q) || r.action.toLowerCase().includes(f.q)));
}

function actionOptions(select, current) {
  select.innerHTML = "";
  const groups = [...state.groupsForRules];
  if (current && !["DIRECT", "REJECT"].includes(current) && !groups.includes(current)) groups.unshift(current);
  const og = h("optgroup", { label: "Через VPN" });
  groups.forEach((g) => og.append(h("option", { value: g }, "VPN · " + g)));
  select.append(og, h("option", { value: "DIRECT" }, "Напрямую"), h("option", { value: "REJECT" }, "Блокировать"));
  if (current) select.value = current;
}

function renderRules() {
  const body = $("#rulesBody");
  const list = filteredRules();
  $("#ruleCount").textContent = state.rules.length ? `· ${state.rules.length}` : "";
  body.innerHTML = "";
  if (!state.rules.length) {
    body.append(emptyState("list", "Правил пока нет", "Нажмите «Добавить», чтобы отправить сайт или программу через VPN"));
  } else if (!list.length) {
    body.append(emptyState("search", "Ничего не найдено", "Измените поиск или фильтры"));
  }
  for (const r of list) {
    const key = ruleKey(r);
    const cat = ruleCat(r);
    const cb = h("input", { type: "checkbox" });
    cb.checked = state.selected.has(key);
    cb.addEventListener("change", () => { cb.checked ? state.selected.add(key) : state.selected.delete(key); renderRules(); });
    const sel = h("select", { class: "input" });
    actionOptions(sel, r.action);
    sel.addEventListener("change", async () => {
      const res = await call("update_rule", { key, item: { kind: r.kind, value: r.value, action: sel.value } });
      if (showApply(res, "Действие изменено")) loadRules(); else sel.value = r.action;
    });
    const chip = h("span", { class: "chip " + (cat === "process" ? "violet" : cat === "ip" ? "" : "blue"), title: KIND_HINT[r.kind] }, KIND_LABEL[r.kind] || r.kind);
    body.append(h("div", { class: "trow" + (state.selected.has(key) ? " sel" : ""), "data-key": key },
      h("label", { class: "cb" }, cb, h("span")),
      h("div", {}, chip),
      h("div", { class: "val ellipsis", title: KIND_HINT[r.kind] }, r.value),
      h("div", {}, sel),
      h("div", { class: "ops" },
        opBtn("up", "Выше", () => moveRule(key, -1)),
        opBtn("down", "Ниже", () => moveRule(key, 1)),
        opBtn("edit", "Изменить", () => openRuleModal({ edit: r })),
        opBtn("trash", "Удалить", async () => {
          if (!(await confirmModal("Удалить правило?", `«${r.value}» больше не будет обрабатываться этим правилом.`, "Удалить", true))) return;
          const res = await call("delete_rules", { keys: [key] });
          if (showApply(res, "Правило удалено")) loadRules();
        }))));
  }
  const n = state.selected.size;
  $("#bulkBar").classList.toggle("hidden", !n);
  $("#bulkCount").textContent = `Выбрано: ${n}`;
  if (n) actionOptions($("#bulkAction"), $("#bulkAction").value || state.defaultGroup);
  const all = $("#ruleAll");
  const visSel = list.filter((r) => state.selected.has(ruleKey(r))).length;
  all.checked = list.length > 0 && visSel === list.length;
  all.indeterminate = visSel > 0 && visSel < list.length;
}

function opBtn(ic, title, fn) {
  return h("button", { class: "btn icon ghost", title, html: icon(ic), onclick: fn });
}

async function moveRule(key, dir) {
  const res = await call("move_rule", { key, dir });
  if (res.ok) {
    await loadRules();
    const row = $(`#rulesBody .trow[data-key="${CSS.escape(key)}"]`);
    if (row) row.classList.add("flash");
  } else toast(res.message, "error");
}

// ----------------------------------------------------------------- модальные окна
let modalEl = null;
function openModal(content) {
  closeModal();
  modalEl = h("div", { class: "modal-back" }, content);
  modalEl.addEventListener("mousedown", (e) => { if (e.target === modalEl) closeModal(); });
  document.body.append(modalEl);
  fillIcons(modalEl);
  const f = modalEl.querySelector("[autofocus]");
  if (f) setTimeout(() => f.focus(), 30);
  return modalEl;
}
function closeModal() {
  if (modalEl) { modalEl.remove(); modalEl = null; }
}

function confirmModal(title, text, okText = "OK", danger = false) {
  return new Promise((resolve) => {
    const done = (v) => { closeModal(); resolve(v); };
    openModal(h("div", { class: "modal", style: { width: "420px" } },
      h("h2", {}, title), h("div", { class: "sub" }, text),
      h("div", { class: "foot" },
        h("button", { class: "btn ghost", onclick: () => done(false) }, "Отмена"),
        h("button", { class: "btn " + (danger ? "danger" : "primary"), autofocus: true, onclick: () => done(true) }, okText))));
  });
}

/** Выбор действия: три карточки + выбор группы для VPN. Возвращает { el, get() }. */
function actionPicker(initial) {
  let kind = actionKind(initial || state.defaultGroup);
  const groupSel = h("select", { class: "input" });
  state.groupsForRules.forEach((g) => groupSel.append(h("option", { value: g }, g)));
  groupSel.value = kind === "vpn" && initial ? initial : state.defaultGroup;
  if (kind === "vpn" && initial && !state.groupsForRules.includes(initial)) {
    groupSel.prepend(h("option", { value: initial }, initial));
    groupSel.value = initial;
  }
  const mk = (k, ic, title, text) => h("button", { type: "button", class: "act " + k, "data-k": k, onclick: () => set(k) },
    h("b", { html: icon(ic) }, title), h("span", {}, text));
  const cards = h("div", { class: "actions3" },
    mk("vpn", "shieldok", "Через VPN", "Открывать через VPN-сервер"),
    mk("direct", "arrowright", "Напрямую", "Всегда без VPN, например банки"),
    mk("block", "ban", "Блокировать", "Не пускать: реклама, трекеры"));
  const pick = h("div", { class: "group-pick" }, "Группа:", groupSel);
  function set(k) {
    kind = k;
    $$(".act", cards).forEach((c) => c.classList.toggle("on", c.dataset.k === k));
    pick.classList.toggle("hidden", k !== "vpn");
  }
  set(kind);
  return { el: h("div", {}, cards, pick), get: () => (kind === "vpn" ? groupSel.value : kind === "direct" ? "DIRECT" : "REJECT") };
}

/** Добавление (tab) или изменение (edit) правила. */
async function openRuleModal({ tab = "domain", edit = null, value = "" } = {}) {
  if (!state.groupsForRules.length) await loadRules(true);
  const isEdit = !!edit;
  if (isEdit) tab = ruleCat(edit) === "process" ? "process" : ruleCat(edit) === "ip" ? "ip" : edit.kind === "DOMAIN-KEYWORD" ? "keyword" : "domain";
  const picker = actionPicker(isEdit ? edit.action : null);
  const pane = h("div");
  const counter = h("span", { class: "hint" });
  let getItems = () => [];

  const tabs = h("div", { class: "tabs" });
  const TABS = [["domain", "Сайт"], ["process", "Программа"], ["ip", "IP-адрес"], ["keyword", "Ключевое слово"]];
  for (const [id, label] of TABS) {
    if (isEdit && id !== tab) continue;
    tabs.append(h("button", { type: "button", "data-t": id, class: id === tab ? "on" : "", onclick: () => showTab(id) }, label));
  }

  function showTab(id) {
    tab = id;
    $$("button", tabs).forEach((b) => b.classList.toggle("on", b.dataset.t === id));
    pane.innerHTML = "";
    counter.textContent = "";
    if (id === "domain") {
      const exact = h("input", { type: "checkbox" });
      if (isEdit && edit.kind === "DOMAIN") exact.checked = true;
      const field = isEdit
        ? h("input", { class: "input", autofocus: true, value: edit.value, placeholder: "example.com" })
        : h("textarea", { class: "input", autofocus: true, placeholder: "tiktok.com\nhttps://www.deepseek.com/chat\n*.example.org", rows: 5 }, value);
      const upd = () => { const n = lines(field.value).length; counter.textContent = !isEdit && n > 1 ? `Будет добавлено: ${n}` : ""; };
      field.addEventListener("input", upd);
      upd();
      pane.append(
        h("div", { class: "field" }, h("label", {}, isEdit ? "Адрес сайта" : "Адреса сайтов — по одному в строке"), field,
          h("span", { class: "hint" }, "Можно вставлять ссылки целиком: лишнее (https://, путь, порт) уберётся само.")),
        h("label", { class: "row", style: { fontSize: "13px", cursor: "pointer", marginBottom: "14px" } },
          h("span", { class: "cb" }, exact, h("span")), "Только точный адрес, без поддоменов"));
      getItems = () => lines(field.value).map((v) => ({ kind: exact.checked ? "DOMAIN" : "DOMAIN-SUFFIX", value: v }));
    } else if (id === "keyword") {
      const field = h("input", { class: "input", autofocus: true, value: isEdit ? edit.value : "", placeholder: "например: googlevideo" });
      pane.append(h("div", { class: "field" }, h("label", {}, "Слово в адресе"), field,
        h("span", { class: "hint" }, "Сработает для любого адреса, где встречается это слово. Используйте осторожно.")));
      getItems = () => (field.value.trim() ? [{ kind: "DOMAIN-KEYWORD", value: field.value.trim() }] : []);
    } else if (id === "ip") {
      const field = isEdit
        ? h("input", { class: "input", autofocus: true, value: edit.value })
        : h("textarea", { class: "input", autofocus: true, rows: 4, placeholder: "149.154.160.0/20\n91.108.4.0/22\n1.2.3.4" });
      pane.append(h("div", { class: "field" }, h("label", {}, "IP или подсеть — по одному в строке"), field,
        h("span", { class: "hint" }, "Нужно редко: для сервисов, которые подключаются по IP без домена (игры, звонки).")));
      getItems = () => lines(field.value).map((v) => ({ kind: "IP-CIDR", value: v }));
    } else if (id === "process") {
      const chosen = new Set(isEdit ? [edit.value] : []);
      const manual = h("input", { class: "input", placeholder: "или введите имя вручную: Telegram.exe", value: isEdit ? edit.value : "" });
      const search = h("input", { class: "input", placeholder: "Поиск среди запущенных программ", autofocus: !isEdit });
      const list = h("div", { class: "proc-list" }, h("div", { class: "empty", style: { padding: "20px" } }, "Загрузка…"));
      let procs = [];
      const render = () => {
        list.innerHTML = "";
        const q = search.value.trim().toLowerCase();
        const shown = procs.filter((p) => !q || p.toLowerCase().includes(q));
        if (!shown.length) list.append(h("div", { class: "empty", style: { padding: "20px" } }, "Нет совпадений"));
        for (const p of shown.slice(0, 300)) {
          const on = chosen.has(p);
          const cb = h("input", { type: "checkbox" });
          cb.checked = on;
          list.append(h("div", { class: "proc" + (on ? " on" : ""), onclick: () => {
            if (isEdit) { chosen.clear(); manual.value = p; }
            chosen.has(p) ? chosen.delete(p) : chosen.add(p);
            render(); upd();
          } }, h("span", { class: "cb" }, cb, h("span")), p));
        }
      };
      const upd = () => {
        const n = getItems().length;
        counter.textContent = !isEdit && n ? `Выбрано: ${n}` : "";
      };
      search.addEventListener("input", render);
      manual.addEventListener("input", upd);
      if (!isEdit) {
        pane.append(h("div", { class: "field" }, h("label", {}, "Запущенные программы"), search, list));
      }
      pane.append(h("div", { class: "field" }, isEdit ? h("label", {}, "Имя программы") : null, manual,
        h("span", { class: "hint" }, "Если программа не запущена — запустите её и откройте это окно снова, или впишите имя .exe.")));
      getItems = () => {
        const set = new Set(chosen);
        const m = manual.value.trim();
        if (isEdit) return m ? [{ kind: "PROCESS-NAME", value: m }] : [];
        if (m) set.add(m);
        return [...set].map((v) => ({ kind: "PROCESS-NAME", value: v }));
      };
      if (!isEdit) invoke("list_processes").then((p) => { procs = p; render(); }).catch(() => { procs = []; render(); });
    }
    fillIcons(pane);
    const f = pane.querySelector("[autofocus]");
    if (f) setTimeout(() => f.focus(), 30);
  }

  const submit = h("button", { class: "btn primary", onclick: async () => {
    const action = picker.get();
    const items = getItems().map((i) => ({ ...i, action }));
    if (!items.length) { toast("Введите значение", "warn"); return; }
    submit.disabled = true;
    try {
      if (isEdit) {
        const res = await invoke("update_rule", { key: ruleKey(edit), item: items[0] });
        if (showApply(res, "Правило изменено")) { closeModal(); loadRules(); }
      } else {
        const res = await invoke("add_rules", { items });
        reportAdd(res);
        if (res.apply.ok && (res.added || res.duplicates)) { closeModal(); loadRules(); }
      }
    } catch (e) { toast(String(e), "error"); }
    finally { submit.disabled = false; }
  } }, isEdit ? "Сохранить" : "Добавить");

  openModal(h("div", { class: "modal" },
    h("h2", {}, isEdit ? "Изменить правило" : "Новое правило"),
    h("div", { class: "sub" }, isEdit ? "Изменения сразу применяются без перезапуска VPN" : "Правило начнёт работать сразу, перезапускать VPN не нужно"),
    tabs, pane,
    h("div", { class: "field" }, h("label", {}, "Что делать"), picker.el),
    h("div", { class: "foot" }, counter, h("span", { class: "grow" }),
      h("button", { class: "btn ghost", onclick: closeModal }, "Отмена"), submit)));
  showTab(tab);
  modalEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.ctrlKey || e.target.tagName === "INPUT")) { e.preventDefault(); submit.click(); }
  });
}

function lines(text) {
  return text.split(/[\r\n]+/).map((s) => s.trim()).filter(Boolean);
}

function reportAdd(res) {
  if (res.errors && res.errors.length) toast("Не добавлено: " + res.errors.join("; "), "error");
  if (!res.apply.ok) { toast(res.apply.message, "error"); return; }
  const parts = [];
  if (res.added) parts.push(`Добавлено: ${res.added}`);
  if (res.duplicates) parts.push(`уже были: ${res.duplicates}`);
  if (!parts.length) return;
  if (res.apply.live) toast(parts.join(", ") + " — уже работает", "ok");
  else toast(parts.join(", ") + ". " + res.apply.message, "info", 4500);
}

// ----------------------------------------------------------------- импорт
function openImportModal() {
  const text = h("textarea", { class: "input", rows: 7, autofocus: true,
    placeholder: "Вставьте список или выберите файл.\n\nПонимаются строки вида:\nDOMAIN-SUFFIX,example.com,Proxy\nexample.com\nTelegram.exe\n149.154.160.0/20" });
  const picker = actionPicker(null);
  const preview = h("div", { class: "preview hidden" });
  const summary = h("span", { class: "hint" });
  let parsed = [];
  let timer = 0;

  async function refresh() {
    if (!text.value.trim()) { parsed = []; preview.classList.add("hidden"); summary.textContent = ""; go.disabled = true; return; }
    parsed = await invoke("preview_import", { text: text.value, action: picker.get() });
    preview.innerHTML = "";
    preview.classList.toggle("hidden", !parsed.length);
    const ok = parsed.filter((p) => p.rule);
    const bad = parsed.length - ok.length;
    for (const p of parsed.slice(0, 300)) {
      preview.append(p.rule
        ? h("div", {}, h("span", { class: "ln" }, p.line), h("span", { class: "chip" }, KIND_LABEL[p.rule.kind] || p.rule.kind), h("span", { class: "grow ellipsis" }, p.rule.value), actionChip(p.rule.action))
        : h("div", { class: "err" }, h("span", { class: "ln" }, p.line), h("span", { class: "grow ellipsis" }, p.source.trim()), h("span", {}, p.error)));
    }
    summary.textContent = `Готово к импорту: ${ok.length}` + (bad ? `, с ошибками: ${bad}` : "");
    go.disabled = !ok.length;
  }
  const schedule = () => { clearTimeout(timer); timer = setTimeout(refresh, 250); };
  text.addEventListener("input", schedule);
  picker.el.addEventListener("click", schedule);
  picker.el.addEventListener("change", schedule);

  const fileBtn = h("button", { class: "btn sm", onclick: async () => {
    try {
      const path = await pickOpenFile();
      if (!path) return;
      text.value = await invoke("read_text_file", { path: typeof path === "string" ? path : path.path });
      refresh();
    } catch (e) { toast(String(e), "error"); }
  } }, iconEl("folder"), "Выбрать файл…");

  const go = h("button", { class: "btn primary", disabled: true, onclick: async () => {
    const items = parsed.filter((p) => p.rule).map((p) => ({ kind: p.rule.kind, value: p.rule.value, action: p.rule.action }));
    go.disabled = true;
    try {
      const res = await invoke("add_rules", { items });
      reportAdd(res);
      if (res.apply.ok) { closeModal(); loadRules(); }
    } catch (e) { toast(String(e), "error"); }
    finally { go.disabled = false; }
  } }, "Импортировать");

  openModal(h("div", { class: "modal", style: { width: "640px" } },
    h("h2", {}, "Импорт правил"),
    h("div", { class: "sub" }, "Из файла экспорта, из списка доменов или прямо из строк config.yaml. Дубликаты пропускаются."),
    h("div", { class: "field" }, h("div", { class: "row" }, h("label", { class: "grow", style: { fontSize: "12.5px", fontWeight: 600, color: "var(--muted)" } }, "Список"), fileBtn), text),
    h("div", { class: "field" }, h("label", {}, "Действие для строк без указанного действия"), picker.el),
    preview,
    h("div", { class: "foot" }, summary, h("span", { class: "grow" }), h("button", { class: "btn ghost", onclick: closeModal }, "Отмена"), go)));
}

// ================================================================= сайты
function bindSites() {
  segBind("#sitesSeg", "s", (v) => { state.sitesFilter.seg = v; renderHosts(); });
  segBind("#scopeSeg", "scope", (v) => { state.sitesFilter.scope = v; renderHosts(); });
  $("#sitesSearch").addEventListener("input", (e) => { state.sitesFilter.q = e.target.value.trim().toLowerCase(); renderHosts(); });
  $("#sitesClear").addEventListener("click", async () => { await invoke("clear_hosts"); state.hosts = []; renderHosts(); });
}

function setSitesSeg(v) {
  state.sitesFilter.seg = v;
  $$("#sitesSeg button").forEach((b) => b.classList.toggle("on", b.dataset.s === v));
  renderHosts();
}

async function loadHosts(countOnly) {
  try {
    state.hosts = await invoke("get_hosts");
  } catch { return; }
  const broken = state.hosts.filter((x) => x.suspicious && !matchRule(x.host)).length;
  $("#navSuspicious").textContent = broken || "";
  $("#brokenCount").textContent = broken || "";
  if (!countOnly) renderHosts();
}

const isIp = (s) => /^[\d.]+$/.test(s) || s.includes(":");

/** Пользовательское правило, которое уже покрывает этот адрес. */
function matchRule(host) {
  const hst = host.toLowerCase();
  return state.rules.find((r) => {
    const v = r.value.toLowerCase();
    if (r.kind === "DOMAIN-SUFFIX") return hst === v || hst.endsWith("." + v);
    if (r.kind === "DOMAIN") return hst === v;
    if (r.kind === "DOMAIN-KEYWORD") return hst.includes(v);
    if (r.kind.startsWith("IP-CIDR")) return v.split("/")[0] === hst;
    return false;
  });
}

function routeChip(x) {
  if (x.route === "DIRECT") return h("span", { class: "chip blue", title: "Правило: " + x.rule }, h("span", { class: "dot" }), "Напрямую");
  if (x.route === "REJECT" || x.route === "REJECT-DROP") return h("span", { class: "chip bad", title: "Правило: " + x.rule }, h("span", { class: "dot" }), "Блок");
  return h("span", { class: "chip ok ellipsis", title: `${x.group} → ${x.route}\nПравило: ${x.rule}`, style: { maxWidth: "165px" } }, h("span", { class: "dot" }), x.group && x.group !== x.route ? x.group : "VPN");
}

function renderHosts() {
  const body = $("#sitesBody");
  if (!body) return;
  const f = state.sitesFilter;
  $("#brokenBanner").classList.toggle("hidden", f.seg !== "broken");
  let list = state.hosts.filter((x) => {
    if (f.seg === "broken" && !(x.suspicious && !matchRule(x.host))) return false;
    if (f.seg === "direct" && x.route !== "DIRECT") return false;
    if (f.seg === "vpn" && (x.route === "DIRECT" || x.route.startsWith("REJECT"))) return false;
    if (f.q && !x.host.toLowerCase().includes(f.q) && !x.process.toLowerCase().includes(f.q)) return false;
    return true;
  });
  body.innerHTML = "";
  if (!list.length) {
    const st = state.status;
    if (st && !st.api_ok) body.append(emptyState("wifi", "Mihomo не отвечает", "Включите VPN, чтобы видеть соединения"));
    else if (f.seg === "broken") body.append(emptyState("check", "Проблемных адресов не видно", "Откройте сайт, который не грузится, и загляните сюда снова через пару секунд"));
    else body.append(emptyState("activity", "Пока пусто", "Адреса появятся, как только программы начнут подключаться"));
    return;
  }
  for (const x of list.slice(0, 250)) {
    const target = f.scope === "suffix" ? x.suffix : x.host;
    const existing = matchRule(x.host);
    const add = (action) => async () => {
      const kind = isIp(x.host) ? "IP-CIDR" : f.scope === "suffix" ? "DOMAIN-SUFFIX" : "DOMAIN";
      const res = await call("add_rules", { items: [{ kind, value: isIp(x.host) ? x.host : target, action }] });
      reportAdd(res);
      loadRules(true);
    };
    const addCell = existing
      ? h("div", { class: "has-rule" }, "Правило: ", actionChip(existing.action),
          h("button", { class: "btn icon ghost", title: "Изменить правило", html: icon("edit"), onclick: () => openRuleModal({ edit: existing }) }))
      : h("div", { class: "add" },
          h("button", { class: "btn vpn", title: `Добавить ${target} в VPN (${state.defaultGroup})`, onclick: add(state.defaultGroup) }, "В VPN"),
          h("button", { class: "btn direct", title: `${target}: всегда напрямую`, onclick: add("DIRECT") }, "Напрямую"),
          h("button", { class: "btn block icon", title: `Блокировать ${target}`, html: icon("ban"), onclick: add("REJECT") }));
    body.append(h("div", { class: "trow" + (x.suspicious && !existing ? " susp" : "") },
      h("div", { class: "host" },
        h("span", { class: "val ellipsis", title: x.host }, x.host),
        h("small", { class: "ellipsis" }, (x.active ? `${x.active} акт. · ` : "") + `${x.count} ${plural(x.count, "подключение", "подключения", "подключений")} · ${fmtAgo(x.last_seen)}`)),
      h("div", { class: "ellipsis muted", title: x.process }, x.process || "—"),
      h("div", {}, routeChip(x)),
      h("div", { class: "traffic num" }, "↓ " + fmtBytes(x.down)),
      addCell));
  }
}

// ================================================================= настройки
function bindSettings() {
  $$("[data-open]").forEach((b) => b.addEventListener("click", () => invoke("open_target", { what: b.dataset.open })));
  $$("[data-set]").forEach((el) =>
    el.addEventListener("change", async () => {
      if (!state.settings) return;
      const s = { ...state.settings };
      s[el.dataset.set] = el.type === "checkbox" ? el.checked : el.value;
      try {
        await invoke("save_settings", { settings: s });
        state.settings = s;
        toast("Сохранено", "ok", 1400);
        if (el.dataset.set === "default_proxy_group" || el.dataset.set === "main_group") loadRules(true);
        if (el.dataset.set.startsWith("smart")) loadSmart();
        if (state.page === "settings") loadExtras();
      } catch (e) {
        toast(String(e), "error");
        fillSettings();
      }
    })
  );
}

// ----------------------------------------------------------------- Discord, обновления, игры
function bindExtras() {
  $$("[data-discord]").forEach((b) => b.addEventListener("click", async () => {
    b.disabled = true;
    try {
      const msg = await invoke("discord_action", { action: b.dataset.discord });
      if (msg) toast(msg, "ok", 3500);
    } catch (e) { toast(String(e), "error"); }
    finally { b.disabled = false; loadExtras(); }
  }));
  $$("[data-upd]").forEach((b) => b.addEventListener("click", async () => {
    const a = b.dataset.upd;
    if (a === "tray_update" && !(await confirmModal("Обновить трей?", "Новая версия скачается с GitHub, трей закроется и сразу запустится уже обновлённым. VPN при этом не выключается. Прошлая версия сохранится как mihomo-tray.prev.exe.", "Обновить"))) return;
    if (a === "core" && !(await confirmModal("Обновить Mihomo?", "VPN выключится на несколько секунд: служба остановится, файл заменится, служба запустится. Если новая версия не поднимется — вернётся старая.", "Обновить"))) return;
    b.disabled = true;
    if (a !== "tray") toast(a === "check" ? "Проверяю…" : a === "tray_update" ? "Скачиваю новую версию трея…" : "Обновляю, это может занять минуту…", "info", 2500);
    setTimeout(loadExtras, 500);
    try {
      const msg = await invoke("updates_action", { action: a });
      if (msg) toast(msg, "ok", 5000);
    } catch (e) { toast(String(e), "error"); }
    finally { b.disabled = false; loadExtras(); }
  }));
  $("#gameExtra").addEventListener("change", async (e) => {
    const list = e.target.value.split(/[,;\n]/).map((x) => x.trim()).filter(Boolean);
    await saveSettingsPatch({ game_processes: list });
  });
}

async function saveSettingsPatch(patch) {
  const s = { ...state.settings, ...patch };
  try { await invoke("save_settings", { settings: s }); state.settings = s; toast("Сохранено", "ok", 1400); }
  catch (e) { toast(String(e), "error"); }
}

const line = (ok, text, sub) => h("div", { class: "info-line" }, h("span", { class: "dot " + (ok === true ? "ok" : ok === false ? "bad" : "") }), h("span", {}, text), sub ? h("span", { class: "dim" }, sub) : null);

async function loadExtras() {
  try {
    const [d, u] = await Promise.all([invoke("discord_status"), invoke("updates_status")]);
    const di = $("#discordInfo");
    di.innerHTML = "";
    // при TUN весь трафик Discord (и голос) уже идёт через Mihomo — файлы прокси не нужны
    $("#discordBtns").classList.toggle("hidden", !!d.tun);
    $$("[data-discord-opt]").forEach((el) => el.classList.toggle("hidden", !!d.tun));
    if (d.tun) {
      di.append(line(true, "Режим TUN включён", "· файлы для Discord не нужны"));
      di.append(h("div", { class: "hint" }, "Весь трафик Discord, включая голос, уже идёт через VPN. Трей ничего не копирует в папку Discord."));
      const left = d.flavors.filter((f) => f.installed).map((f) => f.name);
      if (left.length) di.append(h("div", { class: "hint" }, `В папке ${left.join(", ")} остались старые файлы прокси — они не мешают.`));
    } else {
      if (!d.found) di.append(line(null, "Discord не найден", "в %LocalAppData%"));
      for (const f of d.flavors) {
        di.append(line(f.installed, `${f.name} ${f.version}: ${f.installed ? "прокси установлен" : "прокси НЕ установлен"}`, f.running ? "· запущен" : ""));
      }
      di.append(line(d.master, d.master ? "Копия файлов прокси сохранена у трея" : "Копии файлов прокси нет — нажмите «Скачать свежие файлы»"));
      if (d.last) di.append(h("div", { class: "hint" }, "Последнее: " + d.last));
    }

    const ui = $("#updInfo");
    ui.innerHTML = "";
    const hasNew = u.core_latest && u.core_current && cmpVer(u.core_latest, u.core_current) > 0;
    ui.append(line(u.core_latest ? !hasNew : null, `Mihomo ${u.core_current || "?"}`,
      u.core_latest ? (hasNew ? `· доступна ${u.core_latest}` : "· последняя версия") : "· ещё не проверяли"));
    const trayNew = u.tray_latest && u.tray_asset && cmpVer(u.tray_latest, u.tray_current) > 0;
    ui.append(line(u.tray_error ? null : !trayNew, `Трей ${u.tray_current}`,
      u.tray_pending ? "· обновление скачано" : trayNew ? `· доступна ${u.tray_latest} на GitHub` : u.tray_latest ? "· последняя версия" : u.tray_error ? "· " + u.tray_error : "· ещё не проверяли"));
    if (u.busy) ui.append(h("div", { class: "hint", style: { color: "var(--accent-2)" } }, "Идёт: " + u.busy + "…"));
    if (u.last) ui.append(h("div", { class: "hint" }, u.last));
    if (u.error) ui.append(h("div", { class: "hint", style: { color: "var(--bad)" } }, u.error));
    $("#updCoreBtn").disabled = !hasNew || !!u.busy;
    $("#updTrayBtn").classList.toggle("hidden", !u.tray_pending);
    const tgb = $("#updTrayGhBtn");
    tgb.classList.toggle("hidden", !trayNew || u.tray_pending);
    tgb.disabled = !!u.busy;
    tgb.lastChild.textContent = trayNew ? `Обновить трей до ${u.tray_latest}` : "Обновить трей";
    const notes = $("#updNotes");
    notes.classList.toggle("hidden", !(trayNew && u.tray_notes));
    notes.textContent = trayNew && u.tray_notes ? `Что нового в ${u.tray_latest}:\n${u.tray_notes}` : "";

    const g = state.status?.game;
    $("#gameInfo").innerHTML = "";
    $("#gameInfo").append(line(g ? true : null, g ? `Сейчас включён: ${g}` : "Игр не запущено"));
    const ge = $("#gameExtra");
    if (document.activeElement !== ge) ge.value = (state.settings?.game_processes || []).join(", ");

    const il = $("#ignoredList");
    il.innerHTML = "";
    const ign = state.settings?.ignored_hosts || [];
    if (!ign.length) il.append(h("span", { class: "hint" }, "пусто"));
    ign.forEach((d) => il.append(h("span", { class: "chip" }, d, h("button", { class: "chip-x", title: "Снова спрашивать", onclick: () => saveSettingsPatch({ ignored_hosts: ign.filter((x) => x !== d) }).then(loadExtras) }, "×"))));
    fillIcons($("#page-settings"));
  } catch (e) { console.error(e); }
}

function cmpVer(a, b) {
  const p = (v) => (v || "").replace(/^v/i, "").split(/[.-]/).map((x) => parseInt(x, 10) || 0);
  const x = p(a), y = p(b);
  for (let i = 0; i < 3; i++) if ((x[i] || 0) !== (y[i] || 0)) return (x[i] || 0) - (y[i] || 0);
  return 0;
}

async function loadSettings() {
  try {
    const [s, p, b] = await Promise.all([invoke("get_settings"), invoke("get_paths"), invoke("list_backups")]);
    state.settings = s;
    state.paths = p;
    if (!state.groupsForRules.length) await loadRules(true);
    fillSettings();
    renderBackups(b);
    loadExtras();
  } catch (e) { toast(String(e), "error"); }
}

function fillSettings() {
  const s = state.settings;
  for (const el of $$("[data-set]")) {
    const k = el.dataset.set;
    if (el.type === "checkbox") el.checked = !!s[k];
    else {
      el.innerHTML = "";
      el.append(h("option", { value: "" }, "Первая из конфига"));
      const opts = [...state.groupsForRules];
      if (s[k] && !opts.includes(s[k])) opts.unshift(s[k]);
      opts.forEach((g) => el.append(h("option", { value: g }, g)));
      el.value = s[k] || "";
    }
  }
  const p = state.paths;
  $("#paths").innerHTML = "";
  $("#paths").append(
    h("span", {}, "Конфиг"), h("span", { title: p.config }, p.config),
    h("span", {}, "Данные трея"), h("span", { title: p.data_dir }, p.data_dir),
    h("span", {}, "Права"), h("span", {}, p.elevated ? "администратор" : "обычные (управление службой через запрос UAC)"),
    h("span", {}, "Версия"), h("span", {}, "Mihomo Tray " + p.version));
  $("#autoRestartHint").textContent = p.elevated
    ? "Поднимать службу, если она упала сама (не больше 3 раз за 10 минут)"
    : "Работает только при запуске трея с правами администратора (через задачу автозапуска)";
}

function backupDate(name) {
  const m = name.match(/config-(\d{4})(\d{2})(\d{2})-(\d{2})(\d{2})(\d{2})/);
  if (!m) return name;
  const d = new Date(Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +m[6]));
  return d.toLocaleString("ru-RU", { day: "2-digit", month: "2-digit", year: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

function renderBackups(list) {
  const box = $("#backups");
  box.innerHTML = "";
  if (!list.length) { box.append(h("div", { class: "hint" }, "Пока нет — появятся после первого изменения правил.")); return; }
  for (const b of list) {
    box.append(h("div", { class: "backup" },
      iconEl("history"),
      h("span", { class: "grow" }, backupDate(b.name)),
      h("span", { class: "dim" }, fmtBytes(b.size)),
      h("button", { class: "btn sm", onclick: async () => {
        if (!(await confirmModal("Восстановить конфиг?", `config.yaml будет заменён копией от ${backupDate(b.name)}. Текущая версия тоже сохранится в резервных копиях.`, "Восстановить"))) return;
        const res = await call("restore_backup", { name: b.name });
        if (showApply(res, "Конфиг восстановлен")) { loadRules(); loadSettings(); }
      } }, "Восстановить")));
  }
  fillIcons(box);
}

init();
