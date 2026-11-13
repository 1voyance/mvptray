//! Иконка в трее, всплывающее окно (flyout) и уведомления.

use crate::monitor::{SharedRef, Sink, Status};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Rect};
use tauri_plugin_notification::NotificationExt;

const TRAY_ID: &str = "main";
static FLYOUT_HIDDEN_AT: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ---------------------------------------------------------------- иконка

/// Рисует иконку 32×32: цветной круг со светлой буквой «V».
pub fn draw_icon(health: &str) -> Image<'static> {
    const N: usize = 32;
    let (r, g, b) = match health {
        "ok" => (52u8, 211u8, 153u8),
        "warn" => (251, 191, 36),
        _ => (120, 132, 156),
    };
    let mut px = vec![0u8; N * N * 4];
    let c = N as f32 / 2.0;
    let seg = |x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32| -> f32 {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        let (qx, qy) = (ax + t * dx - x, ay + t * dy - y);
        (qx * qx + qy * qy).sqrt()
    };
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - c).powi(2) + (fy - c).powi(2)).sqrt();
            let disc = (c - 0.5 - d).clamp(0.0, 1.0); // сглаженный край круга
            if disc <= 0.0 {
                continue;
            }
            // «V»: два отрезка, толщина ~3.4 px
            let dv = seg(fx, fy, 9.5, 10.0, 16.0, 22.5).min(seg(fx, fy, 16.0, 22.5, 22.5, 10.0));
            let glyph = (1.9 - dv).clamp(0.0, 1.0);
            // лёгкий градиент сверху вниз
            let shade = 1.0 - (fy / N as f32) * 0.18;
            let mix = |base: u8, fg: u8| -> u8 {
                let b = base as f32 * shade;
                (b + (fg as f32 - b) * glyph).round().clamp(0.0, 255.0) as u8
            };
            let i = (y * N + x) * 4;
            px[i] = mix(r, 255);
            px[i + 1] = mix(g, 255);
            px[i + 2] = mix(b, 255);
            px[i + 3] = (disc * 255.0) as u8;
        }
    }
    Image::new_owned(px, N as u32, N as u32)
}

