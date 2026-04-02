use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::RwLock;

use crate::entity::damage_packet::ParsedDamagePacket;
use crate::entity::job_class::JobClass;
use crate::entity::special_damage::SpecialDamage;
use crate::entity::summon_resolver;

/// Maximum idle gap before a fight is considered ended and a new one begins.
const IDLE_RESET_MS: i64 = 30_000;

// ───── Aggregate data structures ─────

#[derive(Debug, Clone)]
pub struct SkillCombatData {
    pub skill_code: i32,
    pub is_dot: bool,
    pub hit_count: i32,
    pub total_damage: i32,
    pub min_damage: i32,
    pub max_damage: i32,
    pub crit_count: i32,
    pub back_count: i32,
    pub parry_count: i32,
    pub perfect_count: i32,
    pub double_count: i32,
    pub multi_hit_count: i32,
    pub multi_hit_damage: i32,
    pub multi_hit_hits: i32,
    pub heal_amount: i32,
    pub hit_timestamps: Vec<i64>,
    pub spec_flags: [bool; 5],
}

impl SkillCombatData {
    fn new(skill_code: i32, is_dot: bool) -> Self {
        Self {
            skill_code,
            is_dot,
            hit_count: 0,
            total_damage: 0,
            min_damage: i32::MAX,
            max_damage: 0,
            crit_count: 0,
            back_count: 0,
            parry_count: 0,
            perfect_count: 0,
            double_count: 0,
            multi_hit_count: 0,
            multi_hit_damage: 0,
            multi_hit_hits: 0,
            heal_amount: 0,
            hit_timestamps: Vec::new(),
            spec_flags: [false; 5],
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActorCombatData {
    pub total_damage: i64,
    pub job: Option<JobClass>,
    /// Skills keyed by (raw_skill_code, is_dot)
    pub skills: HashMap<(i32, bool), SkillCombatData>,
}

impl ActorCombatData {
    fn new() -> Self {
        Self {
            total_damage: 0,
            job: None,
            skills: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TargetCombatData {
    pub target_id: i32,
    pub total_damage: i64,
    pub first_damage_time: i64,
    pub last_damage_time: i64,
    pub last_packet_id: i64,
    /// Per raw-actor aggregated combat data
    pub actors: HashMap<i32, ActorCombatData>,
}

impl TargetCombatData {
    fn new(target_id: i32, timestamp: i64) -> Self {
        Self {
            target_id,
            total_damage: 0,
            first_damage_time: timestamp,
            last_damage_time: timestamp,
            last_packet_id: -1,
            actors: HashMap::new(),
        }
    }
}

// ───── Main storage ─────

pub struct DataStorage {
    inner: RwLock<Inner>,
    damage_generation: AtomicI64,
}

struct Inner {
    /// Aggregated combat data per target (replaces raw packet storage)
    target_combat: HashMap<i32, TargetCombatData>,
    /// Job class detected per actor (across all targets, for summon matching)
    actor_jobs: HashMap<i32, JobClass>,

    nickname_storage: HashMap<i32, String>,
    pending_nicknames: HashMap<i32, String>,
    permanent_nicknames: HashMap<i32, String>,
    summon_storage: HashMap<i32, i32>,
    mob_storage: HashMap<i32, i32>,
    mob_hp_data: HashMap<i32, i32>,
    known_player_ids: HashSet<i32>,
    confirmed_summon_ids: HashSet<i32>,
    hostile_target_ids: HashSet<i32>,
    current_target: i32,

    // Local player
    local_player_id: Option<i64>,
    local_character_name: Option<String>,
}

impl DataStorage {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                target_combat: HashMap::new(),
                actor_jobs: HashMap::new(),
                nickname_storage: HashMap::new(),
                pending_nicknames: HashMap::new(),
                permanent_nicknames: HashMap::new(),
                summon_storage: HashMap::new(),
                mob_storage: HashMap::new(),
                mob_hp_data: HashMap::new(),
                known_player_ids: HashSet::new(),
                confirmed_summon_ids: HashSet::new(),
                hostile_target_ids: HashSet::new(),
                current_target: 0,
                local_player_id: None,
                local_character_name: None,
            }),
            damage_generation: AtomicI64::new(0),
        }
    }

    pub fn damage_generation(&self) -> i64 {
        self.damage_generation.load(Ordering::Relaxed)
    }

    pub fn set_local_character_name(&self, name: Option<String>) {
        self.inner.write().local_character_name = name;
    }

    pub fn local_character_name(&self) -> Option<String> {
        self.inner.read().local_character_name.clone()
    }

    pub fn set_local_player_id(&self, id: Option<i64>) {
        self.inner.write().local_player_id = id;
    }

    pub fn local_player_id(&self) -> Option<i64> {
        self.inner.read().local_player_id
    }

    pub fn append_damage(&self, pdp: ParsedDamagePacket) {
        let mut inner = self.inner.write();
        let skill_code = pdp.skill_code();
        let actor_id = pdp.actor_id();
        let target_id = pdp.target_id();

        // Skip NPC actors using NPC skills
        let uses_npc_skill = (1_000_000..=9_999_999).contains(&skill_code);
        if inner.mob_storage.contains_key(&actor_id)
            && !inner.summon_storage.contains_key(&actor_id)
            && uses_npc_skill
        {
            return;
        }

        // Track player skill usage
        if is_player_skill(skill_code) && !inner.confirmed_summon_ids.contains(&actor_id) {
            let is_new = inner.known_player_ids.insert(actor_id);
            if is_new {
                inner.summon_storage.remove(&actor_id);
                purge_friendly_damage(&mut inner, actor_id);
            }
        }

        // Skip friendly actions
        if is_friendly_action(&inner, actor_id, target_id) {
            return;
        }

        // Track hostile targets
        let resolved = summon_resolver::resolve(actor_id, &inner.summon_storage);
        if inner.known_player_ids.contains(&resolved) {
            inner.hostile_target_ids.insert(target_id);
        }

        // Track actor job
        if let Some(job) = JobClass::convert_from_skill(skill_code) {
            inner.actor_jobs.entry(actor_id).or_insert(job);
        }

        let timestamp = pdp.timestamp();
        let packet_id = pdp.id();

        // Get or create target combat data
        let target_data = inner.target_combat.entry(target_id).or_insert_with(|| {
            TargetCombatData::new(target_id, timestamp)
        });

        // Idle reset check (30s gap)
        if target_data.last_damage_time > 0
            && timestamp - target_data.last_damage_time > IDLE_RESET_MS
        {
            tracing::info!("Idle reset: target {} — gap {}ms", target_id,
                timestamp - target_data.last_damage_time);
            *target_data = TargetCombatData::new(target_id, timestamp);
        }

        // Update target timing
        if timestamp < target_data.first_damage_time {
            target_data.first_damage_time = timestamp;
        }
        if timestamp > target_data.last_damage_time {
            target_data.last_damage_time = timestamp;
        }
        let total_dmg = pdp.total_damage();
        target_data.total_damage += total_dmg as i64;
        target_data.last_packet_id = packet_id;

        // Update actor data within target
        let actor_data = target_data.actors.entry(actor_id).or_insert_with(ActorCombatData::new);
        actor_data.total_damage += total_dmg as i64;
        if actor_data.job.is_none() {
            actor_data.job = JobClass::convert_from_skill(skill_code);
        }

        // Update skill data
        let skill_key = (skill_code, pdp.is_dot());
        let skill_data = actor_data.skills.entry(skill_key).or_insert_with(|| {
            SkillCombatData::new(skill_code, pdp.is_dot())
        });
        skill_data.hit_count += 1;
        skill_data.total_damage += total_dmg;
        let hit_dmg = pdp.damage();
        if hit_dmg < skill_data.min_damage { skill_data.min_damage = hit_dmg; }
        if hit_dmg > skill_data.max_damage { skill_data.max_damage = hit_dmg; }
        if pdp.is_crit() { skill_data.crit_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Back) { skill_data.back_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Parry) { skill_data.parry_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Perfect) { skill_data.perfect_count += 1; }
        if pdp.specials().contains(&SpecialDamage::Double) { skill_data.double_count += 1; }
        if pdp.multi_hit_count() > 0 {
            skill_data.multi_hit_count += 1;
            skill_data.multi_hit_damage += pdp.multi_hit_damage();
            skill_data.multi_hit_hits += pdp.multi_hit_count();
        }
        skill_data.heal_amount += pdp.heal_amount();
        skill_data.hit_timestamps.push(timestamp);
        for (i, &flag) in pdp.spec_flags().iter().enumerate() {
            if flag { skill_data.spec_flags[i] = true; }
        }

        self.damage_generation.fetch_add(1, Ordering::Relaxed);

        // Apply pending nickname
        apply_pending_nickname(&mut inner, actor_id);
    }

    pub fn append_mob(&self, mid: i32, code: i32) {
        self.inner.write().mob_storage.insert(mid, code);
    }

    pub fn append_mob_hp(&self, mid: i32, hp: i32) {
        if hp > 0 {
            self.inner.write().mob_hp_data.insert(mid, hp);
        }
    }

    pub fn is_mob(&self, id: i32) -> bool {
        self.inner.read().mob_storage.contains_key(&id)
    }

    pub fn is_damage_target(&self, id: i32) -> bool {
        self.inner.read().target_combat.contains_key(&id)
    }

    pub fn is_summon(&self, id: i32) -> bool {
        self.inner.read().summon_storage.contains_key(&id)
    }

    pub fn is_confirmed_summon(&self, id: i32) -> bool {
        self.inner.read().confirmed_summon_ids.contains(&id)
    }

    pub fn register_confirmed_summon_by_id(&self, summon_id: i32, owner_id: i32) {
        tracing::debug!("Summon confirmed (5F 00): {} owned by {}", summon_id, owner_id);
        let mut inner = self.inner.write();
        inner.confirmed_summon_ids.insert(summon_id);
        inner.known_player_ids.remove(&summon_id);
        inner.summon_storage.insert(summon_id, owner_id);
        purge_friendly_damage(&mut inner, summon_id);
    }

    pub fn append_summon(&self, summoner: i32, summon: i32) {
        let mut inner = self.inner.write();

        // Guards from Kotlin
        if inner.nickname_storage.contains_key(&summon) { return; }
        if inner.known_player_ids.contains(&summon) { return; }
        if inner.hostile_target_ids.contains(&summon) { return; }
        if inner.summon_storage.contains_key(&summoner) { return; }
        if inner.mob_storage.contains_key(&summoner) && !inner.summon_storage.contains_key(&summoner) { return; }

        // Job compatibility check
        let summon_job = inner.actor_jobs.get(&summon).copied();
        let owner_job = inner.actor_jobs.get(&summoner).copied();
        if let (Some(sj), Some(oj)) = (summon_job, owner_job) {
            if sj != oj { return; }
        }

        tracing::debug!("Summon linked: {} owned by {}", summon, summoner);
        inner.summon_storage.insert(summon, summoner);
    }

    pub fn append_nickname(&self, uid: i32, nickname: &str) {
        let mut inner = self.inner.write();
        append_nickname_inner(&mut inner, uid, nickname);
    }

    pub fn set_permanent_nickname(&self, uid: i32, nickname: &str) {
        let mut inner = self.inner.write();
        inner.permanent_nicknames.insert(uid, nickname.to_string());
        append_nickname_inner(&mut inner, uid, nickname);
    }

    pub fn cache_pending_nickname(&self, uid: i32, nickname: &str) {
        let mut inner = self.inner.write();
        if inner.nickname_storage.contains_key(&uid) { return; }
        inner.pending_nicknames.insert(uid, nickname.to_string());
    }

    pub fn has_nickname(&self, uid: i32) -> bool {
        self.inner.read().nickname_storage.contains_key(&uid)
    }

    pub fn get_nickname(&self, uid: i32) -> Option<String> {
        self.inner.read().nickname_storage.get(&uid).cloned()
    }

    pub fn actor_appears_in_combat(&self, actor_id: i32) -> bool {
        let inner = self.inner.read();
        // Check if actor appears as an attacker in any target
        for target_data in inner.target_combat.values() {
            if target_data.actors.contains_key(&actor_id) {
                return true;
            }
        }
        // Check if actor is a target
        if inner.target_combat.contains_key(&actor_id) {
            return true;
        }
        inner.summon_storage.contains_key(&actor_id)
    }

    pub fn get_nicknames(&self) -> HashMap<i32, String> {
        self.inner.read().nickname_storage.clone()
    }

    pub fn get_summon_data(&self) -> HashMap<i32, i32> {
        self.inner.read().summon_storage.clone()
    }

    pub fn get_mob_hp_data(&self) -> HashMap<i32, i32> {
        self.inner.read().mob_hp_data.clone()
    }

    pub fn get_mob_data(&self) -> HashMap<i32, i32> {
        self.inner.read().mob_storage.clone()
    }

    pub fn set_current_target(&self, target: i32) {
        self.inner.write().current_target = target;
    }

    pub fn current_target(&self) -> i32 {
        self.inner.read().current_target
    }

    /// Get a snapshot of all target combat aggregates.
    /// This is cheap: clones a small map of aggregates, not raw packets.
    pub fn get_combat_snapshot(&self) -> HashMap<i32, TargetCombatData> {
        self.inner.read().target_combat.clone()
    }

    pub fn flush(&self) {
        let mut inner = self.inner.write();
        inner.target_combat.clear();
        inner.actor_jobs.clear();
        inner.summon_storage.clear();
        inner.known_player_ids.clear();
        inner.confirmed_summon_ids.clear();
        inner.hostile_target_ids.clear();
        inner.mob_hp_data.clear();
        inner.current_target = 0;
    }

    pub fn reset_nicknames(&self) {
        let mut inner = self.inner.write();
        inner.nickname_storage.clear();
        inner.pending_nicknames.clear();
        let permanent: Vec<(i32, String)> = inner.permanent_nicknames.iter().map(|(&k, v)| (k, v.clone())).collect();
        for (uid, nick) in permanent {
            inner.nickname_storage.insert(uid, nick);
        }
    }
}

fn append_nickname_inner(inner: &mut Inner, uid: i32, nickname: &str) {
    let existing = inner.nickname_storage.get(&uid);
    if let Some(existing) = existing {
        if existing == nickname {
            if let Some(ref local_name) = inner.local_character_name {
                if local_name.trim() == nickname.trim() {
                    inner.local_player_id = Some(uid as i64);
                }
            }
            return;
        }
        if nickname.as_bytes().len() == 2 && nickname.as_bytes().len() < existing.as_bytes().len() {
            return;
        }
    }

    inner.nickname_storage.insert(uid, nickname.to_string());

    if !inner.confirmed_summon_ids.contains(&uid) {
        inner.summon_storage.remove(&uid);
    }

    if !inner.confirmed_summon_ids.contains(&uid) {
        let is_new = inner.known_player_ids.insert(uid);
        if is_new {
            purge_friendly_damage(inner, uid);
        }
    }

    if let Some(ref local_name) = inner.local_character_name {
        if local_name.trim() == nickname.trim() {
            inner.local_player_id = Some(uid as i64);
        }
    }
}

fn apply_pending_nickname(inner: &mut Inner, uid: i32) {
    if inner.nickname_storage.contains_key(&uid) { return; }
    if let Some(pending) = inner.pending_nicknames.remove(&uid) {
        append_nickname_inner(inner, uid, &pending);
    }
}

fn is_friendly_action(inner: &Inner, actor_id: i32, target_id: i32) -> bool {
    let resolved_actor = summon_resolver::resolve(actor_id, &inner.summon_storage);
    let resolved_target = summon_resolver::resolve(target_id, &inner.summon_storage);
    inner.known_player_ids.contains(&resolved_actor) && inner.known_player_ids.contains(&resolved_target)
}

/// Remove friendly-fire damage from aggregates when a new player is identified.
fn purge_friendly_damage(inner: &mut Inner, _uid: i32) {
    let mut to_remove: Vec<(i32, Vec<i32>)> = Vec::new();

    for (&target_id, target_data) in &inner.target_combat {
        let mut actors_to_remove = Vec::new();
        for &actor_id in target_data.actors.keys() {
            if is_friendly_action(inner, actor_id, target_id) {
                actors_to_remove.push(actor_id);
            }
        }
        if !actors_to_remove.is_empty() {
            to_remove.push((target_id, actors_to_remove));
        }
    }

    for (target_id, actor_ids) in to_remove {
        if let Some(target_data) = inner.target_combat.get_mut(&target_id) {
            for actor_id in actor_ids {
                if let Some(actor_data) = target_data.actors.remove(&actor_id) {
                    target_data.total_damage -= actor_data.total_damage;
                }
            }
            if target_data.actors.is_empty() {
                inner.target_combat.remove(&target_id);
            }
        }
    }
}

pub fn is_player_skill(skill_code: i32) -> bool {
    (10_000_000..=29_999_999).contains(&skill_code) || (30_000_000..=30_999_999).contains(&skill_code)
}
