use std::collections::HashSet;
use std::sync::Arc;

use lz4_flex::decompress;

use crate::combat::data_storage::DataStorage;
use crate::entity::damage_packet::ParsedDamagePacket;
use crate::entity::special_damage::SpecialDamage;
use crate::i18n::lookup::{NpcLookup, SkillLookup};

/// VarInt decode result.
#[derive(Debug, Clone, Copy)]
pub struct VarIntResult {
    pub value: i32,
    pub length: i32,
}

impl VarIntResult {
    pub fn invalid() -> Self {
        Self { value: -1, length: -1 }
    }
}

/// Context for resolving a compact skill aggregation packet.
#[derive(Debug, Clone)]
struct PendingCompactSkillContext {
    actor_id: i32,
    skill_raw: i32,
}

/// Bounded LRU set for deduplication.
struct BoundedHashSet {
    set: Vec<String>,
    max_size: usize,
}

impl BoundedHashSet {
    fn new(max_size: usize) -> Self {
        Self {
            set: Vec::new(),
            max_size,
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.set.iter().any(|s| s == key)
    }

    fn insert(&mut self, key: String) -> bool {
        if self.contains(&key) {
            return false;
        }
        if self.set.len() >= self.max_size {
            self.set.remove(0);
        }
        self.set.push(key);
        true
    }
}

/// The core binary protocol parser for AION 2 game packets.
/// Ported exactly from Kotlin StreamProcessor.
pub struct StreamProcessor {
    data_storage: Arc<DataStorage>,
    skill_lookup: Arc<SkillLookup>,
    npc_lookup: Arc<NpcLookup>,
    seen_embedded_hexes: BoundedHashSet,
    dot_damage_skill_ids: HashSet<i32>,
    pending_compact_skill_context: Option<PendingCompactSkillContext>,
    /// When set, override the timestamp on all created packets (for replay mode).
    override_timestamp: Option<i64>,
}

impl StreamProcessor {
    pub fn new(data_storage: Arc<DataStorage>, skill_lookup: Arc<SkillLookup>, npc_lookup: Arc<NpcLookup>) -> Self {
        Self {
            data_storage,
            skill_lookup,
            npc_lookup,
            seen_embedded_hexes: BoundedHashSet::new(16_384),
            dot_damage_skill_ids: HashSet::new(), // loaded lazily
            pending_compact_skill_context: None,
            override_timestamp: None,
        }
    }

    pub fn set_dot_skill_ids(&mut self, ids: HashSet<i32>) {
        self.dot_damage_skill_ids = ids;
    }

    /// Set an override timestamp for all packets created by this processor.
    /// Used in replay mode to use capture-time timestamps instead of wall clock.
    pub fn set_override_timestamp(&mut self, ts: Option<i64>) {
        self.override_timestamp = ts;
    }

    /// Parse as many complete packets as possible from the buffer.
    /// Returns the number of bytes consumed.
    pub fn consume_stream(&mut self, buffer: &[u8]) -> usize {
        let mut offset = 0;

        while offset < buffer.len() {
            // 1. Skip zero padding
            if buffer[offset] == 0x00 {
                offset += 1;
                continue;
            }

            let length_info = read_varint(buffer, offset);
            if length_info.length <= 0 || length_info.value <= 0 {
                if offset + 5 > buffer.len() {
                    break;
                }
                offset += 1;
                continue;
            }

            // 2. AION 2 quirk: length - 3 == physical size
            let total_packet_bytes = (length_info.value - 3) as usize;

            // Resync on invalid sizes
            if total_packet_bytes == 0 || total_packet_bytes > 65535 {
                offset += 1;
                continue;
            }

            // 3. TCP fragmentation check (anti-stall gate)
            if offset + total_packet_bytes > buffer.len() {
                if total_packet_bytes > 16384 {
                    offset += 1;
                    continue;
                }
                break; // Legitimate fragment
            }

            // 4. Check for FF FF compressed bundle
            let payload_start = length_info.length as usize;
            let is_bundle = offset + total_packet_bytes <= buffer.len()
                && payload_start + 1 < total_packet_bytes
                && buffer[offset + payload_start] == 0xFF
                && buffer[offset + payload_start + 1] == 0xFF;

            if is_bundle {
                let bundle_size = total_packet_bytes + 1;
                if offset + bundle_size > buffer.len() {
                    break;
                }
                let bundle_payload = &buffer[offset + payload_start..offset + bundle_size];
                self.unwrap_bundle(bundle_payload);
                offset += bundle_size;
            } else {
                let full_packet = &buffer[offset..offset + total_packet_bytes];
                self.parse_perfect_packet(full_packet);
                offset += total_packet_bytes;
            }
        }

        // Scan for embedded 04 8D ownership sub-packets
        if buffer.len() >= 4 {
            self.scan_for_embedded_04_8d(buffer);
        }

        offset
    }

    fn unwrap_bundle(&mut self, payload: &[u8]) {
        // payload starts at FF FF
        // Format: FF FF (2) + decompressed_size (4 LE) + LZ4 compressed data
        if payload.len() < 7 {
            return;
        }

        let decompressed_size = u32::from_le_bytes([
            payload[2], payload[3], payload[4], payload[5],
        ]) as usize;

        if decompressed_size == 0 || decompressed_size > 1_000_000 {
            return;
        }

        let compressed = &payload[6..];
        let decompressed = match decompress(compressed, decompressed_size) {
            Ok(d) => d,
            Err(_) => return,
        };

        // Walk decompressed data as varint-framed inner packets
        self.pending_compact_skill_context = None;
        let mut offset = 0;

        while offset < decompressed.len() {
            if decompressed[offset] == 0x00 {
                offset += 1;
                continue;
            }

            let length_info = read_varint(&decompressed, offset);
            if length_info.length <= 0 || length_info.value <= 0 {
                break;
            }

            if length_info.value <= 3 {
                offset += 1;
                continue;
            }
            let inner_total_bytes = (length_info.value - 3) as usize;

            let inner_packet_end = offset + inner_total_bytes;
            if inner_packet_end > decompressed.len() {
                break;
            }

            let inner_packet = &decompressed[offset..inner_packet_end];

            // Check for nested FF-FF bundle
            let inner_payload_start = length_info.length as usize;
            let is_nested_bundle = inner_packet.len() > inner_payload_start + 1
                && inner_packet[inner_payload_start] == 0xFF
                && inner_packet[inner_payload_start + 1] == 0xFF;

            if is_nested_bundle {
                let nested_payload = &inner_packet[inner_payload_start..];
                self.unwrap_bundle(nested_payload);
            } else {
                if let Some(ctx) = self.extract_pending_compact_skill_context(inner_packet) {
                    self.pending_compact_skill_context = Some(ctx);
                }
                self.parse_perfect_packet(inner_packet);
            }

            offset += inner_total_bytes;
        }

        // Scan for embedded 04 8D and 40 36 in decompressed data
        self.scan_for_embedded_04_8d(&decompressed);
        self.scan_for_embedded_40_36(&decompressed);

        self.pending_compact_skill_context = None;
    }

