//! Игровой режим.
//!
//! Сами игры идут напрямую (DIRECT) по правилам конфига, поэтому трей не трогает их
//! трафик. Пока запущена игра (или OBS), трей просто не мешает:
//!   • тест скорости нод на паузе (он забирает канал);
//!   • умный выбор не меняет ноду ради «чуть лучше» — только если она умерла,
//!     чтобы голос в Discord и стрим не прерывались;
//!   • полная проверка нод — раз в 3 минуты вместо раза в минуту;
//!   • подсказки «сайт не открывается» не всплывают поверх игры.

use crate::monitor::{SharedRef, Sink};
use crate::util;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

static ACTIVE: Mutex<Option<String>> = Mutex::new(None);

/// Имя игры, если игровой режим сейчас включён.
pub fn active() -> Option<String> {
    ACTIVE.lock().ok().and_then(|g| g.clone())
}

/// Папки, где лежат игры из популярных магазинов.
const GAME_DIRS: &[&str] = &[
    "\\steamapps\\common\\",
    "\\epic games\\",
    "\\riot games\\",
    "\\xboxgames\\",
    "\\ea games\\",
    "\\electronic arts\\",
    "\\ubisoft game launcher\\games\\",
    "\\gog galaxy\\games\\",
    "\\battle.net\\games\\",
    "\\windowsapps\\microsoft.minecraft",
];

/// Известные игры вне этих папок.
const KNOWN: &[&str] = &[
    "cs2.exe", "dota2.exe", "valorant-win64-shipping.exe", "league of legends.exe", "r5apex.exe",
    "r5apex_dx12.exe", "fortniteclient-win64-shipping.exe", "overwatch.exe", "robloxplayerbeta.exe",
    "gta5.exe", "gta5_enhanced.exe", "rainbowsix.exe", "tslgame.exe", "eldenring.exe", "wow.exe",
    "destiny2.exe", "rustclient.exe", "escapefromtarkov.exe", "deadlock.exe", "project8.exe",
    "warthunder.exe", "aces.exe", "worldoftanks.exe", "tarkov.exe", "minecraft.windows.exe",
    "cod.exe", "modernwarfare.exe", "bf2042.exe", "marvel-win64-shipping.exe", "javaw.exe",
];

/// Служебные процессы, которые лежат в папках игр, но игрой не являются.
fn is_helper(name: &str) -> bool {
    const BAD: &[&str] = &[
        "crash", "anticheat", "easyanticheat", "beservice", "battleye", "launcher", "redist", "setup",
        "unins", "update", "helper", "webhelper", "service", "reporter", "overlay", "vc_redist",
        "dxsetup", "cefprocess", "steam.exe", "epicgameslauncher", "riotclient",
    ];
    BAD.iter().any(|b| name.contains(b))
}

/// Ищет запущенную игру. Возвращает «красивое» имя процесса.
pub fn detect(procs: &[(String, String)], extra: &[String], obs: bool) -> Option<String> {
    let extra: Vec<String> = extra.iter().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
    for (name, path) in procs {
        // javaw.exe считаем игрой только если это Minecraft
        if name == "javaw.exe" && !path.contains("minecraft") {
            continue;
        }
        let hit = extra.iter().any(|e| e == name || format!("{e}.exe") == *name)
            || KNOWN.contains(&name.as_str())
            || (GAME_DIRS.iter().any(|d| path.contains(d)) && name.ends_with(".exe") && !is_helper(name))
            || (obs && (name == "obs64.exe" || name == "obs32.exe"));
        if hit {
            return Some(name.clone());
        }
    }
    None
}

pub fn spawn(shared: SharedRef, sink: impl Sink) {
    thread::Builder::new()
        .name("games".into())
        .spawn(move || {
            let mut misses = 0;
            loop {
                let s = shared.settings();
                let found = if s.game_mode {
                    detect(&util::processes(), &s.game_processes, s.game_mode_obs)
                } else {
                    None
                };
                let cur = active();
                match (&cur, &found) {
                    (None, Some(g)) => {
                        *ACTIVE.lock().unwrap() = Some(g.clone());
                        misses = 0;
                        crate::log_info!("игровой режим: {g}");
                        sink.notify(
                            "Игровой режим",
                            &format!("{g}: тест скорости на паузе, нода без нужды не меняется — голос и стрим не прервутся"),
                        );
                        shared.poke.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    (Some(_), Some(g)) => {
                        misses = 0;
                        *ACTIVE.lock().unwrap() = Some(g.clone());
                    }
                    (Some(g), None) => {
                        // выход из режима — только если игры нет 2 проверки подряд
                        misses += 1;
                        if misses >= 2 {
                            *ACTIVE.lock().unwrap() = None;
                            crate::log_info!("игровой режим выключен ({g} закрыта)");
                            shared.poke.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    (None, None) => {}
                }
                thread::sleep(Duration::from_secs(10));
            }
        })
        .expect("games thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(n: &str, path: &str) -> (String, String) {
        (n.into(), path.into())
    }
    #[test]
    fn detection() {
        let base = vec![p("chrome.exe", "c:\\program files\\google\\chrome.exe"), p("steam.exe", "c:\\steam\\steam.exe")];
        assert_eq!(detect(&base, &[], true), None);
        let mut g = base.clone();
        g.push(p("unitycrashhandler64.exe", "d:\\steamlibrary\\steamapps\\common\\x\\unitycrashhandler64.exe"));
        assert_eq!(detect(&g, &[], true), None, "служебный процесс — не игра");
        g.push(p("hollow knight.exe", "d:\\steamlibrary\\steamapps\\common\\hollow knight\\hollow knight.exe"));
        assert_eq!(detect(&g, &[], true).as_deref(), Some("hollow knight.exe"));
        assert_eq!(detect(&[p("cs2.exe", "")], &[], false).as_deref(), Some("cs2.exe"));
        assert_eq!(detect(&[p("obs64.exe", "")], &[], false), None);
        assert_eq!(detect(&[p("obs64.exe", "")], &[], true).as_deref(), Some("obs64.exe"));
        assert_eq!(detect(&[p("mygame.exe", "")], &["MyGame".into()], false).as_deref(), Some("mygame.exe"));
        assert_eq!(detect(&[p("javaw.exe", "c:\\java\\bin\\javaw.exe")], &[], false), None);
    }
}
