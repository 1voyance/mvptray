//! Discord + discord-voice-proxy без ручной возни.
//!
//! discord-voice-proxy работает так: DWrite.dll, force-proxy.dll и proxy.txt лежат в
//! папке %LocalAppData%\Discord\app-X.X.XXXX. После каждого обновления Discord создаёт
//! новую папку app-…, а старую удаляет — и голос снова идёт мимо VPN.
//! Трей хранит свою копию файлов (tray-rs\discord-proxy), замечает новую версию
//! Discord и сам кладёт файлы в новую папку.

use crate::monitor::{SharedRef, Sink};
use crate::util;
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime};

const DLLS: &[&str] = &["DWrite.dll", "force-proxy.dll"];
const CFG: &str = "proxy.txt";
const FLAVORS: &[(&str, &str)] = &[("Discord", "Discord.exe"), ("DiscordPTB", "DiscordPTB.exe"), ("DiscordCanary", "DiscordCanary.exe")];
const REPO_API: &str = "https://api.github.com/repos/runetfreedom/discord-voice-proxy/releases/latest";

static LAST: Mutex<String> = Mutex::new(String::new());

fn set_last(s: impl Into<String>) {
    let s = s.into();
    crate::log_info!("discord: {s}");
    *LAST.lock().unwrap() = s;
}

fn local_appdata() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

pub fn master_dir() -> PathBuf {
    let d = util::data_dir().join("discord-proxy");
    let _ = fs::create_dir_all(&d);
    d
}

fn has_dlls(dir: &Path) -> bool {
    DLLS.iter().all(|f| dir.join(f).is_file())
}

/// Версия из имени папки app-1.0.9187 → [1, 0, 9187].
fn version_of(name: &str) -> Option<Vec<u32>> {
    let v = name.strip_prefix("app-")?;
    v.split('.').map(|p| p.parse().ok()).collect()
}

/// Папки app-* от новой к старой.
fn app_dirs(base: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(Vec<u32>, PathBuf)> = fs::read_dir(base)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    Some((version_of(&name)?, e.path()))
                })
                .filter(|(_, p)| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().map(|(_, p)| p).collect()
}

#[derive(Serialize, Clone)]
pub struct FlavorInfo {
    name: String,
    version: String,
    installed: bool,
    running: bool,
}

#[derive(Serialize)]
pub struct DiscordStatus {
    found: bool,
    flavors: Vec<FlavorInfo>,
    master: bool,
    master_dir: String,
    last: String,
    /// Включён режим TUN: весь трафик Discord и так идёт через Mihomo, файлы не нужны.
    tun: bool,
}

/// Включён ли режим TUN. Сначала спрашиваем работающий Mihomo (он знает точно),
/// если он выключен — читаем `tun: enable:` из config.yaml.
pub fn tun_enabled(api: &crate::api::Api) -> bool {
    if let Ok(cfg) = api.configs() {
        if let Some(b) = cfg.get("tun").and_then(|t| t.get("enable")).and_then(Value::as_bool) {
            return b;
        }
    }
    crate::rules::load_doc().map(|d| d.section_flag("tun", "enable")).unwrap_or(false)
}

pub const TUN_NOTE: &str = "Режим TUN включён — файлы для Discord не нужны, голос уже идёт через VPN";

pub fn status(tun: bool) -> DiscordStatus {
    let procs = util::processes();
    let mut flavors = Vec::new();
    if let Some(la) = local_appdata() {
        for (name, exe) in FLAVORS {
            let base = la.join(name);
            if let Some(latest) = app_dirs(&base).into_iter().next() {
                flavors.push(FlavorInfo {
                    name: name.to_string(),
                    version: latest.file_name().unwrap_or_default().to_string_lossy().trim_start_matches("app-").to_string(),
                    installed: has_dlls(&latest),
                    running: procs.iter().any(|(n, _)| n == &exe.to_lowercase()),
                });
            }
        }
    }
    DiscordStatus {
        found: !flavors.is_empty(),
        flavors,
        master: has_dlls(&master_dir()),
        master_dir: master_dir().display().to_string(),
        last: LAST.lock().unwrap().clone(),
        tun,
    }
}

