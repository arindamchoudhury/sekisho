//! An A2A agent's card, checked before Sekisho pins it (D163, design 9.8):
//! a JSON object of at most 1 MiB with a `name` and either v1.0's
//! `supportedInterfaces` or v0.3's `url`. Plan 5 serves the pinned copy.

use serde_json::Value;

use crate::model::PinnedCard;
use crate::unicode::scan;

/// The largest card read.
pub const CARD_LIMIT: usize = 1 << 20;

/// A v0.3 card that names no transport speaks JSON-RPC.
const V03_DEFAULT_BINDING: &str = "JSONRPC";

fn text<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn push_once(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|v| v == value) {
        list.push(value.to_owned());
    }
}

/// Checks a card as fetched, and keeps the versions and bindings it lists.
pub fn check_card(bytes: &[u8]) -> Result<PinnedCard, String> {
    if bytes.len() > CARD_LIMIT {
        return Err("the card is larger than 1 MiB".to_owned());
    }
    let Ok(body) = std::str::from_utf8(bytes) else {
        return Err("the card isn't UTF-8 text".to_owned());
    };
    if let Some(found) = scan(body).critical.first() {
        return Err(format!("the card holds a hidden character at {found}"));
    }
    let json: Value =
        serde_json::from_str(body).map_err(|e| format!("the card isn't JSON: {e}"))?;
    let Some(obj) = json.as_object() else {
        return Err("the card isn't a JSON object".to_owned());
    };
    if text(obj, "name").is_none() {
        return Err("the card has no name".to_owned());
    }
    let mut versions = Vec::new();
    let mut bindings = Vec::new();
    if let Some(interfaces) = obj.get("supportedInterfaces") {
        let list = interfaces
            .as_array()
            .filter(|l| !l.is_empty())
            .ok_or("the card's supportedInterfaces lists no interface")?;
        for (i, interface) in list.iter().enumerate() {
            let fields = interface
                .as_object()
                .and_then(|o| Some((text(o, "protocolBinding")?, text(o, "protocolVersion")?)));
            let Some((binding, version)) = fields else {
                return Err(format!(
                    "the card's supportedInterfaces[{i}] needs a protocolBinding and a protocolVersion"
                ));
            };
            push_once(&mut versions, version);
            push_once(&mut bindings, binding);
        }
    } else if text(obj, "url").is_some() {
        if let Some(version) = text(obj, "protocolVersion") {
            push_once(&mut versions, version);
        }
        push_once(
            &mut bindings,
            text(obj, "preferredTransport").unwrap_or(V03_DEFAULT_BINDING),
        );
        let more = obj.get("additionalInterfaces").and_then(Value::as_array);
        for interface in more.into_iter().flatten() {
            if let Some(transport) = interface.as_object().and_then(|o| text(o, "transport")) {
                push_once(&mut bindings, transport);
            }
        }
    } else {
        return Err(
            "the card names no interface: v1.0's supportedInterfaces or v0.3's url".to_owned(),
        );
    }
    Ok(PinnedCard {
        json,
        versions,
        bindings,
    })
}
