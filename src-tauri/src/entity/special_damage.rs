use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SpecialDamage {
    Back,
    Critical,
    Parry,
    Perfect,
    Double,
    Endure,
    Unknown4,
    PowerShard,
    Smite,
}
