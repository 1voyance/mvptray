// Всплывающее окно у иконки в трее.
"use strict";

let st = null;

$$("[data-icon]").forEach((el) => (el.innerHTML = icon(el.dataset.icon)));

function render(s) {
  st = s;
  const busy = s.busy;
  $("#power").className = "power " + s.health + (busy ? " busy" : "");
  const pill = $("#pill");
  pill.className = "chip " + (s.health === "ok" ? "ok" : s.health === "warn" ? "warn" : "off");
  pill.lastChild.textContent = busy ? "…" : s.health === "ok" ? "Подключено" : s.health === "warn" ? "Проблема" : "Выключено";

  let t;
  if (busy === "start") t = "Включение…";
  else if (busy === "stop") t = "Выключение…";
  else if (busy === "restart") t = "Перезапуск…";
  else if (s.health === "ok") t = s.mode === "direct" ? "Напрямую" : "Защищено";
  else if (s.health === "warn") t = "Есть проблема";
  else t = "VPN выключен";
  const el = $("#st");
  el.textContent = t;
  el.className = "st " + (busy ? "" : s.health);

  const node = $("#node");
  node.innerHTML = "";
  if (s.api_ok && s.node && s.health !== "warn") {
    node.append(h("b", {}, s.node), " · ", h("span", { class: "delay " + delayClass(s.delay) }, delayText(s.delay)));
    node.title = (s.chain || []).join(" → ");
  } else {
    node.textContent = s.health === "off" ? "Нажмите, чтобы включить" : s.reason;
  }

  const gm = $("#game");
  gm.classList.toggle("hidden", !s.game);
  if (s.game) gm.textContent = "🎮 Игровой режим: " + s.game;
  $("#down").textContent = fmtSpeed(s.down_speed);
  $("#up").textContent = fmtSpeed(s.up_speed);
  if (s.history_down && s.history_down.length > 1) sparkline($("#spark"), s.history_down.slice(-30), s.history_up.slice(-30));
  $$("#mode button").forEach((b) => {
    b.classList.toggle("on", b.dataset.mode === s.mode);
    b.disabled = !s.api_ok;
  });
}

$("#power").addEventListener("click", () => {
  if (!st || st.busy || st.service === "pending") return;
  const running = st.api_ok || st.service === "running";
  call("service_action", { action: running ? "stop" : "start" });
});

$$("#mode button").forEach((b) => b.addEventListener("click", () => call("set_mode", { mode: b.dataset.mode })));

$$("[data-nav]").forEach((b) =>
  b.addEventListener("click", async () => {
    await invoke("hide_flyout");
    await invoke("show_main", { page: b.dataset.nav });
  })
);

$("#quit").addEventListener("click", () => invoke("quit"));

async function quickAdd() {
  const input = $("#quickDomain");
  const v = input.value.trim();
  if (!v) return;
  try {
    const rv = await invoke("get_rules");
    const res = await invoke("add_rules", { items: [{ kind: "DOMAIN-SUFFIX", value: v, action: rv.default_group || "Proxy" }] });
    if (res.errors.length) { toast(res.errors[0], "error"); return; }
    if (!res.apply.ok) { toast(res.apply.message, "error"); return; }
    toast(res.added ? (res.apply.live ? "Добавлено в VPN — уже работает" : res.apply.message) : "Такое правило уже есть", res.added ? "ok" : "info", 2200);
    input.value = "";
  } catch (e) { toast(String(e), "error"); }
}
$("#quickAdd").addEventListener("click", quickAdd);
$("#quickDomain").addEventListener("keydown", (e) => { if (e.key === "Enter") quickAdd(); });

document.addEventListener("keydown", (e) => { if (e.key === "Escape") invoke("hide_flyout"); });

listen("status", render);
listen("toast", (t) => toast(t.text, t.kind));
listen("flyout-shown", () => invoke("get_status").then(render).catch(() => {}));
invoke("get_status").then(render).catch(() => {});