// ---------------------------------------------------------------- трей

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Открыть Mihomo", true, None::<&str>)?;
    let start = MenuItem::with_id(app, "start", "Включить VPN", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", "Выключить VPN", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart", "Перезапустить VPN", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &sep1, &start, &stop, &restart, &sep2, &quit])?;

    #[allow(deprecated)]
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(draw_icon("off"))
        .tooltip("Mihomo VPN")
        .menu(&menu)
        .menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "open" => show_main(app, None),
            "quit" => app.exit(0),
            a @ ("start" | "stop" | "restart") => {
                let _ = app.emit("tray-action", a.to_string());
                let shared = app.state::<SharedRef>().inner().clone();
                run_service_from_tray(app.clone(), shared, a.to_string());
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| match ev {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } => {
                toggle_flyout(tray.app_handle(), rect);
            }
            TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => {
                hide_flyout(tray.app_handle());
                show_main(tray.app_handle(), None);
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn run_service_from_tray(app: AppHandle, shared: SharedRef, action: String) {
    std::thread::spawn(move || {
        if action == "stop" {
            shared.user_stopped.store(true, Ordering::Relaxed);
        }
        *shared.busy.lock().unwrap() = Some(action.clone());
        shared.poke.store(true, Ordering::Relaxed);
        let res = crate::service::control(&action);
        *shared.busy.lock().unwrap() = None;
        if let Err(e) = res {
            if action == "stop" {
                shared.user_stopped.store(false, Ordering::Relaxed);
            }
            let _ = app.notification().builder().title("Mihomo VPN").body(e).show();
        }
        shared.poke.store(true, Ordering::Relaxed);
    });
}

// ---------------------------------------------------------------- окна
//
// Окна (WebView2) живут только пока нужны. Скрытое окно всё равно держит процессы
// msedgewebview2 (сотни МБ) и продолжает рисовать анимации, поэтому:
//   • при скрытии анимации ставятся на паузу (класс `asleep`);
//   • если ни одно окно не показывалось IDLE секунд — все окна уничтожаются,
//     процессы WebView2 завершаются; при следующем клике окно создаётся заново.

const WINDOWS: [&str; 3] = ["main", "flyout", "prompt"];
static LAST_UI_ACTIVITY: AtomicU64 = AtomicU64::new(0);
static CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static PENDING_NAV: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Через сколько секунд простоя выгружать окна (MIHOMO_TRAY_IDLE_SECS, по умолчанию 120).
fn idle_secs() -> u64 {
    std::env::var("MIHOMO_TRAY_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(120)
}

pub fn touch() {
    LAST_UI_ACTIVITY.store(now_ms(), Ordering::Relaxed);
}

/// Страница, которую просили открыть до того, как окно успело загрузиться.
pub fn take_nav() -> Option<String> {
    PENDING_NAV.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Пауза/возобновление анимаций в окне.
pub fn set_asleep(w: &tauri::WebviewWindow, asleep: bool) {
    let _ = w.eval(if asleep {
        "document.documentElement.classList.add('asleep')"
    } else {
        "document.documentElement.classList.remove('asleep')"
    });
}

/// Окно по имени; если оно выгружено — создаётся заново из tauri.conf.json.
/// Создание нельзя вызывать из главного потока (обработчики трея, синхронные команды):
/// на Windows это зависание, поэтому используйте `with_window`.
pub fn ensure_window(app: &AppHandle, label: &str) -> Option<(tauri::WebviewWindow, bool)> {
    if let Some(w) = app.get_webview_window(label) {
        return Some((w, false));
    }
    let _g = CREATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(w) = app.get_webview_window(label) {
        return Some((w, false));
    }
    let cfg = app.config().app.windows.iter().find(|c| c.label == label)?.clone();
    match tauri::WebviewWindowBuilder::from_config(app, &cfg).and_then(|b| b.build()) {
        Ok(w) => Some((w, true)),
        Err(e) => {
            crate::log_err!("не удалось создать окно {label}: {e}");
            app.get_webview_window(label).map(|w| (w, false))
        }
    }
}

/// Выполнить действие с окном: если оно есть — сразу, если выгружено — создать в фоне.
fn with_window(app: &AppHandle, label: &'static str, f: impl FnOnce(&AppHandle, tauri::WebviewWindow, bool) + Send + 'static) {
    touch();
    if let Some(w) = app.get_webview_window(label) {
        f(app, w, false);
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        if let Some((w, created)) = ensure_window(&app, label) {
            f(&app, w, created);
        }
    });
}

pub fn refresh_visibility(app: &AppHandle) {
    let vis = ["main", "flyout"]
        .iter()
        .filter_map(|l| app.get_webview_window(l))
        .any(|w| w.is_visible().unwrap_or(false));
    let shared = app.state::<SharedRef>();
    shared.ui_visible.store(vis, Ordering::Relaxed);
    touch();
    if vis {
        shared.poke.store(true, Ordering::Relaxed);
    }
}

pub fn show_main(app: &AppHandle, page: Option<&str>) {
    let page = page.map(String::from);
    if app.get_webview_window("main").is_none() {
        if let Some(p) = &page {
            *PENDING_NAV.lock().unwrap_or_else(|e| e.into_inner()) = Some(p.clone());
        }
    }
    with_window(app, "main", move |app, w, created| {
        set_asleep(&w, false);
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        if !created {
            if let Some(p) = page {
                let _ = w.emit("navigate", p);
            }
        }
        refresh_visibility(app);
    });
}

pub fn hide_flyout(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("flyout") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
            set_asleep(&w, true);
            FLYOUT_HIDDEN_AT.store(now_ms(), Ordering::Relaxed);
        }
    }
    refresh_visibility(app);
}

/// Выгрузка окон после простоя: без окон процессы WebView2 завершаются сами.
pub fn spawn_reaper(app: AppHandle) {
    touch();
    std::thread::Builder::new()
        .name("reaper".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            let open: Vec<tauri::WebviewWindow> = WINDOWS.iter().filter_map(|l| app.get_webview_window(l)).collect();
            if open.is_empty() {
                continue;
            }
            if open.iter().any(|w| w.is_visible().unwrap_or(false)) || crate::prompt::current().is_some() {
                touch();
                continue;
            }
            if now_ms().saturating_sub(LAST_UI_ACTIVITY.load(Ordering::Relaxed)) < idle_secs() * 1000 {
                continue;
            }
            let _g = CREATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let n = open.iter().filter(|w| w.destroy().is_ok()).count();
            app.state::<SharedRef>().ui_visible.store(false, Ordering::Relaxed);
            crate::log_info!("окна выгружены из памяти после простоя: {n}");
        })
        .expect("reaper thread");
}

