fn kotost_aura_net_server(game: &mut crate::game::Game<'_>, channel: &str, data: &[u8]) {
    crate::game::kotost_aura_edit::net_from_server(game, channel, data);
}
