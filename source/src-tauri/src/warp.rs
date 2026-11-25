//! Автоматический служебный Cloudflare WARP (WireGuard) для обхода блокировок
//! при обновлении подписок, плюс автоисправление сломанного DNS в config.yaml.
//!
//! Ключ генерируется локально на ПК пользователя (X25519) и регистрируется
//! напрямую через официальный API Cloudflare WARP. Нода `WARP-Sub` скрыта
//! (`hidden: true`) и используется только для скачивания проблемных подписок.

use crate::rules::Doc;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;
use x25519_dalek::{PublicKey, StaticSecret};

pub const WARP_PROXY_NAME: &str = "WARP-Sub";
pub const WARP_DIALER_GROUP: &str = "WARP-Dialer";
const WARP_REG_URL: &str = "https://api.cloudflareclient.com/v0a2158/reg";
const WARP_ENDPOINT_IP: &str = "162.159.192.1";
const WARP_ENDPOINT_PORT: u16 = 2408;

#[derive(Clone, Debug, PartialEq)]
pub struct WarpConfig {
    pub private_key: String,
    pub public_key: String,
    pub ipv4: String,
    pub ipv6: String,
    pub reserved: [u8; 3],
}

fn generate_keypair() -> Result<(String, String), String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("генерация ключа: {e}"))?;
    let secret = StaticSecret::from(bytes);
    let public = PublicKey::from(&secret);
    Ok((B64.encode(secret.to_bytes()), B64.encode(public.as_bytes())))
}

