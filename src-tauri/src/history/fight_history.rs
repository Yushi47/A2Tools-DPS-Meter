use std::path::PathBuf;

use tracing::info;

use crate::entity::fight_record::{FightRecord, FightSummary};

/// Manages saving and loading fight records as JSON files.
pub struct FightHistoryManager {
    history_dir: PathBuf,
}

impl FightHistoryManager {
    pub fn new(app_data_dir: PathBuf) -> Self {
        let history_dir = app_data_dir.join("history");
        let _ = std::fs::create_dir_all(&history_dir);
        Self { history_dir }
    }

    pub fn save_fight(&self, record: &FightRecord) -> Result<(), String> {
        let file_path = self.history_dir.join(format!("{}.json", record.id));
        let json = serde_json::to_string_pretty(record)
            .map_err(|e| format!("Serialization error: {}", e))?;
        std::fs::write(&file_path, json)
            .map_err(|e| format!("Write error: {}", e))?;
        info!("Fight saved: {}", record.id);
        Ok(())
    }

    pub fn load_fight(&self, id: &str) -> Result<FightRecord, String> {
        let file_path = self.history_dir.join(format!("{}.json", id));
        let json = std::fs::read_to_string(&file_path)
            .map_err(|e| format!("Read error: {}", e))?;
        serde_json::from_str(&json)
            .map_err(|e| format!("Parse error: {}", e))
    }

    pub fn delete_fight(&self, id: &str) -> Result<(), String> {
        let file_path = self.history_dir.join(format!("{}.json", id));
        std::fs::remove_file(&file_path)
            .map_err(|e| format!("Delete error: {}", e))?;
        info!("Fight deleted: {}", id);
        Ok(())
    }

    pub fn list_fights(&self) -> Vec<FightSummary> {
        let mut summaries = Vec::new();
        let entries = match std::fs::read_dir(&self.history_dir) {
            Ok(e) => e,
            Err(_) => return summaries,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Ok(json) = std::fs::read_to_string(&path) {
                    if let Ok(record) = serde_json::from_str::<FightRecord>(&json) {
                        summaries.push(FightSummary {
                            id: record.id,
                            boss_name: record.boss_name,
                            target_id: record.target_id,
                            start_time_ms: record.start_time_ms,
                            duration_ms: record.duration_ms,
                            total_damage: record.total_damage,
                            jobs: record.jobs,
                            job_ids: record.job_ids,
                            is_train: record.is_train,
                            is_live: false,
                            app_version: record.app_version,
                            mob_code: record.mob_code,
                        });
                    }
                }
            }
        }

        summaries.sort_by(|a, b| b.start_time_ms.cmp(&a.start_time_ms));
        summaries
    }

    pub fn export_fight_json(&self, record: &FightRecord) -> Result<String, String> {
        serde_json::to_string(record)
            .map_err(|e| format!("Serialization error: {}", e))
    }
}
