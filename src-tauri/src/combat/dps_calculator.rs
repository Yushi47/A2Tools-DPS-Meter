use std::collections::{HashMap, HashSet};
use std::sync::Arc;


use crate::combat::data_storage::DataStorage;
use crate::combat::ping_tracker::PingTracker;
use crate::entity::damage_packet::ParsedDamagePacket;
use crate::entity::details_context::*;
use crate::entity::dps_data::DpsData;
use crate::entity::fight_record::FightRecord;
use crate::entity::job_class::JobClass;
use crate::entity::personal_data::PersonalData;
use crate::entity::summon_resolver;
use crate::entity::target_info::TargetInfo;
use crate::i18n::lookup::{NpcLookup, SkillLookup};

/// Train mob NPC type codes.
const TRAIN_MOB_CODES: &[i32] = &[
    2300229, 2300919, 2310229, 2310919, 2320229, 2320919,
    2400032, 2400392, 2500075, 2500076, 2701376,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSelectionMode {
    BossTargets,
    MostDamage,
    MostRecent,
    LastHitByMe,
    AllTargets,
    TrainTargets,
}

impl TargetSelectionMode {
    pub fn from_id(id: &str) -> Self {
        match id {
            "bossTargets" => Self::BossTargets,
            "mostDamage" => Self::MostDamage,
            "mostRecent" => Self::MostRecent,
            "lastHitByMe" => Self::LastHitByMe,
            "allTargets" => Self::AllTargets,
            "trainTargets" => Self::TrainTargets,
            _ => Self::LastHitByMe,
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::BossTargets => "bossTargets",
            Self::MostDamage => "mostDamage",
            Self::MostRecent => "mostRecent",
            Self::LastHitByMe => "lastHitByMe",
            Self::AllTargets => "allTargets",
            Self::TrainTargets => "trainTargets",
        }
    }
}

pub struct DpsCalculator {
    data_storage: Arc<DataStorage>,
    skill_lookup: Arc<SkillLookup>,
    npc_lookup: Arc<NpcLookup>,
    ping_tracker: Arc<PingTracker>,
    target_info_map: HashMap<i32, TargetInfo>,
    current_target: i32,
    last_dps_snapshot: Option<DpsData>,
    target_selection_mode: TargetSelectionMode,
    last_local_hit_time: i64,
    last_known_local_id: Option<i64>,
    all_targets_window_ms: i64,
    nickname_job_cache: HashMap<String, String>,
    /// Track which boss targets have already been auto-saved to avoid duplicates.
    saved_boss_targets: HashSet<i32>,
}

impl DpsCalculator {
    pub fn new(
        data_storage: Arc<DataStorage>,
        skill_lookup: Arc<SkillLookup>,
        npc_lookup: Arc<NpcLookup>,
        ping_tracker: Arc<PingTracker>,
    ) -> Self {
        Self {
            data_storage,
            skill_lookup,
            npc_lookup,
            ping_tracker,
            target_info_map: HashMap::new(),
            current_target: 0,
            last_dps_snapshot: None,
            target_selection_mode: TargetSelectionMode::LastHitByMe,
            last_local_hit_time: -1,
            last_known_local_id: None,
            all_targets_window_ms: 120_000,
            nickname_job_cache: HashMap::new(),
            saved_boss_targets: HashSet::new(),
        }
    }

    pub fn set_target_selection_mode(&mut self, id: &str) {
        self.target_selection_mode = TargetSelectionMode::from_id(id);
    }

    pub fn set_all_targets_window_ms(&mut self, ms: i64) {
        self.all_targets_window_ms = ms.clamp(10_000, 900_000);
    }

    pub fn restart_target_selection(&mut self, clear_damage: bool) {
        self.last_local_hit_time = -1;
        self.current_target = 0;
        self.target_info_map.clear();
        self.last_dps_snapshot = None;
        self.saved_boss_targets.clear();
        if clear_damage {
            self.data_storage.flush();
        }
        self.data_storage.set_current_target(0);
    }

    pub fn get_dps(&mut self) -> DpsData {
        let current_local_id = self.data_storage.local_player_id();
        if current_local_id != self.last_known_local_id {
            self.last_known_local_id = current_local_id;
            self.restart_target_selection(false);
        }

        let pdp_map = self.data_storage.get_by_target_snapshot();
        let actor_map = self.data_storage.get_by_actor_snapshot();
        let nickname_data = self.data_storage.get_nicknames();
        let summon_data = self.data_storage.get_summon_data();

        // Update target info and detect idle resets
        let mut idle_reset_targets: Vec<(i32, i64)> = Vec::new();
        for (target, packets) in &pdp_map {
            for pdp in packets {
                let info = self.target_info_map.entry(*target).or_insert_with(|| {
                    TargetInfo::new(*target, pdp.timestamp())
                });
                info.process_pdp(pdp);
            }
            // Check if an idle reset was triggered for this target
            if let Some(reset_ts) = self.target_info_map.get_mut(target).and_then(|i| i.take_idle_reset()) {
                idle_reset_targets.push((*target, reset_ts));
            }
        }
        // Prune old packets for targets that had idle resets
        for (target_id, reset_ts) in &idle_reset_targets {
            self.data_storage.prune_target_before(*target_id, *reset_ts);
            tracing::info!("Idle reset: target {} — pruned packets before {}", target_id, reset_ts);
        }

        let mut dps_data = DpsData::new();
        dps_data.local_player_id = current_local_id;

        // Decide target
        let (target_ids, target_name, tracking_id) = self.decide_target(&pdp_map, &actor_map, &nickname_data, &summon_data);
        dps_data.target_name = target_name;
        dps_data.target_mode = self.target_selection_mode.id().to_string();
        self.current_target = tracking_id;
        dps_data.target_id = self.current_target;
        self.data_storage.set_current_target(self.current_target);

        // Get packets for targets
        let pdps: Vec<&ParsedDamagePacket> = if target_ids.len() > 1 || self.current_target == 0 {
            target_ids.iter()
                .flat_map(|tid| pdp_map.get(tid).into_iter().flatten())
                .collect()
        } else {
            pdp_map.get(&self.current_target).map(|v| v.iter().collect()).unwrap_or_default()
        };

        // Calculate battle time
        let mut battle_time = if let Some(info) = self.target_info_map.get(&self.current_target) {
            info.battle_time()
        } else {
            0
        };

        if battle_time == 0 && !pdps.is_empty() {
            battle_time = 1000;
        }

        if battle_time == 0 || pdps.is_empty() {
            if let Some(ref mut snapshot) = self.last_dps_snapshot {
                // Update target name in cache (may have changed due to language switch)
                snapshot.target_name = dps_data.target_name.clone();
                snapshot.target_mode = dps_data.target_mode.clone();
                snapshot.target_id = dps_data.target_id;
                return snapshot.clone();
            }
            return dps_data;
        }

        let mut total_damage: f64 = 0.0;

        // Build canonical ID map
        let nickname_to_canonical = build_nickname_canonical_map(&pdps, &summon_data, &nickname_data);

        for pdp in &pdps {
            total_damage += pdp.total_damage() as f64;

            let raw_uid = summon_resolver::resolve(pdp.actor_id(), &summon_data);
            if raw_uid <= 0 { continue; }
            let nickname = resolve_nickname(raw_uid, &nickname_data, &summon_data);
            let uid = *nickname_to_canonical.get(&nickname).unwrap_or(&raw_uid);

            let raw_sc = pdp.skill_code();
            let sc = {
                let base = raw_sc - (raw_sc % 10000);
                let bn = self.skill_lookup.get_skill_name(base);
                if !bn.is_empty() {
                    let rn = self.skill_lookup.get_skill_name(raw_sc);
                    if rn.is_empty() || rn == bn { base } else { raw_sc }
                } else { raw_sc }
            };
            let skill_name = self.skill_lookup.lookup_skill_name(sc);

            let entry = dps_data.map.entry(uid).or_insert_with(|| {
                let cached_job = self.cached_job(&nickname);
                if let Some(job) = cached_job {
                    PersonalData::with_job(nickname.clone(), job)
                } else {
                    PersonalData::new(nickname.clone())
                }
            });

            // Update nickname if changed
            if entry.nickname != nickname {
                entry.nickname = nickname.clone();
            }

            entry.process_pdp(pdp, &skill_name);

            if entry.job.is_empty() {
                if let Some(job) = JobClass::convert_from_skill(pdp.skill_code()) {
                    entry.job = job.class_name().to_string();
                    self.cache_job(&nickname, job.class_name());
                }
            }
        }

        // Orphan summon inference
        let mut orphan_merges: Vec<(i32, i32)> = Vec::new();
        for (&uid, data) in &dps_data.map {
            if summon_data.contains_key(&uid) { continue; }
            if nickname_data.contains_key(&uid) { continue; }
            let job = &data.job;
            if job.is_empty() { continue; }
            let same_job: Vec<_> = dps_data.map.iter()
                .filter(|(oid, od)| **oid != uid && od.job == *job && nickname_data.contains_key(oid))
                .map(|(&oid, _)| oid)
                .collect();
            if same_job.len() == 1 {
                orphan_merges.push((uid, same_job[0]));
            }
        }
        for (orphan, owner) in orphan_merges {
            if let Some(orphan_data) = dps_data.map.remove(&orphan) {
                if let Some(owner_data) = dps_data.map.get_mut(&owner) {
                    owner_data.merge_from(&orphan_data);
                }
            }
        }

        // Filter and compute DPS
        let local_ids = self.resolve_local_ids(&summon_data);
        let mut to_remove = Vec::new();
        for (&uid, data) in &mut dps_data.map {
            if data.job.is_empty() {
                if local_ids.as_ref().is_some_and(|ids| ids.contains(&uid)) {
                    data.job = "Unknown".to_string();
                } else {
                    to_remove.push(uid);
                    continue;
                }
            }
            data.dps = data.amount / battle_time.max(1000) as f64 * 1000.0;
            data.damage_contribution = data.amount / total_damage * 100.0;
        }
        for uid in to_remove {
            dps_data.map.remove(&uid);
        }

        dps_data.battle_time = battle_time;
        if !dps_data.map.is_empty() {
            self.last_dps_snapshot = Some(dps_data.clone());
        }
        dps_data
    }

    fn decide_target(
        &mut self,
        pdp_map: &HashMap<i32, Vec<ParsedDamagePacket>>,
        _actor_map: &HashMap<i32, Vec<ParsedDamagePacket>>,
        _nickname_data: &HashMap<i32, String>,
        summon_data: &HashMap<i32, i32>,
    ) -> (HashSet<i32>, String, i32) {
        let mob_data = self.data_storage.get_mob_data();

        match self.target_selection_mode {
            TargetSelectionMode::MostDamage => {
                let best = self.target_info_map.iter()
                    .max_by_key(|(_, info)| info.damaged_amount);
                match best {
                    Some((&id, _info)) => {
                        let name = self.resolve_target_name(id);
                        (HashSet::from([id]), name, id)
                    }
                    None => (HashSet::new(), String::new(), 0),
                }
            }
            TargetSelectionMode::MostRecent => {
                let best = self.target_info_map.iter()
                    .max_by_key(|(_, info)| info.last_damage_time());
                match best {
                    Some((&id, _)) => {
                        let name = self.resolve_target_name(id);
                        (HashSet::from([id]), name, id)
                    }
                    None => (HashSet::new(), String::new(), 0),
                }
            }
            TargetSelectionMode::BossTargets => {
                // Find targets that are bosses
                let boss_targets: Vec<_> = self.target_info_map.keys()
                    .filter(|&&tid| {
                        if let Some(&mob_code) = mob_data.get(&tid) {
                            self.npc_lookup.is_boss(mob_code)
                        } else {
                            false
                        }
                    })
                    .cloned()
                    .collect();

                if let Some(&best) = boss_targets.iter()
                    .max_by_key(|&&tid| self.target_info_map.get(&tid).map(|i| i.damaged_amount).unwrap_or(0))
                {
                    let name = self.resolve_target_name(best);
                    (HashSet::from([best]), name, best)
                } else {
                    // Fall back to most damage
                    let best = self.target_info_map.iter()
                        .max_by_key(|(_, info)| info.damaged_amount);
                    match best {
                        Some((&id, _)) => {
                            let name = self.resolve_target_name(id);
                            (HashSet::from([id]), name, id)
                        }
                        None => (HashSet::new(), String::new(), 0),
                    }
                }
            }
            TargetSelectionMode::AllTargets => {
                let all: HashSet<i32> = self.target_info_map.keys().cloned().collect();
                (all, "All Targets".to_string(), 0)
            }
            TargetSelectionMode::TrainTargets => {
                let trains: HashSet<i32> = self.target_info_map.keys()
                    .filter(|&&tid| {
                        mob_data.get(&tid).is_some_and(|code| TRAIN_MOB_CODES.contains(code))
                    })
                    .cloned()
                    .collect();
                (trains, "Train".to_string(), 0)
            }
            TargetSelectionMode::LastHitByMe => {
                let local_ids = self.resolve_local_ids(summon_data);
                if let Some(ref ids) = local_ids {
                    // Find the target most recently hit by the local player
                    let mut best_target: Option<(i32, i64)> = None;
                    for (&target, packets) in pdp_map {
                        for pdp in packets.iter().rev() {
                            let resolved = summon_resolver::resolve(pdp.actor_id(), summon_data);
                            if ids.contains(&resolved) {
                                let ts = pdp.timestamp();
                                if best_target.is_none() || ts > best_target.unwrap().1 {
                                    best_target = Some((target, ts));
                                }
                                break;
                            }
                        }
                    }
                    match best_target {
                        Some((id, _)) => {
                            let name = self.resolve_target_name(id);
                            (HashSet::from([id]), name, id)
                        }
                        // Local player hasn't hit anything yet — fall back to most recent target
                        None => {
                            let best = self.target_info_map.iter()
                                .max_by_key(|(_, info)| info.last_damage_time());
                            match best {
                                Some((&id, _)) => {
                                    let name = self.resolve_target_name(id);
                                    (HashSet::from([id]), name, id)
                                }
                                None => (HashSet::new(), String::new(), 0),
                            }
                        }
                    }
                } else {
                    // No local player identified yet — fall back to most damaged target
                    let best = self.target_info_map.iter()
                        .max_by_key(|(_, info)| info.damaged_amount);
                    match best {
                        Some((&id, _)) => {
                            let name = self.resolve_target_name(id);
                            (HashSet::from([id]), name, id)
                        }
                        None => (HashSet::new(), String::new(), 0),
                    }
                }
            }
        }
    }

    fn resolve_target_name(&self, target_id: i32) -> String {
        let mob_data = self.data_storage.get_mob_data();
        if let Some(&code) = mob_data.get(&target_id) {
            let name = self.npc_lookup.get_npc_name(code);
            if !name.is_empty() {
                return name;
            }
        }
        String::new()
    }

    fn resolve_local_ids(&self, summon_data: &HashMap<i32, i32>) -> Option<HashSet<i32>> {
        let local_id = self.data_storage.local_player_id()? as i32;
        let mut ids = HashSet::new();
        ids.insert(local_id);
        // Include summons owned by local player
        for (&summon, &owner) in summon_data {
            if summon_resolver::resolve(owner, summon_data) == local_id {
                ids.insert(summon);
            }
        }
        Some(ids)
    }

    fn cached_job(&self, nickname: &str) -> Option<String> {
        let key = nickname.trim().to_lowercase();
        if key.is_empty() || key.chars().all(|c| c.is_ascii_digit()) { return None; }
        self.nickname_job_cache.get(&key)
            .filter(|j| !j.is_empty() && *j != "Unknown")
            .cloned()
    }

    fn cache_job(&mut self, nickname: &str, job: &str) {
        if job.is_empty() || job == "Unknown" { return; }
        let key = nickname.trim().to_lowercase();
        if key.is_empty() || key.chars().all(|c| c.is_ascii_digit()) { return; }
        self.nickname_job_cache.insert(key, job.to_string());
    }

    /// Create FightRecord snapshots for boss targets that have accumulated
    /// enough data (>= 5 seconds of battle time and > 0 total damage) and have
    /// not been saved before. Returns a list of records ready to be persisted.
    pub fn snapshot_boss_fights(&mut self) -> Vec<FightRecord> {
        self.snapshot_boss_fights_inner(false)
    }

    pub fn snapshot_boss_fights_force(&mut self) -> Vec<FightRecord> {
        self.snapshot_boss_fights_inner(true)
    }

    fn snapshot_boss_fights_inner(&mut self, force: bool) -> Vec<FightRecord> {
        let mob_data = self.data_storage.get_mob_data();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let mut records = Vec::new();

        // Find targets worth saving: bosses, or any fight with >= 10s battle time
        let boss_target_ids: Vec<i32> = self.target_info_map.keys()
            .filter(|&&tid| {
                if self.saved_boss_targets.contains(&tid) {
                    return false;
                }
                if let Some(&code) = mob_data.get(&tid) {
                    self.npc_lookup.is_boss(code) || TRAIN_MOB_CODES.contains(&code)
                } else {
                    false
                }
            })
            .cloned()
            .collect();



        if !boss_target_ids.is_empty() {
            tracing::debug!("snapshot_boss_fights: {} candidate targets", boss_target_ids.len());
        }
        for target_id in boss_target_ids {
            let info = match self.target_info_map.get(&target_id) {
                Some(i) => i,
                None => continue,
            };

            let battle_time = info.battle_time();
            if battle_time < 5_000 || info.damaged_amount <= 0 {
                continue;
            }

            // Save when fight is idle (ended), periodically during long fights, or forced
            let idle_time = now_ms - info.last_damage_time();
            let is_ended = idle_time >= 10_000;
            let is_periodic = battle_time >= 15_000;
            if !force && !is_ended && !is_periodic {
                continue;
            }

            // Generate fight record via get_target_details
            let details = self.get_target_details(target_id, None);
            let nickname_data = self.data_storage.get_nicknames();
            let summon_data_snap = self.data_storage.get_summon_data();

            // Build actors list with nicknames from the full nickname storage
            // to ensure all actor IDs have their names resolved
            let mut record_actors: HashMap<i32, (String, String)> = HashMap::new(); // id -> (nick, job)
            for skill in &details.skills {
                let uid = skill.actor_id;
                record_actors.entry(uid).or_insert_with(|| {
                    let nick = resolve_nickname(uid, &nickname_data, &summon_data_snap);
                    let job = if !skill.job.is_empty() { skill.job.clone() }
                        else { JobClass::convert_from_skill(skill.code).map(|j| j.class_name().to_string()).unwrap_or_default() };
                    (nick, job)
                });
                // Update job if empty
                let entry = record_actors.get_mut(&uid).unwrap();
                if entry.1.is_empty() && !skill.job.is_empty() {
                    entry.1 = skill.job.clone();
                }
            }
            // Obscure non-local player nicknames for privacy
            let local_id = self.data_storage.local_player_id().unwrap_or(-1) as i32;
            let actors: Vec<DetailsActorSummary> = record_actors.iter()
                .map(|(&id, (nick, job))| {
                    let display_nick = if id == local_id {
                        nick.clone()
                    } else {
                        crate::entity::fight_record::obscure_nickname(nick)
                    };
                    let job_class = JobClass::convert_from_skill(
                        // Find a skill code for this actor to determine job
                        details.skills.iter()
                            .find(|s| s.actor_id == id && !s.job.is_empty())
                            .map(|s| s.code)
                            .unwrap_or(0)
                    );
                    DetailsActorSummary {
                        actor_id: id,
                        nickname: display_nick,
                        job: job.clone(),
                        job_id: job_class.map(|j| j.class_prefix()).unwrap_or(0),
                    }
                })
                .collect();

            // Get mob type code for i18n resolution
            let mob_code = mob_data.get(&target_id).copied().unwrap_or(0);
            let boss_name = self.resolve_target_name(target_id);

            // Collect job IDs for language-independent storage
            let job_ids: Vec<i32> = actors.iter()
                .filter(|a| a.job_id > 0)
                .map(|a| a.job_id)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let jobs: Vec<String> = actors.iter()
                .filter(|a| !a.job.is_empty() && a.job != "Unknown")
                .map(|a| a.job.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();

            let id = format!("auto_{}_{}", target_id, info.target_damage_started);

            let is_train = TRAIN_MOB_CODES.contains(&mob_code);
            let record = FightRecord {
                id,
                boss_name,
                target_id,
                start_time_ms: info.target_damage_started,
                duration_ms: battle_time,
                total_damage: info.damaged_amount as i32,
                jobs,
                job_ids,
                details,
                actors,
                is_train,
                app_version: crate::entity::fight_record::APP_VERSION.to_string(),
                mob_code,
            };

            // Only mark as "saved" when fight has ended — allow periodic re-saves during active fights
            if is_ended {
                self.saved_boss_targets.insert(target_id);
            }
            records.push(record);
        }

        records
    }

    pub fn get_details_context(&self) -> DetailsContext {
        let pdp_map = self.data_storage.get_by_target_snapshot();
        let nickname_data = self.data_storage.get_nicknames();
        let summon_data = self.data_storage.get_summon_data();
        let mob_hp_data = self.data_storage.get_mob_hp_data();
        let mob_data = self.data_storage.get_mob_data();

        let mut actor_meta: HashMap<i32, (String, String)> = HashMap::new(); // uid -> (nickname, job)
        let mut targets = Vec::new();

        for (&target_id, pdps) in &pdp_map {
            let mut total_damage: i32 = 0;
            let mut actor_damage: HashMap<i32, i32> = HashMap::new();
            let pdp_refs: Vec<&ParsedDamagePacket> = pdps.iter().collect();
            let canonical = build_nickname_canonical_map(&pdp_refs, &summon_data, &nickname_data);

            for pdp in pdps {
                let raw_uid = summon_resolver::resolve(pdp.actor_id(), &summon_data);
                if raw_uid <= 0 { continue; }
                let nickname = resolve_nickname(raw_uid, &nickname_data, &summon_data);
                let uid = *canonical.get(&nickname).unwrap_or(&raw_uid);
                let damage = pdp.total_damage();
                total_damage += damage;
                *actor_damage.entry(uid).or_insert(0) += damage;

                actor_meta.entry(uid).or_insert_with(|| {
                    (resolve_nickname(uid, &nickname_data, &summon_data), String::new())
                });

                if actor_meta.get(&uid).unwrap().1.is_empty() {
                    if let Some(job) = JobClass::convert_from_skill(pdp.skill_code()) {
                        actor_meta.get_mut(&uid).unwrap().1 = job.class_name().to_string();
                    }
                }
            }

            // Orphan summon inference: unlinked actors with detected job matching
            // exactly one named player of that class get merged into that player.
            let target_actor_ids: HashSet<i32> = actor_damage.keys().copied().collect();
            let mut orphan_merges: Vec<(i32, i32)> = Vec::new();
            for (&uid, (_, job)) in &actor_meta {
                if !target_actor_ids.contains(&uid) { continue; }
                if summon_data.contains_key(&uid) { continue; }
                if nickname_data.contains_key(&uid) { continue; }
                if job.is_empty() { continue; }
                let same_job: Vec<i32> = actor_meta.iter()
                    .filter(|(oid, (_, oj))| **oid != uid && *oj == *job
                        && nickname_data.contains_key(oid)
                        && target_actor_ids.contains(oid))
                    .map(|(oid, _)| *oid)
                    .collect();
                if same_job.len() == 1 {
                    orphan_merges.push((uid, same_job[0]));
                }
            }
            for (orphan, owner) in &orphan_merges {
                if let Some(dmg) = actor_damage.remove(orphan) {
                    *actor_damage.entry(*owner).or_insert(0) += dmg;
                }
                actor_meta.remove(orphan);
            }
            // Remove actors with no job and no nickname
            let remove_ids: Vec<i32> = actor_damage.keys()
                .filter(|id| {
                    actor_meta.get(id).is_some_and(|(_, job)| job.is_empty() && !nickname_data.contains_key(id))
                })
                .copied().collect();
            for id in remove_ids {
                actor_damage.remove(&id);
                actor_meta.remove(&id);
            }

            let target_name = if let Some(&code) = mob_data.get(&target_id) {
                self.npc_lookup.get_npc_name(code)
            } else {
                String::new()
            };

            let info = self.target_info_map.get(&target_id);
            targets.push(DetailsTargetSummary {
                target_id,
                target_name,
                max_hp: mob_hp_data.get(&target_id).copied().unwrap_or(0),
                battle_time: info.map(|i| i.battle_time()).unwrap_or(0),
                last_damage_time: info.map(|i| i.last_damage_time()).unwrap_or(0),
                total_damage,
                actor_damage,
            });
        }

        let actors: Vec<DetailsActorSummary> = actor_meta.iter()
            .map(|(&id, (nick, job))| {
                let job_id = JobClass::convert_from_skill(
                    // Find any skill from this actor to get job prefix
                    pdp_map.values().flatten()
                        .find(|p| {
                            let resolved = summon_resolver::resolve(p.actor_id(), &summon_data);
                            resolved == id && JobClass::convert_from_skill(p.skill_code()).is_some()
                        })
                        .map(|p| p.skill_code())
                        .unwrap_or(0)
                ).map(|j| j.class_prefix()).unwrap_or(0);
                DetailsActorSummary {
                    actor_id: id,
                    nickname: nick.clone(),
                    job: job.clone(),
                    job_id,
                }
            })
            .collect();

        DetailsContext {
            current_target_id: self.current_target,
            targets,
            actors,
        }
    }

    pub fn get_target_details(&self, target_id: i32, actor_ids: Option<&[i32]>) -> TargetDetailsResponse {
        let pdp_map = self.data_storage.get_by_target_snapshot();
        let pdps = match pdp_map.get(&target_id) {
            Some(p) => p,
            None => return TargetDetailsResponse {
                target_id,
                max_hp: 0,
                total_target_damage: 0,
                battle_time: 0,
                start_time: 0,
                skills: Vec::new(),
                ping_history: Vec::new(),
            },
        };

        let summon_data = self.data_storage.get_summon_data();
        let nickname_data = self.data_storage.get_nicknames();
        let mob_hp_data = self.data_storage.get_mob_hp_data();

        let pdp_refs: Vec<&ParsedDamagePacket> = pdps.iter().collect();
        let canonical = build_nickname_canonical_map(&pdp_refs, &summon_data, &nickname_data);

        // Build orphan summon map: orphan actor -> owner actor
        // Matches logic in get_details_context orphan inference
        let mut orphan_to_owner: HashMap<i32, i32> = HashMap::new();
        {
            let mut actor_jobs: HashMap<i32, String> = HashMap::new();
            for pdp in pdps {
                let raw_uid = summon_resolver::resolve(pdp.actor_id(), &summon_data);
                if raw_uid <= 0 { continue; }
                let uid = *canonical.get(&resolve_nickname(raw_uid, &nickname_data, &summon_data)).unwrap_or(&raw_uid);
                if actor_jobs.contains_key(&uid) { continue; }
                if let Some(job) = JobClass::convert_from_skill(pdp.skill_code()) {
                    actor_jobs.insert(uid, job.class_name().to_string());
                }
            }
            let mut seen = HashSet::new();
            for pdp in pdps {
                let raw_uid = summon_resolver::resolve(pdp.actor_id(), &summon_data);
                if raw_uid <= 0 { continue; }
                if summon_data.contains_key(&raw_uid) || nickname_data.contains_key(&raw_uid) { continue; }
                if !seen.insert(raw_uid) { continue; }
                let job = match JobClass::convert_from_skill_loose(pdp.skill_code()) {
                    Some(j) => j.class_name().to_string(),
                    None => continue,
                };
                let matching: Vec<i32> = actor_jobs.iter()
                    .filter(|(id, j)| **id != raw_uid && **j == job && nickname_data.contains_key(id))
                    .map(|(id, _)| *id)
                    .collect();
                if matching.len() == 1 {
                    orphan_to_owner.insert(raw_uid, matching[0]);
                }
            }
        }

        // Build expanded actor ID set (includes summons owned by selected actors)
        let filter_uids: Option<HashSet<i32>> = actor_ids.map(|ids| {
            let canonical_ids: HashSet<i32> = ids.iter()
                .map(|&id| {
                    let nick = resolve_nickname(id, &nickname_data, &summon_data);
                    *canonical.get(&nick).unwrap_or(&id)
                })
                .collect();
            let mut expanded = HashSet::from_iter(ids.iter().copied());
            // Include any actor whose canonical ID matches a selected player
            for pdp in pdps {
                let raw_uid = summon_resolver::resolve(pdp.actor_id(), &summon_data);
                if raw_uid <= 0 { continue; }
                // Remap orphan summons to their inferred owner
                let remapped = *orphan_to_owner.get(&raw_uid).unwrap_or(&raw_uid);
                let nick = resolve_nickname(remapped, &nickname_data, &summon_data);
                let uid = *canonical.get(&nick).unwrap_or(&remapped);
                if canonical_ids.contains(&uid) {
                    expanded.insert(raw_uid);
                }
            }
            // Also include orphan summons whose owner is selected
            for (&orphan, &owner) in &orphan_to_owner {
                let nick = resolve_nickname(owner, &nickname_data, &summon_data);
                let uid = *canonical.get(&nick).unwrap_or(&owner);
                if canonical_ids.contains(&uid) {
                    expanded.insert(orphan);
                }
            }
            expanded
        });

        // First pass: compute total damage and battle time from ALL actors (unfiltered)
        let mut total_damage: i32 = 0;
        let mut start_time: Option<i64> = None;
        let mut end_time: Option<i64> = None;
        for pdp in pdps {
            let raw_uid = summon_resolver::resolve(pdp.actor_id(), &summon_data);
            if raw_uid <= 0 { continue; }
            total_damage += pdp.total_damage();
            let ts = pdp.timestamp();
            start_time = Some(start_time.map_or(ts, |s: i64| s.min(ts)));
            end_time = Some(end_time.map_or(ts, |e: i64| e.max(ts)));
        }

        // Second pass: build skill entries (filtered by actor if specified)
        let mut skill_map: HashMap<(i32, i32), DetailSkillEntry> = HashMap::new();
        for pdp in pdps {
            let raw_uid = summon_resolver::resolve(pdp.actor_id(), &summon_data);
            if raw_uid <= 0 { continue; }

            // Filter by actor IDs if specified
            if let Some(ref filter) = filter_uids {
                if !filter.contains(&raw_uid) { continue; }
            }

            // Remap orphan summons to their inferred owner
            let remapped = *orphan_to_owner.get(&raw_uid).unwrap_or(&raw_uid);
            let nickname = resolve_nickname(remapped, &nickname_data, &summon_data);
            let uid = *canonical.get(&nickname).unwrap_or(&remapped);

            let damage = pdp.total_damage();
            let ts = pdp.timestamp();
            let raw_skill = pdp.skill_code();
            // Normalize skill code: variant -> base for aggregation and name lookup
            let skill_code = {
                let base = raw_skill - (raw_skill % 10000);
                let base_name = self.skill_lookup.get_skill_name(base);
                if !base_name.is_empty() {
                    let raw_name = self.skill_lookup.get_skill_name(raw_skill);
                    if raw_name.is_empty() || raw_name == base_name {
                        base
                    } else {
                        raw_skill
                    }
                } else {
                    raw_skill
                }
            };
            // Separate key for DOT vs hit entries so they don't merge
            let dot_offset = if pdp.is_dot() { 1_000_000_000 } else { 0 };
            let key = (uid, skill_code + dot_offset);
            let mut skill_name = self.skill_lookup.lookup_skill_name(skill_code);
            // Append " - DOT" so the frontend can pair DOTs with their parent skill
            if pdp.is_dot() && !skill_name.is_empty() {
                skill_name = format!("{} - DOT", skill_name);
            }
            let job = JobClass::convert_from_skill(skill_code)
                .map(|j| j.class_name().to_string())
                .unwrap_or_default();

            let entry = skill_map.entry(key).or_insert_with(|| DetailSkillEntry {
                actor_id: uid,
                code: skill_code,
                name: skill_name,
                time: 0,
                dmg: 0,
                multi_hit_count: 0,
                multi_hit_damage: 0,
                multi_hit_hits: 0,
                min_dmg: i32::MAX,
                max_dmg: 0,
                crit: 0,
                parry: 0,
                back: 0,
                perfect: 0,
                double: 0,
                heal: 0,
                job,
                is_dot: pdp.is_dot(),
                hit_timestamps: Vec::new(),
                specs: pdp.spec_flags().to_vec(),
            });

            entry.time += 1;
            entry.dmg += damage;
            if pdp.multi_hit_count() > 0 {
                entry.multi_hit_count += 1;
                entry.multi_hit_damage += pdp.multi_hit_damage();
                entry.multi_hit_hits += pdp.multi_hit_count();
            }
            let hit_dmg = pdp.damage();
            if hit_dmg < entry.min_dmg { entry.min_dmg = hit_dmg; }
            if hit_dmg > entry.max_dmg { entry.max_dmg = hit_dmg; }
            if pdp.is_crit() { entry.crit += 1; }
            if pdp.specials().contains(&crate::entity::special_damage::SpecialDamage::Back) { entry.back += 1; }
            if pdp.specials().contains(&crate::entity::special_damage::SpecialDamage::Parry) { entry.parry += 1; }
            if pdp.specials().contains(&crate::entity::special_damage::SpecialDamage::Perfect) { entry.perfect += 1; }
            if pdp.specials().contains(&crate::entity::special_damage::SpecialDamage::Double) { entry.double += 1; }
            entry.heal += pdp.heal_amount();
            entry.hit_timestamps.push(ts);
        }

        // Fix min_dmg and normalize timestamps relative to fight start
        let fight_start = start_time.unwrap_or(0);
        for entry in skill_map.values_mut() {
            if entry.min_dmg == i32::MAX { entry.min_dmg = 0; }
            for ts in &mut entry.hit_timestamps {
                *ts -= fight_start;
            }
        }

        let battle_time = match (start_time, end_time) {
            (Some(s), Some(e)) => (e - s).max(0),
            _ => 0,
        };

        let ping_history = if start_time.is_some() && end_time.is_some() {
            self.ping_tracker.get_ping_history(start_time.unwrap(), end_time.unwrap())
                .into_iter()
                .map(|(ts, ping)| PingPoint { ts_ms: ts - fight_start, ping_ms: ping })
                .collect()
        } else {
            Vec::new()
        };

        TargetDetailsResponse {
            target_id,
            max_hp: mob_hp_data.get(&target_id).copied().unwrap_or(0),
            total_target_damage: total_damage,
            battle_time,
            start_time: start_time.unwrap_or(0),
            skills: skill_map.into_values().collect(),
            ping_history,
        }
    }
}

fn resolve_nickname(uid: i32, nicknames: &HashMap<i32, String>, summon_data: &HashMap<i32, i32>) -> String {
    if let Some(name) = nicknames.get(&uid) {
        return name.clone();
    }
    let resolved = summon_resolver::resolve(uid, summon_data);
    if let Some(name) = nicknames.get(&resolved) {
        return name.clone();
    }
    uid.to_string()
}

fn build_nickname_canonical_map(
    pdps: &[&ParsedDamagePacket],
    summon_data: &HashMap<i32, i32>,
    nickname_data: &HashMap<i32, String>,
) -> HashMap<String, i32> {
    let mut nickname_damage: HashMap<String, HashMap<i32, i32>> = HashMap::new();

    for pdp in pdps {
        let uid = summon_resolver::resolve(pdp.actor_id(), summon_data);
        if uid <= 0 { continue; }
        let nickname = resolve_nickname(uid, nickname_data, summon_data);
        let id_damage = nickname_damage.entry(nickname).or_default();
        *id_damage.entry(uid).or_insert(0) += pdp.total_damage();
    }

    let mut result = HashMap::new();
    for (nickname, id_damage) in &nickname_damage {
        let direct_owner = id_damage.keys().find(|&&id| nickname_data.get(&id).is_some_and(|n| n == nickname));
        let canonical = direct_owner.copied()
            .or_else(|| id_damage.iter().max_by_key(|(_, d)| *d).map(|(id, _)| *id));
        if let Some(id) = canonical {
            result.insert(nickname.clone(), id);
        }
    }
    result
}
