fn kotost_aura_incoming(entity: i32, damage_type: &str, causing: i32, direct: i32, amount: &mut f32, canceled: &mut bool) {
    crate::game::kotost_aura::incoming(entity, damage_type, causing, direct, amount, canceled);
}
