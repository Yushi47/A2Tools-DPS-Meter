use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::RwLock;

use crate::entity::damage_packet::ParsedDamagePacket;
use crate::entity::job_class::JobClass;
use crate::entity::summon_resolver;

const MAX_STORED_PACKETS: usize = 200_000;
const MAX_SNAPSHOT_PACKETS: usize = 100_000;

/// In-memory storage for all captured damage packets, nicknames, summons, and mob data.
pub struct DataStorage {
    inner: RwLock<Inner>,
    damage_generation: AtomicI64,
}

struct Inner {
    by_target: HashMap<i32, Vec<ParsedDamagePacket>>,
    by_actor: HashMap<i32, Vec<ParsedDamagePacket>>,
    nickname_storage: HashMap<i32, String>,
    pending_nicknames: HashMap<i32, String>,
    permanent_nicknames: HashMap<i32, String>,
    summon_storage: HashMap<i32, i32>,
    mob_storage: HashMap<i32, i32>,
    mob_hp_data: HashMap<i32, i32>,
    known_player_ids: HashSet<i32>,
    confirmed_summon_ids: HashSet<i32>,
    hostile_target_ids: HashSet<i32>,
    packet_order: VecDeque<ParsedDamagePacket>,
    current_target: i32,
    snapshot_dirty: bool,
    cached_by_target: HashMap<i32, Vec<ParsedDamagePacket>>,
    cached_by_actor: HashMap<i32, Vec<ParsedDamagePacket>>,

    // Local player
    local_player_id: Option<i64>,
    local_character_name: Option<String>,
}

