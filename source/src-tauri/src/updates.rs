//! Обновления в один клик: ядро Mihomo, базы GeoIP, наборы правил, панель Zashboard
//! и сам трей (новые версии берутся из GitHub Releases репозитория трея).

use crate::monitor::{SharedRef, Sink};
use crate::service;
use crate::util;
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

const CORE_API: &str = "https://api.github.com/repos/MetaCubeX/mihomo/releases/latest";
/// Репозиторий трея: отсюда берутся новые версии (GitHub Releases).
pub const TRAY_REPO: &str = "1voyance/mvptray";
/// Имя exe в релизе. Можно выложить и zip с этим exe внутри.
const TRAY_ASSET: &str = "mihomo-tray.exe";

#[derive(Serialize, Clone, Default)]
pub struct UpdState {
    pub core_current: String,
    pub core_latest: String,
    pub core_asset: String,
    pub checked_at: u64,
    pub busy: Option<String>,
    pub last: String,
    pub error: Option<String>,
    pub tray_current: String,
    pub tray_pending: bool,
    pub tray_latest: String,
    pub tray_asset: String,
    pub tray_notes: String,
    pub tray_page: String,
    pub tray_checked_at: u64,
    pub tray_error: Option<String>,
}

static STATE: Mutex<Option<UpdState>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut UpdState) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(|| UpdState { tray_current: env!("CARGO_PKG_VERSION").into(), ..Default::default() }))
}

fn pending_tray() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.with_file_name("mihomo-tray.new.exe");
    p.is_file().then_some(p)
}

pub fn view() -> UpdState {
    with(|s| {
        s.tray_pending = pending_tray().is_some();
        s.clone()
    })
}

/// v1.19.15 → (1,19,15). Альфы и прочее — None.
fn parse_ver(v: &str) -> Option<(u32, u32, u32)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let mut it = v.split(['.', '-']).map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

