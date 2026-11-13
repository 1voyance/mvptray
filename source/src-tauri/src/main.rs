// В релизе без консольного окна.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod util;
mod api;
mod monitor;
mod rules;
mod discord;
mod games;
mod prompt;
mod service;
mod smart;
mod subs;
mod updates;
mod tray;

use monitor::{HostEntry, Shared, SharedRef, Status};
use rules::{ApplyResult, ImportLine, Rule};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::thread;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use util::Settings;

type S<'a> = State<'a, SharedRef>;

// ================================================================ статус и служба

#[tauri::command]
fn get_status(s: S) -> Status {
    s.status.lock().unwrap().clone()
}

/// Запуск/остановка/перезапуск службы. Выполняется в фоне; итог приходит событием "toast".
#[tauri::command]
fn service_action(app: AppHandle, s: S, action: String) -> Result<(), String> {
    {
        let mut busy = s.busy.lock().unwrap();
        if busy.is_some() {
            return Err("Подождите, предыдущее действие ещё выполняется".into());
        }
        *busy = Some(action.clone());
    }
    if action == "stop" {
        s.user_stopped.store(true, Ordering::Relaxed);
    }
    s.poke.store(true, Ordering::Relaxed);
    let shared = s.inner().clone();
    thread::spawn(move || {
        let res = service::control(&action);
        *shared.busy.lock().unwrap() = None;
        let (kind, text) = match (&res, action.as_str()) {
            (Ok(()), "start") => ("ok", "VPN включён".to_string()),
            (Ok(()), "stop") => ("ok", "VPN выключен".to_string()),
            (Ok(()), _) => ("ok", "VPN перезапущен".to_string()),
            (Err(e), _) => {
                if action == "stop" {
                    shared.user_stopped.store(false, Ordering::Relaxed);
                }
                ("error", e.clone())
            }
        };
        let _ = app.emit("toast", Toast { kind: kind.into(), text });
        shared.poke.store(true, Ordering::Relaxed);
    });
    Ok(())
}

#[derive(Serialize, Clone)]
struct Toast {
    kind: String,
    text: String,
}

#[tauri::command]
fn set_mode(s: S, mode: String) -> Result<(), String> {
    if !matches!(mode.as_str(), "rule" | "global" | "direct") {
        return Err("неизвестный режим".into());
    }
    s.api().set_mode(&mode)?;
    log_info!("режим: {mode}");
    s.poke.store(true, Ordering::Relaxed);
    Ok(())
}

// ================================================================ группы и ноды

#[derive(Serialize)]
struct NodeInfo {
    name: String,
    kind: String,
    delay: Option<u32>,
}

#[derive(Serialize)]
struct GroupInfo {
    name: String,
    kind: String,
    now: String,
    selectable: bool,
    nodes: Vec<NodeInfo>,
}

