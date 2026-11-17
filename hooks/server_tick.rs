fn kotost_aura_server_tick(game: &mut crate::game::Game<'_>, phase: horizon_client::mod_api::ServerTickPhase) {
    crate::game::kotost_aura::server_tick_hook(game, phase);
}
