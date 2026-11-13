//! Подсказка «сайт не открывается — отправить в VPN?».
//!
//! Признак блокировки: соединение шло напрямую, программа отправила запрос
//! (TLS-приветствие), а в ответ не пришло почти ничего — и так несколько раз.
//! Тогда в углу экрана всплывает маленькое окно с кнопками, фокус оно не забирает.

use crate::games;
use crate::monitor::{HostEntry, SharedRef};
use crate::rules::{self, Rule};
use crate::util;
use serde::Serialize;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Serialize, Clone, Debug)]
pub struct Suggestion {
    pub host: String,
    pub suffix: String,
    pub process: String,
}

static PENDING: Mutex<Option<Suggestion>> = Mutex::new(None);
static ASKED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

const EXCLUDE_PROC: &[&str] = &[
    "svchost.exe", "backgroundtaskhost.exe", "searchhost.exe", "smartscreen.exe", "mihomo.exe",
    "mihomo-tray.exe", "system", "msmpeng.exe", "compattelrunner.exe", "wuauclt.exe", "musnotification.exe",
];
const NOISE: &[&str] = &[
    "msftconnecttest.com", "msftncsi.com", "windows.com", "microsoft.com", "windowsupdate.com", "live.com",
    "office.com", "office.net", "lencr.org", "digicert.com", "sectigo.com", "globalsign.com", "letsencrypt.org",
    "ntp.org", "local", "lan", "localhost", "arpa",
];

pub fn is_candidate(h: &HostEntry, now: u64, ignored: &[String], asked: &HashSet<String>, rules: &[Rule]) -> bool {
    if h.route != "DIRECT" || h.host.parse::<IpAddr>().is_ok() || !h.host.contains('.') {
        return false;
    }
    // отправили запрос, а в ответ почти ничего
    if h.down >= 600 || h.up < 200 {
        return false;
    }
    if !(h.count >= 2 || h.last_seen.saturating_sub(h.first_seen) >= 6) || now.saturating_sub(h.last_seen) > 30 {
        return false;
    }
    if EXCLUDE_PROC.contains(&h.process.to_lowercase().as_str()) {
        return false;
    }
    let host = h.host.to_lowercase();
    let hit = |d: &str| host == d || host.ends_with(&format!(".{d}"));
    if NOISE.iter().any(|d| hit(d)) || ignored.iter().any(|d| hit(&d.to_lowercase())) || asked.contains(&h.suffix) {
        return false;
    }
    !rules::covers(rules, &h.host)
}

pub fn spawn(app: AppHandle, shared: SharedRef) {
    thread::Builder::new()
        .name("prompt".into())
        .spawn(move || {
            let mut last_prompt = 0u64;
            loop {
                thread::sleep(Duration::from_secs(5));
                let s = shared.settings();
                let now = util::now_secs();
                if !s.suggest_blocked || games::active().is_some() || PENDING.lock().unwrap().is_some() || now.saturating_sub(last_prompt) < 90 {
                    continue;
                }
                let hosts = shared.hosts();
                // config.yaml читаем, только если есть хоть один подозрительный адрес
                // (раньше файл разбирался каждые 5 секунд впустую)
                {
                    let g = ASKED.lock().unwrap();
                    let empty = HashSet::new();
                    let asked = g.as_ref().unwrap_or(&empty);
                    if !hosts.iter().any(|h| is_candidate(h, now, &s.ignored_hosts, asked, &[])) {
                        continue;
                    }
                }
                let rules = rules::load_doc().map(|d| d.rules()).unwrap_or_default();
                let found = {
                    let mut g = ASKED.lock().unwrap();
                    let asked = g.get_or_insert_with(HashSet::new);
                    let c = hosts.iter().find(|h| is_candidate(h, now, &s.ignored_hosts, asked, &rules)).cloned();
                    if let Some(h) = &c {
                        asked.insert(h.suffix.clone());
                    }
                    c
                };
                let Some(h) = found else { continue };
                last_prompt = now;
                let sug = Suggestion { host: h.host.clone(), suffix: h.suffix.clone(), process: h.process.clone() };
                crate::log_info!("подсказка: {} похоже заблокирован ({})", sug.host, sug.process);
                *PENDING.lock().unwrap() = Some(sug.clone());
                show(&app, &sug);
            }
        })
        .expect("prompt thread");
}

