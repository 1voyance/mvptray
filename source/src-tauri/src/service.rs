//! Управление службой Windows «mihomo» (WinSW: mihomo-svc.exe).

use crate::util::{self, hidden_command, lossy};
use serde::Serialize;
use std::sync::OnceLock;

pub const SERVICE_NAME: &str = "mihomo";

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SvcState {
    Running,
    Stopped,
    Pending,
    Missing,
    Unknown,
}

/// Состояние службы через `sc query`. Названия состояний (RUNNING и т.п.)
/// в выводе sc не локализуются, поэтому разбор работает и на русской Windows.
pub fn state() -> SvcState {
    let out = match hidden_command("sc.exe").args(["query", SERVICE_NAME]).output() {
        Ok(o) => o,
        Err(_) => return SvcState::Unknown,
    };
    let text = lossy(&out.stdout) + &lossy(&out.stderr);
    if text.contains("1060") {
        return SvcState::Missing;
    }
    if text.contains("RUNNING") {
        SvcState::Running
    } else if text.contains("PENDING") {
        SvcState::Pending
    } else if text.contains("STOPPED") {
        SvcState::Stopped
    } else {
        SvcState::Unknown
    }
}

/// Запущен ли трей с правами администратора. Проверяется один раз.
pub fn is_elevated() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| {
        #[cfg(windows)]
        {
            hidden_command("net.exe").arg("session").output().map(|o| o.status.success()).unwrap_or(false)
        }
        #[cfg(not(windows))]
        {
            false
        }
    })
}

/// Процесс ядра (mihomo или чужой клиент на его основе).
#[derive(Clone, Debug, Serialize)]
pub struct CoreProc {
    pub pid: u32,
    pub name: String,
    pub exe: String,
    pub parent: String,
    /// Наш mihomo.exe из папки Mihomo (а не Clash Verge, FlClash и т.п.)
    pub ours: bool,
}

pub fn core_processes() -> Vec<CoreProc> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet));
    let dir = util::mihomo_dir().display().to_string().to_lowercase();
    let mut out = Vec::new();
    for (pid, p) in sys.processes() {
        let name = p.name().to_string_lossy().to_lowercase();
        if !(name.contains("mihomo") || name.contains("clash")) {
            continue;
        }
        if name.starts_with("mihomo-svc") || name.starts_with("mihomo-tray") {
            continue;
        }
        let exe = p.exe().map(|e| e.display().to_string()).unwrap_or_default();
        let parent = p
            .parent()
            .and_then(|pp| sys.process(pp))
            .map(|pp| pp.name().to_string_lossy().into_owned())
            .unwrap_or_default();
        // путь у процесса, запущенного от администратора, может быть недоступен — тогда судим по имени
        let ours = name == "mihomo.exe" && (exe.is_empty() || exe.to_lowercase().starts_with(&dir));
        out.push(CoreProc { pid: pid.as_u32(), name: p.name().to_string_lossy().into_owned(), exe, parent, ours });
    }
    out
}

fn api_alive() -> bool {
    crate::api::Api::from_config().version().is_ok()
}

