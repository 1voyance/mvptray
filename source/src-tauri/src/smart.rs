//! Умный автовыбор ноды.
//!
//! Родной url-test в Mihomo раз в несколько минут делает ОДИН замер пинга и берёт
//! самую быструю ноду. Из-за этого он часто выбирает ноду с удачным разовым пингом,
//! но с потерями, скачками задержки или низкой скоростью, и долго сидит на умершей.
//!
//! Здесь трей сам оценивает каждую ноду:
//!   • пинг раз в минуту, а у текущей ноды — раз в 15 секунд;
//!   • по последним 10 замерам: медиана пинга, разброс (джиттер), процент потерь;
//!   • скорость скачивания (тест через служебную группу, ваш трафик не затрагивается);
//!   • итоговая оценка = медиана + 1,5×разброс + штраф за потери, с поправкой на скорость.
//! Переключение:
//!   • сразу, если текущая нода дважды подряд не ответила;
//!   • иначе — только если другая нода лучше минимум на 25 % и прошло 5 минут
//!     с прошлого переключения (без «прыжков» туда-сюда).
//! Выбор фиксируется в группе Auto через API. Если трей закрыт, Auto продолжает
//! работать сама: при смерти зафиксированной ноды Mihomo перейдёт на самую быструю.

use crate::api::{self, Api, ApiError};
use crate::monitor::{SharedRef, Sink};
use crate::rules;
use crate::util;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const WINDOW: usize = 10;
const FULL_ROUND: u64 = 60;
const QUICK_CHECK: u64 = 15;
const MIN_SWITCH_GAP: u64 = 300;
const BETTER_BY: f64 = 0.75;
const PROBE_TIMEOUT_MS: u32 = 3000;
const SPEED_EVERY: u64 = 6 * 3600;
const SPEED_VALID: u64 = 24 * 3600;
const SPEED_SECONDS: f64 = 3.0;
const SPEED_MAX_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Default, Clone)]
struct NodeStat {
    /// (время, задержка): 0 — не ответила.
    samples: VecDeque<(u64, u32)>,
    /// (байт/с, когда измерено)
    speed: Option<(u64, u64)>,
    speed_error: Option<String>,
}

impl NodeStat {
    fn push(&mut self, d: u32) {
        self.samples.push_back((util::now_secs(), d));
        while self.samples.len() > WINDOW {
            self.samples.pop_front();
        }
    }

    fn metrics(&self) -> Metrics {
        let n = self.samples.len();
        let ok: Vec<u32> = self.samples.iter().map(|s| s.1).filter(|d| *d > 0).collect();
        let fails = n - ok.len();
        let mut sorted = ok.clone();
        sorted.sort_unstable();
        let median = if sorted.is_empty() { None } else { Some(sorted[sorted.len() / 2]) };
        let jitter = if ok.len() >= 2 {
            let sum: u32 = ok.windows(2).map(|w| w[0].abs_diff(w[1])).sum();
            Some(sum / (ok.len() as u32 - 1))
        } else {
            None
        };
        Metrics {
            samples: n,
            median,
            jitter,
            loss: if n == 0 { 0.0 } else { fails as f64 / n as f64 },
            last: self.samples.back().map(|s| s.1),
        }
    }

    /// Дважды подряд не ответила (или единственный замер — неудачный).
    fn dead(&self) -> bool {
        let mut it = self.samples.iter().rev();
        match (it.next(), it.next()) {
            (Some((_, 0)), Some((_, 0))) => true,
            (Some((_, 0)), None) => true,
            _ => false,
        }
    }

    fn valid_speed(&self) -> Option<u64> {
        self.speed.filter(|(_, at)| util::now_secs().saturating_sub(*at) < SPEED_VALID).map(|(s, _)| s)
    }
}

struct Metrics {
    samples: usize,
    median: Option<u32>,
    jitter: Option<u32>,
    loss: f64,
    last: Option<u32>,
}

