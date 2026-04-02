use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::personal_data::PersonalData;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DpsData {
    pub map: HashMap<i32, PersonalData>,
    pub target_name: String,
    pub target_mode: String,
    pub target_id: i32,
    pub battle_time: i64,
    pub local_player_id: Option<i64>,
}

impl DpsData {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            target_name: String::new(),
            target_mode: "bossTargets".to_string(),
            target_id: 0,
            battle_time: 0,
            local_player_id: None,
        }
    }
}
