use std::collections::HashMap;

pub fn t(lang: &str, key: &str) -> String {
    TRANSLATIONS
        .get(lang)
        .and_then(|m| m.get(key))
        .or_else(|| TRANSLATIONS.get("en").and_then(|m| m.get(key)))
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

#[allow(dead_code)]
pub fn available_languages() -> Vec<(&'static str, &'static str)> {
    vec![
        ("en", "English"),
        ("de", "Deutsch"),
        ("es", "Español"),
        ("fr", "Français"),
        ("pt_BR", "Português (BR)"),
        ("it", "Italiano"),
        ("tr", "Türkçe"),
        ("ru", "Русский"),
        ("zh_CN", "中文"),
        ("jpn_JP", "日本語"),
        ("ko", "한국어"),
        ("ar", "العربية"),
        ("ca", "Català"),
        ("nl", "Nederlands"),
        ("pl", "Polski"),
        ("ro", "Română"),
        ("ukr_UA", "Українська"),
        ("he", "עברית"),
    ]
}

fn flatten_json(value: &serde_json::Value, prefix: &str, map: &mut HashMap<String, String>) {
    match value {
        serde_json::Value::Object(obj) => {
            for (k, v) in obj {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                flatten_json(v, &key, map);
            }
        }
        serde_json::Value::String(s) => {
            map.insert(prefix.to_string(), s.clone());
        }
        _ => {}
    }
}

lazy_static::lazy_static! {
    static ref TRANSLATIONS: HashMap<&'static str, HashMap<String, String>> = {
        let mut m = HashMap::new();
        for (code, json) in [
            ("en", include_str!("../locales/en.json")),
            ("de", include_str!("../locales/de.json")),
            ("es", include_str!("../locales/es.json")),
            ("fr", include_str!("../locales/fr.json")),
            ("it", include_str!("../locales/it.json")),
            ("tr", include_str!("../locales/tr.json")),
            ("ru", include_str!("../locales/ru.json")),
            ("zh_CN", include_str!("../locales/zh_CN.json")),
            ("jpn_JP", include_str!("../locales/jpn_JP.json")),
            ("ko", include_str!("../locales/ko.json")),
            ("ar", include_str!("../locales/ar.json")),
            ("pt_BR", include_str!("../locales/pt_BR.json")),
            ("ca", include_str!("../locales/ca.json")),
            ("nl", include_str!("../locales/nl.json")),
            ("pl", include_str!("../locales/pl.json")),
            ("ro", include_str!("../locales/ro.json")),
            ("ukr_UA", include_str!("../locales/ukr_UA.json")),
            ("he", include_str!("../locales/he.json")),
        ] {
            let value: serde_json::Value = serde_json::from_str(json).unwrap();
            let mut map = HashMap::new();
            flatten_json(&value, "", &mut map);
            m.insert(code, map);
        }
        m
    };
}
