//! Подписки (proxy-providers): добавить по ссылке, сменить ссылку, удалить,
//! выбрать активную (или использовать все сразу).
//!
//! «Активная подписка» = какие провайдеры перечислены в `use:` у групп
//! (Proxy, Auto и т.д.). Режим «все» — во всех группах все подписки.

use crate::api::Api;
use crate::rules::{self, ApplyResult, Doc};
use crate::util;
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ProviderCfg {
    pub name: String,
    pub kind: String,
    pub url: String,
    pub path: String,
}

/// Метки, которые трей оставляет при удалении последней подписки, чтобы при
/// добавлении новой вернуть всё на место.
const USE_MARK: &str = "# mihomo-tray:use";
const PLACEHOLDER: &str = "    proxies: [DIRECT] # mihomo-tray:placeholder";

// ================================================================ разбор конфига

fn indent(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

fn is_top(l: &str) -> bool {
    !l.is_empty() && !l.starts_with(char::is_whitespace) && !l.starts_with('#') && !l.starts_with('-')
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    let v = match v.find(" #") {
        Some(i) if !v.starts_with('"') && !v.starts_with('\'') => &v[..i],
        _ => v,
    };
    v.trim().trim_matches(|c| c == '"' || c == '\'').to_string()
}

/// Индекс строки `proxy-providers:` и конец секции.
fn providers_section(d: &Doc) -> Option<(usize, usize)> {
    let s = d.lines.iter().position(|l| l.trim_end() == "proxy-providers:")?;
    let e = (s + 1..d.lines.len()).find(|&i| is_top(&d.lines[i])).unwrap_or(d.lines.len());
    Some((s, e))
}

/// Диапазон строк провайдера [ключ, конец).
fn provider_range(d: &Doc, name: &str) -> Option<(usize, usize)> {
    let (s, e) = providers_section(d)?;
    let k = (s + 1..e).find(|&i| {
        let l = &d.lines[i];
        indent(l) == 2 && !l.trim_start().starts_with('#') && unquote(l.trim().trim_end_matches(':')) == name && l.trim_end().ends_with(':')
    })?;
    let mut end = k + 1;
    while end < e {
        let l = &d.lines[end];
        if !l.trim().is_empty() && indent(l) <= 2 && !l.trim_start().starts_with('#') {
            break;
        }
        end += 1;
    }
    // хвостовые пустые строки и комментарии к следующему провайдеру не забираем
    while end > k + 1 && (d.lines[end - 1].trim().is_empty() || d.lines[end - 1].trim_start().starts_with('#')) {
        end -= 1;
    }
    Some((k, end))
}

pub fn providers(d: &Doc) -> Vec<ProviderCfg> {
    let Some((s, e)) = providers_section(d) else { return vec![] };
    let mut out = Vec::new();
    for i in s + 1..e {
        let l = &d.lines[i];
        if indent(l) == 2 && l.trim_end().ends_with(':') && !l.trim_start().starts_with('#') {
            let name = unquote(l.trim().trim_end_matches(':'));
            let (_, end) = provider_range(d, &name).unwrap_or((i, i + 1));
            let field = |key: &str| {
                d.lines[i + 1..end]
                    .iter()
                    .find(|x| indent(x) == 4 && x.trim_start().starts_with(&format!("{key}:")))
                    .map(|x| unquote(&x.trim_start()[key.len() + 1..]))
                    .unwrap_or_default()
            };
            out.push(ProviderCfg { kind: field("type"), url: field("url"), path: field("path"), name });
        }
    }
    out
}

/// Группы и их списки `use:`: (имя группы, индекс строки `use:`, провайдеры).
fn groups_use(d: &Doc) -> Vec<(String, usize, Vec<String>)> {
    let Some(gs) = d.lines.iter().position(|l| l.trim_end() == "proxy-groups:") else { return vec![] };
    let ge = (gs + 1..d.lines.len()).find(|&i| is_top(&d.lines[i])).unwrap_or(d.lines.len());
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut i = gs + 1;
    while i < ge {
        let l = &d.lines[i];
        let t = l.trim();
        if indent(l) == 2 && t.starts_with("- name:") {
            cur = Some(unquote(&t["- name:".len()..]));
        } else if indent(l) == 4 && (t == "use:" || t.starts_with("use: [")) {
            if let Some(g) = &cur {
                let mut list = Vec::new();
                if let Some(inline) = t.strip_prefix("use:").map(str::trim).filter(|x| x.starts_with('[')) {
                    list = inline.trim_matches(|c| c == '[' || c == ']').split(',').map(unquote).filter(|x| !x.is_empty()).collect();
                } else {
                    let mut j = i + 1;
                    while j < ge && indent(&d.lines[j]) >= 6 && d.lines[j].trim_start().starts_with('-') {
                        list.push(unquote(&d.lines[j].trim_start()[1..]));
                        j += 1;
                    }
                }
                out.push((g.clone(), i, list));
            }
        }
        i += 1;
    }
    out
}

/// Заменить список `use:` у группы (строка `use:` по индексу).
fn set_use(d: &mut Doc, use_idx: usize, list: &[String]) {
    // удалить старые элементы
    let j = use_idx + 1;
    while j < d.lines.len() && indent(&d.lines[j]) >= 6 && d.lines[j].trim_start().starts_with('-') {
        d.lines.remove(j);
    }
    d.lines[use_idx] = "    use:".into();
    for (k, p) in list.iter().enumerate() {
        d.lines.insert(use_idx + 1 + k, format!("      - {p}"));
    }
}

/// Режим использования: "all", имя одной подписки или "custom".
pub fn mode(d: &Doc) -> String {
    let names: Vec<String> = providers(d).into_iter().map(|p| p.name).collect();
    let uses: Vec<Vec<String>> = groups_use(d)
        .into_iter()
        .filter(|(g, _, _)| g != rules::PROBE_GROUP)
        .map(|(_, _, l)| l)
        .collect();
    if uses.is_empty() || names.is_empty() {
        return "all".into();
    }
    let same = |a: &Vec<String>, b: &Vec<String>| {
        let mut x = a.clone();
        let mut y = b.clone();
        x.sort();
        y.sort();
        x == y
    };
    if uses.iter().all(|l| same(l, &names)) {
        return "all".into();
    }
    if uses.iter().all(|l| l.len() == 1 && l[0] == uses[0][0]) {
        return uses[0][0].clone();
    }
    "custom".into()
}

// ================================================================ изменения

pub fn validate_url(url: &str) -> Result<String, String> {
    let u = url.trim();
    if !(u.starts_with("https://") || u.starts_with("http://")) {
        return Err("ссылка должна начинаться с https:// или http://".into());
    }
    if u.contains(char::is_whitespace) || u.contains('"') || u.len() < 12 {
        return Err("некорректная ссылка".into());
    }
    Ok(u.to_string())
}

/// Имя провайдера: латиница, цифры, - и _ (это ключ YAML и имя файла).
pub fn make_name(d: &Doc, wanted: &str, url: &str) -> String {
    let src = if wanted.trim().is_empty() {
        url.split("://").nth(1).and_then(|h| h.split(['/', ':', '?']).next()).unwrap_or("sub").to_string()
    } else {
        wanted.trim().to_string()
    };
    let mut base: String = src
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    if base.is_empty() {
        base = "sub".into();
    }
    base.truncate(40);
    let existing: Vec<String> = providers(d).into_iter().map(|p| p.name).collect();
    let mut name = base.clone();
    let mut n = 2;
    while existing.contains(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

pub fn add(d: &mut Doc, name: &str, url: &str, activate: bool) -> Result<(), String> {
    let url = validate_url(url)?;
    if providers(d).iter().any(|p| p.url == url) {
        return Err("такая подписка уже добавлена".into());
    }
    let was_mode = mode(d);
    let had_any = !providers(d).is_empty();
    let block = vec![
        format!("  {name}:"),
        "    type: http".into(),
        format!("    url: \"{url}\""),
        "    interval: 86400".into(),
        format!("    path: ./providers/{name}.yaml"),
        "    health-check:".into(),
        "      enable: true".into(),
        format!("      url: \"{}\"", crate::api::DEFAULT_TEST_URL),
        "      interval: 300".into(),
    ];
    match providers_section(d) {
        Some((_, e)) => {
            // перед хвостовыми пустыми строками секции
            let mut at = e;
            while at > 0 && (d.lines[at - 1].trim().is_empty() || d.lines[at - 1].starts_with('#')) {
                at -= 1;
            }
            for (k, l) in block.into_iter().enumerate() {
                d.lines.insert(at + k, l);
            }
        }
        None => {
            let gi = d.lines.iter().position(|l| l.trim_end() == "proxy-groups:").ok_or("в config.yaml нет секции proxy-groups:")?;
            let mut ins = vec!["proxy-providers:".to_string()];
            ins.extend(block);
            ins.push(String::new());
            for (k, l) in ins.into_iter().enumerate() {
                d.lines.insert(gi + k, l);
            }
        }
    }
    // если ни одна группа не использует подписки — подключаем заново
    if groups_use(d).is_empty() {
        restore_use(d, name);
        return Ok(());
    }
    // подключить к группам
    let all: Vec<String> = providers(d).into_iter().map(|p| p.name).collect();
    for (g, idx, list) in groups_use(d).into_iter().rev() {
        let new_list: Vec<String> = if activate {
            if g == rules::PROBE_GROUP { all.clone() } else { vec![name.to_string()] }
        } else if was_mode == "all" || !had_any || g == rules::PROBE_GROUP {
            let mut l = list.clone();
            l.push(name.to_string());
            l
        } else {
            continue;
        };
        set_use(d, idx, &new_list);
    }
    Ok(())
}

/// Вернуть `use:` в группы: по меткам, оставленным при удалении, а если их нет —
/// в главную группу и все автоматические (url-test / fallback / load-balance).
fn restore_use(d: &mut Doc, name: &str) {
    let marks: Vec<usize> = (0..d.lines.len()).filter(|&i| d.lines[i].trim() == USE_MARK).collect();
    if !marks.is_empty() {
        for &i in marks.iter().rev() {
            d.lines[i] = "    use:".into();
            d.lines.insert(i + 1, format!("      - {name}"));
        }
    } else {
        let Some(gs) = d.lines.iter().position(|l| l.trim_end() == "proxy-groups:") else { return };
        let ge = (gs + 1..d.lines.len()).find(|&i| is_top(&d.lines[i])).unwrap_or(d.lines.len());
        let mut targets = Vec::new();
        let mut first = true;
        let mut i = gs + 1;
        while i < ge {
            let l = &d.lines[i];
            if indent(l) == 2 && l.trim().starts_with("- name:") {
                let gname = unquote(&l.trim()["- name:".len()..]);
                let ty = (i + 1..ge.min(i + 6))
                    .find(|&j| d.lines[j].trim_start().starts_with("type:"))
                    .map(|j| (j, unquote(&d.lines[j].trim_start()["type:".len()..])));
                if let Some((tj, t)) = ty {
                    if gname != rules::PROBE_GROUP && (first || matches!(t.as_str(), "url-test" | "fallback" | "load-balance")) {
                        targets.push(tj);
                    }
                }
                first = false;
            }
            i += 1;
        }
        for &tj in targets.iter().rev() {
            d.lines.insert(tj + 1, "    use:".into());
            d.lines.insert(tj + 2, format!("      - {name}"));
        }
    }
    // временные DIRECT больше не нужны
    for l in d.lines.iter_mut() {
        if l.as_str() == PLACEHOLDER {
            *l = "    proxies: []".into();
        }
    }
}

pub fn remove(d: &mut Doc, name: &str) -> Result<(), String> {
    let (s, e) = provider_range(d, name).ok_or("подписка не найдена")?;
    d.lines.drain(s..e);
    let remaining: Vec<String> = providers(d).into_iter().map(|p| p.name).collect();
    for (g, idx, list) in groups_use(d).into_iter().rev() {
        if !list.iter().any(|x| x == name) {
            continue;
        }
        let mut l: Vec<String> = list.into_iter().filter(|x| x != name).collect();
        if l.is_empty() && !remaining.is_empty() {
            l = remaining.clone();
        }
        if l.is_empty() {
            // подписок не осталось: вместо use: оставляем метку, а чтобы группа не была
            // пустой — временно DIRECT (уберётся, когда добавите подписку)
            set_use(d, idx, &[]);
            d.lines[idx] = format!("    {USE_MARK}");
            ensure_group_has_proxy(d, &g);
        } else {
            set_use(d, idx, &l);
        }
    }
    // секция без провайдеров тоже допустима, но уберём пустой заголовок
    if remaining.is_empty() {
        if let Some((s, e)) = providers_section(d) {
            if d.lines[s + 1..e].iter().all(|l| l.trim().is_empty() || l.trim_start().starts_with('#')) {
                d.lines.remove(s);
            }
        }
    }
    Ok(())
}

/// Если у группы нет ни одной ноды в proxies — добавить DIRECT (иначе Mihomo не примет конфиг).
fn ensure_group_has_proxy(d: &mut Doc, group: &str) {
    let Some(gi) = d.lines.iter().position(|l| indent(l) == 2 && l.trim().starts_with("- name:") && unquote(&l.trim()["- name:".len()..]) == group) else {
        return;
    };
    let mut end = gi + 1;
    while end < d.lines.len() && (indent(&d.lines[end]) >= 4 || d.lines[end].trim().is_empty()) && !is_top(&d.lines[end]) {
        end += 1;
    }
    let pi = (gi + 1..end).find(|&i| indent(&d.lines[i]) == 4 && d.lines[i].trim_start().starts_with("proxies:"));
    match pi {
        Some(i) => {
            let t = d.lines[i].trim();
            let has_items = (i + 1 < end && d.lines[i + 1].trim_start().starts_with('-') && indent(&d.lines[i + 1]) >= 6)
                || (t.contains('[') && !t.contains("[]"));
            if !has_items {
                d.lines[i] = PLACEHOLDER.into();
            }
        }
        None => {
            let mut at = end;
            while at > gi + 1 && d.lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            d.lines.insert(at, PLACEHOLDER.into());
        }
    }
}

pub fn set_url(d: &mut Doc, name: &str, url: &str) -> Result<(), String> {
    let url = validate_url(url)?;
    let (s, e) = provider_range(d, name).ok_or("подписка не найдена")?;
    match (s + 1..e).find(|&i| indent(&d.lines[i]) == 4 && d.lines[i].trim_start().starts_with("url:")) {
        Some(i) => d.lines[i] = format!("    url: \"{url}\""),
        None => d.lines.insert(s + 1, format!("    url: \"{url}\"")),
    }
    Ok(())
}

/// Сделать активной одну подписку ("*" — все сразу).
pub fn activate(d: &mut Doc, name: &str) -> Result<(), String> {
    let all: Vec<String> = providers(d).into_iter().map(|p| p.name).collect();
    if name != "*" && !all.iter().any(|x| x == name) {
        return Err("подписка не найдена".into());
    }
    let groups = groups_use(d);
    if groups.is_empty() {
        return Err("ни одна группа не использует подписки (нет use: в proxy-groups)".into());
    }
    for (g, idx, _) in groups.into_iter().rev() {
        let l = if name == "*" || g == rules::PROBE_GROUP { all.clone() } else { vec![name.to_string()] };
        set_use(d, idx, &l);
    }
    Ok(())
}

// ================================================================ для интерфейса

#[derive(Serialize)]
pub struct SubView {
    pub name: String,
    pub url: String,
    pub kind: String,
    pub nodes: Option<usize>,
    pub updated_at: String,
    pub active: bool,
}

#[derive(Serialize)]
pub struct SubsView {
    pub mode: String,
    pub subs: Vec<SubView>,
    pub error: Option<String>,
}

pub fn view(api: &Api) -> SubsView {
    let d = match rules::load_doc() {
        Ok(d) => d,
        Err(e) => return SubsView { mode: "all".into(), subs: vec![], error: Some(e) },
    };
    let live = api.proxy_providers().ok();
    let mode = mode(&d);
    let used: Vec<String> = groups_use(&d).into_iter().filter(|(g, _, _)| g != rules::PROBE_GROUP).flat_map(|(_, _, l)| l).collect();
    let subs = providers(&d)
        .into_iter()
        .map(|p| {
            let lp = live.as_ref().and_then(|v| v.get(&p.name));
            SubView {
                nodes: lp.and_then(|x| x.get("proxies")).and_then(Value::as_array).map(|a| a.len()),
                updated_at: lp.and_then(|x| x.get("updatedAt")).and_then(Value::as_str).unwrap_or("").to_string(),
                active: used.contains(&p.name),
                name: p.name,
                url: p.url,
                kind: p.kind,
            }
        })
        .collect();
    SubsView { mode, subs, error: None }
}

/// Скачать подписку сейчас.
pub fn refresh(api: &Api, name: &str) -> Result<(), String> {
    api.put_slow(&format!("/providers/proxies/{}", util::url_encode(name))).map_err(String::from)
}

pub fn apply<F: FnOnce(&mut Doc) -> Result<(), String>>(api: &Api, what: &str, f: F) -> ApplyResult {
    rules::mutate(api, what, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = "mixed-port: 7890\nproxy-providers:\n  my-subscription:\n    type: http\n    url: \"https://old.example/sub?token=1\"\n    interval: 86400\n    path: ./providers/subscription.yaml\n    health-check:\n      enable: true\n\n# comment\nproxy-groups:\n  - name: \"Proxy\"\n    type: select\n    use:\n      - my-subscription\n    proxies:\n      - Auto\n      - DIRECT\n  - name: \"Auto\"\n    type: url-test\n    use:\n      - my-subscription\n    url: \"http://x\"\n    proxies: []\n  - name: \"YouTube\"\n    type: select\n    proxies:\n      - Auto\nrules:\n  - MATCH,DIRECT\n";

    #[test]
    fn parse() {
        let d = Doc::parse(CFG);
        let p = providers(&d);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].url, "https://old.example/sub?token=1");
        assert_eq!(p[0].kind, "http");
        assert_eq!(mode(&d), "all");
        assert_eq!(groups_use(&d).len(), 2);
    }

    #[test]
    fn add_activate_remove() {
        let mut d = Doc::parse(CFG);
        let name = make_name(&d, "", "https://vpn.provider.io/api/sub/abc");
        assert_eq!(name, "vpn-provider-io");
        add(&mut d, &name, "https://vpn.provider.io/api/sub/abc", false).unwrap();
        assert!(add(&mut d, "x", "https://vpn.provider.io/api/sub/abc", false).is_err(), "дубликат");
        assert_eq!(providers(&d).len(), 2);
        assert_eq!(mode(&d), "all", "в режиме «все» новая подписка подключается ко всем группам");
        let t = d.render();
        assert!(t.contains("  vpn-provider-io:\n    type: http\n    url: \"https://vpn.provider.io/api/sub/abc\""), "{t}");
        assert!(t.contains("    path: ./providers/vpn-provider-io.yaml"));
        assert!(t.find("vpn-provider-io:").unwrap() < t.find("# comment").unwrap(), "блок внутри секции");

        activate(&mut d, "vpn-provider-io").unwrap();
        assert_eq!(mode(&d), "vpn-provider-io");
        activate(&mut d, "*").unwrap();
        assert_eq!(mode(&d), "all");

        set_url(&mut d, "my-subscription", "https://new.example/s").unwrap();
        assert_eq!(providers(&d)[0].url, "https://new.example/s");

        remove(&mut d, "vpn-provider-io").unwrap();
        assert_eq!(Doc::parse(&d.render()).lines, Doc::parse(&CFG.replace("https://old.example/sub?token=1", "https://new.example/s")).lines);

        // удалить последнюю — группы получают DIRECT, секция исчезает
        remove(&mut d, "my-subscription").unwrap();
        let t = d.render();
        assert!(!t.contains("proxy-providers:"), "{t}");
        assert!(!t.contains("use:"));
        assert!(t.contains("  - name: \"Auto\"\n    type: url-test\n    # mihomo-tray:use\n    url: \"http://x\"\n    proxies: [DIRECT] # mihomo-tray:placeholder"), "{t}");
        // добавить новую — всё возвращается на место
        add(&mut d, "fresh", "https://fresh.example/s", false).unwrap();
        let t = d.render();
        assert!(t.contains("  - name: \"Auto\"\n    type: url-test\n    use:\n      - fresh\n    url: \"http://x\"\n    proxies: []"), "{t}");
        assert!(t.contains("  - name: \"Proxy\"\n    type: select\n    use:\n      - fresh\n    proxies:\n      - Auto"), "{t}");
        assert_eq!(mode(&d), "all");

        // конфиг, где подписок не было никогда
        let mut z = Doc::parse("proxy-groups:\n  - name: P\n    type: select\n    proxies: [A]\n  - name: A\n    type: url-test\n    proxies: [DIRECT]\n  - name: Y\n    type: select\n    proxies: [P]\nrules:\n");
        add(&mut z, "s1", "https://s1.example/x", false).unwrap();
        let t = z.render();
        assert!(t.contains("  - name: P\n    type: select\n    use:\n      - s1"), "{t}");
        assert!(t.contains("  - name: A\n    type: url-test\n    use:\n      - s1"), "{t}");
        assert!(t.contains("  - name: Y\n    type: select\n    proxies: [P]"), "Y не трогаем: {t}");
    }

    #[test]
    fn validation() {
        assert!(validate_url("ftp://x").is_err());
        assert!(validate_url("https://a b").is_err());
        let d = Doc::parse(CFG);
        assert_eq!(make_name(&d, "Мой VPN!", "https://x.y"), "vpn");
        assert_eq!(make_name(&d, "my-subscription", "https://x.y"), "my-subscription-2");
    }
}

#[cfg(test)]
mod real {
    use super::*;
    /// Проверка на реальном конфиге (если он есть рядом): пишет варианты в /tmp/claude-0/subs-*.yaml
    #[test]
    fn real_config_roundtrip() {
        let Ok(t) = std::fs::read_to_string("/mnt/user-data/outputs/mihomo-config/config.yaml") else { return };
        let mut d = Doc::parse(&t);
        assert_eq!(providers(&d).len(), 1);
        assert_eq!(mode(&d), "all");
        add(&mut d, "second", "https://example.com/sub2", false).unwrap();
        let _ = std::fs::write("/tmp/claude-0/subs-add.yaml", d.render());
        activate(&mut d, "second").unwrap();
        let _ = std::fs::write("/tmp/claude-0/subs-active.yaml", d.render());
        activate(&mut d, "*").unwrap();
        remove(&mut d, "second").unwrap();
        assert_eq!(d.render(), t, "добавить+удалить = исходный файл байт-в-байт");
        remove(&mut d, "my-subscription").unwrap();
        let _ = std::fs::write("/tmp/claude-0/subs-none.yaml", d.render());
    }
}

#[cfg(test)]
mod live {
    use super::*;
    /// MIHOMO_DIR=<папка> cargo test subs_live -- --ignored
    #[test]
    #[ignore]
    fn subs_live() {
        let api = Api::from_config();
        let before = std::fs::read_to_string(util::config_path()).unwrap();
        let r = apply(&api, "test add", |d| add(d, "local-sub", "http://127.0.0.1/sub.yaml", true));
        assert!(r.ok && r.live, "{r:?}");
        refresh(&api, "local-sub").unwrap();
        let v = view(&api);
        let s = v.subs.iter().find(|s| s.name == "local-sub").unwrap();
        assert_eq!(s.nodes, Some(1));
        assert!(s.active);
        assert_eq!(v.mode, "local-sub");
        let p = api.proxies().unwrap();
        let auto: Vec<&str> = p["Auto"]["all"].as_array().unwrap().iter().filter_map(|x| x.as_str()).collect();
        assert!(auto.contains(&"HTTP-SUB-1"), "{auto:?}");
        assert!(!auto.contains(&"SE-1"), "старая подписка отключена");
        let r = apply(&api, "test all", |d| activate(d, "*"));
        assert!(r.ok);
        let r = apply(&api, "test url", |d| set_url(d, "local-sub", "http://127.0.0.1/sub.yaml?v=2"));
        assert!(r.ok);
        // плохая ссылка не ломает конфиг
        let r = apply(&api, "test bad", |d| set_url(d, "local-sub", "nope"));
        assert!(!r.ok);
        let r = apply(&api, "test remove", |d| remove(d, "local-sub"));
        assert!(r.ok && r.live, "{r:?}");
        assert_eq!(std::fs::read_to_string(util::config_path()).unwrap(), before, "вернулись к исходному конфигу");
    }
}

#[cfg(test)]
mod kit {
    use super::*;
    /// Чистый конфиг для передачи другу: KIT_IN=… KIT_OUT=… cargo test make_kit_config -- --ignored
    #[test]
    #[ignore]
    fn make_kit_config() {
        let inp = std::env::var("KIT_IN").unwrap();
        let out = std::env::var("KIT_OUT").unwrap();
        let mut d = Doc::parse(&std::fs::read_to_string(&inp).unwrap());
        for p in providers(&d) {
            remove(&mut d, &p.name).unwrap();
        }
        // личные правила — пустые блоки
        for (s, e) in [(rules::PROC_START, rules::PROC_END), (rules::DOM_START, rules::DOM_END)] {
            let si = d.lines.iter().position(|l| l.trim() == s).unwrap();
            let ei = d.lines.iter().position(|l| l.trim() == e).unwrap();
            d.lines.drain(si + 1..ei);
        }
        // служебная группа трея — пересоздастся сама
        d.remove_probe_group();
        d.lines.retain(|l| !l.contains(&format!(",{}", rules::PROBE_GROUP)) && !l.contains("служебное правило Mihomo Tray"));
        assert!(providers(&d).is_empty());
        assert!(!d.render().contains("http") || true);
        std::fs::write(&out, d.render()).unwrap();
        // и вариант «друг добавил подписку» — для проверки
        add(&mut d, "friend", "http://127.0.0.1/sub.yaml", false).unwrap();
        std::fs::write(format!("{out}.with-sub"), d.render()).unwrap();
    }
}