    fn parse_perfect_packet(&mut self, packet: &[u8]) -> bool {
        if packet.len() < 3 {
            return false;
        }

        let parsed_damage = self.parsing_damage(packet, true, false);
        let parsed_ownership = self.parse_summon_ownership_packet(packet);
        let parsed_summon = self.parse_summon_packet(packet);
        let parsed_name = self.parse_actor_name_binding_rules(packet)
            || self.parse_loot_attribution_actor_name(packet)
            || self.parsing_nickname(packet);
        let parsed_hp = self.parse_hp_mp_update_packet(packet);
        self.parse_death_packet(packet);

        if !parsed_damage && !parsed_name && !parsed_summon && !parsed_ownership && !parsed_hp {
            self.parse_dot_packet(packet);
        }

        parsed_damage || parsed_name
    }

    // ===== DEATH PACKET (41 36) =====

    fn parse_death_packet(&self, packet: &[u8]) {
        let length_info = read_varint(packet, 0);
        if length_info.length < 0 {
            return;
        }
        let offset = length_info.length as usize;
        if offset + 1 >= packet.len() {
            return;
        }
        // Death opcode. Pre-2026-06 it was 0x3641 ([0x41,0x36]); the June 2026
        // update shifted the 0x36 spawn/death family by +1, so it is now 0x3642
        // ([0x42,0x36]). Accept both — the flag==3 check below rejects anything
        // that isn't actually a combat death.
        if packet[offset + 1] != 0x36 || (packet[offset] != 0x41 && packet[offset] != 0x42) {
            return;
        }
        let mut pos = offset + 2;

        let entity_info = read_varint(packet, pos);
        if entity_info.length <= 0 {
            return;
        }
        let entity_id = entity_info.value;
        pos += entity_info.length as usize;

        // Skip VarInt (always 0)
        let skip_info = read_varint(packet, pos);
        if skip_info.length <= 0 {
            return;
        }
        pos += skip_info.length as usize;

        // Death flag: 1 = zone-init (entity loaded dead), 3 = combat death
        let flag_info = read_varint(packet, pos);
        if flag_info.length <= 0 {
            return;
        }

        if flag_info.value == 3 {
            tracing::trace!("Death event: entity {} killed in combat", entity_id);
            self.data_storage.mark_entity_dead(entity_id);
        }
    }

    // ===== DOT PACKET =====

    fn parse_dot_packet(&mut self, packet: &[u8]) {
        let length_info = read_varint(packet, 0);
        if length_info.length < 0 {
            return;
        }
        let offset = length_info.length as usize;

        if packet.len() <= offset + 1 {
            return;
        }
        if packet[offset] != 0x05 || packet[offset + 1] != 0x38 {
            return;
        }
        let mut offset = offset + 2;
        let target_info = read_varint(packet, offset);
        if target_info.length < 0 {
            return;
        }
        offset += target_info.length as usize;

        if offset >= packet.len() {
            return;
        }
        let effect_type = packet[offset] as u32;
        offset += 1;
        // Effect type: 0x02 = damage (pre-patch), 0x0A = damage (post-patch 2026-04-01).
        // Non-damage values: 0x00 = status, 0x01 = heal, 0x08 = buff, 0x09 = heal, 0x0B = HoT.
        // Must use exact match, NOT bitmask — 0x0B (HoT) has bit 1 set and would leak through.
        if effect_type != 0x02 && effect_type != 0x0A {
            return;
        }

        let actor_info = read_varint(packet, offset);
        if actor_info.length < 0 || actor_info.value == target_info.value {
            tracing::debug!("DOT: bad actor or self-damage");
            return;
        }
        offset += actor_info.length as usize;

        let unknown_info = read_varint(packet, offset);
        if unknown_info.length < 0 {
            tracing::debug!("DOT: bad unknown varint");
            return;
        }
        offset += unknown_info.length as usize;

        if offset + 4 > packet.len() {
            tracing::debug!("DOT: packet too short for skill code");
            return;
        }
        let skill_code = parse_u32_le(packet, offset) as i32 / 100;
        offset += 4;

        if !is_valid_skill_code(skill_code) {
            tracing::debug!("DOT: invalid skill code {}", skill_code);
            return;
        }

        if !self.dot_damage_skill_ids.contains(&skill_code) {
            tracing::trace!("DOT: skill {} not in dot_ids (set size={})", skill_code, self.dot_damage_skill_ids.len());
            return;
        }

        let damage_info = read_varint(packet, offset);
        if damage_info.length < 0 || damage_info.value <= 0 {
            tracing::debug!("DOT: bad damage varint");
            return;
        }

        let mut pdp = ParsedDamagePacket::new();
        if let Some(ts) = self.override_timestamp {
            pdp.set_timestamp(ts);
        }
        pdp.set_dot(true);
        pdp.set_target_id(target_info.value);
        pdp.set_actor_id(actor_info.value);
        pdp.set_skill_code(skill_code);
        pdp.set_damage(damage_info.value);

        if pdp.actor_id() != pdp.target_id() {
            self.data_storage.append_damage(pdp);
        }
    }

    // ===== HP/MP UPDATE =====

    fn parse_hp_mp_update_packet(&self, packet: &[u8]) -> bool {
        let length_info = read_varint(packet, 0);
        if length_info.length < 0 {
            return false;
        }
        let offset = length_info.length as usize;
        if offset + 1 >= packet.len() {
            return false;
        }
        if packet[offset] != 0x1B || packet[offset + 1] != 0x92 {
            return false;
        }

        let mut pos = offset + 2;
        let actor_info = read_varint(packet, pos);
        if actor_info.length <= 0 || actor_info.value < 100 || actor_info.value > 9_999_999 {
            return false;
        }
        let actor_id = actor_info.value;
        pos += actor_info.length as usize;

        let hp_info = read_varint(packet, pos);
        if hp_info.length <= 0 {
            return false;
        }
        pos += hp_info.length as usize;

        let hp_max_info = read_varint(packet, pos);
        if hp_max_info.length <= 0 || hp_max_info.value <= 0 || hp_max_info.value > 50_000_000 {
            return false;
        }

        // Always store HP — the entity may not be in mob_data yet if spawn
        // packet arrived before the capture started
        self.data_storage.append_mob_hp(actor_id, hp_max_info.value);

        true
    }

    // ===== SUMMON OWNERSHIP (04 8D) =====

    fn parse_summon_ownership_packet(&self, packet: &[u8]) -> bool {
        let length_info = read_varint(packet, 0);
        if length_info.length < 0 {
            return false;
        }
        let offset = length_info.length as usize;
        if offset + 1 >= packet.len() {
            return false;
        }
        if packet[offset] != 0x04 || packet[offset + 1] != 0x8D {
            return false;
        }

        let mut pos = offset + 2;
        let summon_info = read_varint(packet, pos);
        if summon_info.length <= 0 || summon_info.value < 100 {
            return false;
        }
        let summon_id = summon_info.value;
        pos += summon_info.length as usize;

        // Skip 4-byte fixed field
        if pos + 4 > packet.len() {
            return false;
        }
        pos += 4;

        let owner_info = read_varint(packet, pos);
        if owner_info.length <= 0 || owner_info.value < 100 {
            return false;
        }
        let owner_id = owner_info.value;
        pos += owner_info.length as usize;

        if owner_id == summon_id {
            return false;
        }

        // Only link confirmed summons
        if self.data_storage.is_confirmed_summon(summon_id) {
            self.data_storage.append_summon(owner_id, summon_id);
        }

        // Name field after owner ID
        let meta_info = read_varint(packet, pos);
        if meta_info.length > 0 {
            pos += meta_info.length as usize;
            if pos < packet.len() {
                let name_len = packet[pos] as usize;
                if (1..=36).contains(&name_len) && pos + 1 + name_len <= packet.len() {
                    self.register_utf8_nickname(packet, owner_id, pos + 1, name_len);
                }
            }
        }

        true
    }

