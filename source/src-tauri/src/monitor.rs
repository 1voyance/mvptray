//! Фоновый опрос Mihomo: статус, скорость, активная нода, недавние адреса, уведомления.

use crate::api::{self, Api};
use crate::rules;
use crate::service::{self, SvcState};
use crate::util::{self, Settings};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const HISTORY: usize = 60;
const MAX_HOSTS: usize = 400;

#[derive(Serialize, Clone, Default, Debug)]
pub struct Status {
    pub service: String,
    pub api_ok: bool,
    /// ok | warn | off
    pub health: String,
    /// Короткое пояснение к health: «Нода не отвечает», «API недоступен»…
    pub reason: String,
    pub mode: String,
    pub version: String,
    pub main_group: String,
    pub node: String,
    pub chain: Vec<String>,
    pub delay: Option<u32>,
    pub up_speed: u64,
    pub down_speed: u64,
    pub up_total: u64,
    pub down_total: u64,
    pub conns: usize,
    pub history_up: Vec<u64>,
    pub history_down: Vec<u64>,
    pub busy: Option<String>,
    pub elevated: bool,
    pub panel_url: String,
    /// Игровой режим: имя игры.
    pub game: Option<String>,
    /// Как запущено ядро, если не службой (пусто — всё штатно).
    pub core_note: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct HostEntry {
    pub host: String,
    pub suffix: String,
    /// Куда ушёл трафик: DIRECT / REJECT / имя ноды.
    pub route: String,
    /// Группа, выбранная правилом (первый элемент цепочки с конца).
    pub group: String,
    pub rule: String,
    pub process: String,
    pub count: u32,
    pub up: u64,
    pub down: u64,
    pub first_seen: u64,
    pub last_seen: u64,
    pub active: u32,
    /// Похоже на «сайт не открывается»: шло напрямую и почти ничего не скачалось.
    pub suspicious: bool,
}

#[derive(Default)]
struct HostTracker {
    hosts: HashMap<String, HostEntry>,
    /// id соединения → (host, up, down) на прошлом опросе
    seen: HashMap<String, (String, u64, u64)>,
}

impl HostTracker {
    fn update(&mut self, conns: &[Value]) {
        let now = util::now_secs();
        let mut alive: HashMap<String, (String, u64, u64)> = HashMap::new();
        for h in self.hosts.values_mut() {
            h.active = 0;
        }
        for c in conns {
            let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let md = c.get("metadata").cloned().unwrap_or(Value::Null);
            let s = |k: &str| md.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let mut host = s("host");
            if host.is_empty() {
                host = s("sniffHost");
            }
            if host.is_empty() {
                host = s("destinationIP");
            }
            if host.is_empty() || id.is_empty() {
                continue;
            }
            let up = c.get("upload").and_then(Value::as_u64).unwrap_or(0);
            let down = c.get("download").and_then(Value::as_u64).unwrap_or(0);
            let chains: Vec<String> = c
                .get("chains")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let process = {
                let p = s("process");
                if p.is_empty() {
                    s("processPath").rsplit(['\\', '/']).next().unwrap_or("").to_string()
                } else {
                    p
                }
            };
            let rule = {
                let r = c.get("rule").and_then(Value::as_str).unwrap_or("");
                let p = c.get("rulePayload").and_then(Value::as_str).unwrap_or("");
                if p.is_empty() { r.to_string() } else { format!("{r}({p})") }
            };

            let (prev_up, prev_down, is_new) = match self.seen.get(&id) {
                Some((_, u, d)) => (*u, *d, false),
                None => (0, 0, true),
            };
            alive.insert(id, (host.clone(), up, down));

            let e = self.hosts.entry(host.clone()).or_insert_with(|| HostEntry {
                suffix: rules::suggest_suffix(&host),
                host: host.clone(),
                route: String::new(),
                group: String::new(),
                rule: String::new(),
                process: String::new(),
                count: 0,
                up: 0,
                down: 0,
                first_seen: now,
                last_seen: now,
                active: 0,
                suspicious: false,
            });
            if is_new {
                e.count += 1;
            }
            e.up += up.saturating_sub(prev_up);
            e.down += down.saturating_sub(prev_down);
            e.last_seen = now;
            e.active += 1;
            e.route = chains.first().cloned().unwrap_or_default();
            e.group = chains.last().cloned().unwrap_or_default();
            e.rule = rule;
            if !process.is_empty() {
                e.process = process;
            }
        }
        self.seen = alive;
        for e in self.hosts.values_mut() {
            let direct = e.route == "DIRECT";
            let lived = e.last_seen.saturating_sub(e.first_seen) >= 4 || e.count >= 3;
            e.suspicious = direct && e.down < 2048 && lived;
        }
        if self.hosts.len() > MAX_HOSTS {
            let mut v: Vec<(String, u64)> = self.hosts.iter().map(|(k, e)| (k.clone(), e.last_seen)).collect();
            v.sort_by_key(|x| x.1);
            for (k, _) in v.into_iter().take(self.hosts.len() - MAX_HOSTS) {
                self.hosts.remove(&k);
            }
        }
    }
}

/// Общее состояние приложения.
pub struct Shared {
    pub status: Mutex<Status>,
    hosts: Mutex<HostTracker>,
    pub settings: Mutex<Settings>,
    pub api: Mutex<Api>,
    pub busy: Mutex<Option<String>>,
    /// Пользователь сам выключил VPN — не считать это падением.
    pub user_stopped: AtomicBool,
    /// Открыто какое-либо окно — опрашиваем чаще.
    pub ui_visible: AtomicBool,
    /// Время (сек) последней ручной смены ноды — чтобы не слать уведомление о своей же смене.
    pub manual_select_at: AtomicU64,
    /// Принудительный опрос вне очереди.
    pub poke: AtomicBool,
    /// Собственные замеры пинга для нод без истории: нода → (задержка, когда).
    measured: Mutex<HashMap<String, (u32, Instant)>>,
    measuring: Mutex<std::collections::HashSet<String>>,
    /// Когда замер ноды не удался — чтобы не повторять его без паузы.
    measure_failed: Mutex<HashMap<String, Instant>>,
}

/// Пауза перед повтором неудавшегося замера пинга.
const MEASURE_RETRY: Duration = Duration::from_secs(30);

pub type SharedRef = Arc<Shared>;

impl Shared {
    pub fn new() -> SharedRef {
        Arc::new(Shared {
            status: Mutex::new(Status::default()),
            hosts: Mutex::new(HostTracker::default()),
            settings: Mutex::new(util::load_settings()),
            api: Mutex::new(Api::from_config()),
            busy: Mutex::new(None),
            user_stopped: AtomicBool::new(false),
            ui_visible: AtomicBool::new(false),
            manual_select_at: AtomicU64::new(0),
            poke: AtomicBool::new(false),
            measured: Mutex::new(HashMap::new()),
            measuring: Mutex::new(Default::default()),
            measure_failed: Mutex::new(HashMap::new()),
        })
    }