/// Оценка ноды: меньше — лучше. None — нода не отвечает совсем.
fn score(st: &NodeStat, best_speed: Option<u64>) -> Option<f64> {
    let m = st.metrics();
    let median = m.median? as f64;
    let jitter = m.jitter.unwrap_or(0) as f64;
    let mut s = median + 1.5 * jitter + 600.0 * m.loss;
    if let (Some(sp), Some(best)) = (st.valid_speed(), best_speed) {
        // нода вдвое медленнее лучшей получает штраф ×1,41, вчетверо — ×2
        let ratio = (best as f64 / sp.max(1) as f64).clamp(1.0, 4.0);
        s *= ratio.sqrt();
    }
    Some(s)
}

#[derive(Default)]
pub struct SmartState {
    nodes: HashMap<String, NodeStat>,
    order: Vec<String>,
    current: String,
    reason: String,
    last_switch: u64,
    last_full: u64,
    last_quick: u64,
    last_speed: u64,
    /// Мы сами зафиксировали ноду в группе — при выключении нужно снять фиксацию.
    we_fixed: bool,
    speed_progress: Option<(usize, usize, String)>,
    history: VecDeque<(u64, String)>,
    error: Option<String>,
}

#[derive(Serialize)]
pub struct NodeView {
    name: String,
    samples: usize,
    median: Option<u32>,
    jitter: Option<u32>,
    loss: f64,
    last: Option<u32>,
    speed: Option<u64>,
    speed_error: Option<String>,
    score: Option<f64>,
    dead: bool,
}

#[derive(Serialize)]
pub struct SmartView {
    enabled: bool,
    group: String,
    current: String,
    pinned: String,
    reason: String,
    last_switch: u64,
    speed_progress: Option<(usize, usize, String)>,
    last_speed: u64,
    nodes: Vec<NodeView>,
    history: Vec<(u64, String)>,
    error: Option<String>,
}

pub static STATE: Mutex<Option<SmartState>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut SmartState) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(|| SmartState {
        // первый автотест скорости — через 10 минут после запуска, не сразу
        last_speed: util::now_secs().saturating_sub(SPEED_EVERY - 600),
        ..Default::default()
    }))
}