/// Оформление окна flyout на Windows.
/// У окна без заголовка Windows оставляет системную рамку, которая у вас рисуется белой.
/// Поэтому: убираем стили рамки, а затем обрезаем окно регионом ровно по клиентской
/// области со скруглёнными углами — всё, что снаружи, физически не отображается.
pub fn style_flyout(w: &tauri::WebviewWindow) {
    style_popup(w, 12.0);
}

#[cfg(windows)]
pub fn style_popup(w: &tauri::WebviewWindow, radius: f64) {
    #[repr(C)]
    #[derive(Default)]
    struct RECT {
        l: i32,
        t: i32,
        r: i32,
        b: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct POINT {
        x: i32,
        y: i32,
    }
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const std::ffi::c_void, size: u32) -> i32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetWindowLongPtrW(hwnd: isize, idx: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: isize, idx: i32, v: isize) -> isize;
        fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
        fn GetWindowRect(hwnd: isize, r: *mut RECT) -> i32;
        fn GetClientRect(hwnd: isize, r: *mut RECT) -> i32;
        fn ClientToScreen(hwnd: isize, p: *mut POINT) -> i32;
        fn SetWindowRgn(hwnd: isize, rgn: isize, redraw: i32) -> i32;
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn CreateRoundRectRgn(l: i32, t: i32, r: i32, b: i32, w: i32, h: i32) -> isize;
    }
    const GWL_STYLE: i32 = -16;
    const GWL_EXSTYLE: i32 = -20;
    const WS_BORDER: isize = 0x0080_0000;
    const WS_DLGFRAME: isize = 0x0040_0000;
    const WS_THICKFRAME: isize = 0x0004_0000;
    const WS_EX_DLGMODALFRAME: isize = 0x0000_0001;
    const WS_EX_WINDOWEDGE: isize = 0x0000_0100;
    const WS_EX_CLIENTEDGE: isize = 0x0000_0200;
    const WS_EX_STATICEDGE: isize = 0x0002_0000;
    const SWP_FLAGS: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0010 | 0x0020; // NOSIZE|NOMOVE|NOZORDER|NOACTIVATE|FRAMECHANGED
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWCP_DONOTROUND: u32 = 1;
    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

    let Ok(h) = w.hwnd() else { return };
    let hwnd = h.0 as isize;
    let scale = w.scale_factor().unwrap_or(1.0);
    unsafe {
        let dark: u32 = 1;
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const u32 as _, 4);
        DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &DWMWA_COLOR_NONE as *const u32 as _, 4);
        // углы скругляем сами регионом, системное скругление только мешает
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_DONOTROUND as *const u32 as _, 4);

        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let new_style = style & !(WS_BORDER | WS_DLGFRAME | WS_THICKFRAME);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_ex = ex & !(WS_EX_DLGMODALFRAME | WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE);
        if new_style != style || new_ex != ex {
            SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
            SetWindowPos(hwnd, 0, 0, 0, 0, 0, SWP_FLAGS);
        }

        // регион = клиентская область (в координатах окна) со скруглёнными углами
        let (mut wr, mut cr, mut origin) = (RECT::default(), RECT::default(), POINT::default());
        if GetWindowRect(hwnd, &mut wr) == 0 || GetClientRect(hwnd, &mut cr) == 0 || ClientToScreen(hwnd, &mut origin) == 0 {
            return;
        }
        let (ox, oy) = (origin.x - wr.l, origin.y - wr.t);
        let d = (radius * 2.0 * scale).round() as i32; // диаметр скругления, как в CSS
        let rgn = CreateRoundRectRgn(ox, oy, ox + cr.r + 1, oy + cr.b + 1, d, d);
        if rgn != 0 {
            // после SetWindowRgn регионом владеет система — удалять его не нужно
            SetWindowRgn(hwnd, rgn, 1);
        }
    }
}

