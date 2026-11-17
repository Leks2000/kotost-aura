fn kotost_aura_camera_pose(game: &crate::game::Game<'_>, position: &mut [f64; 3], yaw: &mut f32, pitch: &mut f32) {
    crate::game::kotost_aura_edit::camera_pose(game, position, yaw, pitch);
}