/// PowerShell-скрипт от имени администратора (один запрос UAC на всё действие).
fn run_elevated_ps(script: &str) -> Result<(), String> {
    use base64::Engine;
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    let b64 = base64::engine::general_purpose::STANDARD.encode(utf16);
    let outer = format!(
        "$p = Start-Process -FilePath 'powershell.exe' -ArgumentList '-NoProfile -WindowStyle Hidden -EncodedCommand {b64}' -Verb RunAs -WindowStyle Hidden -PassThru -Wait; exit $p.ExitCode"
    );
    let out = hidden_command("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &outer])
        .output()
        .map_err(|e| format!("не удалось запустить PowerShell: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let msg = lossy(&out.stderr);
    if msg.contains("canceled") || msg.contains("отмен") {
        Err("запрос прав администратора отклонён".into())
    } else {
        crate::log_err!("UAC: {}", msg.trim());
        Err("не удалось выполнить действие (подробности в логе)".into())
    }
}

fn svc_cmd(action: &str) -> Result<(), String> {
    let dir = util::mihomo_dir();
    let svc = dir.join("mihomo-svc.exe");
    if !svc.exists() {
        return Err(format!("не найден {}", svc.display()));
    }
    if is_elevated() {
        let out = hidden_command(&svc)
            .arg(action)
            .current_dir(dir)
            .output()
            .map_err(|e| format!("не удалось запустить mihomo-svc.exe: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let msg = (lossy(&out.stdout) + &lossy(&out.stderr)).trim().to_string();
        crate::log_err!("mihomo-svc {action}: {msg}");
        return Err(if msg.is_empty() { format!("код выхода {:?}", out.status.code()) } else { msg });
    }
    let q = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
    run_elevated_ps(&format!("Set-Location '{}'; & '{}' {action}; exit $LASTEXITCODE", q(dir), q(&svc)))
}

/// Выключение: останавливаем службу И добиваем mihomo.exe, если он запущен не службой
/// (например, вручную из консоли) — иначе VPN «сам включается обратно».
fn stop_all() -> Result<(), String> {
    let svc_running = matches!(state(), SvcState::Running | SvcState::Pending);
    let procs = core_processes();
    let ours: Vec<u32> = procs.iter().filter(|p| p.ours).map(|p| p.pid).collect();
    let others: Vec<&CoreProc> = procs.iter().filter(|p| !p.ours).collect();
    crate::log_info!(
        "выключение: служба={svc_running}, свои процессы={ours:?}, чужие={:?}",
        others.iter().map(|p| format!("{} ({})", p.name, p.exe)).collect::<Vec<_>>()
    );

    if is_elevated() {
        if svc_running {
            svc_cmd("stop")?;
        }
        std::thread::sleep(std::time::Duration::from_millis(800));
        for pid in core_processes().iter().filter(|p| p.ours).map(|p| p.pid) {
            let _ = hidden_command("taskkill.exe").args(["/PID", &pid.to_string(), "/F"]).output();
        }
    } else {
        // без прав: сначала пробуем сами (если mihomo запущен без админа — получится)
        if !svc_running && !ours.is_empty() {
            for pid in &ours {
                let _ = hidden_command("taskkill.exe").args(["/PID", &pid.to_string(), "/F"]).output();
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let left: Vec<u32> = core_processes().iter().filter(|p| p.ours).map(|p| p.pid).collect();
        if svc_running || !left.is_empty() {
            let dir = util::mihomo_dir();
            let svc = dir.join("mihomo-svc.exe").display().to_string().replace('\'', "''");
            let mut script = String::from("$ErrorActionPreference = 'SilentlyContinue'\n");
            if svc_running {
                script.push_str(&format!("& '{svc}' stop | Out-Null\nStart-Sleep -Milliseconds 800\n"));
            }
            script.push_str("Get-Process -Name mihomo | Where-Object { -not $_.Path -or $_.Path -like '");
            script.push_str(&dir.display().to_string().replace('\'', "''"));
            script.push_str("\\*' } | Stop-Process -Force\nexit 0\n");
            run_elevated_ps(&script)?;
        }
    }

    // проверка: ядро действительно остановлено?
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !api_alive() {
            return Ok(());
        }
    }
    let others = core_processes().into_iter().filter(|p| !p.ours).collect::<Vec<_>>();
    if let Some(o) = others.first() {
        return Err(format!(
            "VPN держит другая программа: {}{}. Выключите её там или закройте.",
            o.name,
            if o.exe.is_empty() { String::new() } else { format!(" ({})", o.exe) }
        ));
    }
    Err("Mihomo всё ещё отвечает — не удалось его остановить (подробности в логе)".into())
}

/// start | stop | restart.
/// С правами администратора — напрямую, без окна UAC. Без прав — один запрос UAC.
pub fn control(action: &str) -> Result<(), String> {
    crate::log_info!("служба: {action} (admin={})", is_elevated());
    match action {
        "stop" => stop_all(),
        "start" => {
            if api_alive() {
                return Ok(()); // уже работает
            }
            svc_cmd("start")
        }
        "restart" => {
            // если ядро запущено не службой — после рестарта пусть работает как служба
            if core_processes().iter().any(|p| p.ours) && !matches!(state(), SvcState::Running) {
                stop_all()?;
                return svc_cmd("start");
            }
            svc_cmd("restart")
        }
        _ => Err(format!("неизвестное действие {action}")),
    }
}

/// Список запущенных процессов (уникальные имена .exe) для окна «Добавить процесс».
pub fn running_processes() -> Vec<String> {
    let out = match hidden_command("tasklist.exe").args(["/fo", "csv", "/nh"]).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('"')?.split('"').next().map(str::to_string))
        .filter(|n| n.to_lowercase().ends_with(".exe"))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}