/// Зарегистрировать новый бесплатный ключ Cloudflare WARP.
/// Если передан `mixed_port` (локальный прокси Mihomo), сначала пробует через него,
/// чтобы обойти возможную блокировку API у провайдера; при неудаче — напрямую.
pub fn register(mixed_port: Option<u16>) -> Result<WarpConfig, String> {
    let (priv_b64, pub_b64) = generate_keypair()?;
    let tos = format!("{}.000Z", crate::util::timestamp(false).trim_end_matches('Z').replace(' ', "T"));
    let body = json!({
        "key": pub_b64,
        "install_id": "",
        "fcm_token": "",
        "tos": tos,
        "model": "PC",
        "serial_number": "",
        "locale": "en_US"
    });

    let call = |proxy_url: Option<String>| -> Result<Value, String> {
        let mut b = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(6))
            .timeout(Duration::from_secs(12));
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(std::sync::Arc::new(tls));
        }
        if let Some(p) = proxy_url {
            if let Ok(px) = ureq::Proxy::new(&p) {
                b = b.proxy(px);
            }
        }
        let resp = b
            .build()
            .post(WARP_REG_URL)
            .set("Content-Type", "application/json; charset=UTF-8")
            .set("CF-Client-Version", "a-6.11-2223")
            .set("User-Agent", "okhttp/3.12.1")
            .send_json(&body)
            .map_err(|e| format!("регистрация WARP: {e}"))?;
        let text = resp.into_string().map_err(|e| format!("чтение ответа WARP: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("JSON WARP: {e}"))
    };

    let json_val = match mixed_port {
        Some(port) => call(Some(format!("http://127.0.0.1:{port}"))).or_else(|_| call(None))?,
        None => call(None)?,
    };

    let cfg = json_val.get("config").ok_or("WARP: нет секции config в ответе")?;
    let addrs = cfg.get("interface").and_then(|i| i.get("addresses")).ok_or("WARP: нет addresses")?;
    let ipv4 = addrs.get("v4").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("WARP: нет IPv4")?.to_string();
    let ipv6 = addrs.get("v6").and_then(Value::as_str).unwrap_or("").to_string();
    let peer_pub = cfg
        .get("peers")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|p| p.get("public_key"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("WARP: нет public_key пира")?
        .to_string();
    let cid = cfg.get("client_id").and_then(Value::as_str).ok_or("WARP: нет client_id")?;
    let cid_bytes = B64.decode(cid).map_err(|e| format!("WARP client_id: {e}"))?;
    if cid_bytes.len() < 3 {
        return Err("WARP: короткий client_id".into());
    }

    Ok(WarpConfig {
        private_key: priv_b64,
        public_key: peer_pub,
        ipv4,
        ipv6,
        reserved: [cid_bytes[0], cid_bytes[1], cid_bytes[2]],
    })
}

// ================================================================ правка config.yaml

fn indent(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

fn is_top(l: &str) -> bool {
    !l.is_empty() && !l.starts_with(char::is_whitespace) && !l.starts_with('#') && !l.starts_with('-')
}

fn proxies_section(d: &Doc) -> Option<(usize, usize)> {
    let s = d.lines.iter().position(|l| l.trim_end() == "proxies:")?;
    let e = (s + 1..d.lines.len()).find(|&i| is_top(&d.lines[i])).unwrap_or(d.lines.len());
    Some((s, e))
}

fn warp_entry_range(d: &Doc) -> Option<(usize, usize)> {
    let (s, e) = proxies_section(d)?;
    let k = (s + 1..e).find(|&i| {
        let t = d.lines[i].trim();
        indent(&d.lines[i]) == 2
            && t.starts_with("- name:")
            && t["- name:".len()..].trim().trim_matches(|c| c == '"' || c == '\'') == WARP_PROXY_NAME
    })?;
    let mut end = k + 1;
    while end < e {
        let l = &d.lines[end];
        if !l.trim().is_empty() && indent(l) <= 2 && !l.trim_start().starts_with('#') {
            break;
        }
        end += 1;
    }
    while end > k + 1 && d.lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some((k, end))
}

pub fn has_warp_proxy(d: &Doc) -> bool {
    warp_entry_range(d).is_some()
}

/// Добавить или обновить скрытую ноду `WARP-Sub` в секции `proxies:`.
pub fn ensure_warp_proxy(d: &mut Doc, cfg: &WarpConfig, dialer_proxy: Option<&str>) {
    let mut block = vec![
        format!("  - name: \"{WARP_PROXY_NAME}\""),
        "    type: wireguard".into(),
        format!("    server: {WARP_ENDPOINT_IP}"),
        format!("    port: {WARP_ENDPOINT_PORT}"),
        format!("    ip: {}", cfg.ipv4),
    ];
    if !cfg.ipv6.is_empty() {
        block.push(format!("    ipv6: {}", cfg.ipv6));
    }
    block.push(format!("    private-key: \"{}\"", cfg.private_key));
    block.push(format!("    public-key: \"{}\"", cfg.public_key));
    block.push(format!("    reserved: [{}, {}, {}]", cfg.reserved[0], cfg.reserved[1], cfg.reserved[2]));
    block.push("    mtu: 1280".into());
    block.push("    udp: true".into());
    block.push("    remote-dns-resolve: true".into());
    block.push("    dns: [1.1.1.1]".into());
    if let Some(dp) = dialer_proxy.filter(|s| !s.is_empty()) {
        block.push(format!("    dialer-proxy: \"{dp}\""));
    }
    block.push("    hidden: true".into());

    if let Some((s, e)) = warp_entry_range(d) {
        d.lines.splice(s..e, block);
        return;
    }

    if let Some((_, e)) = proxies_section(d) {
        let mut at = e;
        while at > 0 && (d.lines[at - 1].trim().is_empty() || d.lines[at - 1].starts_with('#')) {
            at -= 1;
        }
        for (k, l) in block.into_iter().enumerate() {
            d.lines.insert(at + k, l);
        }
    } else {
        let insert_at = d
            .lines
            .iter()
            .position(|l| l.trim_end() == "proxy-providers:" || l.trim_end() == "proxy-groups:")
            .unwrap_or(d.lines.len());
        let mut ins = vec!["proxies:".to_string()];
        ins.extend(block);
        ins.push(String::new());
        for (k, l) in ins.into_iter().enumerate() {
            d.lines.insert(insert_at + k, l);
        }
    }
}

/// Изменить `dialer-proxy` у уже существующей ноды `WARP-Sub`.
pub fn set_warp_dialer(d: &mut Doc, dialer_proxy: Option<&str>) {
    let Some((s, e)) = warp_entry_range(d) else { return };
    let existing = (s + 1..e).find(|&i| indent(&d.lines[i]) == 4 && d.lines[i].trim_start().starts_with("dialer-proxy:"));
    match (existing, dialer_proxy.filter(|x| !x.is_empty())) {
        (Some(i), Some(dp)) => d.lines[i] = format!("    dialer-proxy: \"{dp}\""),
        (Some(i), None) => {
            d.lines.remove(i);
        }
        (None, Some(dp)) => d.lines.insert(e, format!("    dialer-proxy: \"{dp}\"")),
        (None, None) => {}
    }
}

/// Убедиться, что в `proxy-groups:` есть скрытая группа `WARP-Dialer` (type: select),
/// через которую `WARP-Sub` может перебирать ноды подписок по API без перезагрузки конфига.
pub fn ensure_warp_dialer_group(d: &mut Doc, provider_names: &[String]) {
    let Some(gs) = d.lines.iter().position(|l| l.trim_end() == "proxy-groups:") else { return };
    let ge = (gs + 1..d.lines.len()).find(|&i| is_top(&d.lines[i])).unwrap_or(d.lines.len());
    let exists = (gs + 1..ge).any(|i| {
        let t = d.lines[i].trim();
        indent(&d.lines[i]) == 2
            && t.starts_with("- name:")
            && t["- name:".len()..].trim().trim_matches(|c| c == '"' || c == '\'') == WARP_DIALER_GROUP
    });
    if exists {
        return;
    }
    let mut block = vec![
        format!("  - name: \"{WARP_DIALER_GROUP}\""),
        "    type: select".into(),
        "    hidden: true".into(),
    ];
    if !provider_names.is_empty() {
        block.push("    use:".into());
        for p in provider_names {
            block.push(format!("      - {p}"));
        }
    }
    block.push("    proxies:".into());
    block.push("      - Auto".into());

    let mut at = ge;
    while at > gs + 1 && (d.lines[at - 1].trim().is_empty() || d.lines[at - 1].starts_with('#')) {
        at -= 1;
    }
    for (k, l) in block.into_iter().enumerate() {
        d.lines.insert(at + k, l);
    }
}

// ================================================================ автоисправление сломанного DNS

/// Заменить сломанный `https://77.88.8.8/dns-query` (Яндекс не поддерживает DoH на IP 77.88.8.8)
/// на рабочие DoH-серверы AdGuard / Google. Кастомные настройки пользователя не трогаем.
pub fn heal_broken_dns(d: &mut Doc) -> bool {
    let Some(s) = d.lines.iter().position(|l| l.trim_end() == "dns:") else { return false };
    let e = (s + 1..d.lines.len()).find(|&i| is_top(&d.lines[i])).unwrap_or(d.lines.len());
    let has_broken = d.lines[s + 1..e]
        .iter()
        .any(|l| !l.trim_start().starts_with('#') && l.contains("https://77.88.8.8/dns-query"));
    if !has_broken {
        return false;
    }
    let mut changed = false;
    for l in &mut d.lines[s + 1..e] {
        if l.trim_start().starts_with('#') {
            continue;
        }
        if l.contains("https://77.88.8.8/dns-query") {
            *l = l.replace("https://77.88.8.8/dns-query", "https://dns.adguard-dns.com/dns-query");
            changed = true;
        } else if l.contains("https://1.1.1.1/dns-query") {
            *l = l.replace("https://1.1.1.1/dns-query", "https://dns.google/dns-query");
            changed = true;
        } else if l.trim() == "- 1.1.1.1" {
            *l = l.replace("1.1.1.1", "8.8.8.8");
            changed = true;
        }
    }
    changed
}

// ================================================================ частичный bootstrap при 16 КБ-обрыве

/// Попытаться скачать начало подписки (до обрыва ТСПУ на 16 КБ) и спасти хотя бы 1–3 полные ноды,
/// чтобы Mihomo смог подняться и зарегистрировать WARP поверх рабочей ноды.
pub fn try_partial_bootstrap(url: &str) -> Option<String> {
    let mut b = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .timeout_read(Duration::from_secs(5));
    if let Ok(tls) = native_tls::TlsConnector::new() {
        b = b.tls_connector(std::sync::Arc::new(tls));
    }
    let agent = b.build();
    let resp = agent
        .get(url)
        .set("User-Agent", "clash.meta")
        .call()
        .ok()?;
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 1024];
    let mut reader = resp.into_reader();
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= 128 * 1024 {
                    break;
                }
            }
            Err(_) => break, // обрыв/таймаут — используем то, что успело долететь
        }
    }
    let text = String::from_utf8_lossy(&buf);
    salvage_partial_yaml(&text)
}