    pub fn api(&self) -> Api {
        self.api.lock().unwrap().clone()
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn hosts(&self) -> Vec<HostEntry> {
        let mut v: Vec<HostEntry> = self.hosts.lock().unwrap().hosts.values().cloned().collect();
        v.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(b.down.cmp(&a.down)));
        v
    }

    pub fn clear_hosts(&self) {
        let mut t = self.hosts.lock().unwrap();
        t.hosts.clear();
    }

    pub fn set_measured(&self, node: &str, d: u32) {
        self.measured.lock().unwrap().insert(node.to_string(), (d, Instant::now()));
    }

    /// Главная группа: из настроек, иначе первая в proxy-groups.
    pub fn main_group(&self) -> String {
        let s = self.settings().main_group;
        if !s.is_empty() {
            return s;
        }
        rules::load_doc().ok().and_then(|d| d.first_group()).unwrap_or_else(|| "Proxy".into())
    }
}

// ================================================================ цикл опроса

/// Что нужно сделать после опроса (вызывающий код обновляет трей).
pub trait Sink: Send + 'static {
    fn on_status(&self, st: &Status, health_changed: bool);
    fn notify(&self, title: &str, body: &str);
}

struct Memory {
    last_up: u64,
    last_down: u64,
    last_at: Instant,
    hist_up: VecDeque<u64>,
    hist_down: VecDeque<u64>,
    prev_running: Option<bool>,
    notified_down: bool,
    prev_leaf: String,
    /// Ноды, о которых уже сообщили «не отвечает» (до восстановления).
    dead_notified: std::collections::HashSet<String>,
    started: Instant,
    providers_updated: HashMap<String, String>,
    providers_checked: Option<Instant>,
    api_refreshed: Instant,
    last_health: String,
    restarts: VecDeque<Instant>,
    tick: u64,
    core_note: String,
}