/// Сохраняет себе файлы из любой папки Discord, где они уже лежат (новые важнее).
fn refresh_master() {
    let Some(la) = local_appdata() else { return };
    let master = master_dir();
    for (name, _) in FLAVORS {
        for dir in app_dirs(&la.join(name)) {
            if !has_dlls(&dir) {
                continue;
            }
            for f in DLLS.iter().chain(std::iter::once(&CFG)) {
                let src = dir.join(f);
                let dst = master.join(f);
                if src.is_file() && newer(&src, &dst) {
                    let _ = fs::copy(&src, &dst);
                }
            }
            return;
        }
    }
}

fn newer(src: &Path, dst: &Path) -> bool {
    let m = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    match (m(src), m(dst)) {
        (Some(a), Some(b)) => a > b || fs::metadata(src).map(|x| x.len()).ok() != fs::metadata(dst).map(|x| x.len()).ok(),
        (Some(_), None) => true,
        _ => false,
    }
}

fn default_proxy_txt() -> String {
    let port = crate::rules::load_doc()
        .ok()
        .and_then(|d| d.top_value("mixed-port").or_else(|| d.top_value("port")))
        .unwrap_or_else(|| "7890".into());
    format!("SOCKS5_PROXY_ADDRESS=127.0.0.1\r\nSOCKS5_PROXY_PORT={port}\r\n")
}

/// Кладёт файлы в самую новую папку каждой установленной версии Discord.
/// Возвращает список версий, куда что-то скопировано.
pub fn install_all() -> Result<Vec<String>, String> {
    refresh_master();
    let master = master_dir();
    if !has_dlls(&master) {
        return Err("нет файлов discord-voice-proxy — нажмите «Скачать»".into());
    }
    let la = local_appdata().ok_or("не найден %LOCALAPPDATA%")?;
    let mut done = Vec::new();
    for (name, _) in FLAVORS {
        let Some(latest) = app_dirs(&la.join(name)).into_iter().next() else { continue };
        // Discord ещё распаковывает обновление — подождём
        let fresh = fs::metadata(&latest)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map(|d| d < Duration::from_secs(20))
            .unwrap_or(false);
        if fresh && !latest.join(format!("{name}.exe")).is_file() {
            continue;
        }
        let mut copied = false;
        for f in DLLS {
            let dst = latest.join(f);
            if !dst.is_file() {
                fs::copy(master.join(f), &dst).map_err(|e| format!("не удалось скопировать {f}: {e}"))?;
                copied = true;
            }
        }
        let cfg = latest.join(CFG);
        if !cfg.is_file() {
            if master.join(CFG).is_file() {
                fs::copy(master.join(CFG), &cfg).map_err(|e| e.to_string())?;
            } else {
                fs::write(&cfg, default_proxy_txt()).map_err(|e| e.to_string())?;
            }
            copied = true;
        }
        if copied {
            done.push(format!("{name} {}", latest.file_name().unwrap_or_default().to_string_lossy()));
        }
    }
    Ok(done)
}

/// Перезапуск Discord. Запускаем через explorer.exe — так Discord стартует с обычными
/// правами, даже если трей запущен от администратора.
pub fn restart() -> Result<(), String> {
    let la = local_appdata().ok_or("не найден %LOCALAPPDATA%")?;
    let procs = util::processes();
    let mut any = false;
    for (name, exe) in FLAVORS {
        if !procs.iter().any(|(n, _)| n == &exe.to_lowercase()) {
            continue;
        }
        any = true;
        let _ = util::hidden_command("taskkill.exe").args(["/IM", exe, "/F"]).output();
        thread::sleep(Duration::from_millis(1500));
        let latest = app_dirs(&la.join(name)).into_iter().next().ok_or("не найдена папка Discord")?;
        util::hidden_command("explorer.exe").arg(latest.join(exe)).spawn().map_err(|e| e.to_string())?;
    }
    if !any {
        return Err("Discord не запущен".into());
    }
    set_last("Discord перезапущен");
    Ok(())
}

