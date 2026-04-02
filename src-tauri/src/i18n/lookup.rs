use std::collections::HashMap;

use parking_lot::RwLock;

/// Skill code to skill name lookup. Thread-safe and reloadable.
pub struct SkillLookup {
    skills: RwLock<HashMap<i32, String>>,
}

impl SkillLookup {
    pub fn new() -> Self {
        Self { skills: RwLock::new(HashMap::new()) }
    }

    pub fn load_from_json(&self, json_text: &str) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(json_text) {
            let mut skills = self.skills.write();
            skills.clear();
            for (key, value) in map {
                if let Ok(code) = key.parse::<i32>() {
                    skills.insert(code, value);
                }
            }
        }
    }

    pub fn get_skill_name(&self, code: i32) -> String {
        self.skills.read().get(&code).cloned().unwrap_or_default()
    }

    pub fn lookup_skill_name(&self, code: i32) -> String {
        let skills = self.skills.read();
        if let Some(name) = skills.get(&code) {
            return name.clone();
        }
        if (3_000_000..=3_099_999).contains(&code) {
            if let Some(name) = skills.get(&(code * 10 + 1)) {
                return name.clone();
            }
        }
        String::new()
    }

    pub fn contains(&self, code: i32) -> bool {
        self.skills.read().contains_key(&code)
    }
}

/// NPC/boss code to name lookup. Thread-safe and reloadable.
pub struct NpcLookup {
    npcs: RwLock<HashMap<i32, NpcInfo>>,
}

struct NpcInfo {
    name: String,
    is_boss: bool,
}

impl NpcLookup {
    pub fn new() -> Self {
        Self { npcs: RwLock::new(HashMap::new()) }
    }

    pub fn load_from_json(&self, json_text: &str) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, serde_json::Value>>(json_text) {
            let mut npcs = self.npcs.write();
            npcs.clear();
            for (key, value) in &map {
                let code = match key.parse::<i32>() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if let Some(obj) = value.as_object() {
                    let name = obj.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let is_boss = obj.get("isBoss")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    npcs.insert(code, NpcInfo { name, is_boss });
                } else if let Some(name) = value.as_str() {
                    npcs.insert(code, NpcInfo { name: name.to_string(), is_boss: false });
                }
            }
        }
    }

    pub fn get_npc_name(&self, code: i32) -> String {
        self.npcs.read().get(&code).map(|n| n.name.clone()).unwrap_or_default()
    }

    pub fn is_boss(&self, code: i32) -> bool {
        self.npcs.read().get(&code).is_some_and(|n| n.is_boss)
    }
}

/// Load skill and NPC data for a specific language from a data directory.
pub fn load_language(
    skill_lookup: &SkillLookup,
    npc_lookup: &NpcLookup,
    data_dir: &std::path::Path,
    language: &str,
) {
    let lang = match language {
        "ko" | "zh-Hans" | "zh-Hant" | "en" => language,
        _ => "en",
    };

    let skills_path = data_dir.join("i18n").join("skills").join(format!("{}.json", lang));
    if let Ok(text) = std::fs::read_to_string(&skills_path) {
        skill_lookup.load_from_json(&text);
        tracing::info!("Loaded skills ({}) from {}", lang, skills_path.display());
    } else {
        // Fallback to English
        let en_path = data_dir.join("i18n").join("skills").join("en.json");
        if let Ok(text) = std::fs::read_to_string(&en_path) {
            skill_lookup.load_from_json(&text);
        }
    }

    let npcs_path = data_dir.join("i18n").join("npcs").join(format!("{}.json", lang));
    if let Ok(text) = std::fs::read_to_string(&npcs_path) {
        npc_lookup.load_from_json(&text);
        tracing::info!("Loaded NPCs ({}) from {}", lang, npcs_path.display());
    } else {
        let en_path = data_dir.join("i18n").join("npcs").join("en.json");
        if let Ok(text) = std::fs::read_to_string(&en_path) {
            npc_lookup.load_from_json(&text);
        }
    }
}