impl DataStorage {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                by_target: HashMap::new(),
                by_actor: HashMap::new(),
                nickname_storage: HashMap::new(),
                pending_nicknames: HashMap::new(),
                permanent_nicknames: HashMap::new(),
                summon_storage: HashMap::new(),
                mob_storage: HashMap::new(),
                mob_hp_data: HashMap::new(),
                known_player_ids: HashSet::new(),
                confirmed_summon_ids: HashSet::new(),
                hostile_target_ids: HashSet::new(),
                packet_order: VecDeque::new(),
                current_target: 0,
                snapshot_dirty: true,
                cached_by_target: HashMap::new(),
                cached_by_actor: HashMap::new(),
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
                purge_friendly_packets(&mut inner, actor_id);
            }
        }

        // Skip friendly actions
        if is_friendly_action(&inner, actor_id, target_id) {
            return;
        }

        inner.by_actor.entry(actor_id).or_default().push(pdp.clone());
        inner.by_target.entry(target_id).or_default().push(pdp.clone());
        inner.packet_order.push_back(pdp.clone());

        // Track hostile targets
        let resolved = summon_resolver::resolve(actor_id, &inner.summon_storage);
        if inner.known_player_ids.contains(&resolved) {
            inner.hostile_target_ids.insert(target_id);
        }

        inner.snapshot_dirty = true;
        self.damage_generation.fetch_add(1, Ordering::Relaxed);

        // Trim
        while inner.packet_order.len() > MAX_STORED_PACKETS {
            if let Some(oldest) = inner.packet_order.pop_front() {
                remove_packet_refs(&mut inner, &oldest);
            }
        }

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
        let inner = self.inner.read();
        inner.cached_by_target.contains_key(&id) || inner.by_target.contains_key(&id)
    }

    pub fn is_summon(&self, id: i32) -> bool {
        self.inner.read().summon_storage.contains_key(&id)
    }

    pub fn is_confirmed_summon(&self, id: i32) -> bool {
        self.inner.read().confirmed_summon_ids.contains(&id)
    }

    pub fn register_confirmed_summon_by_id(&self, summon_id: i32, owner_id: i32) {
        let mut inner = self.inner.write();
        inner.confirmed_summon_ids.insert(summon_id);
        inner.known_player_ids.remove(&summon_id);
        inner.summon_storage.insert(summon_id, owner_id);
        purge_friendly_packets(&mut inner, summon_id);
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
        let summon_job = infer_job_from_skills(&inner, summon);
        let owner_job = infer_job_from_skills(&inner, summoner);
        if let (Some(sj), Some(oj)) = (summon_job, owner_job) {
            if sj != oj { return; }
        }

        inner.summon_storage.insert(summon, summoner);
    }

    pub fn append_nickname(&self, uid: i32, nickname: &str) {
        let mut inner = self.inner.write();
        append_nickname_inner(&mut inner, uid, nickname);
    }

    /// Register a nickname that survives reset_nicknames() calls.
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
        inner.by_actor.contains_key(&actor_id)
            || inner.by_target.contains_key(&actor_id)
            || inner.summon_storage.contains_key(&actor_id)
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

    /// Remove all packets for a target that have timestamps before `before_ts`.
    /// Used when an idle gap is detected to start a fresh fight window.
    pub fn prune_target_before(&self, target_id: i32, before_ts: i64) {
        let mut inner = self.inner.write();
        // Remove old packets from the target's list
        if let Some(packets) = inner.by_target.get_mut(&target_id) {
            let old_ids: std::collections::HashSet<i64> = packets.iter()
                .filter(|p| p.timestamp() < before_ts)
                .map(|p| p.id())
                .collect();
            if old_ids.is_empty() { return; }
            packets.retain(|p| !old_ids.contains(&p.id()));
            // Also remove from by_actor and packet_order
            for actor_packets in inner.by_actor.values_mut() {
                actor_packets.retain(|p| !old_ids.contains(&p.id()));
            }
            inner.by_actor.retain(|_, v| !v.is_empty());
            inner.packet_order.retain(|p| !old_ids.contains(&p.id()));
            inner.snapshot_dirty = true;
        }
    }

    pub fn current_target(&self) -> i32 {
        self.inner.read().current_target
    }

    /// Get snapshot of packets by target (rebuilds cache if dirty).
    pub fn get_by_target_snapshot(&self) -> HashMap<i32, Vec<ParsedDamagePacket>> {
        let mut inner = self.inner.write();
        rebuild_snapshots(&mut inner);
        inner.cached_by_target.clone()
    }

    /// Get snapshot of packets by actor (rebuilds cache if dirty).
    pub fn get_by_actor_snapshot(&self) -> HashMap<i32, Vec<ParsedDamagePacket>> {
        let mut inner = self.inner.write();
        rebuild_snapshots(&mut inner);
        inner.cached_by_actor.clone()
    }

    pub fn flush(&self) {
        let mut inner = self.inner.write();
        inner.by_actor.clear();
        inner.by_target.clear();
        inner.packet_order.clear();
        inner.summon_storage.clear();
        inner.known_player_ids.clear();
        inner.confirmed_summon_ids.clear();
        inner.hostile_target_ids.clear();
        inner.mob_hp_data.clear();
        inner.current_target = 0;
        inner.snapshot_dirty = true;
        inner.cached_by_target.clear();
        inner.cached_by_actor.clear();
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

fn rebuild_snapshots(inner: &mut Inner) {
    if !inner.snapshot_dirty {
        return;
    }
    let mut by_target: HashMap<i32, Vec<ParsedDamagePacket>> = HashMap::new();
    let mut by_actor: HashMap<i32, Vec<ParsedDamagePacket>> = HashMap::new();
    let size = inner.packet_order.len();
    let skip = size.saturating_sub(MAX_SNAPSHOT_PACKETS);

    for (idx, pdp) in inner.packet_order.iter().enumerate() {
        if idx >= skip {
            by_target.entry(pdp.target_id()).or_default().push(pdp.clone());
            by_actor.entry(pdp.actor_id()).or_default().push(pdp.clone());
        }
    }
    inner.cached_by_target = by_target;
    inner.cached_by_actor = by_actor;
    inner.snapshot_dirty = false;
}

fn append_nickname_inner(inner: &mut Inner, uid: i32, nickname: &str) {
    let existing = inner.nickname_storage.get(&uid);
    if let Some(existing) = existing {
        if existing == nickname {
            // Check local player match
            if let Some(ref local_name) = inner.local_character_name {
                if local_name.trim() == nickname.trim() {
                    inner.local_player_id = Some(uid as i64);
                }
            }
            return;
        }
        // Skip if new name is shorter 2-byte vs existing longer name
        if nickname.as_bytes().len() == 2 && nickname.as_bytes().len() < existing.as_bytes().len() {
            return;
        }
    }

    inner.nickname_storage.insert(uid, nickname.to_string());

    // Remove false summon mapping
    if !inner.confirmed_summon_ids.contains(&uid) {
        inner.summon_storage.remove(&uid);
    }

    // Register as known player
    if !inner.confirmed_summon_ids.contains(&uid) {
        let is_new = inner.known_player_ids.insert(uid);
        if is_new {
            purge_friendly_packets(inner, uid);
        }
    }

    // Check local player
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

fn purge_friendly_packets(inner: &mut Inner, uid: i32) {
    let mut to_remove_ids: HashSet<i64> = HashSet::new();

    if let Some(packets) = inner.by_actor.get(&uid) {
        for pdp in packets {
            if is_friendly_action(inner, pdp.actor_id(), pdp.target_id()) {
                to_remove_ids.insert(pdp.id());
            }
        }
    }
    if let Some(packets) = inner.by_target.get(&uid) {
        for pdp in packets {
            if is_friendly_action(inner, pdp.actor_id(), pdp.target_id()) {
                to_remove_ids.insert(pdp.id());
            }
        }
    }

    if to_remove_ids.is_empty() { return; }

    inner.packet_order.retain(|p| !to_remove_ids.contains(&p.id()));
    for packets in inner.by_actor.values_mut() {
        packets.retain(|p| !to_remove_ids.contains(&p.id()));
    }
    for packets in inner.by_target.values_mut() {
        packets.retain(|p| !to_remove_ids.contains(&p.id()));
    }
    inner.by_actor.retain(|_, v| !v.is_empty());
    inner.by_target.retain(|_, v| !v.is_empty());
    inner.snapshot_dirty = true;
}

fn remove_packet_refs(inner: &mut Inner, pdp: &ParsedDamagePacket) {
    if let Some(packets) = inner.by_actor.get_mut(&pdp.actor_id()) {
        if !packets.is_empty() && packets[0].id() == pdp.id() {
            packets.remove(0);
        }
        if packets.is_empty() {
            inner.by_actor.remove(&pdp.actor_id());
        }
    }
    if let Some(packets) = inner.by_target.get_mut(&pdp.target_id()) {
        if !packets.is_empty() && packets[0].id() == pdp.id() {
            packets.remove(0);
        }
        if packets.is_empty() {
            inner.by_target.remove(&pdp.target_id());
        }
    }
}

fn infer_job_from_skills(inner: &Inner, actor_id: i32) -> Option<JobClass> {
    let packets = inner.by_actor.get(&actor_id)?;
    for pdp in packets {
        if let Some(job) = JobClass::convert_from_skill(pdp.skill_code()) {
            return Some(job);
        }
    }
    None
}

pub fn is_player_skill(skill_code: i32) -> bool {
    (10_000_000..=29_999_999).contains(&skill_code) || (30_000_000..=30_999_999).contains(&skill_code)
}
