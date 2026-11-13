//! Общие утилиты: пути, лог в файл, настройки, время, запуск процессов без окна.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- пути

static MIHOMO_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Папка Mihomo (там лежат config.yaml, mihomo-svc.exe).
/// Порядок поиска: переменная MIHOMO_DIR → папка exe и её родители → C:\mihomo.
pub fn mihomo_dir() -> &'static Path {
    MIHOMO_DIR.get_or_init(|| {
        if let Ok(v) = std::env::var("MIHOMO_DIR") {
            let p = PathBuf::from(v);
            if p.join("config.yaml").exists() {
                return p;
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            let mut cur = exe.parent().map(Path::to_path_buf);
            for _ in 0..3 {
                match cur {
                    Some(ref d) if d.join("config.yaml").exists() => return d.clone(),
                    Some(ref d) => cur = d.parent().map(Path::to_path_buf),
                    None => break,
                }
            }
        }
        PathBuf::from(r"C:\mihomo")
    })
}

pub fn config_path() -> PathBuf {
    mihomo_dir().join("config.yaml")
}

/// Своя папка трея: настройки, логи, бэкапы конфига.
pub fn data_dir() -> PathBuf {
    let d = mihomo_dir().join("tray-rs");
    let _ = fs::create_dir_all(&d);
    d
}

pub fn backups_dir() -> PathBuf {
    let d = data_dir().join("backups");
    let _ = fs::create_dir_all(&d);
    d
}

pub fn logs_dir() -> PathBuf {
    let d = data_dir().join("logs");
    let _ = fs::create_dir_all(&d);
    d
}

// ---------------------------------------------------------------- время

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Дата по количеству дней от 1970-01-01 (алгоритм Howard Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Локальное время Москвы не вычисляем (нет tz-базы) — пишем UTC с пометкой.
pub fn timestamp(compact: bool) -> String {
    let s = now_secs() as i64;
    let (y, m, d) = civil_from_days(s.div_euclid(86_400));
    let t = s.rem_euclid(86_400);
    let (hh, mm, ss) = (t / 3600, (t % 3600) / 60, t % 60);
    if compact {
        format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}Z")
    }
}

// ---------------------------------------------------------------- лог

static LOG_LOCK: Mutex<()> = Mutex::new(());
const LOG_MAX: u64 = 1024 * 1024;

/// Пишет строку в tray-rs\logs\tray.log; при превышении 1 МБ файл переименовывается в tray.old.log.
pub fn log(level: &str, msg: &str) {
    let _g = LOG_LOCK.lock();
    let path = logs_dir().join("tray.log");
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > LOG_MAX {
            let _ = fs::rename(&path, logs_dir().join("tray.old.log"));
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} [{}] {}", timestamp(false), level, msg);
    }
    #[cfg(debug_assertions)]
    eprintln!("[{level}] {msg}");
}

#[macro_export]
macro_rules! log_info { ($($a:tt)*) => { $crate::util::log("INFO", &format!($($a)*)) } }
#[macro_export]
macro_rules! log_warn { ($($a:tt)*) => { $crate::util::log("WARN", &format!($($a)*)) } }
#[macro_export]
macro_rules! log_err { ($($a:tt)*) => { $crate::util::log("ERROR", &format!($($a)*)) } }

// ---------------------------------------------------------------- процессы

/// Command, который на Windows не показывает консольное окно.
pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Вывод консольных утилит Windows приходит в OEM-кодировке; нас интересуют только ASCII-слова.
pub fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Открыть файл, папку или URL программой по умолчанию.
pub fn open_path(target: &str) {
    #[cfg(windows)]
    {
        let _ = hidden_command("explorer.exe").arg(target).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("xdg-open").arg(target).spawn();
    }
}