/// Скачивает свежие DWrite.dll и force-proxy.dll из релизов discord-voice-proxy.
pub fn download() -> Result<String, String> {
    let rel: Value = util::web_get(REPO_API, 20)?.into_json().map_err(|e| e.to_string())?;
    let tag = rel.get("tag_name").and_then(Value::as_str).unwrap_or("?").to_string();
    let assets = rel.get("assets").and_then(Value::as_array).cloned().unwrap_or_default();
    let master = master_dir();
    for f in DLLS {
        let url = assets
            .iter()
            .find(|a| a.get("name").and_then(Value::as_str).map(|n| n.eq_ignore_ascii_case(f)).unwrap_or(false))
            .and_then(|a| a.get("browser_download_url").and_then(Value::as_str))
            .ok_or(format!("в релизе {tag} нет файла {f}"))?;
        let mut buf = Vec::new();
        util::web_get(url, 60)?.into_reader().take(20 * 1024 * 1024).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        if buf.len() < 1024 || &buf[..2] != b"MZ" {
            return Err(format!("{f}: скачался не DLL-файл"));
        }
        util::write_atomic(&master.join(f), &buf)?;
    }
    if !master.join(CFG).is_file() {
        fs::write(master.join(CFG), default_proxy_txt()).map_err(|e| e.to_string())?;
    }
    set_last(format!("скачана версия {tag}"));
    Ok(tag)
}

/// Фоновое слежение за обновлениями Discord.
pub fn spawn(shared: SharedRef, sink: impl Sink) {
    thread::Builder::new()
        .name("discord".into())
        .spawn(move || {
          let mut tun_before: Option<bool> = None;
          loop {
            let s = shared.settings();
            // при TUN ничего в папку Discord не кладём: трафик и так идёт через Mihomo
            let tun = s.discord_watch && tun_enabled(&shared.api());
            if s.discord_watch && tun_before != Some(tun) {
                if tun {
                    set_last("режим TUN включён — файлы прокси не нужны, слежение на паузе");
                } else if tun_before == Some(true) {
                    set_last("режим TUN выключен — снова слежу за файлами прокси");
                }
                tun_before = Some(tun);
            }
            if s.discord_watch && !tun {
                match install_all() {
                    Ok(done) if !done.is_empty() => {
                        set_last(format!("файлы прокси перенесены: {}", done.join(", ")));
                        let running = status(false).flavors.iter().any(|f| f.running);
                        if running && s.discord_auto_restart {
                            let _ = restart();
                            sink.notify("Discord обновился", "Файлы прокси перенесены, Discord перезапущен — голос снова через VPN.");
                        } else if running {
                            sink.notify(
                                "Discord обновился",
                                "Файлы прокси перенесены. Перезапустите Discord (или нажмите кнопку в настройках трея), чтобы голос снова шёл через VPN.",
                            );
                        } else {
                            sink.notify("Discord обновился", "Файлы прокси уже на месте — при запуске голос пойдёт через VPN.");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        // «нет файлов» — нормальное состояние, если прокси не используется
                        if !e.starts_with("нет файлов") {
                            crate::log_warn!("discord: {e}");
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(60));
          }
        })
        .expect("discord thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_and_install() {
        assert_eq!(version_of("app-1.0.9187"), Some(vec![1, 0, 9187]));
        assert_eq!(version_of("packages"), None);
        let tmp = std::env::temp_dir().join(format!("dtest-{}", std::process::id()));
        let base = tmp.join("Discord");
        for v in ["app-1.0.9100", "app-1.0.9187", "app-1.0.95"] {
            fs::create_dir_all(base.join(v)).unwrap();
        }
        let dirs = app_dirs(&base);
        assert!(dirs[0].ends_with("app-1.0.9187"), "{dirs:?}");
        assert!(dirs[2].ends_with("app-1.0.95"));
        let _ = fs::remove_dir_all(tmp);
    }
}