pub fn newer(latest: &str, current: &str) -> bool {
    match (parse_ver(latest), parse_ver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn core_version_local() -> String {
    let exe = util::mihomo_dir().join("mihomo.exe");
    util::hidden_command(&exe)
        .arg("-v")
        .output()
        .ok()
        .map(|o| util::lossy(&o.stdout))
        .and_then(|s| s.split_whitespace().find(|w| w.starts_with('v') && parse_ver(w).is_some()).map(String::from))
        .unwrap_or_default()
}

/// Проверка последней версии Mihomo на GitHub.
pub fn check(shared: &SharedRef) -> Result<(), String> {
    let cur = shared.api().version().ok().filter(|v| !v.is_empty()).unwrap_or_else(core_version_local);
    let rel: Value = util::web_get(CORE_API, 20)?.into_json().map_err(|e| e.to_string())?;
    let tag = rel.get("tag_name").and_then(Value::as_str).unwrap_or("").to_string();
    let assets = rel.get("assets").and_then(Value::as_array).cloned().unwrap_or_default();
    let names: Vec<(String, String)> = assets
        .iter()
        .filter_map(|a| Some((a.get("name")?.as_str()?.to_string(), a.get("browser_download_url")?.as_str()?.to_string())))
        .collect();
    // обычная сборка для x64; запасной вариант — compatible (для старых процессоров)
    let want = format!("mihomo-windows-amd64-{tag}.zip");
    let asset = names
        .iter()
        .find(|(n, _)| *n == want)
        .or_else(|| names.iter().find(|(n, _)| n.starts_with("mihomo-windows-amd64-compatible") && n.ends_with(".zip")))
        .map(|(_, u)| u.clone())
        .unwrap_or_default();
    with(|s| {
        s.core_current = cur;
        s.core_latest = tag;
        s.core_asset = asset;
        s.checked_at = util::now_secs();
        s.error = None;
    });
    Ok(())
}

fn begin(what: &str) -> Result<(), String> {
    with(|s| {
        if let Some(b) = &s.busy {
            return Err(format!("уже идёт: {b}"));
        }
        s.busy = Some(what.into());
        s.error = None;
        Ok(())
    })
}

fn finish(res: &Result<String, String>) {
    with(|s| {
        s.busy = None;
        match res {
            Ok(m) => s.last = m.clone(),
            Err(e) => s.error = Some(e.clone()),
        }
    });
    match res {
        Ok(m) => crate::log_info!("обновление: {m}"),
        Err(e) => crate::log_err!("обновление: {e}"),
    }
}

/// Обновление ядра: скачать → остановить службу → заменить exe → запустить →
/// проверить версию. Если новая версия не поднялась — вернуть старую.
pub fn update_core(shared: &SharedRef) -> Result<String, String> {
    begin("обновление Mihomo")?;
    let res = (|| {
        check(shared)?;
        let (cur, tag, url) = with(|s| (s.core_current.clone(), s.core_latest.clone(), s.core_asset.clone()));
        if url.is_empty() {
            return Err("не найден файл сборки для Windows в релизе".into());
        }
        if !newer(&tag, &cur) {
            return Ok(format!("Mihomo уже последней версии ({cur})"));
        }
        // скачиваем, пока VPN ещё работает
        let mut zipbuf = Vec::new();
        util::web_get(&url, 120)?.into_reader().take(200 * 1024 * 1024).read_to_end(&mut zipbuf).map_err(|e| format!("загрузка: {e}"))?;
        let mut za = zip::ZipArchive::new(std::io::Cursor::new(zipbuf)).map_err(|e| format!("архив: {e}"))?;
        let mut exe = Vec::new();
        for i in 0..za.len() {
            let mut f = za.by_index(i).map_err(|e| e.to_string())?;
            if f.name().to_lowercase().ends_with(".exe") {
                f.read_to_end(&mut exe).map_err(|e| e.to_string())?;
                break;
            }
        }
        if exe.len() < 1024 * 1024 || &exe[..2] != b"MZ" {
            return Err("в архиве нет mihomo.exe".into());
        }
        let dir = util::mihomo_dir();
        let target = dir.join("mihomo.exe");
        let bak = dir.join("mihomo.exe.bak");
        let new = dir.join("mihomo.exe.new");
        fs::write(&new, &exe).map_err(|e| format!("запись: {e}"))?;

        shared.user_stopped.store(true, std::sync::atomic::Ordering::Relaxed);
        service::control("stop")?;
        thread::sleep(Duration::from_secs(2));
        let _ = fs::remove_file(&bak);
        fs::rename(&target, &bak).map_err(|e| format!("не удалось убрать старый mihomo.exe: {e}"))?;
        if let Err(e) = fs::rename(&new, &target) {
            let _ = fs::rename(&bak, &target);
            let _ = service::control("start");
            return Err(format!("не удалось поставить новый exe: {e}"));
        }
        service::control("start")?;
        // ждём, пока поднимется новая версия
        for _ in 0..20 {
            thread::sleep(Duration::from_secs(1));
            if let Ok(v) = shared.api().version() {
                if parse_ver(&v) == parse_ver(&tag) {
                    with(|s| s.core_current = v.clone());
                    return Ok(format!("Mihomo обновлён: {cur} → {v}"));
                }
            }
        }
        // откат
        let _ = service::control("stop");
        thread::sleep(Duration::from_secs(2));
        let _ = fs::remove_file(&target);
        let _ = fs::rename(&bak, &target);
        let _ = service::control("start");
        Err(format!("новая версия {tag} не запустилась — возвращена {cur}"))
    })();
    shared.poke.store(true, std::sync::atomic::Ordering::Relaxed);
    finish(&res);
    res
}

/// Базы GeoIP/GeoSite, наборы правил и панель — через API Mihomo, без перезапуска.
pub fn update_data(shared: &SharedRef) -> Result<String, String> {
    begin("обновление баз и правил")?;
    let res = (|| {
        let api = shared.api();
        let mut parts = Vec::new();
        // GeoIP: новые версии — /upgrade/geo, старые — /configs/geo
        let geo = api.post_slow("/upgrade/geo").or_else(|_| api.post_slow("/configs/geo"));
        parts.push(match geo {
            Ok(()) => "базы GeoIP обновлены".to_string(),
            Err(e) => format!("GeoIP: {e}"),
        });
        // наборы правил
        let provs = api.get("/providers/rules").map_err(String::from)?;
        let names: Vec<String> = provs
            .get("providers")
            .and_then(Value::as_object)
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let (mut ok, mut bad) = (0, Vec::new());
        for n in &names {
            match api.put_slow(&format!("/providers/rules/{}", util::url_encode(n))) {
                Ok(()) => ok += 1,
                Err(_) => bad.push(n.clone()),
            }
        }
        parts.push(format!("наборы правил: {ok} из {}", names.len()));
        if !bad.is_empty() {
            parts.push(format!("не обновились: {}", bad.join(", ")));
        }
        if api.post_slow("/upgrade/ui").is_ok() {
            parts.push("панель Zashboard обновлена".into());
        }
        Ok(parts.join("; "))
    })();
    finish(&res);
    res
}

/// Проверка новой версии трея в GitHub Releases репозитория [`TRAY_REPO`].
pub fn check_tray() -> Result<(), String> {
    let url = format!("https://api.github.com/repos/{TRAY_REPO}/releases/latest");
    let res = (|| {
        let rel: Value = match util::web_get(&url, 20) {
            Ok(r) => r.into_json().map_err(|e| e.to_string())?,
            Err(e) if e.starts_with("HTTP 404") => return Err("в репозитории ещё нет релизов".to_string()),
            Err(e) => return Err(e),
        };
        let tag = rel.get("tag_name").and_then(Value::as_str).unwrap_or("").to_string();
        let assets = rel.get("assets").and_then(Value::as_array).cloned().unwrap_or_default();
        let names: Vec<(String, String)> = assets
            .iter()
            .filter_map(|a| Some((a.get("name")?.as_str()?.to_lowercase(), a.get("browser_download_url")?.as_str()?.to_string())))
            .collect();
        // сначала голый exe, потом любой zip с «mihomo-tray» в имени
        let asset = names
            .iter()
            .find(|(n, _)| n == TRAY_ASSET)
            .or_else(|| names.iter().find(|(n, _)| n.starts_with("mihomo-tray") && n.ends_with(".exe")))
            .or_else(|| names.iter().find(|(n, _)| n.starts_with("mihomo-tray") && n.ends_with(".zip")))
            .map(|(_, u)| u.clone())
            .unwrap_or_default();
        let notes: String = rel.get("body").and_then(Value::as_str).unwrap_or("").chars().take(1500).collect();
        let page = rel.get("html_url").and_then(Value::as_str).unwrap_or("").to_string();
        Ok((tag, asset, notes, page))
    })();
    with(|s| {
        s.tray_checked_at = util::now_secs();
        match &res {
            Ok((tag, asset, notes, page)) => {
                s.tray_latest = tag.clone();
                s.tray_asset = asset.clone();
                s.tray_notes = notes.clone();
                s.tray_page = page.clone();
                s.tray_error = None;
            }
            Err(e) => s.tray_error = Some(e.clone()),
        }
    });
    res.map(|_| ())
}

/// Есть ли на GitHub версия трея новее текущей.
pub fn tray_update_available() -> bool {
    with(|s| !s.tray_asset.is_empty() && newer(&s.tray_latest, &s.tray_current))
}

/// Скачивает новую версию трея в mihomo-tray.new.exe рядом с текущим exe.
pub fn download_tray() -> Result<String, String> {
    begin("загрузка новой версии трея")?;
    let res = (|| {
        check_tray()?;
        let (cur, tag, url) = with(|s| (s.tray_current.clone(), s.tray_latest.clone(), s.tray_asset.clone()));
        if !newer(&tag, &cur) {
            return Ok(format!("Трей уже последней версии ({cur})"));
        }
        if url.is_empty() {
            return Err(format!("в релизе {tag} нет файла {TRAY_ASSET}"));
        }
        let mut buf = Vec::new();
        util::web_get(&url, 120)?
            .into_reader()
            .take(100 * 1024 * 1024)
            .read_to_end(&mut buf)
            .map_err(|e| format!("загрузка: {e}"))?;
        let exe = if url.to_lowercase().ends_with(".zip") {
            let mut za = zip::ZipArchive::new(std::io::Cursor::new(buf)).map_err(|e| format!("архив: {e}"))?;
            let mut out = Vec::new();
            for i in 0..za.len() {
                let mut f = za.by_index(i).map_err(|e| e.to_string())?;
                let n = f.name().to_lowercase();
                if n.ends_with(TRAY_ASSET) || (n.contains("mihomo-tray") && n.ends_with(".exe")) {
                    f.read_to_end(&mut out).map_err(|e| e.to_string())?;
                    break;
                }
            }
            out
        } else {
            buf
        };
        if exe.len() < 512 * 1024 || &exe[..2] != b"MZ" {
            return Err("скачанный файл не похож на программу Windows".into());
        }
        let me = std::env::current_exe().map_err(|e| e.to_string())?;
        let part = me.with_file_name("mihomo-tray.new.exe.part");
        let target = me.with_file_name("mihomo-tray.new.exe");
        fs::write(&part, &exe).map_err(|e| format!("запись: {e}"))?;
        let _ = fs::remove_file(&target);
        fs::rename(&part, &target).map_err(|e| format!("запись: {e}"))?;
        Ok(format!("Скачана версия трея {tag}"))
    })();
    finish(&res);
    res
}

/// Обновление трея в один клик: скачать (если ещё не скачано) и установить с перезапуском.
pub fn update_tray() -> Result<(), String> {
    if pending_tray().is_none() {
        download_tray()?;
    }
    if pending_tray().is_none() {
        return Err(with(|s| s.last.clone()));
    }
    apply_tray_update()
}

/// Установка новой версии трея (mihomo-tray.new.exe рядом с exe): трей закрывается,
/// маленький скрипт заменяет файл и запускает трей снова.
pub fn apply_tray_update() -> Result<(), String> {
    let new = pending_tray().ok_or("нет файла mihomo-tray.new.exe")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let script = format!(
        "ping -n 3 127.0.0.1 >nul & copy /y \"{exe}\" \"{prev}\" >nul & move /y \"{new}\" \"{exe}\" >nul & (schtasks /Run /TN MihomoTrayRs >nul 2>&1 || start \"\" \"{exe}\")",
        new = new.display(),
        exe = exe.display(),
        prev = exe.with_file_name("mihomo-tray.prev.exe").display()
    );
    util::hidden_command("cmd.exe").args(["/c", &script]).spawn().map_err(|e| e.to_string())?;
    crate::log_info!("установка обновления трея, перезапуск");
    Ok(())
}

/// Раз в сутки — проверка новых версий ядра и трея; появление mihomo-tray.new.exe.
pub fn spawn(shared: SharedRef, sink: impl Sink) {
    thread::Builder::new()
        .name("updates".into())
        .spawn(move || {
            thread::sleep(Duration::from_secs(30));
            let mut told_core = String::new();
            let mut told_tray = false;
            let mut told_tray_new = String::new();
            loop {
                let s = shared.settings();
                let stale = util::now_secs().saturating_sub(with(|u| u.checked_at)) > 24 * 3600;
                if s.auto_check_updates && stale && shared.status.lock().map(|x| x.api_ok).unwrap_or(false) {
                    match check(&shared) {
                        Ok(()) => {
                            let (cur, lat) = with(|u| (u.core_current.clone(), u.core_latest.clone()));
                            if newer(&lat, &cur) && told_core != lat {
                                told_core = lat.clone();
                                sink.notify("Вышла новая версия Mihomo", &format!("{cur} → {lat}. Обновить можно в Настройках трея."));
                            }
                        }
                        Err(e) => crate::log_warn!("проверка обновлений: {e}"),
                    }
                }
                // новая версия самого трея на GitHub — проверка при запуске и раз в сутки
                let tray_stale = util::now_secs().saturating_sub(with(|u| u.tray_checked_at)) > 24 * 3600;
                if s.auto_check_updates && tray_stale {
                    match check_tray() {
                        Ok(()) if tray_update_available() => {
                            let (cur, lat) = with(|u| (u.tray_current.clone(), u.tray_latest.clone()));
                            if told_tray_new != lat && pending_tray().is_none() {
                                told_tray_new = lat.clone();
                                sink.notify("Вышла новая версия трея", &format!("{cur} → {lat}. Обновить: Настройки → Обновления."));
                            }
                        }
                        Ok(()) => {}
                        Err(e) => crate::log_warn!("проверка обновлений трея: {e}"),
                    }
                }
                let pending = pending_tray().is_some();
                if pending && !told_tray {
                    sink.notify("Готово обновление трея", "Нажмите «Установить» в Настройках → Обновления.");
                }
                told_tray = pending;
                thread::sleep(Duration::from_secs(60));
            }
        })
        .expect("updates thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert!(newer("v1.19.16", "v1.19.15"));
        assert!(newer("v1.20.0", "1.19.99"));
        assert!(!newer("v1.19.15", "v1.19.15"));
        assert!(!newer("alpha-abc", "v1.19.15"));
        assert_eq!(parse_ver("Mihomo"), None);
    }
}