pub fn view(shared: &SharedRef) -> SmartView {
    let s = shared.settings();
    with(|st| {
        let best_speed = st.nodes.values().filter_map(NodeStat::valid_speed).max();
        let mut nodes: Vec<NodeView> = st
            .order
            .iter()
            .map(|name| {
                let ns = st.nodes.get(name).cloned().unwrap_or_default();
                let m = ns.metrics();
                NodeView {
                    name: name.clone(),
                    samples: m.samples,
                    median: m.median,
                    jitter: m.jitter,
                    loss: m.loss,
                    last: m.last,
                    speed: ns.valid_speed(),
                    speed_error: ns.speed_error.clone(),
                    score: score(&ns, best_speed),
                    dead: ns.dead(),
                }
            })
            .collect();
        nodes.sort_by(|a, b| match (a.score, b.score) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        SmartView {
            enabled: s.smart_auto,
            group: s.smart_group.clone(),
            current: st.current.clone(),
            pinned: s.smart_pinned.clone(),
            reason: st.reason.clone(),
            last_switch: st.last_switch,
            speed_progress: st.speed_progress.clone(),
            last_speed: st.last_speed,
            nodes,
            history: st.history.iter().rev().cloned().collect(),
            error: st.error.clone(),
        }
    })
}

/// Принудительная проверка всех нод на следующем такте.
pub fn request_round() {
    with(|st| {
        st.last_full = 0;
        st.last_quick = 0;
    });
}

fn note(st: &mut SmartState, text: String) {
    crate::log_info!("умный выбор: {text}");
    st.history.push_back((util::now_secs(), text));
    while st.history.len() > 30 {
        st.history.pop_front();
    }
}

// ================================================================ цикл

pub fn spawn(app: AppHandle, shared: SharedRef, sink: impl Sink) {
    thread::Builder::new()
        .name("smart".into())
        .spawn(move || {
            thread::sleep(Duration::from_secs(3));
            let emit = {
                let app = app.clone();
                let shared = shared.clone();
                move || {
                    // скрытому окну обновление не нужно — оно перечитает всё при показе
                    if shared.ui_visible.load(Ordering::Relaxed) {
                        let _ = app.emit("smart", ());
                    }
                }
            };
            loop {
                tick(&emit, &shared, &sink);
                thread::sleep(Duration::from_secs(5));
            }
        })
        .expect("smart thread");
}

fn tick(emit: &(dyn Fn() + Sync), shared: &SharedRef, sink: &impl Sink) {
    let settings = shared.settings();
    let api = shared.api();
    let group = if settings.smart_group.is_empty() { "Auto".to_string() } else { settings.smart_group.clone() };

    if !settings.smart_auto {
        let undo = with(|st| std::mem::take(&mut st.we_fixed));
        if undo {
            match api.unfix(&group) {
                Ok(()) => with(|st| note(st, format!("выключен — {group} снова выбирает ноду сама"))),
                Err(e) => crate::log_warn!("снять фиксацию {group}: {e}"),
            }
            emit();
        }
        return;
    }

    // Такт каждые 5 с, а работа есть раз в 15–60 с: если ни проверка нод, ни тест
    // скорости не подошли по времени — не дёргаем API Mihomo вообще.
    {
        let now = util::now_secs();
        let gaming = crate::games::active().is_some();
        let period = if gaming { FULL_ROUND * 3 } else { FULL_ROUND };
        let due = with(|st| {
            st.order.is_empty()
                || st.current.is_empty()
                || now.saturating_sub(st.last_full) >= period
                || now.saturating_sub(st.last_quick) >= QUICK_CHECK
                || (settings.smart_speed_auto && !gaming && st.speed_progress.is_none() && now.saturating_sub(st.last_speed) >= SPEED_EVERY)
        });
        if !due {
            return;
        }
    }

    let proxies = match api.proxies() {
        Ok(p) => p,
        Err(_) => return, // Mihomo выключен — ждём
    };
    let Some(g) = proxies.get(&group) else {
        with(|st| st.error = Some(format!("в конфиге нет группы «{group}»")));
        return;
    };
    let leaves: Vec<String> = g
        .get("all")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|n| !matches!(*n, "DIRECT" | "REJECT" | "REJECT-DROP" | "PASS" | "COMPATIBLE"))
                .filter(|n| proxies.get(*n).and_then(|p| p.get("all")).is_none()) // только ноды, не группы
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if leaves.is_empty() {
        with(|st| st.error = Some(format!("в группе «{group}» нет нод")));
        return;
    }
    let fixed = g.get("fixed").and_then(Value::as_str).unwrap_or("").to_string();
    let now_node = g.get("now").and_then(Value::as_str).unwrap_or("").to_string();
    let url = api::test_url(&proxies, &group);
    let providers = provider_map(&api);

    let now = util::now_secs();
    let (full, quick_node) = with(|st| {
        st.error = None;
        st.order = leaves.clone();
        st.nodes.retain(|k, _| leaves.contains(k));
        if st.current.is_empty() {
            st.current = if fixed.is_empty() { now_node.clone() } else { fixed.clone() };
        }
        // в игре полная проверка реже — меньше фоновой активности
        let period = if crate::games::active().is_some() { FULL_ROUND * 3 } else { FULL_ROUND };
        let full = now.saturating_sub(st.last_full) >= period;
        let quick = !full && now.saturating_sub(st.last_quick) >= QUICK_CHECK && !st.current.is_empty();
        if full {
            st.last_full = now;
        }
        if full || quick {
            st.last_quick = now;
        }
        (full, if quick { Some(st.current.clone()) } else { None })
    });

    let targets: Vec<String> = if full { leaves.clone() } else { quick_node.into_iter().collect() };
    if targets.is_empty() {
        maybe_speed_test(emit, shared, &api, &leaves, &settings);
        return;
    }

    // замеры параллельно
    let results: Vec<(String, Result<Option<u32>, ApiError>)> = thread::scope(|sc| {
        let hs: Vec<_> = targets
            .iter()
            .map(|n| {
                let (api, url, prov) = (&api, &url, providers.get(n).cloned());
                sc.spawn(move || (n.clone(), api.probe_delay(n, prov.as_deref(), url, PROBE_TIMEOUT_MS)))
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut errors = 0;
    with(|st| {
        for (n, r) in results {
            match r {
                Ok(Some(d)) => st.nodes.entry(n).or_default().push(d),
                Ok(None) => {}
                Err(e) => {
                    errors += 1;
                    crate::log_warn!("умный выбор: пинг {n} не удался: {e}");
                }
            }
        }
    });
    if errors == targets.len() && errors > 0 {
        with(|st| st.error = Some("не удалось проверить ноды — Mihomo не отвечает на проверку".into()));
    }

    decide(shared, &api, &group, &fixed, &settings.smart_pinned, sink);
    emit();
    maybe_speed_test(emit, shared, &api, &leaves, &settings);
}

fn provider_map(api: &Api) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Ok(Value::Object(provs)) = api.proxy_providers() {
        for (pname, p) in provs {
            if p.get("vehicleType").and_then(Value::as_str) == Some("Compatible") {
                continue;
            }
            for node in p.get("proxies").and_then(Value::as_array).into_iter().flatten() {
                if let Some(n) = node.get("name").and_then(Value::as_str) {
                    m.insert(n.to_string(), pname.clone());
                }
            }
        }
    }
    m
}

fn fmt_node(st: &NodeStat) -> String {
    let m = st.metrics();
    let mut s = match m.median {
        Some(d) => format!("пинг {d} мс"),
        None => "не отвечает".into(),
    };
    if let Some(j) = m.jitter {
        s.push_str(&format!(", разброс ±{j}"));
    }
    if m.loss > 0.0 {
        s.push_str(&format!(", потери {:.0}%", m.loss * 100.0));
    }
    if let Some(sp) = st.valid_speed() {
        s.push_str(&format!(", {:.1} Мбит/с", sp as f64 * 8.0 / 1_000_000.0));
    }
    s
}

fn decide(shared: &SharedRef, api: &Api, group: &str, fixed: &str, pinned: &str, sink: &impl Sink) {
    let now = util::now_secs();
    let decision: Option<(String, String, bool)> = with(|st| {
        let best_speed = st.nodes.values().filter_map(NodeStat::valid_speed).max();
        let mut cands: Vec<(String, f64)> = st
            .order
            .iter()
            .filter_map(|n| {
                let ns = st.nodes.get(n)?;
                if ns.metrics().last == Some(0) {
                    return None; // последний замер неудачный — не кандидат
                }
                Some((n.clone(), score(ns, best_speed)?))
            })
            .collect();
        cands.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let best = cands.first().cloned();
        let cur = st.current.clone();
        let cur_stat = st.nodes.get(&cur).cloned().unwrap_or_default();

        // закреплённая вручную нода
        if !pinned.is_empty() && st.order.iter().any(|n| n == pinned) {
            let ps = st.nodes.get(pinned).cloned().unwrap_or_default();
            if !ps.dead() {
                if cur != pinned || fixed != pinned {
                    return Some((pinned.to_string(), format!("{pinned} закреплена вами"), true));
                }
                st.reason = format!("{pinned} закреплена вами · {}", fmt_node(&ps));
                return None;
            }
            if let Some((b, _)) = &best {
                if *b != cur {
                    return Some((b.clone(), format!("закреплённая {pinned} не отвечает, временно {b}"), true));
                }
            }
            return None;
        }

        let Some((best_name, best_score)) = best else {
            st.reason = "ни одна нода не отвечает".into();
            return None;
        };
        let best_stat = st.nodes.get(&best_name).cloned().unwrap_or_default();

        // текущая неизвестна или мертва — переключаемся сразу
        if cur.is_empty() || !st.nodes.contains_key(&cur) || cur_stat.dead() {
            if best_name != cur || fixed != best_name {
                let why = if cur.is_empty() || !st.nodes.contains_key(&cur) {
                    format!("выбрана {best_name}: {}", fmt_node(&best_stat))
                } else {
                    format!("{cur} не отвечает → {best_name} ({})", fmt_node(&best_stat))
                };
                return Some((best_name, why, true));
            }
        }

        let cur_score = score(&cur_stat, best_speed);
        let since = now.saturating_sub(st.last_switch);
        // в игровом режиме ради «чуть лучше» не переключаемся — только если нода умерла
        let gaming = crate::games::active().is_some();
        if !gaming && best_name != cur && since >= MIN_SWITCH_GAP && best_stat.samples.len() >= 5 {
            if let Some(cs) = cur_score {
                if best_score < cs * BETTER_BY {
                    let pct = ((1.0 - best_score / cs) * 100.0).round();
                    return Some((
                        best_name.clone(),
                        format!("{best_name} лучше {cur} на {pct}% ({})", fmt_node(&best_stat)),
                        true,
                    ));
                }
            }
        }

        // остаёмся; убедимся, что выбор зафиксирован в Mihomo
        if !cur.is_empty() && fixed != cur {
            return Some((cur.clone(), String::new(), false));
        }
        st.reason = format!("{cur}: {}", fmt_node(&cur_stat));
        None
    });

    let Some((target, why, announce)) = decision else { return };
    shared.manual_select_at.store(now, Ordering::Relaxed);
    match api.select(group, &target) {
        Ok(()) => {
            let prev = with(|st| {
                let prev = std::mem::replace(&mut st.current, target.clone());
                st.we_fixed = true;
                if announce {
                    st.last_switch = now;
                    st.reason = why.clone();
                    note(st, why.clone());
                } else {
                    let ns = st.nodes.get(&target).cloned().unwrap_or_default();
                    st.reason = format!("{target}: {}", fmt_node(&ns));
                }
                prev
            });
            if announce && !prev.is_empty() && prev != target && shared.settings().notify_node {
                sink.notify("Умный выбор ноды", &why);
            }
            shared.poke.store(true, Ordering::Relaxed);
        }
        Err(e) => with(|st| st.error = Some(format!("не удалось выбрать {target} в группе {group}: {e}"))),
    }
}

// ================================================================ тест скорости

fn maybe_speed_test(_emit: &(dyn Fn() + Sync), shared: &SharedRef, api: &Api, leaves: &[String], settings: &util::Settings) {
    if !settings.smart_speed_auto || crate::games::active().is_some() {
        return;
    }
    let due = with(|st| st.speed_progress.is_none() && util::now_secs().saturating_sub(st.last_speed) >= SPEED_EVERY);
    // только когда интернет почти не используется
    let idle = shared.status.lock().map(|s| s.down_speed < 300 * 1024 && s.up_speed < 300 * 1024).unwrap_or(false);
    if due && idle {
        start_speed_test(None, shared.clone(), api.clone(), leaves.to_vec(), true);
    }
}

/// Запуск теста скорости всех нод группы (в фоне). Err — уже идёт или нет нод.
pub fn start_speed_test(app: Option<AppHandle>, shared: SharedRef, api: Api, leaves: Vec<String>, auto: bool) -> bool {
    let started = with(|st| {
        if st.speed_progress.is_some() || leaves.is_empty() {
            return false;
        }
        st.speed_progress = Some((0, leaves.len(), String::new()));
        st.last_speed = util::now_secs();
        true
    });
    if !started {
        return false;
    }
    thread::spawn(move || {
        let emit = || {
            if let Some(a) = &app {
                let _ = a.emit("smart", ());
            }
        };
        let res = run_speed_test(&emit, &shared, &api, &leaves, auto);
        with(|st| {
            st.speed_progress = None;
            match &res {
                Ok(n) => note(st, format!("тест скорости завершён: проверено нод — {n}")),
                Err(e) => {
                    st.error = Some(format!("тест скорости: {e}"));
                    crate::log_warn!("тест скорости: {e}");
                }
            }
        });
        request_round();
        emit();
    });
    true
}

pub fn leaves_of_group(api: &Api, group: &str) -> Vec<String> {
    let Ok(p) = api.proxies() else { return vec![] };
    p.get(group)
        .and_then(|g| g.get("all"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|n| !matches!(*n, "DIRECT" | "REJECT" | "REJECT-DROP" | "PASS" | "COMPATIBLE"))
                .filter(|n| p.get(*n).and_then(|x| x.get("all")).is_none())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn run_speed_test(emit: &dyn Fn(), shared: &SharedRef, api: &Api, leaves: &[String], auto: bool) -> Result<usize, String> {
    // служебная группа: тестовый трафик идёт через неё, ваш трафик не затрагивается.
    // Если в ней нет каких-то нод (появились новые) — пересоздаём.
    let probe_ok = api
        .proxies()
        .ok()
        .and_then(|p| {
            let all: Vec<String> = p
                .get(rules::PROBE_GROUP)?
                .get("all")?
                .as_array()?
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect();
            Some(leaves.iter().all(|n| all.contains(n)))
        })
        .unwrap_or(false);
    if !probe_ok {
        let r = rules::mutate(api, "служебная группа для теста скорости", |d| {
            d.remove_probe_group();
            d.ensure_probe().map(|_| ())
        });
        if !r.ok || !r.live {
            return Err(if r.message.is_empty() { "не удалось добавить служебную группу".into() } else { r.message });
        }
        thread::sleep(Duration::from_secs(2));
    }
    let port = rules::load_doc()
        .ok()
        .and_then(|d| d.top_value("mixed-port").or_else(|| d.top_value("port")))
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or("в config.yaml не задан mixed-port")?;
    let proxy = ureq::Proxy::new(format!("http://127.0.0.1:{port}")).map_err(|e| e.to_string())?;

    let providers = provider_map(api);
    let group = shared.settings().smart_group;
    let url = api.proxies().map(|p| api::test_url(&p, if group.is_empty() { "Auto" } else { &group })).unwrap_or_else(|_| api::DEFAULT_TEST_URL.into());

    let mut done = 0;
    for (i, node) in leaves.iter().enumerate() {
        // автотест уступает, если вы начали что-то качать
        if auto && shared.status.lock().map(|s| s.down_speed > 2 * 1024 * 1024).unwrap_or(false) {
            crate::log_info!("автотест скорости прерван: интернет занят");
            break;
        }
        with(|st| st.speed_progress = Some((i, leaves.len(), node.clone())));
        emit();

        // ноды без пинга по скорости не проверяем — это только трата времени
        let known = with(|st| st.nodes.get(node).map(|ns| (ns.dead(), ns.metrics().last)));
        let alive = match known {
            Some((true, _)) | Some((_, Some(0))) => false,
            Some((_, Some(_))) => true,
            _ => matches!(api.probe_delay(node, providers.get(node).map(String::as_str), &url, 2000), Ok(Some(d)) if d > 0),
        };
        if !alive {
            with(|st| {
                let ns = st.nodes.entry(node.clone()).or_default();
                ns.speed = None;
                ns.speed_error = Some("пропущена: не отвечает на пинг".into());
            });
            crate::log_info!("скорость {node}: пропущена, нет пинга");
            continue;
        }

        if let Err(e) = api.select(rules::PROBE_GROUP, node) {
            crate::log_warn!("тест скорости: выбрать {node}: {e}");
            continue;
        }
        // убедиться, что служебная группа действительно смотрит на эту ноду,
        // иначе можно случайно измерить скорость без VPN
        let now = api.proxies().ok().and_then(|p| p.get(rules::PROBE_GROUP)?.get("now")?.as_str().map(String::from));
        if now.as_deref() != Some(node.as_str()) {
            crate::log_warn!("тест скорости: служебная группа не переключилась на {node}");
            continue;
        }
        let res = measure(&proxy);
        with(|st| {
            let ns = st.nodes.entry(node.clone()).or_default();
            match &res {
                Ok(bps) => {
                    ns.speed = Some((*bps, util::now_secs()));
                    ns.speed_error = None;
                }
                Err(e) => {
                    ns.speed = Some((0, util::now_secs()));
                    ns.speed_error = Some(e.clone());
                }
            }
        });
        match res {
            Ok(b) => crate::log_info!("скорость {node}: {:.1} Мбит/с", b as f64 * 8.0 / 1e6),
            Err(e) => crate::log_warn!("скорость {node}: {e}"),
        }
        done += 1;
    }
    let _ = api.select(rules::PROBE_GROUP, "DIRECT");
    Ok(done)
}

/// Скачивает тестовый файл через служебную группу, возвращает байт/с.
fn measure(proxy: &ureq::Proxy) -> Result<u64, String> {
    const URLS: &[&str] = &["http://cachefly.cachefly.net/50mb.test", "http://speedtest.tele2.net/50MB.zip"];
    let mut last_err = String::new();
    for url in URLS {
        let agent = ureq::AgentBuilder::new()
            .proxy(proxy.clone())
            .timeout_connect(Duration::from_secs(3))
            .timeout_read(Duration::from_secs(4))
            .build();
        let resp = match agent.get(url).call() {
            Ok(r) => r,
            // сервер ответил ошибкой — пробуем запасной файл
            Err(ureq::Error::Status(code, _)) => {
                last_err = format!("HTTP {code}");
                continue;
            }
            // туннель через ноду не поднялся — запасной файл не поможет
            Err(e) => return Err(e.to_string()),
        };
        let mut reader = resp.into_reader();
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        let mut start: Option<Instant> = None;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let s = *start.get_or_insert_with(Instant::now);
                    total += n as u64;
                    if s.elapsed().as_secs_f64() >= SPEED_SECONDS || total >= SPEED_MAX_BYTES {
                        break;
                    }
                }
                Err(e) => {
                    last_err = e.to_string();
                    break;
                }
            }
        }
        if let Some(s) = start {
            let secs = s.elapsed().as_secs_f64().max(0.2);
            if total > 256 * 1024 {
                return Ok((total as f64 / secs) as u64);
            }
        }
        if last_err.is_empty() {
            last_err = "слишком мало данных".into();
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(d: &[u32]) -> NodeStat {
        let mut s = NodeStat::default();
        for x in d {
            s.push(*x);
        }
        s
    }

    #[test]
    fn scoring_prefers_stable() {
        // разово быстрая, но нестабильная с потерями
        let flaky = stat(&[40, 0, 300, 45, 0, 250, 50, 0, 280, 42]);
        // чуть медленнее, но ровная
        let steady = stat(&[90, 95, 88, 92, 91, 94, 89, 93, 90, 92]);
        let a = score(&flaky, None).unwrap();
        let b = score(&steady, None).unwrap();
        assert!(b < a, "ровная нода должна победить: steady={b} flaky={a}");
        assert!(!flaky.dead());
        assert!(stat(&[50, 0, 0]).dead());
        assert!(score(&stat(&[0, 0]), None).is_none());
    }

    #[test]
    fn speed_penalty() {
        let mut fast = stat(&[80; 6]);
        let mut slow = stat(&[60; 6]);
        fast.speed = Some((10_000_000, util::now_secs()));
        slow.speed = Some((1_000_000, util::now_secs()));
        let best = Some(10_000_000);
        assert!(score(&fast, best).unwrap() < score(&slow, best).unwrap(), "в 10 раз медленнее — хуже, даже с пингом ниже");
    }

    struct NoSink;
    impl Sink for NoSink {
        fn on_status(&self, _: &crate::monitor::Status, _: bool) {}
        fn notify(&self, t: &str, b: &str) {
            eprintln!("NOTIFY {t}: {b}");
        }
    }

    /// MIHOMO_DIR=<папка> cargo test smart_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn smart_live() {
        let shared = crate::monitor::Shared::new();
        let api = shared.api();
        let noop = || {};
        // вернуть Auto к родному поведению
        let _ = api.unfix("Auto");
        *STATE.lock().unwrap() = None;
        let mut st0 = shared.settings();
        st0.smart_speed_auto = false;
        st0.smart_auto = true;
        st0.smart_pinned.clear();
        *shared.settings.lock().unwrap() = st0;

        // несколько раундов: живые GOOD-*, мёртвые NL-1 и SE-1
        for _ in 0..3 {
            request_round();
            tick(&noop, &shared, &NoSink);
        }
        let v = view(&shared);
        let names: Vec<_> = v.nodes.iter().map(|n| (n.name.clone(), n.median, n.dead)).collect();
        eprintln!("{names:?} current={} reason={}", v.current, v.reason);
        assert!(v.nodes.iter().any(|n| n.name == "SE-1"), "нода подписки должна быть в списке");
        assert!(v.nodes.iter().find(|n| n.name == "NL-1").unwrap().dead);
        assert!(v.current.starts_with("GOOD-"), "должна быть выбрана живая нода");
        let p = api.proxies().unwrap();
        assert_eq!(p["Auto"]["fixed"].as_str(), Some(v.current.as_str()), "выбор зафиксирован в Mihomo");

        // закрепление
        let mut st = shared.settings();
        let other = if v.current == "GOOD-1" { "GOOD-2" } else { "GOOD-1" };
        st.smart_pinned = other.into();
        *shared.settings.lock().unwrap() = st.clone();
        request_round();
        tick(&noop, &shared, &NoSink);
        assert_eq!(api.proxies().unwrap()["Auto"]["fixed"].as_str(), Some(other));
        // закрепить мёртвую — должен временно уйти на живую
        st.smart_pinned = "NL-1".into();
        *shared.settings.lock().unwrap() = st.clone();
        for _ in 0..2 {
            request_round();
            tick(&noop, &shared, &NoSink);
        }
        let fixed = api.proxies().unwrap()["Auto"]["fixed"].as_str().unwrap_or("").to_string();
        assert!(fixed.starts_with("GOOD-"), "мёртвую закреплённую не выбирать: {fixed}");

        // тест скорости через служебную группу
        st.smart_pinned.clear();
        *shared.settings.lock().unwrap() = st.clone();
        let leaves = leaves_of_group(&api, "Auto");
        let done = run_speed_test(&noop, &shared, &api, &leaves, false).unwrap();
        assert_eq!(done, 2, "мёртвые NL-1 и SE-1 пропускаются");
        let v = view(&shared);
        let g1 = v.nodes.iter().find(|n| n.name == "GOOD-1").unwrap();
        assert!(g1.speed.unwrap_or(0) > 1_000_000, "скорость GOOD-1: {:?} {:?}", g1.speed, g1.speed_error);
        assert_eq!(v.nodes.iter().find(|n| n.name == "NL-1").unwrap().speed_error.as_deref(), Some("пропущена: не отвечает на пинг"));
        assert!(api.proxies().unwrap().get(rules::PROBE_GROUP).is_some());
        // ваш выбор в Auto тест скорости не трогает
        assert!(api.proxies().unwrap()["Auto"]["fixed"].as_str().unwrap_or("").starts_with("GOOD-"));

        // выключение — снимаем фиксацию
        st.smart_auto = false;
        *shared.settings.lock().unwrap() = st;
        tick(&noop, &shared, &NoSink);
        assert_eq!(api.proxies().unwrap()["Auto"]["fixed"].as_str(), Some(""));
    }
}
