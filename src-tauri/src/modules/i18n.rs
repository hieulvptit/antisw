use serde_json::Value;
use std::collections::HashMap;

/// Tray text structure
#[derive(Debug, Clone)]
pub struct TrayTexts {
    pub quit: String,
}

/// Load translations from JSON
fn load_translations(lang: &str) -> HashMap<String, String> {
    let json_content = match lang {
        "en" | "en-US" => include_str!("../../../src/locales/en.json"),
        "vi" | "vi-VN" => include_str!("../../../src/locales/vi.json"),
        "tr" | "tr-TR" => include_str!("../../../src/locales/tr.json"),
        _ => include_str!("../../../src/locales/zh.json"),
    };

    let v: Value = serde_json::from_str(json_content).unwrap_or_else(|_| serde_json::json!({}));

    let mut map = HashMap::new();

    if let Some(tray) = v.get("tray").and_then(|t| t.as_object()) {
        for (key, value) in tray {
            if let Some(s) = value.as_str() {
                map.insert(key.clone(), s.to_string());
            }
        }
    }

    map
}

/// Get tray texts (based on language)
pub fn get_tray_texts(lang: &str) -> TrayTexts {
    let t = load_translations(lang);

    TrayTexts {
        quit: t
            .get("quit")
            .cloned()
            .unwrap_or_else(|| "Quit Application".to_string()),
    }
}