pub fn spawn(app: AppHandle, shared: SharedRef, sink: impl Sink) {
    thread::Builder::new()
        .name("monitor".into())
        .spawn(move || {
            let mut m = Memory {
                last_up: 0,
                last_down: 0,
                last_at: Instant::now(),
                hist_up: VecDeque::from(vec![0; HISTORY]),
                hist_down: VecDeque::from(vec![0; HISTORY]),
                prev_running: None,
                notified_down: false,
                prev_leaf: String::new(),
                dead_notified: Default::default(),
                started: Instant::now(),
                providers_updated: HashMap::new(),
                providers_checked: None,
                api_refreshed: Instant::now(),
                last_health: String::new(),
                restarts: VecDeque::new(),
                tick: 0,
                core_note: String::new(),
            };
            loop {
                let st = poll(&shared, &mut m, &sink);
                let changed = st.health != m.last_health;
                m.last_health = st.health.clone();
                sink.on_status(&st, changed);
                // скрытым окнам статус не нужен: при показе они сами запрашивают get_status
                if shared.ui_visible.load(Ordering::Relaxed) {
                    let _ = app.emit("status", &st);
                }
                *shared.status.lock().unwrap() = st;

                // 1 с, пока окно открыто; 3 с в фоне. poke — опросить сразу.
                let wait = if shared.ui_visible.load(Ordering::Relaxed) { 10 } else { 30 };
                for _ in 0..wait {
                    if shared.poke.swap(false, Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                m.tick += 1;
            }
        })
        .expect("monitor thread");
}

fn poll(shared: &SharedRef, m: &mut Memory, sink: &impl Sink) -> Status {
    let settings = shared.settings();
    if m.api_refreshed.elapsed() > Duration::from_secs(30) {
        *shared.api.lock().unwrap() = Api::from_config();
        m.api_refreshed = Instant::now();
    }
    let api = shared.api();
    let mut st = Status {
        elevated: service::is_elevated(),
        busy: shared.busy.lock().unwrap().clone(),
        main_group: shared.main_group(),
        panel_url: format!("{}/ui", api.base()),
        game: crate::games::active(),
        ..Default::default()
    };

    // --- скорость и соединения (заодно проверка, что API живо)
    let conns = api.connections();
    st.api_ok = conns.is_ok();
    let svc = if st.api_ok { SvcState::Running } else { service::state() };
    st.service = serde_json::to_value(svc).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();

    let dt = m.last_at.elapsed().as_secs_f64().max(0.2);
    m.last_at = Instant::now();
    if let Ok(c) = &conns {
        let up = c.get("uploadTotal").and_then(Value::as_u64).unwrap_or(0);
        let down = c.get("downloadTotal").and_then(Value::as_u64).unwrap_or(0);
        if m.last_up > 0 || m.last_down > 0 {
            st.up_speed = (up.saturating_sub(m.last_up) as f64 / dt) as u64;
            st.down_speed = (down.saturating_sub(m.last_down) as f64 / dt) as u64;
        }
        m.last_up = up;
        m.last_down = down;
        st.up_total = up;
        st.down_total = down;
        let list = c.get("connections").and_then(Value::as_array).cloned().unwrap_or_default();
        st.conns = list.len();
        shared.hosts.lock().unwrap().update(&list);
    } else {
        m.last_up = 0;
        m.last_down = 0;
    }
    m.hist_up.pop_front();
    m.hist_up.push_back(st.up_speed);
    m.hist_down.pop_front();
    m.hist_down.push_back(st.down_speed);
    st.history_up = m.hist_up.iter().copied().collect();
    st.history_down = m.hist_down.iter().copied().collect();

    // --- режим, версия, нода
    if st.api_ok {
        if let Ok(cfg) = api.configs() {
            st.mode = cfg.get("mode").and_then(Value::as_str).unwrap_or("").to_lowercase();
        }
        if m.tick % 20 == 0 || shared.status.lock().unwrap().version.is_empty() {
            st.version = api.version().unwrap_or_default();
        } else {
            st.version = shared.status.lock().unwrap().version.clone();
        }
        if let Ok(proxies) = api.proxies() {
            let start = if st.mode == "global" { "GLOBAL".to_string() } else { st.main_group.clone() };
            st.chain = api::resolve_chain(&proxies, &start);
            st.node = st.chain.last().cloned().unwrap_or_default();
            if st.mode == "direct" {
                st.node = "DIRECT".into();
            }
            let hist = proxies.get(&st.node).and_then(api::last_delay);
            let measured = shared.measured.lock().unwrap().get(&st.node).cloned();
            let fresh = |at: Instant| at.elapsed() < Duration::from_secs(300);
            // своя история Mihomo важнее; свой замер — только для нод без истории
            st.delay = match (hist, measured) {
                (Some(h), _) => Some(h),
                (None, Some((d, at))) if fresh(at) => Some(d),
                _ => None,
            };
            let need = hist.is_none() && !measured.map(|(_, at)| fresh(at)).unwrap_or(false);
            // недавняя неудача — ждём, иначе после перезагрузки конфига замер
            // повторялся десятки раз в секунду
            let failed_recently = shared
                .measure_failed
                .lock()
                .unwrap()
                .get(&st.node)
                .map(|at| at.elapsed() < MEASURE_RETRY)
                .unwrap_or(false);
            if need
                && !failed_recently
                && !matches!(st.node.as_str(), "" | "DIRECT" | "REJECT" | "COMPATIBLE" | "PASS")
                && shared.measuring.lock().unwrap().insert(st.node.clone())
            {
                let url = api::test_url(&proxies, &st.main_group);
                // ноды подписки проверяются через провайдера (напрямую Mihomo отвечает 404)
                let provider = proxies.get(&st.node).and_then(|p| p.get("_provider")).and_then(Value::as_str).map(String::from);
                let (a, n, sh) = (api.clone(), st.node.clone(), shared.clone());
                thread::spawn(move || {
                    let ok = match a.probe_delay(&n, provider.as_deref(), &url, 5000) {
                        Ok(Some(d)) => {
                            sh.set_measured(&n, d);
                            sh.measure_failed.lock().unwrap().remove(&n);
                            true
                        }
                        Ok(None) => false,
                        // не смогли проверить — это не значит, что нода мертва
                        Err(e) => {
                            crate::log_warn!("пинг {n}: {e}");
                            false
                        }
                    };
                    sh.measuring.lock().unwrap().remove(&n);
                    if ok {
                        sh.poke.store(true, Ordering::Relaxed);
                    } else {
                        let mut f = sh.measure_failed.lock().unwrap();
                        f.retain(|_, at| at.elapsed() < MEASURE_RETRY);
                        f.insert(n, Instant::now());
                    }
                });
            }
        }
    }

    // --- как запущено ядро (раз в ~20 опросов)
    if m.tick % 20 == 0 {
        let svc_now = if svc == SvcState::Running && st.api_ok { service::state() } else { svc };
        let procs = service::core_processes();
        let note = if !st.api_ok || svc_now == SvcState::Running {
            String::new()
        } else if let Some(o) = procs.iter().find(|p| !p.ours) {
            format!("VPN работает через другую программу: {}", o.name)
        } else if let Some(o) = procs.iter().find(|p| p.ours) {
            let from = if o.parent.is_empty() { String::new() } else { format!(" (запущен из {})", o.parent) };
            format!("Mihomo запущен не службой{from} — после выключения сам не включится, пока его снова не запустят")
        } else {
            String::new()
        };
        if note != m.core_note && !note.is_empty() {
            crate::log_warn!("{note}; процессы: {:?}", procs);
        }
        m.core_note = note;
    }
    st.core_note = m.core_note.clone();

    // --- итоговое здоровье
    let running = st.api_ok || svc == SvcState::Running;
    let dead = st.api_ok && st.delay == Some(0) && st.mode != "direct";
    (st.health, st.reason) = if !running {
        match svc {
            SvcState::Missing => ("off".into(), "Служба mihomo не установлена".into()),
            SvcState::Pending => ("warn".into(), "Служба запускается…".into()),
            _ => ("off".into(), "VPN выключен".into()),
        }
    } else if !st.api_ok {
        ("warn".into(), "Служба запущена, но Mihomo не отвечает".into())
    } else if dead {
        ("warn".into(), "Нода не отвечает".into())
    } else if st.mode == "direct" {
        ("ok".into(), "Режим «Напрямую»: весь трафик без VPN".into())
    } else {
        ("ok".into(), "Защищено".into())
    };

    // --- уведомления и автоперезапуск
    let now_s = util::now_secs();
    if let Some(prev) = m.prev_running {
        if prev && !running && svc != SvcState::Pending {
            if !shared.user_stopped.load(Ordering::Relaxed) {
                m.notified_down = true;
                if settings.notify_service {
                    sink.notify("VPN отключился", "Служба Mihomo остановилась сама.");
                }
                crate::log_warn!("служба остановилась неожиданно");
                if settings.auto_restart {
                    try_auto_restart(shared, m, sink);
                }
            }
        }
        if !prev && running {
            shared.user_stopped.store(false, Ordering::Relaxed);
            if m.notified_down && settings.notify_service {
                sink.notify("VPN снова работает", "Служба Mihomo запущена.");
            }
            m.notified_down = false;
        }
    }
    if svc != SvcState::Pending {
        m.prev_running = Some(running);
    }

    if st.api_ok && !st.node.is_empty() {
        let manual = now_s.saturating_sub(shared.manual_select_at.load(Ordering::Relaxed)) < 8;
        if !m.prev_leaf.is_empty() && m.prev_leaf != st.node && !manual && settings.notify_node {
            sink.notify("Нода сменилась", &format!("{} → {}", m.prev_leaf, st.node));
        }
        m.prev_leaf = st.node.clone();
        // о мёртвой ноде сообщаем один раз, пока она снова не ответит
        if dead {
            let fresh_start = m.started.elapsed() < Duration::from_secs(20);
            if !fresh_start && m.dead_notified.insert(st.node.clone()) {
                crate::log_warn!("нода не отвечает: {}", st.node);
                // при умном выборе о мёртвой ноде сообщает он сам (вместе с переключением)
                if settings.notify_dead_node && !settings.smart_auto {
                    sink.notify("Нода не отвечает", &format!("{} не прошла проверку пинга", st.node));
                }
            }
        } else if st.delay.map(|d| d > 0).unwrap_or(false) {
            m.dead_notified.remove(&st.node);
        }
    }

    if st.api_ok && m.providers_checked.map(|t| t.elapsed() > Duration::from_secs(30)).unwrap_or(true) {
        m.providers_checked = Some(Instant::now());
        if let Ok(Value::Object(provs)) = api.proxy_providers() {
            for (name, p) in provs {
                if p.get("vehicleType").and_then(Value::as_str) != Some("HTTP") {
                    continue;
                }
                let upd = p.get("updatedAt").and_then(Value::as_str).unwrap_or("").to_string();
                let count = p.get("proxies").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                if let Some(old) = m.providers_updated.get(&name) {
                    if *old != upd && !upd.is_empty() && settings.notify_subscription {
                        sink.notify("Подписка обновлена", &format!("{name}: {count} нод"));
                    }
                }
                m.providers_updated.insert(name, upd);
            }
        }
    }

    st
}

fn try_auto_restart(shared: &SharedRef, m: &mut Memory, sink: &impl Sink) {
    while m.restarts.front().map(|t| t.elapsed() > Duration::from_secs(600)).unwrap_or(false) {
        m.restarts.pop_front();
    }
    if m.restarts.len() >= 3 {
        crate::log_warn!("автоперезапуск: 3 попытки за 10 минут, больше не пытаемся");
        sink.notify("Автоперезапуск остановлен", "Служба падает слишком часто. Посмотрите лог Mihomo.");
        return;
    }
    if !service::is_elevated() {
        // без прав админа перезапуск вызвал бы окно UAC внезапно — не делаем
        crate::log_warn!("автоперезапуск пропущен: трей запущен без прав администратора");
        return;
    }
    m.restarts.push_back(Instant::now());
    let sh = shared.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        match service::control("start") {
            Ok(()) => crate::log_info!("автоперезапуск: служба запущена"),
            Err(e) => crate::log_err!("автоперезапуск: {e}"),
        }
        sh.poke.store(true, Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tracker_counts_and_suspicious() {
        let mut t = HostTracker::default();
        let c = |id: &str, host: &str, down: u64, chain: &str| {
            json!({"id": id, "upload": 100, "download": down, "chains": [chain, "Proxy"],
                   "rule": "Match", "rulePayload": "", "metadata": {"host": host, "process": "chrome.exe"}})
        };
        t.update(&[c("1", "a.example.com", 10, "DIRECT"), c("2", "b.com", 5000, "NL")]);
        t.update(&[c("1", "a.example.com", 20, "DIRECT"), c("3", "a.example.com", 0, "DIRECT")]);
        t.update(&[c("4", "a.example.com", 0, "DIRECT")]);
        let a = &t.hosts["a.example.com"];
        assert_eq!(a.count, 3);
        assert_eq!(a.down, 20);
        assert_eq!(a.suffix, "example.com");
        assert!(a.suspicious);
        assert!(!t.hosts["b.com"].suspicious);
        assert_eq!(t.hosts["b.com"].active, 0);
    }
}
