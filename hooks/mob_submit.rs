fn kotost_aura_mob_submit(game: &crate::game::Game<'_>, mob: &NativeMobRender<'_>, draw: &mut BlockEntityDraw, drawn: &mut bool) {
    crate::game::kotost_aura_edit::mob_submit(game, mob, draw, drawn);
}