// ---------------------------------------------------------------- настройки

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Уведомление, если служба упала или поднялась.
    pub notify_service: bool,
    /// Уведомление, если сменилась активная нода.
    pub notify_node: bool,
    /// Уведомление, если обновилась подписка.
    pub notify_subscription: bool,
    /// Уведомление, если нода перестала отвечать.
    pub notify_dead_node: bool,
    /// Автоматически поднимать службу, если она упала сама (не по кнопке).
    pub auto_restart: bool,
    /// Главная группа, по которой показываем ноду и пинг. Пусто = первая из config.yaml.
    pub main_group: String,
    /// Группа по умолчанию для новых правил «через VPN». Пусто = главная группа.
    pub default_proxy_group: String,
    /// Умный автовыбор ноды (пинг + стабильность + потери + скорость).
    pub smart_auto: bool,
    /// Группа, которой управляет умный выбор (обычно Auto).
    pub smart_group: String,
    /// Закреплённая пользователем нода (пусто — выбирать автоматически).
    pub smart_pinned: String,
    /// Тест скорости нод раз в 6 часов, когда интернет не нагружен.
    pub smart_speed_auto: bool,
    /// Следить за обновлениями Discord и переносить файлы discord-voice-proxy.
    pub discord_watch: bool,
    /// После переноса файлов перезапускать Discord самому.
    pub discord_auto_restart: bool,
    /// Предлагать отправить в VPN сайты, которые не открываются напрямую.
    pub suggest_blocked: bool,
    /// Домены, по которым больше не спрашивать.
    pub ignored_hosts: Vec<String>,
    /// Игровой режим при запуске игр.
    pub game_mode: bool,
    /// Считать OBS игрой (стрим тоже не должен прерываться).
    pub game_mode_obs: bool,
    /// Дополнительные процессы-игры (имя .exe).
    pub game_processes: Vec<String>,
    /// Раз в сутки проверять новую версию Mihomo.
    pub auto_check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify_service: true,
            notify_node: true,
            notify_subscription: true,
            notify_dead_node: true,
            auto_restart: false,
            main_group: String::new(),
            default_proxy_group: String::new(),
            smart_auto: true,
            smart_group: "Auto".into(),
            smart_pinned: String::new(),
            smart_speed_auto: true,
            discord_watch: true,
            discord_auto_restart: false,
            suggest_blocked: true,
            ignored_hosts: Vec::new(),
            game_mode: true,
            game_mode_obs: true,
            game_processes: Vec::new(),
            auto_check_updates: true,
        }
    }
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn load_settings() -> Settings {
    match fs::read_to_string(settings_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log("WARN", &format!("settings.json повреждён ({e}), используются значения по умолчанию"));
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

pub fn save_settings(s: &Settings) -> Result<(), String> {
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    write_atomic(&settings_path(), json.as_bytes())
}

/// Запись через временный файл + rename, чтобы при сбое не остался обрезанный файл.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp-write");
    fs::write(&tmp, data).map_err(|e| format!("не удалось записать {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("не удалось заменить {}: {e}", path.display())
    })
}

// ---------------------------------------------------------------- интернет

/// HTTP(S)-клиент для загрузок из интернета (GitHub). via_mihomo — через mixed-port
/// Mihomo (по правилам, т.е. GitHub пойдёт через VPN), иначе напрямую.
pub fn web_agent(via_mihomo: bool, timeout_s: u64) -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(8))
        .timeout_read(std::time::Duration::from_secs(timeout_s))
        .user_agent(concat!("mihomo-tray/", env!("CARGO_PKG_VERSION")));
    if let Ok(tls) = native_tls::TlsConnector::new() {
        b = b.tls_connector(std::sync::Arc::new(tls));
    }
    if via_mihomo {
        let port = crate::rules::load_doc()
            .ok()
            .and_then(|d| d.top_value("mixed-port").or_else(|| d.top_value("port")))
            .and_then(|p| p.parse::<u16>().ok());
        if let Some(port) = port {
            if let Ok(p) = ureq::Proxy::new(format!("http://127.0.0.1:{port}")) {
                b = b.proxy(p);
            }
        }
    }
    b.build()
}

/// GET с попыткой сначала через Mihomo, потом напрямую.
pub fn web_get(url: &str, timeout_s: u64) -> Result<ureq::Response, String> {
    let mut last = String::new();
    for via in [true, false] {
        match web_agent(via, timeout_s).get(url).call() {
            Ok(r) => return Ok(r),
            Err(ureq::Error::Status(c, _)) => return Err(format!("HTTP {c}: {url}")),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// Снимок запущенных процессов: (имя в нижнем регистре, путь в нижнем регистре).
pub fn processes() -> Vec<(String, String)> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::OnlyIfNotSet));
    sys.processes()
        .values()
        .map(|p| {
            (
                p.name().to_string_lossy().to_lowercase(),
                p.exe().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default(),
            )
        })
        .collect()
}

/// Процентное кодирование для имён групп/нод в URL (кириллица, пробелы, эмодзи).
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn civil() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_768), (2026, 11, 11));
    }
    #[test]
    fn enc() {
        assert_eq!(url_encode("Proxy"), "Proxy");
        assert_eq!(url_encode("a b/ю"), "a%20b%2F%D1%8E");
    }
}