#[tauri::command]
fn get_groups(s: S) -> Result<Vec<GroupInfo>, String> {
    let proxies = s.api().proxies()?;
    // порядок групп как в конфиге — его отдаёт GLOBAL.all
    let order: Vec<String> = proxies
        .get("GLOBAL")
        .and_then(|g| g.get("all"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for name in order {
        let Some(p) = proxies.get(&name) else { continue };
        let Some(all) = p.get("all").and_then(Value::as_array) else { continue };
        if p.get("hidden").and_then(Value::as_bool) == Some(true) || name == rules::PROBE_GROUP {
            continue;
        }
        let kind = p.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let nodes = all
            .iter()
            .filter_map(Value::as_str)
            .map(|n| {
                let np = proxies.get(n);
                NodeInfo {
                    name: n.to_string(),
                    kind: np.and_then(|x| x.get("type")).and_then(Value::as_str).unwrap_or("").to_string(),
                    delay: np.and_then(api::last_delay),
                }
            })
            .collect();
        out.push(GroupInfo {
            now: p.get("now").and_then(Value::as_str).unwrap_or("").to_string(),
            selectable: kind == "Selector",
            kind,
            name,
            nodes,
        });
    }
    Ok(out)
}

#[tauri::command]
fn select_node(s: S, group: String, node: String) -> Result<(), String> {
    s.manual_select_at.store(util::now_secs(), Ordering::Relaxed);
    s.api().select(&group, &node)?;
    log_info!("выбрана нода {group} → {node}");
    s.poke.store(true, Ordering::Relaxed);
    Ok(())
}

/// Пинг всех нод группы. Долгая операция — выполняем асинхронно.
#[tauri::command]
async fn test_group(s: S<'_>, group: String) -> Result<(), String> {
    let api = s.api();
    let res = tauri::async_runtime::spawn_blocking(move || {
        let url = api.proxies().map(|p| api::test_url(&p, &group)).unwrap_or_else(|_| api::DEFAULT_TEST_URL.to_string());
        api.group_delay(&group, &url)
    })
    .await;
    s.poke.store(true, Ordering::Relaxed);
    match res {
        Ok(r) => r.map_err(String::from),
        Err(e) => Err(e.to_string()),
    }
}

// ================================================================ умный выбор

#[tauri::command]
fn get_smart(s: S) -> smart::SmartView {
    smart::view(s.inner())
}

/// Закрепить ноду (пустая строка — снять закрепление и вернуть автовыбор).
#[tauri::command]
fn smart_pin(s: S, node: String) -> Result<(), String> {
    let mut st = s.settings();
    st.smart_pinned = node.clone();
    util::save_settings(&st)?;
    *s.settings.lock().unwrap() = st;
    log_info!("умный выбор: закреплено «{node}»");
    smart::request_round();
    Ok(())
}

#[tauri::command]
fn smart_round() {
    smart::request_round();
}

#[tauri::command]
fn smart_speed_test(app: AppHandle, s: S) -> Result<(), String> {
    let group = s.settings().smart_group;
    let api = s.api();
    let leaves = smart::leaves_of_group(&api, if group.is_empty() { "Auto" } else { &group });
    if leaves.is_empty() {
        return Err("нет нод для проверки (Mihomo выключен?)".into());
    }
    if !smart::start_speed_test(Some(app), s.inner().clone(), api, leaves, false) {
        return Err("тест скорости уже идёт".into());
    }
    Ok(())
}

// ================================================================ Discord, обновления, подсказки

#[tauri::command]
async fn discord_status(s: S<'_>) -> Result<discord::DiscordStatus, String> {
    let api = s.api();
    tauri::async_runtime::spawn_blocking(move || discord::status(discord::tun_enabled(&api)))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn discord_action(s: S<'_>, action: String) -> Result<String, String> {
    let api = s.api();
    tauri::async_runtime::spawn_blocking(move || match action.as_str() {
        // при TUN файлы discord-voice-proxy не нужны — ничего не скачиваем и не копируем
        "install" | "download" if discord::tun_enabled(&api) => Ok(discord::TUN_NOTE.to_string()),
        "install" => discord::install_all().map(|d| {
            if d.is_empty() { "Файлы уже на месте".to_string() } else { format!("Файлы скопированы: {}", d.join(", ")) }
        }),
        "restart" => discord::restart().map(|_| "Discord перезапущен".to_string()),
        "download" => discord::download().and_then(|tag| {
            let _ = discord::install_all();
            Ok(format!("Скачана версия {tag}"))
        }),
        "folder" => {
            util::open_path(&discord::master_dir().display().to_string());
            Ok(String::new())
        }
        _ => Err("неизвестное действие".into()),
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn updates_status() -> updates::UpdState {
    updates::view()
}

#[tauri::command]
async fn updates_action(app: AppHandle, s: S<'_>, action: String) -> Result<String, String> {
    let shared = s.inner().clone();
    if action == "tray" {
        updates::apply_tray_update()?;
        app.exit(0);
        return Ok(String::new());
    }
    if action == "tray_update" {
        // скачать новую версию с GitHub и сразу установить (трей перезапустится)
        tauri::async_runtime::spawn_blocking(updates::update_tray).await.map_err(|e| e.to_string())??;
        app.exit(0);
        return Ok(String::new());
    }
    tauri::async_runtime::spawn_blocking(move || match action.as_str() {
        "check" => {
            let tray = updates::check_tray();
            let core = updates::check(&shared);
            let u = updates::view();
            let mut parts = Vec::new();
            match core {
                Ok(()) if updates::newer(&u.core_latest, &u.core_current) => parts.push(format!("Mihomo: доступна {}", u.core_latest)),
                Ok(()) => parts.push(format!("Mihomo последней версии ({})", u.core_current)),
                Err(e) => parts.push(format!("Mihomo: {e}")),
            }
            match tray {
                Ok(()) if updates::tray_update_available() => parts.push(format!("трей: доступна {}", u.tray_latest)),
                Ok(()) => parts.push(format!("трей последней версии ({})", u.tray_current)),
                Err(e) => parts.push(format!("трей: {e}")),
            }
            Ok(parts.join("; "))
        }
        "tray_download" => updates::download_tray(),
        "core" => updates::update_core(&shared),
        "data" => updates::update_data(&shared),
        _ => Err("неизвестное действие".into()),
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn prompt_current() -> Option<prompt::Suggestion> {
    prompt::current()
}

#[tauri::command]
async fn prompt_answer(app: AppHandle, s: S<'_>, answer: String) -> Result<String, String> {
    let shared = s.inner().clone();
    let app2 = app.clone();
    let res = tauri::async_runtime::spawn_blocking(move || prompt::answer(&app2, &shared, &answer)).await.map_err(|e| e.to_string())?;
    {
        use tauri_plugin_notification::NotificationExt;
        let text = match &res {
            Ok(t) => t.clone(),
            Err(e) => format!("Не удалось добавить правило: {e}"),
        };
        if !text.is_empty() {
            let _ = app.notification().builder().title("Mihomo VPN").body(text).show();
        }
    }
    res
}

// ================================================================ подписки

#[tauri::command]
fn subs_list(s: S) -> subs::SubsView {
    subs::view(&s.api())
}

#[derive(Serialize)]
struct SubResult {
    apply: ApplyResult,
    name: String,
}

#[tauri::command]
async fn subs_action(s: S<'_>, action: String, name: String, url: Option<String>, activate: Option<bool>) -> Result<SubResult, String> {
    let shared = s.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let api = shared.api();
        let url = url.unwrap_or_default();
        let mut final_name = name.clone();
        let apply = match action.as_str() {
            "add" => {
                let d = rules::load_doc()?;
                final_name = subs::make_name(&d, &name, &url);
                let n = final_name.clone();
                subs::apply(&api, &format!("подписка {n}: добавление"), |d| subs::add(d, &n, &url, activate.unwrap_or(false)))
            }
            "remove" => subs::apply(&api, &format!("подписка {name}: удаление"), |d| subs::remove(d, &name)),
            "set_url" => subs::apply(&api, &format!("подписка {name}: новая ссылка"), |d| subs::set_url(d, &name, &url)),
            "activate" => subs::apply(&api, &format!("подписка {name}: активная"), |d| subs::activate(d, &name)),
            "refresh" => {
                subs::refresh(&api, &name)?;
                ApplyResult { ok: true, live: true, message: String::new() }
            }
            _ => return Err("неизвестное действие".into()),
        };
        // после добавления/смены ссылки — сразу скачать ноды
        if apply.ok && apply.live && matches!(action.as_str(), "add" | "set_url") {
            if let Err(e) = subs::refresh(&api, &final_name) {
                log_warn!("загрузка подписки {final_name}: {e}");
            }
        }
        shared.poke.store(true, Ordering::Relaxed);
        smart::request_round();
        Ok(SubResult { apply, name: final_name })
    })
    .await
    .map_err(|e| e.to_string())?
}

// ================================================================ правила

#[derive(Serialize)]
struct RulesView {
    rules: Vec<Rule>,
    /// Группы для выбора действия «через VPN».
    groups: Vec<String>,
    default_group: String,
    error: Option<String>,
}

fn group_names(s: &SharedRef) -> Vec<String> {
    if let Ok(p) = s.api().proxies() {
        let v: Vec<String> = p
            .get("GLOBAL")
            .and_then(|g| g.get("all"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter(|n| p.get(*n).and_then(|x| x.get("all")).is_some())
                    .filter(|n| *n != rules::PROBE_GROUP && p.get(*n).and_then(|x| x.get("hidden")).and_then(Value::as_bool) != Some(true))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if !v.is_empty() {
            return v;
        }
    }
    vec![s.main_group()]
}

fn default_group(s: &SharedRef) -> String {
    let d = s.settings().default_proxy_group;
    if d.is_empty() { s.main_group() } else { d }
}

#[tauri::command]
fn get_rules(s: S) -> RulesView {
    let shared = s.inner();
    match rules::load_doc() {
        Ok(d) => RulesView { rules: d.rules(), groups: group_names(shared), default_group: default_group(shared), error: None },
        Err(e) => RulesView { rules: vec![], groups: vec![], default_group: String::new(), error: Some(e) },
    }
}

#[derive(Deserialize)]
struct RuleInput {
    kind: String,
    value: String,
    action: String,
}

#[derive(Serialize)]
struct AddResult {
    added: usize,
    duplicates: usize,
    errors: Vec<String>,
    apply: ApplyResult,
}

#[tauri::command]
fn add_rules(s: S, items: Vec<RuleInput>) -> AddResult {
    let mut errors = Vec::new();
    let mut valid = Vec::new();
    for it in &items {
        match rules::normalize(&it.kind, &it.value, &it.action) {
            Ok(r) => valid.push(r),
            Err(e) => errors.push(format!("{}: {e}", it.value)),
        }
    }
    let (mut added, mut dups) = (0, 0);
    let apply = if valid.is_empty() {
        ApplyResult { ok: errors.is_empty(), live: true, message: String::new() }
    } else {
        rules::mutate(&s.api(), &format!("добавление {} правил", valid.len()), |d| {
            for r in &valid {
                if d.add(r)? { added += 1 } else { dups += 1 }
            }
            Ok(())
        })
    };
    if !apply.ok {
        added = 0;
    }
    AddResult { added, duplicates: dups, errors, apply }
}

#[tauri::command]
fn update_rule(s: S, key: String, item: RuleInput) -> ApplyResult {
    match rules::normalize(&item.kind, &item.value, &item.action) {
        Ok(r) => rules::mutate(&s.api(), &format!("изменение {key}"), |d| d.replace(&key, &r)),
        Err(e) => ApplyResult { ok: false, live: false, message: e },
    }
}

#[derive(Deserialize)]
struct RuleUpdate {
    key: String,
    item: RuleInput,
}

/// Изменение нескольких правил одной записью конфига (один бэкап, одна перезагрузка Mihomo).
#[tauri::command]
fn update_rules(s: S, items: Vec<RuleUpdate>) -> ApplyResult {
    let mut norm = Vec::with_capacity(items.len());
    for it in &items {
        match rules::normalize(&it.item.kind, &it.item.value, &it.item.action) {
            Ok(r) => norm.push((it.key.clone(), r)),
            Err(e) => return ApplyResult { ok: false, live: false, message: format!("{}: {e}", it.item.value) },
        }
    }
    rules::mutate(&s.api(), &format!("изменение {} правил", norm.len()), |d| {
        for (k, r) in &norm {
            d.replace(k, r)?;
        }
        Ok(())
    })
}

#[tauri::command]
fn take_nav() -> Option<String> {
    tray::take_nav()
}

#[tauri::command]
fn delete_rules(s: S, keys: Vec<String>) -> ApplyResult {
    rules::mutate(&s.api(), &format!("удаление {} правил", keys.len()), |d| {
        let missing: Vec<&String> = keys.iter().filter(|k| !d.remove(k)).collect();
        if missing.len() == keys.len() {
            return Err("правила не найдены (возможно, конфиг изменили)".into());
        }
        Ok(())
    })
}

#[tauri::command]
fn move_rule(s: S, key: String, dir: i32) -> ApplyResult {
    rules::mutate(&s.api(), &format!("перемещение {key}"), |d| d.shift(&key, dir.signum()))
}

#[tauri::command]
fn preview_import(s: S, text: String, action: Option<String>) -> Vec<ImportLine> {
    let a = action.filter(|a| !a.is_empty()).unwrap_or_else(|| default_group(s.inner()));
    rules::parse_import(&text, &a)
}

#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() > 5 * 1024 * 1024 {
        return Err("файл больше 5 МБ".into());
    }
    std::fs::read_to_string(&path).map_err(|e| format!("не удалось прочитать файл: {e}"))
}

#[tauri::command]
fn export_rules(path: String) -> Result<usize, String> {
    let d = rules::load_doc()?;
    let r = d.rules();
    util::write_atomic(std::path::Path::new(&path), rules::export_text(&r).as_bytes())?;
    log_info!("экспортировано {} правил в {path}", r.len());
    Ok(r.len())
}

#[tauri::command]
fn list_processes() -> Vec<String> {
    service::running_processes()
}

// ================================================================ недавние адреса

#[tauri::command]
fn get_hosts(s: S) -> Vec<HostEntry> {
    s.hosts()
}

#[tauri::command]
fn clear_hosts(s: S) {
    s.clear_hosts();
}

// ================================================================ настройки

#[tauri::command]
fn get_settings(s: S) -> Settings {
    s.settings()
}

#[tauri::command]
fn save_settings(s: S, settings: Settings) -> Result<(), String> {
    util::save_settings(&settings)?;
    *s.settings.lock().unwrap() = settings;
    s.poke.store(true, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
fn list_backups() -> Vec<rules::BackupInfo> {
    rules::list_backups()
}

#[tauri::command]
fn restore_backup(s: S, name: String) -> ApplyResult {
    rules::restore_backup(&s.api(), &name)
}

#[derive(Serialize)]
struct Paths {
    mihomo_dir: String,
    config: String,
    data_dir: String,
    elevated: bool,
    version: String,
}

#[tauri::command]
fn get_paths() -> Paths {
    Paths {
        mihomo_dir: util::mihomo_dir().display().to_string(),
        config: util::config_path().display().to_string(),
        data_dir: util::data_dir().display().to_string(),
        elevated: service::is_elevated(),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

#[tauri::command]
fn open_target(s: S, what: String) {
    let target = match what.as_str() {
        "panel" => format!("{}/ui", s.api().base()),
        "logs" => util::logs_dir().display().to_string(),
        "backups" => util::backups_dir().display().to_string(),
        "folder" => util::mihomo_dir().display().to_string(),
        _ => return,
    };
    util::open_path(&target);
}

// ================================================================ окна

#[tauri::command]
fn show_main(app: AppHandle, page: Option<String>) {
    tray::show_main(&app, page.as_deref());
}

#[tauri::command]
fn hide_flyout(app: AppHandle) {
    tray::hide_flyout(&app);
}

#[tauri::command]
fn quit(app: AppHandle) {
    log_info!("выход");
    app.exit(0);
}

// ================================================================ запуск

fn main() {
    std::panic::set_hook(Box::new(|info| {
        util::log("PANIC", &info.to_string());
    }));
    log_info!("запуск, папка Mihomo: {}", util::mihomo_dir().display());

    let shared = Shared::new();
    let start_hidden = std::env::args().any(|a| a == "--hidden" || a == "--tray");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // повторный запуск — просто показываем уже работающее окно
            tray::show_main(app, None);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(shared.clone())
        .invoke_handler(tauri::generate_handler![
            get_status,
            subs_list,
            subs_action,
            discord_status,
            discord_action,
            updates_status,
            updates_action,
            prompt_current,
            prompt_answer,
            get_smart,
            smart_pin,
            smart_round,
            smart_speed_test,
            service_action,
            set_mode,
            get_groups,
            select_node,
            test_group,
            get_rules,
            add_rules,
            update_rule,
            update_rules,
            take_nav,
            delete_rules,
            move_rule,
            preview_import,
            read_text_file,
            export_rules,
            list_processes,
            get_hosts,
            clear_hosts,
            get_settings,
            save_settings,
            list_backups,
            restore_backup,
            get_paths,
            open_target,
            show_main,
            hide_flyout,
            quit
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            tray::create(&handle)?;
            if let Some(f) = handle.get_webview_window("flyout") {
                tray::style_flyout(&f);
            }
            monitor::spawn(handle.clone(), shared.clone(), tray::TraySink::new(handle.clone()));
            smart::spawn(handle.clone(), shared.clone(), tray::TraySink::new(handle.clone()));
            games::spawn(shared.clone(), tray::TraySink::new(handle.clone()));
            discord::spawn(shared.clone(), tray::TraySink::new(handle.clone()));
            updates::spawn(shared.clone(), tray::TraySink::new(handle.clone()));
            prompt::spawn(handle.clone(), shared.clone());
            tray::spawn_reaper(handle.clone());
            if !start_hidden {
                tray::show_main(&handle, None);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            let app = window.app_handle();
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    // закрытие окна = свернуть в трей
                    api.prevent_close();
                    let _ = window.hide();
                    if let Some(w) = app.get_webview_window(window.label()) {
                        tray::set_asleep(&w, true);
                    }
                    tray::refresh_visibility(app);
                }
                WindowEvent::Focused(false) if window.label() == "flyout" => {
                    tray::hide_flyout(app);
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("ошибка запуска приложения")
        .run(|_app, event| {
            // окна выгружаются после простоя; без окон приложение должно жить в трее.
            // Настоящий выход (app.exit) приходит с кодом — его не трогаем.
            if let tauri::RunEvent::ExitRequested { code: None, api, .. } = event {
                api.prevent_exit();
            }
        });
}