fn show(app: &AppHandle, sug: &Suggestion) {
    // окно могло быть выгружено после простоя — создаём заново (мы не в главном потоке).
    // Свежее окно само спросит prompt_current при загрузке.
    let Some((w, _created)) = crate::tray::ensure_window(app, "prompt") else {
        *PENDING.lock().unwrap() = None;
        return;
    };
    crate::tray::touch();
    crate::tray::set_asleep(&w, false);
    let _ = w.emit("prompt", sug);
    if let Ok(Some(m)) = w.primary_monitor() {
        let wa = m.work_area();
        let scale = w.scale_factor().unwrap_or(1.0);
        let size = w.outer_size().unwrap_or(tauri::PhysicalSize::new((380.0 * scale) as u32, (176.0 * scale) as u32));
        let gap = (12.0 * scale) as i32;
        let x = wa.position.x + wa.size.width as i32 - size.width as i32 - gap;
        let y = wa.position.y + wa.size.height as i32 - size.height as i32 - gap;
        let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
    }
    crate::tray::style_popup(&w, 12.0);
    crate::tray::show_no_activate(&w);
    crate::tray::style_popup(&w, 12.0);
}

pub fn current() -> Option<Suggestion> {
    PENDING.lock().unwrap().clone()
}

/// Ответ пользователя: vpn | skip | never. Возвращает текст для показа.
pub fn answer(app: &AppHandle, shared: &SharedRef, answer: &str) -> Result<String, String> {
    let sug = PENDING.lock().unwrap().take();
    if let Some(w) = app.get_webview_window("prompt") {
        crate::tray::hide_no_activate(&w);
        crate::tray::set_asleep(&w, true);
    }
    crate::tray::touch();
    let Some(sug) = sug else { return Ok(String::new()) };
    match answer {
        "vpn" => {
            let group = {
                let d = shared.settings().default_proxy_group;
                if d.is_empty() { shared.main_group() } else { d }
            };
            let r = rules::normalize("DOMAIN-SUFFIX", &sug.suffix, &group)?;
            let res = rules::mutate(&shared.api(), &format!("подсказка: {} → VPN", sug.suffix), |d| d.add(&r).map(|_| ()));
            if !res.ok {
                return Err(res.message);
            }
            Ok(format!("{} теперь через VPN — обновите страницу", sug.suffix))
        }
        "never" => {
            let mut s = shared.settings();
            if !s.ignored_hosts.contains(&sug.suffix) {
                s.ignored_hosts.push(sug.suffix.clone());
            }
            util::save_settings(&s)?;
            *shared.settings.lock().unwrap() = s;
            Ok(format!("Про {} больше не спрошу", sug.suffix))
        }
        _ => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn h(host: &str, route: &str, up: u64, down: u64, count: u32, proc_: &str) -> HostEntry {
        HostEntry {
            host: host.into(),
            suffix: rules::suggest_suffix(host),
            route: route.into(),
            group: route.into(),
            rule: "Match".into(),
            process: proc_.into(),
            count,
            up,
            down,
            first_seen: 100,
            last_seen: 110,
            active: 0,
            suspicious: true,
        }
    }
    #[test]
    fn candidates() {
        let none = HashSet::new();
        let rules = vec![Rule { kind: "DOMAIN-SUFFIX".into(), value: "covered.com".into(), action: "Proxy".into(), extra: String::new() }];
        assert!(is_candidate(&h("www.rutracker.org", "DIRECT", 517, 0, 3, "chrome.exe"), 115, &[], &none, &rules));
        assert!(!is_candidate(&h("www.rutracker.org", "NL-1", 517, 0, 3, "chrome.exe"), 115, &[], &none, &rules), "через VPN — не предлагать");
        assert!(!is_candidate(&h("vk.com", "DIRECT", 900, 50_000, 3, "chrome.exe"), 115, &[], &none, &rules), "загрузилось — значит работает");
        assert!(!is_candidate(&h("a.covered.com", "DIRECT", 517, 0, 3, "chrome.exe"), 115, &[], &none, &rules), "уже есть правило");
        assert!(!is_candidate(&h("x.msftconnecttest.com", "DIRECT", 517, 0, 3, "chrome.exe"), 115, &[], &none, &rules));
        assert!(!is_candidate(&h("site.org", "DIRECT", 517, 0, 3, "svchost.exe"), 115, &[], &none, &rules));
        assert!(!is_candidate(&h("site.org", "DIRECT", 517, 0, 3, "chrome.exe"), 115, &["site.org".into()], &none, &rules), "«не спрашивать»");
        assert!(!is_candidate(&h("site.org", "DIRECT", 517, 0, 3, "chrome.exe"), 500, &[], &none, &rules), "давно было");
    }
}
