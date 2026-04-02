use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobClass {
    Gladiator,
    Templar,
    Ranger,
    Assassin,
    Sorcerer,
    Cleric,
    Elementalist,
    Chanter,
}

impl JobClass {
    pub fn class_name(&self) -> &'static str {
        match self {
            JobClass::Gladiator => "검성",
            JobClass::Templar => "수호성",
            JobClass::Ranger => "궁성",
            JobClass::Assassin => "살성",
            JobClass::Sorcerer => "마도성",
            JobClass::Cleric => "치유성",
            JobClass::Elementalist => "정령성",
            JobClass::Chanter => "호법성",
        }
    }

    pub fn class_prefix(&self) -> i32 {
        match self {
            JobClass::Gladiator => 11,
            JobClass::Templar => 12,
            JobClass::Assassin => 13,
            JobClass::Ranger => 14,
            JobClass::Sorcerer => 15,
            JobClass::Elementalist => 16,
            JobClass::Cleric => 17,
            JobClass::Chanter => 18,
        }
    }

    fn from_prefix(prefix: i32) -> Option<JobClass> {
        match prefix {
            11 => Some(JobClass::Gladiator),
            12 => Some(JobClass::Templar),
            13 => Some(JobClass::Assassin),
            14 => Some(JobClass::Ranger),
            15 => Some(JobClass::Sorcerer),
            16 => Some(JobClass::Elementalist),
            17 => Some(JobClass::Cleric),
            18 => Some(JobClass::Chanter),
            _ => None,
        }
    }

    /// Strict job detection from skill code.
    pub fn convert_from_skill(skill_code: i32) -> Option<JobClass> {
        // PC Elementalist specific 6-digit skills
        if (100510..=103500).contains(&skill_code)
            || (109300..=109362).contains(&skill_code)
        {
            return Some(JobClass::Elementalist);
        }

        // 8-digit standard player skills
        if (10_000_000..=19_999_999).contains(&skill_code) {
            let prefix = skill_code / 1_000_000;
            let sub = (skill_code / 10000) % 100;

            // Exclude generic mob skills (sub 00) for ALL classes
            if sub == 0 {
                if prefix == 16 {
                    let command_range = (skill_code / 100) % 100;
                    if (11..=13).contains(&command_range) {
                        return Some(JobClass::Elementalist);
                    }
                }
                return None;
            }

            // Elementalist strict whitelist
            if prefix == 16 {
                let is_pc_range = matches!(sub,
                    1..=8 | 14 | 15 | 17 | 19 | 21..=26 |
                    30 | 31 | 32 | 34 | 35 | 36 | 37 |
                    70..=76 | 80
                );
                return if is_pc_range { Some(JobClass::Elementalist) } else { None };
            }

            return Self::from_prefix(prefix);
        }

        None
    }

    /// Loose prefix-only job detection for orphan summon inference.
    pub fn convert_from_skill_loose(skill_code: i32) -> Option<JobClass> {
        if (100510..=103500).contains(&skill_code)
            || (109300..=109362).contains(&skill_code)
        {
            return Some(JobClass::Elementalist);
        }

        if (10_000_000..=19_999_999).contains(&skill_code) {
            let prefix = skill_code / 1_000_000;
            let sub = (skill_code / 10000) % 100;
            if sub == 0 {
                if prefix == 16 {
                    let command_range = (skill_code / 100) % 100;
                    if (11..=13).contains(&command_range) {
                        return Some(JobClass::Elementalist);
                    }
                }
                return None;
            }
            return Self::from_prefix(prefix);
        }

        None
    }
}