#[cfg(not(windows))]
pub fn style_popup(_w: &tauri::WebviewWindow, _radius: f64) {}

/// Показать окно поверх всех, не забирая фокус (чтобы не мешать набору текста/игре).
#[cfg(windows)]
pub fn show_no_activate(w: &tauri::WebviewWindow) {
    #[link(name = "user32")]
    extern "system" {
        fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
        fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    }
    let Ok(h) = w.hwnd() else { return };
    let hwnd = h.0 as isize;
    unsafe {
        ShowWindow(hwnd, 4); // SW_SHOWNOACTIVATE
        SetWindowPos(hwnd, -1, 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0010); // HWND_TOPMOST, NOSIZE|NOMOVE|NOACTIVATE
    }
}

#[cfg(not(windows))]
pub fn show_no_activate(w: &tauri::WebviewWindow) {
    let _ = w.show();
}

/// Скрыть окно, показанное через `show_no_activate`.
/// Обычный `w.hide()` тут не работает: tao хранит свой флаг видимости и меняет
/// окно, только если флаг изменился, а `show_no_activate` показывает окно в обход
/// tao — для него окно так и осталось «скрытым», и hide() ничего не делает.
#[cfg(windows)]
pub fn hide_no_activate(w: &tauri::WebviewWindow) {
    #[link(name = "user32")]
    extern "system" {
        fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    }
    let _ = w.hide();
    if let Ok(h) = w.hwnd() {
        unsafe {
            ShowWindow(h.0 as isize, 0); // SW_HIDE
        }
    }
}

#[cfg(not(windows))]
pub fn hide_no_activate(w: &tauri::WebviewWindow) {
    let _ = w.hide();
}

fn toggle_flyout(app: &AppHandle, rect: Rect) {
    if let Some(w) = app.get_webview_window("flyout") {
        if w.is_visible().unwrap_or(false) {
            hide_flyout(app);
            return;
        }
    }
    // клик по иконке сам снимает фокус с flyout — не открываем его тут же заново
    if now_ms().saturating_sub(FLYOUT_HIDDEN_AT.load(Ordering::Relaxed)) < 300 {
        return;
    }
    with_window(app, "flyout", move |app, w, _created| place_and_show_flyout(app, &w, rect));
}