/// Из обрезанного Clash YAML вытаскивает все завершённые ноды из секции `proxies:`.
pub fn salvage_partial_yaml(raw: &str) -> Option<String> {
    let lines: Vec<&str> = raw.lines().map(|l| l.trim_end_matches('\r')).collect();
    let p_idx = lines.iter().position(|l| l.trim_end() == "proxies:")?;
    let mut item_starts: Vec<usize> = Vec::new();
    let mut end_idx = lines.len();
    for (i, &l) in lines.iter().enumerate().skip(p_idx + 1) {
        if is_top(l) {
            end_idx = i;
            break;
        }
        if indent(l) == 2 && l.trim_start().starts_with("- name:") {
            item_starts.push(i);
        }
    }
    // Если файл дошёл до следующей секции — все ноды целые; иначе отбрасываем последнюю (обрезанную).
    let cut = if end_idx < lines.len() {
        if item_starts.is_empty() { return None; }
        end_idx
    } else if item_starts.len() >= 2 {
        *item_starts.last()?
    } else {
        return None;
    };
    let body = &lines[p_idx + 1..cut];
    if body.is_empty() {
        return None;
    }
    let mut out = String::from("proxies:\n");
    for l in body {
        out.push_str(l);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warp_proxy_insert_and_update() {
        let mut d = Doc::parse("mixed-port: 7890\nproxy-providers:\n  sub:\n    type: http\nproxy-groups:\n  - name: Proxy\n");
        let cfg = WarpConfig {
            private_key: "PRIV=".into(),
            public_key: "PUB=".into(),
            ipv4: "172.16.0.2".into(),
            ipv6: "2606::1".into(),
            reserved: [1, 2, 3],
        };
        assert!(!has_warp_proxy(&d));
        ensure_warp_proxy(&mut d, &cfg, Some("Auto"));
        assert!(has_warp_proxy(&d));
        let rendered = d.render();
        assert!(rendered.contains("proxies:\n  - name: \"WARP-Sub\""));
        assert!(rendered.contains("dialer-proxy: \"Auto\""));
        assert!(rendered.contains("hidden: true"));

        set_warp_dialer(&mut d, None);
        assert!(!d.render().contains("dialer-proxy:"));
        set_warp_dialer(&mut d, Some("Proxy"));
        assert!(d.render().contains("dialer-proxy: \"Proxy\""));
    }

    #[test]
    fn heal_dns_only_when_broken() {
        let broken = "dns:\n  enable: true\n  default-nameserver:\n    - 77.88.8.8\n    - 1.1.1.1\n  nameserver:\n    - https://77.88.8.8/dns-query\n    - https://1.1.1.1/dns-query\nrules:\n";
        let mut d = Doc::parse(broken);
        assert!(heal_broken_dns(&mut d));
        let t = d.render();
        assert!(!t.contains("https://77.88.8.8/dns-query"));
        assert!(t.contains("https://dns.adguard-dns.com/dns-query"));
        assert!(t.contains("https://dns.google/dns-query"));
        assert!(t.contains("- 8.8.8.8"));
        // повторный вызов ничего не меняет
        assert!(!heal_broken_dns(&mut d));

        // кастомный DNS не трогаем
        let custom = "dns:\n  nameserver:\n    - https://1.1.1.1/dns-query\n";
        let mut d2 = Doc::parse(custom);
        assert!(!heal_broken_dns(&mut d2));
    }

    #[test]
    fn salvage_truncated_yaml() {
        let partial = "mixed-port: 7890\nproxies:\n  - name: NL-1\n    type: vless\n    server: 1.2.3.4\n  - name: NL-2\n    type: vless\n    server: 5.6.7.8\n  - name: DE-3\n    type: vle";
        let salvaged = salvage_partial_yaml(partial).unwrap();
        assert!(salvaged.contains("NL-1"));
        assert!(salvaged.contains("NL-2"));
        assert!(!salvaged.contains("DE-3"));
    }
}