    // ===== EMBEDDED 04 8D SCAN =====

    fn scan_for_embedded_04_8d(&self, data: &[u8]) -> bool {
        let mut found_any = false;
        let mut search_offset = 0;
        let pattern: [u8; 2] = [0x04, 0x8D];

        while search_offset + 1 < data.len() {
            let idx = find_pattern(data, search_offset, &pattern);
            if idx.is_none() {
                break;
            }
            let idx = idx.unwrap();

            search_offset = idx + 2;
            if search_offset >= data.len() {
                break;
            }

            let summon_info = read_varint(data, search_offset);
            if summon_info.length <= 0 || !(100..=9_999_999).contains(&summon_info.value) {
                continue;
            }
            let summon_id = summon_info.value;

            let fixed_field_start = search_offset + summon_info.length as usize;
            if fixed_field_start + 4 > data.len() {
                continue;
            }

            // Scan for E0/E2 07 anchor
            let after_fixed = fixed_field_start + 4;
            let scan_end = std::cmp::min(data.len() - 1, after_fixed + 128);
            let mut anchor_idx = None;
            for i in after_fixed..scan_end {
                if (data[i] == 0xE0 || data[i] == 0xE2) && data[i + 1] == 0x07 {
                    anchor_idx = Some(i);
                    break;
                }
            }

            let anchor_idx = match anchor_idx {
                Some(i) => i,
                None => continue,
            };

            // Read owner ID backward from anchor
            let mut owner_id: i32 = -1;
            for v_len in 1..=3usize {
                if anchor_idx < v_len {
                    continue;
                }
                let v_start = anchor_idx - v_len;
                if v_start < after_fixed && v_start > 0 {
                    // skip if out of range but allow 0
                }
                if !can_read_varint(data, v_start) {
                    continue;
                }
                let v = read_varint(data, v_start);
                if v.length == v_len as i32 && (100..=99_999).contains(&v.value) {
                    owner_id = v.value;
                    break;
                }
            }
            if owner_id == -1 || owner_id == summon_id {
                continue;
            }

            // Read owner name
            let name_len_idx = anchor_idx + 2;
            if name_len_idx >= data.len() {
                continue;
            }
            let name_len = data[name_len_idx] as usize;
            if !(2..=36).contains(&name_len) || name_len_idx + 1 + name_len > data.len() {
                continue;
            }
            let name_bytes = &data[name_len_idx + 1..name_len_idx + 1 + name_len];
            let name = match std::str::from_utf8(name_bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let sanitized = match sanitize_nickname(name) {
                Some(s) => s,
                None => continue,
            };
            if sanitized.len() < 2 {
                continue;
            }

            if self.data_storage.is_confirmed_summon(summon_id) {
                self.data_storage.append_summon(owner_id, summon_id);
            }
            self.data_storage.append_nickname(owner_id, &sanitized);
            found_any = true;

            search_offset = name_len_idx + 1 + name_len;
        }

        found_any
    }

    // ===== EMBEDDED 40 36 SCAN =====

    fn scan_for_embedded_40_36(&mut self, data: &[u8]) {
        let mut i = 0;
        while i + 5 < data.len() {
            // Spawn family shifted +1 in June 2026: mob/summon 0x40->0x41,
            // player 0x44->0x45. Accept both old and new leading bytes.
            if data[i + 1] == 0x36 && matches!(data[i], 0x40 | 0x41 | 0x44 | 0x45) {
                if i > 0 && data[i - 1] == 0x00 {
                    i += 2;
                    continue;
                }
                let target_info = read_varint(data, i + 2);
                if target_info.length > 0 && (100..=9_999_999).contains(&target_info.value) {
                    if data[i] == 0x44 || data[i] == 0x45 {
                        // 44/45 36 = player spawn — extract name
                        self.parse_player_spawn_name(data, i + 2);
                    } else {
                        // 40/41 36 = summon/mob spawn
                        let mut real_id = target_info.value;
                        if real_id > 1_000_000 {
                            real_id = (real_id & 0x3FFF) | 0x4000;
                        }
                        if !self.data_storage.is_mob(real_id) {
                            self.parse_summon_spawn_at(data, i + 2);
                        }
                    }
                }
                i += 2 + target_info.length.max(0) as usize;
            } else {
                i += 1;
            }
        }
    }

    /// Extract player name from a 44 36 player spawn sub-packet.
    /// Structure: <actor_varint> <data...> 07 <name_length> <name_bytes>
    fn parse_player_spawn_name(&self, data: &[u8], offset_after_opcode: usize) {
        let actor_info = read_varint(data, offset_after_opcode);
        if actor_info.length <= 0 || !(100..=99_999).contains(&actor_info.value) {
            return;
        }
        let actor_id = actor_info.value;
        let scan_start = offset_after_opcode + actor_info.length as usize;
        let scan_end = std::cmp::min(data.len().saturating_sub(2), scan_start + 40);

        for j in scan_start..scan_end {
            if data[j] == 0x07 {
                let len_idx = j + 1;
                if len_idx >= data.len() { break; }
                let name_len = data[len_idx] as usize;
                if !(1..=36).contains(&name_len) { continue; }
                let name_start = len_idx + 1;
                let name_end = name_start + name_len;
                if name_end > data.len() { break; }
                if let Ok(name) = std::str::from_utf8(&data[name_start..name_end]) {
                    if let Some(sanitized) = sanitize_nickname(name) {
                        if sanitized.len() >= 2 {
                            self.data_storage.append_nickname(actor_id, &sanitized);
                            return;
                        }
                    }
                }
            }
        }
    }

    // ===== SUMMON PACKET (40 36) =====

    fn parse_summon_packet(&mut self, packet: &[u8]) -> bool {
        let length_info = read_varint(packet, 0);
        if length_info.length < 0 {
            return false;
        }
        let offset = length_info.length as usize;
        if offset + 1 >= packet.len() {
            return false;
        }
        if packet[offset + 1] != 0x36 {
            return false;
        }
        // Player spawn: 0x3644 pre-2026-06, 0x3645 after the June 2026 +1 shift.
        if packet[offset] == 0x44 || packet[offset] == 0x45 {
            self.parse_player_spawn_name(packet, offset + 2);
            return false;
        }
        // Mob/summon spawn: 0x3640 pre-2026-06, 0x3641 after the shift.
        if packet[offset] != 0x40 && packet[offset] != 0x41 {
            return false;
        }
        self.parse_summon_spawn_at(packet, offset + 2)
    }

    fn parse_summon_spawn_at(&mut self, packet: &[u8], offset_after_opcode: usize) -> bool {
        let mut offset = offset_after_opcode;
        let target_info = read_varint(packet, offset);
        if target_info.length < 0 {
            return false;
        }
        offset += target_info.length as usize;

        let mut real_actor_id = target_info.value;
        if real_actor_id > 1_000_000 {
            real_actor_id = (real_actor_id & 0x3FFF) | 0x4000;
        }

        // Detect summon spawn: [xx] 10 00 pattern
        if offset + 2 < packet.len()
            && packet[offset + 1] == 0x10
            && packet[offset + 2] == 0x00
        {
            let owner_id = self.extract_summon_owner_from_spawn(packet, offset);
            if owner_id > 0 && owner_id != real_actor_id {
                self.extract_and_register_mob_type(packet, offset, real_actor_id);
                self.data_storage.register_confirmed_summon_by_id(real_actor_id, owner_id);
                return true;
            }
        }

        // Detect summon spawn with embedded owner name: [xx] 00 01 <name_len> <name>
        // Used by summons like Divine Aura where the owner's name is in the spawn packet.
        if offset + 3 < packet.len()
            && packet[offset + 1] == 0x00
            && packet[offset + 2] == 0x01
        {
            let name_len = packet[offset + 3] as usize;
            if (2..=36).contains(&name_len) && offset + 4 + name_len <= packet.len() {
                if let Ok(name) = std::str::from_utf8(&packet[offset + 4..offset + 4 + name_len]) {
                    if let Some(sanitized) = sanitize_nickname(name) {
                        if sanitized.len() >= 2 {
                            if let Some(owner_id) = self.data_storage.find_id_by_nickname(&sanitized) {
                                if owner_id != real_actor_id {
                                    self.extract_and_register_mob_type(packet, offset, real_actor_id);
                                    self.data_storage.register_confirmed_summon_by_id(real_actor_id, owner_id);
                                    tracing::trace!(
                                        "Summon confirmed (00 01 name): {} owned by {} ({})",
                                        real_actor_id, owner_id, sanitized
                                    );
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }

        self.extract_and_register_mob_type(packet, offset, real_actor_id);
        false
    }

    fn extract_summon_owner_from_spawn(&self, packet: &[u8], start_offset: usize) -> i32 {
        let anchor: [u8; 8] = [0x80, 0x75, 0xD5, 0x2A, 0xBB, 0x03, 0x00, 0x00];
        let max_search = std::cmp::min(packet.len().saturating_sub(anchor.len()), start_offset + 120);
        for i in start_offset..=max_search {
            if packet[i..].starts_with(&anchor) {
                let owner_info = read_varint(packet, i + anchor.len());
                if owner_info.length > 0 && (100..=99_999).contains(&owner_info.value) {
                    return owner_info.value;
                }
            }
        }
        -1
    }

    fn extract_and_register_mob_type(&self, packet: &[u8], start_offset: usize, real_actor_id: i32) {
        let mut scan_offset = start_offset;
        let max_scan = std::cmp::min(packet.len().saturating_sub(2), start_offset + 60);

        while scan_offset < max_scan {
            if packet[scan_offset] == 0x00
                && (packet[scan_offset + 1] == 0x40 || packet[scan_offset + 1] == 0x00)
                && packet[scan_offset + 2] == 0x02
            {
                if scan_offset >= start_offset + 3 {
                    let b1 = packet[scan_offset - 3] as i32;
                    let b2 = packet[scan_offset - 2] as i32;
                    let b3 = packet[scan_offset - 1] as i32;
                    let mob_type_id = b1 | (b2 << 8) | (b3 << 16);
                    self.data_storage.append_mob(real_actor_id, mob_type_id);

                    // Register boss entities from NPC DB
                    if self.npc_lookup.is_boss(mob_type_id) {
                        self.data_storage.register_boss(real_actor_id);
                    }

                    // Try to extract HP
                    let mut hp_scan = scan_offset + 3;
                    let hp_end = std::cmp::min(packet.len().saturating_sub(2), hp_scan + 64);
                    while hp_scan < hp_end {
                        if packet[hp_scan] == 0x01 {
                            let current_hp = read_varint(packet, hp_scan + 1);
                            if current_hp.length > 0 && current_hp.value > 0 {
                                let max_hp = read_varint(packet, hp_scan + 1 + current_hp.length as usize);
                                if max_hp.length > 0 && max_hp.value >= current_hp.value {
                                    self.data_storage.append_mob_hp(real_actor_id, max_hp.value);
                                    break;
                                }
                            }
                        }
                        hp_scan += 1;
                    }
                }
                break;
            }
            scan_offset += 1;
        }
    }

    // ===== ACTOR NAME BINDING =====

    fn parse_actor_name_binding_rules(&self, packet: &[u8]) -> bool {
        let mut i = 0;
        let mut last_anchor: Option<(i32, usize, usize)> = None; // (actor_id, start, end)
        let mut named_actors = HashSet::new();

        while i < packet.len() {
            if packet[i] == 0x36 {
                // Skip spawn opcodes (40/41 36 mob, 44/45 36 player) — the
                // 0x36 family shifted +1 in June 2026.
                if i > 0 && matches!(packet[i - 1], 0x40 | 0x41 | 0x44 | 0x45) {
                    i += 1;
                    continue;
                }
                if i + 1 >= packet.len() {
                    i += 1;
                    continue;
                }
                let actor_info = read_varint(packet, i + 1);
                last_anchor = if actor_info.length > 0 && actor_info.value >= 100 {
                    Some((actor_info.value, i, i + 1 + actor_info.length as usize))
                } else {
                    None
                };
                i += 1;
                continue;
            }

            if packet[i] == 0x07 {
                if let Some(name_info) = self.read_utf8_name(packet, i) {
                    if let Some((actor_id, _, end_idx)) = last_anchor {
                        if !named_actors.contains(&actor_id) {
                            let distance = i as isize - end_idx as isize;
                            if (0..=64).contains(&distance) {
                                if self.register_utf8_nickname(packet, actor_id, name_info.0, name_info.1) {
                                    named_actors.insert(actor_id);
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
            i += 1;
        }
        false
    }

    fn read_utf8_name(&self, packet: &[u8], anchor_index: usize) -> Option<(usize, usize)> {
        let length_index = anchor_index + 1;
        if length_index >= packet.len() {
            return None;
        }
        let name_length = packet[length_index] as usize;
        if !(1..=36).contains(&name_length) {
            return None;
        }
        let name_start = length_index + 1;
        let name_end = name_start + name_length;
        if name_end > packet.len() {
            return None;
        }
        let name_bytes = &packet[name_start..name_end];
        let name = std::str::from_utf8(name_bytes).ok()?;
        let sanitized = sanitize_nickname(name)?;
        if sanitized.is_empty() {
            return None;
        }
        Some((name_start, name_length))
    }

    fn register_utf8_nickname(&self, packet: &[u8], actor_id: i32, name_start: usize, name_length: usize) -> bool {
        if self.data_storage.has_nickname(actor_id) {
            return false;
        }
        if self.data_storage.is_summon(actor_id) {
            return false;
        }
        if name_length == 0 || name_length > 36 {
            return false;
        }
        let name_end = name_start + name_length;
        if name_end > packet.len() {
            return false;
        }
        let name_bytes = &packet[name_start..name_end];
        let name = match std::str::from_utf8(name_bytes) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let sanitized = match sanitize_nickname(name) {
            Some(s) => s,
            None => return false,
        };
        self.data_storage.append_nickname(actor_id, &sanitized);
        true
    }

    // ===== LOOT ATTRIBUTION ACTOR NAME =====

    fn parse_loot_attribution_actor_name(&self, packet: &[u8]) -> bool {
        let mut candidates: std::collections::HashMap<i32, (String, Vec<u8>)> = std::collections::HashMap::new();
        let mut idx = 0;

        while idx + 2 < packet.len() {
            let marker = packet[idx] as u32;
            let marker_next = packet[idx + 1] as u32;
            let is_marker = (0xF0..=0xFF).contains(&marker) && (marker_next == 0x03 || marker_next == 0xA3);

            if is_marker {
                // Scan backward for actor ID
                let mut actor_info: Option<VarIntResult> = None;
                let min_offset = idx.saturating_sub(8);
                for actor_offset in min_offset..idx {
                    if !can_read_varint(packet, actor_offset) {
                        continue;
                    }
                    let candidate = read_varint(packet, actor_offset);
                    if candidate.length <= 0 || actor_offset + candidate.length as usize != idx {
                        continue;
                    }
                    if !(100..=99999).contains(&candidate.value) {
                        continue;
                    }
                    actor_info = Some(candidate);
                    break;
                }

                let actor_info = match actor_info {
                    Some(a) => a,
                    None => { idx += 1; continue; }
                };

                let length_idx = idx + 2;
                if length_idx >= packet.len() {
                    idx += 1;
                    continue;
                }
                let name_length = packet[length_idx] as usize;
                if !(1..=36).contains(&name_length) {
                    idx += 1;
                    continue;
                }
                let name_start = length_idx + 1;
                let name_end = name_start + name_length;
                if name_end > packet.len() {
                    idx += 1;
                    continue;
                }
                let name_bytes = &packet[name_start..name_end];
                let name = match std::str::from_utf8(name_bytes) {
                    Ok(s) => s,
                    Err(_) => { idx = name_end; continue; }
                };
                let sanitized = match sanitize_nickname(name) {
                    Some(s) => s,
                    None => { idx = name_end; continue; }
                };

                let actor_id = actor_info.value;
                let existing = candidates.get(&actor_id);
                if existing.is_none() || name_bytes.len() > existing.unwrap().1.len() {
                    candidates.insert(actor_id, (sanitized, name_bytes.to_vec()));
                }
                idx = name_end;
                continue;
            }
            idx += 1;
        }

        if candidates.is_empty() {
            return false;
        }

        let mut found_any = false;
        let allow_prepopulate = candidates.len() > 1;

        for (actor_id, (name, _)) in &candidates {
            let existing = self.data_storage.get_nickname(*actor_id);
            let has_cjk = name.chars().any(|ch| {
                matches!(unicode_script(ch), UnicodeScript::Han | UnicodeScript::Hangul)
            });

            if !allow_prepopulate && !self.data_storage.actor_appears_in_combat(*actor_id) && !has_cjk {
                if existing.is_none() {
                    self.data_storage.cache_pending_nickname(*actor_id, name);
                }
                continue;
            }

            if existing.is_some() {
                continue;
            }

            self.data_storage.append_nickname(*actor_id, name);
            found_any = true;
        }

        found_any
    }

    // ===== NICKNAME PARSING =====

    fn parsing_nickname(&self, packet: &[u8]) -> bool {
        let mut parsed_any = false;
        let mut search_offset = 0;

        while search_offset + 2 < packet.len() {
            // PATTERN A: E2/E0 07 anchor
            if (packet[search_offset] == 0xE2 || packet[search_offset] == 0xE0)
                && packet[search_offset + 1] == 0x07
            {
                let len_idx = search_offset + 2;
                if len_idx < packet.len() {
                    let name_len = packet[len_idx] as usize;
                    if (2..=36).contains(&name_len) && len_idx + 1 + name_len <= packet.len() {
                        let np = &packet[len_idx + 1..len_idx + 1 + name_len];
                        if let Ok(possible_name) = std::str::from_utf8(np) {
                            if !possible_name.is_empty() && possible_name.chars().next().unwrap().is_alphanumeric() {
                                if let Some(sanitized) = sanitize_nickname(possible_name) {
                                    if sanitized.len() >= 2 {
                                        for v_len in 1..=3usize {
                                            if search_offset < v_len {
                                                continue;
                                            }
                                            let v_start = search_offset - v_len;
                                            if can_read_varint(packet, v_start) {
                                                let v = read_varint(packet, v_start);
                                                if v.length == v_len as i32 && (100..=9_999_999).contains(&v.value) {
                                                    self.data_storage.append_nickname(v.value, &sanitized);
                                                    parsed_any = true;
                                                    search_offset = len_idx + 1 + name_len;
                                                    // Skip guild name
                                                    search_offset = self.skip_guild_name(packet, search_offset);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // PATTERN B: 0F 1D 37 block anchor
            if search_offset + 2 < packet.len()
                && packet[search_offset] == 0x0F
                && packet[search_offset + 1] == 0x1D
                && packet[search_offset + 2] == 0x37
            {
                let id_offset = search_offset + 3;
                if can_read_varint(packet, id_offset) {
                    let block_actor = read_varint(packet, id_offset);
                    if (100..=9_999_999).contains(&block_actor.value) {
                        let mut block_scan = id_offset + block_actor.length as usize;
                        let block_end = std::cmp::min(packet.len(), block_scan + 500);

                        while block_scan + 3 < block_end {
                            // Stop at terminator. The leading byte changed
                            // 0x06 -> 0x0E in the June 2026 update; accept both.
                            if (packet[block_scan] == 0x06 || packet[block_scan] == 0x0E) && packet[block_scan + 1] == 0x00 && packet[block_scan + 2] == 0x36 {
                                break;
                            }
                            // Name must be preceded by 00 00
                            if packet[block_scan] == 0x00 && packet[block_scan + 1] == 0x00 {
                                let len_idx = block_scan + 2;
                                if len_idx < packet.len() {
                                    let name_len = packet[len_idx] as usize;
                                    if (2..=36).contains(&name_len) && len_idx + 1 + name_len <= packet.len() {
                                        let np = &packet[len_idx + 1..len_idx + 1 + name_len];
                                        if let Ok(possible_name) = std::str::from_utf8(np) {
                                            if !possible_name.is_empty() && possible_name.chars().next().unwrap().is_alphanumeric() {
                                                if let Some(sanitized) = sanitize_nickname(possible_name) {
                                                    if sanitized.len() >= 2 {
                                                        self.data_storage.append_nickname(block_actor.value, &sanitized);
                                                        parsed_any = true;
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            block_scan += 1;
                        }
                    }
                }
            }

            // PATTERN D: Terminator anchor (04/00 4C)
            if search_offset + 1 < packet.len() {
                let b0 = packet[search_offset] as u32;
                let b1 = packet[search_offset + 1] as u32;

                if (b0 == 0x04 || b0 == 0x00) && b1 == 0x4C {
                    let id_idx = search_offset + 2;
                    if can_read_varint(packet, id_idx) {
                        let player_info = read_varint(packet, id_idx);
                        if player_info.length > 0 && (100..=9_999_999).contains(&player_info.value) {
                            let stop_at = std::cmp::min(packet.len().saturating_sub(2), id_idx + 128);
                            let mut scan_idx = id_idx + player_info.length as usize;

                            while scan_idx < stop_at {
                                // Terminator: leading byte changed 0x06 -> 0x0E
                                // in the June 2026 update; accept both.
                                if (packet[scan_idx] == 0x06 || packet[scan_idx] == 0x0E)
                                    && packet[scan_idx + 1] == 0x00
                                    && packet[scan_idx + 2] == 0x36
                                {
                                    // Look backwards for name
                                    for test_len in 2..=36usize {
                                        if scan_idx < test_len + 1 + id_idx {
                                            continue;
                                        }
                                        let len_byte_idx = scan_idx - test_len - 1;
                                        if len_byte_idx <= id_idx {
                                            continue;
                                        }
                                        let possible_len = packet[len_byte_idx] as usize;
                                        if possible_len == test_len {
                                            let np = &packet[len_byte_idx + 1..len_byte_idx + 1 + test_len];
                                            if let Ok(possible_name) = std::str::from_utf8(np) {
                                                if !possible_name.is_empty() && possible_name.chars().next().unwrap().is_alphanumeric() {
                                                    if let Some(sanitized) = sanitize_nickname(possible_name) {
                                                        if sanitized.len() >= 2 {
                                                            // Try to find earlier name (player name vs guild)
                                                            let before_name = self.find_name_before(
                                                                packet, len_byte_idx,
                                                                id_idx + player_info.length as usize,
                                                            );
                                                            let final_name = before_name.unwrap_or(sanitized);
                                                            self.data_storage.append_nickname(player_info.value, &final_name);
                                                            parsed_any = true;
                                                            search_offset = scan_idx;
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    break;
                                }
                                scan_idx += 1;
                            }
                        }
                    }
                }
            }

            search_offset += 1;
        }
        parsed_any
    }

    fn find_name_before(&self, packet: &[u8], before_idx: usize, min_idx: usize) -> Option<String> {
        for test_len in 2..=36usize {
            for gap in 0..=1usize {
                if before_idx < gap + test_len + 1 {
                    continue;
                }
                let name_len_idx = before_idx - gap - test_len - 1;
                if name_len_idx < min_idx {
                    continue;
                }
                let possible_len = packet[name_len_idx] as usize;
                if possible_len != test_len {
                    continue;
                }
                let np = &packet[name_len_idx + 1..name_len_idx + 1 + test_len];
                if let Ok(possible_name) = std::str::from_utf8(np) {
                    if possible_name.is_empty() || !possible_name.chars().next().unwrap().is_alphanumeric() {
                        continue;
                    }
                    if let Some(sanitized) = sanitize_nickname(possible_name) {
                        if sanitized.len() >= 2 {
                            return Some(sanitized);
                        }
                    }
                }
            }
        }
        None
    }

    fn skip_guild_name(&self, packet: &[u8], start_index: usize) -> usize {
        if start_index >= packet.len() {
            return start_index;
        }
        let mut offset = start_index;
        if packet[offset] == 0x00 {
            offset += 1;
            if offset >= packet.len() {
                return offset;
            }
        }
        let length = packet[offset] as usize;
        if !(1..=36).contains(&length) {
            return offset;
        }
        let name_start = offset + 1;
        let name_end = name_start + length;
        if name_end > packet.len() {
            return offset;
        }
        if std::str::from_utf8(&packet[name_start..name_end]).is_err() {
            return offset;
        }
        name_end
    }

    // ===== EMBEDDED DAMAGE PACKET =====

    fn try_parse_embedded_damage_packet(&mut self, packet: &[u8]) -> bool {
        if packet.len() < 6 {
            return false;
        }
        let mut parsed_any = false;
        let mut search_offset = 0;

        while search_offset + 1 < packet.len() {
            if packet[search_offset] != 0x04 || packet[search_offset + 1] != 0x38 {
                search_offset += 1;
                continue;
            }

            let _remaining_size = packet.len() - search_offset;
            let raw_key = to_hex_range(packet, search_offset, std::cmp::min(search_offset + 64, packet.len()));
            if self.seen_embedded_hexes.contains(&raw_key) {
                search_offset += 1;
                continue;
            }

            let mut headless = vec![0xFF, 0x01];
            headless.extend_from_slice(&packet[search_offset..]);

            if self.parsing_damage_inner(&headless, false, true) {
                self.seen_embedded_hexes.insert(raw_key);
                parsed_any = true;
                search_offset += 2;
            } else {
                search_offset += 1;
            }
        }
        parsed_any
    }

    // ===== DAMAGE PARSING =====

    fn parsing_damage(&mut self, packet: &[u8], allow_embedded_scan: bool, require_trusted: bool) -> bool {
        self.parsing_damage_inner(packet, allow_embedded_scan, require_trusted)
    }

    fn parsing_damage_inner(&mut self, packet: &[u8], allow_embedded_scan: bool, require_trusted: bool) -> bool {
        let length_info = read_varint(packet, 0);
        if length_info.length < 0 {
            return false;
        }
        let mut offset = length_info.length as usize;

        if offset >= packet.len() || offset + 1 >= packet.len() {
            return false;
        }

        // STRICT GATEKEEPER: 04 38
        if packet[offset] != 0x04 || packet[offset + 1] != 0x38 {
            if allow_embedded_scan {
                return self.try_parse_embedded_damage_packet(packet);
            }
            return false;
        }
        offset += 2;

        let mut parsed_any = false;
        let mask = 0x0F;

        while offset < packet.len() {
            let _checkpoint = offset;

            // Chained hit marker
            let mut is_chained = false;
            if offset + 1 < packet.len() && packet[offset] == 0x01 && packet[offset + 1] == 0x00 {
                offset += 2;
                is_chained = true;
            }

            if parsed_any && !is_chained {
                break;
            }

            // Target
            let target_value = match try_read_varint(packet, &mut offset) {
                Some(v) if v >= 100 => v,
                _ => { break; }
            };

            // Switch value
            let switch_value = match try_read_varint(packet, &mut offset) {
                Some(v) => v,
                None => break,
            };
            let and_result = switch_value & mask;

            if !(4..=7).contains(&and_result) {
                break;
            }

            // Unused flag
            if try_read_varint(packet, &mut offset).is_none() { break; }

            // Actor
            let actor_value = match try_read_varint(packet, &mut offset) {
                Some(v) if v >= 100 => v,
                _ => { break; }
            };

            // Exact 4-byte skill ID
            if offset + 4 > packet.len() {
                break;
            }
            let mut exact_skill_code = i64::from(packet[offset] as u32)
                | (i64::from(packet[offset + 1] as u32) << 8)
                | (i64::from(packet[offset + 2] as u32) << 16)
                | (i64::from(packet[offset + 3] as u32) << 24);
            offset += 4;

            // Theostone raw item IDs
            if (3_000_000..=3_099_999).contains(&exact_skill_code) {
                exact_skill_code = exact_skill_code * 10 + 1;
            }

            if !(1..=299_999_999).contains(&exact_skill_code) {
                break;
            }

            // Skip 7-digit NPC skills
            if (1_000_000..=9_999_999).contains(&exact_skill_code) {
                break;
            }

            // Skip 1-byte UID field
            if offset < packet.len() {
                offset += 1;
            }

            let dummy_type = match try_read_varint(packet, &mut offset) {
                Some(v) => v,
                None => break,
            };
            let damage_type = dummy_type as u8;

            let temp_v: usize = match and_result {
                5 => 12,
                6 => 10,
                7 => 14,
                _ => 8,
            };

            // Special damage flags
            let mut specials = Vec::new();
            if [5, 6, 7].contains(&and_result) && offset < packet.len() {
                let special_byte = packet[offset] as u32;
                if special_byte & 0x01 != 0 { specials.push(SpecialDamage::Back); }
                if special_byte & 0x04 != 0 { specials.push(SpecialDamage::Parry); }
                if special_byte & 0x08 != 0 { specials.push(SpecialDamage::Perfect); }
                if special_byte & 0x10 != 0 { specials.push(SpecialDamage::Double); }
                if special_byte & 0x40 != 0 { specials.push(SpecialDamage::Smite); }
                if special_byte & 0x80 != 0 { specials.push(SpecialDamage::PowerShard); }
            }
            if damage_type == 3 {
                specials.push(SpecialDamage::Critical);
            }

            offset += temp_v;
            if offset >= packet.len() {
                break;
            }

            // Struct data extraction
            let first_value = match try_read_varint(packet, &mut offset) {
                Some(v) => v,
                None => break,
            };
            let after_first_offset = offset;
            let second_value = match try_read_varint(packet, &mut offset) {
                Some(v) => v,
                None => break,
            };

            let first_is_damage = should_treat_first_value_as_damage(first_value, second_value, and_result, damage_type as i32);

            let mut final_damage = if first_is_damage {
                offset = after_first_offset;
                first_value
            } else {
                second_value
            };

            // Multi-hit extra field
            if (switch_value & 0x30) == 0x30 && offset < packet.len() {
                try_read_varint(packet, &mut offset);
            }

            let mut hit_count = 0;
            let pre_hit_offset = offset;

            if offset < packet.len() {
                let is_marker_next = offset + 1 < packet.len()
                    && packet[offset + 1] == 0x00
                    && (1..=7).contains(&(packet[offset] as i32));

                if !is_marker_next {
                    if let Some(peek_val) = try_read_varint(packet, &mut offset) {
                        if (0..=25).contains(&peek_val) {
                            hit_count = peek_val;
                        } else {
                            let is_marker_after = offset + 1 < packet.len()
                                && packet[offset + 1] == 0x00
                                && (1..=7).contains(&(packet[offset] as i32));
                            if !is_marker_after {
                                if let Some(actual) = try_read_varint(packet, &mut offset) {
                                    if (0..=25).contains(&actual) {
                                        hit_count = actual;
                                    } else {
                                        offset = pre_hit_offset;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if final_damage < 0 || final_damage > 99_999_999 {
                break;
            }

            // Extract multi-hits
            let mut multi_hit_count = 0;
            let mut multi_hit_damage = 0;
            let mut first_multi_hit_value: Option<i32> = None;
            let mut all_multi_hits_match = true;

            if hit_count > 0 && offset < packet.len() {
                let safe_max = std::cmp::min(hit_count, 25);
                let multi_hit_cap = std::cmp::max(final_damage, 500_000);
                let mut hits_read = 0;

                while hits_read < safe_max && offset < packet.len() {
                    let is_marker_next = offset + 1 < packet.len()
                        && packet[offset + 1] == 0x00
                        && (1..=7).contains(&(packet[offset] as i32));
                    let is_next_packet = offset + 1 < packet.len()
                        && packet[offset] == 0x04
                        && packet[offset + 1] == 0x38;

                    if is_marker_next || is_next_packet {
                        break;
                    }

                    let hit_value = match try_read_varint(packet, &mut offset) {
                        Some(v) => v,
                        None => break,
                    };

                    if hit_value > multi_hit_cap || hit_value < 50 {
                        multi_hit_damage = 0;
                        first_multi_hit_value = None;
                        all_multi_hits_match = true;
                        break;
                    }

                    match first_multi_hit_value {
                        None => first_multi_hit_value = Some(hit_value),
                        Some(fv) if fv != hit_value => all_multi_hits_match = false,
                        _ => {}
                    }

                    multi_hit_damage += hit_value;
                    hits_read += 1;
                }
                multi_hit_count = hits_read;
            }

            if switch_value == 54 && hit_count > multi_hit_count && multi_hit_count == 1 {
                if let Some(fv) = first_multi_hit_value {
                    if all_multi_hits_match {
                        multi_hit_count = hit_count;
                        multi_hit_damage = fv * hit_count;
                    }
                }
            }

            if should_use_repeated_hit_damage(switch_value, second_value, multi_hit_count, first_multi_hit_value, all_multi_hits_match) {
                final_damage = first_multi_hit_value.unwrap();
            }

            if multi_hit_count > 0 && multi_hit_damage > 0 && final_damage > multi_hit_damage {
                final_damage -= multi_hit_damage;
            }

            // Compact skill context handling
            let pending = self.pending_compact_skill_context.clone();
            let aggregated_compact = pending.as_ref().is_some_and(|ctx| {
                exact_skill_code as i32 == 99_745_942
                    && actor_value == ctx.actor_id
                    && hit_count > 1
                    && multi_hit_damage > 0
                    && second_value > multi_hit_damage
            });

            let raw_for_spec = if aggregated_compact {
                pending.as_ref().unwrap().skill_raw
            } else {
                exact_skill_code as i32
            };
            let spec_flags = decode_spec_flags(raw_for_spec);
            let resolved_skill_code = if aggregated_compact {
                pending.as_ref().unwrap().skill_raw
            } else {
                self.normalize_skill_id(exact_skill_code as i32)
            };

            if aggregated_compact {
                final_damage = second_value - multi_hit_damage;
                self.pending_compact_skill_context = None;
            }

            // Heal/life-steal suffix: [0x03, 0x00] marker + HealAmount VarInt
            let mut heal_amount = 0;
            if offset + 1 < packet.len()
                && packet[offset] == 0x03
                && packet[offset + 1] == 0x00
            {
                offset += 2;
                if let Some(heal_val) = try_read_varint(packet, &mut offset) {
                    if heal_val > 0 && heal_val < 10_000_000 {
                        heal_amount = heal_val;
                    }
                }
            }

            if require_trusted && !self.is_trusted_recovered_damage_shape(actor_value, target_value, dummy_type as u8, final_damage, resolved_skill_code) {
                break;
            }

            if actor_value != target_value {
                let mut pdp = ParsedDamagePacket::new();
                if let Some(ts) = self.override_timestamp {
                    pdp.set_timestamp(ts);
                }
                pdp.set_target_id(target_value);
                pdp.set_actor_id(actor_value);
                pdp.set_skill_code(resolved_skill_code);
                pdp.set_spec_flags(spec_flags);
                pdp.set_type(dummy_type);
                pdp.set_specials(specials);
                pdp.set_multi_hit_count(multi_hit_count);
                pdp.set_multi_hit_damage(multi_hit_damage);
                pdp.set_heal_amount(heal_amount);
                pdp.set_damage(final_damage);
                pdp.set_hex_payload(to_hex(packet));

                self.data_storage.append_damage(pdp);
            }

            parsed_any = true;
        }

        parsed_any
    }

    fn extract_pending_compact_skill_context(&self, packet: &[u8]) -> Option<PendingCompactSkillContext> {
        let length_info = read_varint(packet, 0);
        if length_info.length <= 0 || length_info.length as usize >= packet.len() {
            return None;
        }
        let body = &packet[length_info.length as usize..];

        // Find marker: 08 3B/3D 38 00 00
        let mut marker_index: Option<usize> = None;
        for idx in 0..body.len().saturating_sub(4) {
            if body[idx] == 0x08
                && (body[idx + 1] == 0x3B || body[idx + 1] == 0x3D)
                && body[idx + 2] == 0x38
                && body[idx + 3] == 0x00
                && body[idx + 4] == 0x00
            {
                marker_index = Some(idx);
                break;
            }
        }
        let marker_index = marker_index?;

        // Find compact opcode 38
        let mut compact_opcode: Option<usize> = None;
        for idx in (marker_index + 5)..body.len() {
            if body[idx] == 0x38 {
                compact_opcode = Some(idx);
                break;
            }
        }
        let compact_opcode = compact_opcode?;
        if compact_opcode + 2 >= body.len() {
            return None;
        }

        let actor_info = read_varint(body, compact_opcode + 1);
        if actor_info.length <= 0 || actor_info.value < 100 {
            return None;
        }

        let uid_offset = compact_opcode + 1 + actor_info.length as usize;
        if uid_offset >= body.len() {
            return None;
        }
        let skill_offset = uid_offset + 1;
        if skill_offset + 3 > body.len() {
            return None;
        }

        let mut candidates = Vec::new();
        if skill_offset + 4 <= body.len() {
            let full_skill = parse_u32_le(body, skill_offset) as i32;
            candidates.push(full_skill);
        }
        let compact_skill = (body[skill_offset] as i32)
            | ((body[skill_offset + 1] as i32) << 8)
            | ((body[skill_offset + 2] as i32) << 16);
        candidates.push(compact_skill);

        for candidate in candidates {
            if self.is_known_skill_code(candidate) {
                return Some(PendingCompactSkillContext {
                    actor_id: actor_info.value,
                    skill_raw: self.normalize_skill_id(candidate),
                });
            }
        }
        None
    }

    // ===== HELPERS =====

    fn normalize_skill_id(&self, raw: i32) -> i32 {
        if (30_000_000..=30_999_999).contains(&raw) {
            return raw;
        }
        let base = raw - (raw % 10000);
        let base_name = self.skill_lookup.get_skill_name(base);
        if base_name.is_empty() {
            return raw;
        }
        let raw_name = self.skill_lookup.get_skill_name(raw);
        if raw_name.is_empty() {
            return base;
        }
        if raw_name != base_name {
            return raw;
        }
        base
    }

    fn is_known_skill_code(&self, skill_code: i32) -> bool {
        if !is_valid_skill_code(skill_code) {
            return false;
        }
        let normalized = self.normalize_skill_id(skill_code);
        if !is_valid_skill_code(normalized) {
            return false;
        }
        if (30_000_000..=30_999_999).contains(&normalized) {
            return true;
        }
        !self.skill_lookup.get_skill_name(normalized).is_empty()
            || !self.skill_lookup.get_skill_name(skill_code).is_empty()
    }

    fn is_trusted_recovered_damage_shape(&self, actor_id: i32, target_id: i32, damage_type: u8, damage: i32, skill_code: i32) -> bool {
        if actor_id == target_id || !(1..=3).contains(&(damage_type as i32)) || damage <= 0 {
            return false;
        }
        self.is_known_skill_code(skill_code)
    }
}

// ===== FREE FUNCTIONS =====

pub fn read_varint(bytes: &[u8], offset: usize) -> VarIntResult {
    let mut value: i32 = 0;
    let mut shift = 0;
    let mut count = 0;

    loop {
        if offset + count >= bytes.len() {
            return VarIntResult::invalid();
        }

        let byte_val = bytes[offset + count] as u32;
        count += 1;

        value |= ((byte_val & 0x7F) as i32) << shift;

        if byte_val & 0x80 == 0 {
            return VarIntResult { value, length: count as i32 };
        }

        shift += 7;
        if shift >= 32 {
            return VarIntResult::invalid();
        }
    }
}

fn try_read_varint(bytes: &[u8], offset: &mut usize) -> Option<i32> {
    let result = read_varint(bytes, *offset);
    if result.length <= 0 {
        return None;
    }
    *offset += result.length as usize;
    if result.value < 0 { None } else { Some(result.value) }
}

fn can_read_varint(bytes: &[u8], offset: usize) -> bool {
    if offset >= bytes.len() {
        return false;
    }
    let mut idx = offset;
    let mut count = 0;
    while idx < bytes.len() && count < 5 {
        let byte_val = bytes[idx] as u32;
        if byte_val & 0x80 == 0 {
            return true;
        }
        idx += 1;
        count += 1;
    }
    false
}

fn parse_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
    ])
}

fn is_valid_skill_code(skill_code: i32) -> bool {
    (1..=299_999_999).contains(&skill_code)
}

fn decode_spec_flags(raw: i32) -> [bool; 5] {
    let mut result = [false; 5];
    if (30_000_000..=30_999_999).contains(&raw) {
        return result;
    }
    let mut suffix = (raw % 10000) / 10;
    if suffix <= 0 {
        return result;
    }
    while suffix > 0 {
        let slot = suffix % 10;
        if slot < 1 || slot > 5 {
            return [false; 5];
        }
        result[(slot - 1) as usize] = true;
        suffix /= 10;
    }
    result
}

fn should_treat_first_value_as_damage(first_value: i32, second_value: i32, and_result: i32, damage_type: i32) -> bool {
    if !(1_000..=99_999_999).contains(&first_value) { return false; }
    if !(0..=25).contains(&second_value) { return false; }
    if first_value > 5_000_000 { return false; }
    and_result == 6 && damage_type == 3
}

fn should_use_repeated_hit_damage(switch_value: i32, encoded_damage: i32, multi_hit_count: i32, first_multi_hit_value: Option<i32>, all_match: bool) -> bool {
    let repeated = match first_multi_hit_value {
        Some(v) => v,
        None => return false,
    };
    if switch_value != 54 { return false; }
    if multi_hit_count <= 0 || !all_match { return false; }
    let main_component = encoded_damage - multi_hit_count * repeated;
    if main_component > repeated { return false; }
    encoded_damage / 10 == repeated
}

fn find_pattern(data: &[u8], start: usize, pattern: &[u8]) -> Option<usize> {
    if data.len() < pattern.len() + start {
        return None;
    }
    for i in start..=data.len() - pattern.len() {
        if data[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    None
}

fn to_hex_range(bytes: &[u8], start: usize, end: usize) -> String {
    let s = start.min(bytes.len());
    let e = end.min(bytes.len());
    bytes[s..e].iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ")
}

fn to_hex(bytes: &[u8]) -> String {
    to_hex_range(bytes, 0, bytes.len())
}

fn sanitize_nickname(nickname: &str) -> Option<String> {
    let trimmed = nickname.split('\0').next().unwrap_or("").trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut result = String::new();
    let mut only_numbers = true;
    let mut has_cjk = false;

    for ch in trimmed.chars() {
        if !ch.is_alphanumeric() {
            if result.is_empty() {
                return None;
            }
            break;
        }
        if ch == '\u{FFFD}' {
            if result.is_empty() {
                return None;
            }
            break;
        }
        if ch.is_control() {
            if result.is_empty() {
                return None;
            }
            break;
        }
        result.push(ch);
        if ch.is_alphabetic() {
            only_numbers = false;
        }
        if is_cjk_char(ch) {
            has_cjk = true;
        }
    }

    if result.is_empty() || only_numbers {
        return None;
    }

    if result.chars().count() < 2 && !has_cjk {
        return None;
    }

    Some(result)
}

fn is_cjk_char(ch: char) -> bool {
    let cp = ch as u32;
    // CJK Unified Ideographs
    (0x4E00..=0x9FFF).contains(&cp)
    // Hangul Syllables
    || (0xAC00..=0xD7AF).contains(&cp)
    // CJK Extension A/B
    || (0x3400..=0x4DBF).contains(&cp)
    || (0x20000..=0x2A6DF).contains(&cp)
    // Hangul Jamo
    || (0x1100..=0x11FF).contains(&cp)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum UnicodeScript {
    Han,
    Hangul,
    Other,
}

fn unicode_script(ch: char) -> UnicodeScript {
    let cp = ch as u32;
    if (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp) || (0x20000..=0x2A6DF).contains(&cp) {
        UnicodeScript::Han
    } else if (0xAC00..=0xD7AF).contains(&cp) || (0x1100..=0x11FF).contains(&cp) {
        UnicodeScript::Hangul
    } else {
        UnicodeScript::Other
    }
}