fn place_and_show_flyout(app: &AppHandle, w: &tauri::WebviewWindow, rect: Rect) {
    let scale = w.scale_factor().unwrap_or(1.0);
    let size = w.outer_size().unwrap_or(tauri::PhysicalSize::new((340.0 * scale) as u32, (520.0 * scale) as u32));
    let (fw, fh) = (size.width as i32, size.height as i32);
    let pos = rect.position.to_physical::<i32>(scale);
    let rsz = rect.size.to_physical::<i32>(scale);
    let gap = (10.0 * scale) as i32;

    let monitor = w
        .available_monitors()
        .ok()
        .and_then(|ms| {
            ms.into_iter().find(|m| {
                let (mp, ms) = (m.position(), m.size());
                pos.x >= mp.x && pos.x < mp.x + ms.width as i32 && pos.y >= mp.y && pos.y < mp.y + ms.height as i32
            })
        })
        .or_else(|| w.primary_monitor().ok().flatten());

    let mut x = pos.x + rsz.width / 2 - fw / 2;
    let mut y = pos.y - fh - gap; // панель задач снизу
    if let Some(m) = monitor {
        let (mp, ms) = (m.position(), m.size());
        let (mx, my, mw, mh) = (mp.x, mp.y, ms.width as i32, ms.height as i32);
        if pos.y < my + mh / 2 {
            y = pos.y + rsz.height + gap; // панель задач сверху
        }
        x = x.clamp(mx + gap, mx + mw - fw - gap);
        y = y.clamp(my + gap, my + mh - fh - gap);
    }
    let _ = w.set_position(PhysicalPosition::new(x, y));
    set_asleep(w, false);
    style_flyout(w);
    let _ = w.show();
    style_flyout(w); // после показа размеры клиентской области окончательные
    let _ = w.set_focus();
    let _ = w.emit("flyout-shown", ());
    refresh_visibility(app);
}

// ---------------------------------------------------------------- связь с монитором

pub struct TraySink {
    app: AppHandle,
}

impl TraySink {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

pub fn fmt_speed(b: u64) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} МБ/с", b / 1024.0 / 1024.0)
    } else if b >= 1024.0 {
        format!("{:.0} КБ/с", b / 1024.0)
    } else {
        format!("{b:.0} Б/с")
    }
}

/// Подсказка у иконки (Windows ограничивает её 127 символами).
pub fn tooltip(st: &Status) -> String {
    let mut s = match st.health.as_str() {
        "off" => format!("Mihomo — {}", st.reason),
        _ if !st.api_ok => format!("Mihomo — {}", st.reason),
        _ => {
            let mut t = String::from("Mihomo — ");
            let node = if st.node.chars().count() > 36 { st.node.chars().take(35).collect::<String>() + "…" } else { st.node.clone() };
            t.push_str(&node);
            match st.delay {
                Some(0) => t.push_str(", не отвечает"),
                Some(d) => t.push_str(&format!(", {d} мс")),
                None => {}
            }
            t.push_str(&format!("\n↓ {}  ↑ {}", fmt_speed(st.down_speed), fmt_speed(st.up_speed)));
            if st.mode == "global" {
                t.push_str("\nРежим: глобальный");
            } else if st.mode == "direct" {
                t.push_str("\nРежим: напрямую");
            }
            t
        }
    };
    if s.chars().count() > 127 {
        s = s.chars().take(126).collect::<String>() + "…";
    }
    s
}

impl Sink for TraySink {
    fn on_status(&self, st: &Status, health_changed: bool) {
        if let Some(tray) = self.app.tray_by_id(TRAY_ID) {
            if health_changed {
                let _ = tray.set_icon(Some(draw_icon(&st.health)));
            }
            let _ = tray.set_tooltip(Some(tooltip(st)));
        }
    }

    fn notify(&self, title: &str, body: &str) {
        crate::log_info!("уведомление: {title}: {body}");
        let _ = self.app.notification().builder().title(title).body(body).show();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn icon_and_tooltip() {
        let img = draw_icon("ok");
        assert_eq!(img.rgba().len(), 32 * 32 * 4);
        let st = Status {
            health: "ok".into(),
            api_ok: true,
            node: "🇳🇱 Нидерланды-1".into(),
            delay: Some(48),
            down_speed: 5 * 1024 * 1024,
            up_speed: 1200,
            ..Default::default()
        };
        let t = tooltip(&st);
        assert!(t.contains("48 мс") && t.contains("5.0 МБ/с") && t.chars().count() <= 127, "{t}");
    }
}
