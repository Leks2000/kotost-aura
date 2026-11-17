fn kotost_aura_death(entity: u32, kind: &str, damage_type: &str, attacker: u32, cancel: &mut bool) {
    crate::game::kotost_aura::living_death(entity, kind, damage_type, attacker, cancel);
}
