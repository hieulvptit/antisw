use std::path::PathBuf;

pub const VNPAY_BASE_URL: &str = "https://genai.vnoffice.io.vn/aicoding";

pub fn get_settings_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "Cannot determine home directory".to_string())?;
    Ok(home.join(".claude").join("settings.json"))
}

pub fn has_vnpay_config() -> Result<bool, String> {
    let path = get_settings_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    let json: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));
    Ok(json
        .get("env")
        .and_then(|v| v.as_object())
        .map(|e| e.contains_key("ANTHROPIC_AUTH_TOKEN") && e.contains_key("ANTHROPIC_BASE_URL"))
        .unwrap_or(false))
}

pub fn apply_vnpay_jwt(jwt: &str) -> Result<(), String> {
    let path = get_settings_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create ~/.claude directory: {}", e))?;
    }

    let mut json: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read settings: {}", e))?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if !json.is_object() {
        json = serde_json::json!({});
    }

    {
        let obj = json.as_object_mut().unwrap();
        let env_entry = obj
            .entry("env".to_string())
            .or_insert(serde_json::json!({}));
        if !env_entry.is_object() {
            *env_entry = serde_json::json!({});
        }
        let env_obj = env_entry.as_object_mut().unwrap();
        env_obj.insert(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            serde_json::Value::String(jwt.to_string()),
        );
        env_obj.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            serde_json::Value::String(VNPAY_BASE_URL.to_string()),
        );
    }

    let json_str = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize settings: {}", e))?;
    std::fs::write(&path, json_str)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;

    crate::modules::logger::log_info(&format!("Login CLI settings applied: {}", path.display()));
    Ok(())
}

pub fn remove_vnpay_config() -> Result<(), String> {
    let path = get_settings_path()?;
    if !path.exists() {
        return Ok(());
    }

    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read settings: {}", e))?;
    let mut json: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));

    if let Some(obj) = json.as_object_mut() {
        if let Some(env) = obj.get_mut("env") {
            if let Some(env_obj) = env.as_object_mut() {
                env_obj.remove("ANTHROPIC_AUTH_TOKEN");
                env_obj.remove("ANTHROPIC_BASE_URL");
            }
        }
    }

    let json_str = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize settings: {}", e))?;
    std::fs::write(&path, json_str)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;

    crate::modules::logger::log_info(&format!(
        "Claude CLI VNPAY settings removed: {}",
        path.display()
    ));
    Ok(())
}
