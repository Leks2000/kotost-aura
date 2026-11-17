fn kotost_aura_command(game: &mut crate::game::Game<'_>, player: &NativeServerPlayer, command: &str, ran: &mut bool) {
    crate::game::kotost_aura::perform_command(game, player, command, ran);
}
