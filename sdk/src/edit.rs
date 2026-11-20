//! The client half: the edit itself. Everything is timed from the moment the server's edit message
//! arrives: a 0.5 s pre-roll (whoosh, the FOV pulls back), then beats at `bpm`: the camera cuts every
//! two beats between shots (the player's face, the black void, the character from below, an orbit, the
//! sky, "MOGGED" in grey, a close-up), the FOV punches in and the camera shakes on each beat, a caption
//! word per beat, letterbox bars, white flashes on the cuts, a whip blur on each cut (vanilla's menu
//! blur), grey and red through vanilla post chains (`kotost_aura:grey` / `kotost_aura:red`, this
//! package's `assets/kotost_aura/post_effect/*.json`, added by the mixin on the frame's list), and a beat
//! of vanilla sounds. A mini edit (between big ones, or a revenge) is two seconds of punches and captions.
//! The berry character is a husk named `BERRY_NAME`, drawn here with our own cubes.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use mod_api::client::hud_layout::PoseOp;
use mod_api::serde_json::{self, Value};
use mod_api::{
    BlockEntityDrawContext, CameraPoseEvent, CameraType, ClientHost, CubeFace, DrawContext, Flow, GuiDraw, Host,
    IPayloadContext, IPayloadHandler, MusicLayer, PayloadBytes, PayloadHandleError, RenderLivingPreEvent,
    Shake, SoundPlay, SoundSource, Text,
};

use crate::{BERRY_NAME, THEMES, config_mtime, lock, num, read_config};

const FX_GREY: &str = "kotost_aura:grey";
const FX_RED: &str = "kotost_aura:red";
/// The model's turn toward the viewer: flip to -1.0 if the berry character shows its back.
const FACE_SIGN: f32 = 1.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Shot {
    Face,
    Void,
    Hero,
    Orbit,
    Sky,
    Mogged,
    Close,
}
const PLAN: [Shot; 6] = [Shot::Face, Shot::Void, Shot::Hero, Shot::Orbit, Shot::Sky, Shot::Mogged];

#[derive(Clone)]
struct Ccfg {
    flashes: bool,
    volume: f32,
    beat_volume: f32,
    beat: f32,
    big_seconds: f32,
    track: String,
    words: HashMap<String, Vec<String>>,
    whispers: Vec<String>,
}

fn load_ccfg() -> Ccfg {
    let c = read_config();
    let list = |key: &str| -> Vec<String> {
        c.get(key).map(|v| v.split('|').map(|w| w.trim().to_owned()).filter(|w| !w.is_empty()).collect()).unwrap_or_default()
    };
    let mut words = HashMap::new();
    for t in THEMES.iter().chain(["revenge"].iter()) {
        let mut w = list(&format!("words_{t}"));
        if w.is_empty() {
            w = vec!["МИНУС".to_owned(), "АУРА".to_owned()];
        }
        words.insert((*t).to_owned(), w);
    }
    Ccfg {
        // Off by default: white flashes and red pulses only for a player who turns them on.
        flashes: num(&c, "screen_flashes", 0.0) != 0.0,
        volume: num(&c, "volume", 1.0).clamp(0.0, 2.0) as f32,
        beat_volume: num(&c, "beat_volume", 1.0).clamp(0.0, 2.0) as f32,
        beat: 60.0 / num(&c, "bpm", 150.0).clamp(60.0, 220.0) as f32,
        big_seconds: num(&c, "edit_seconds", 6.0).clamp(1.0, 30.0) as f32,
        track: c.get("track").cloned().unwrap_or_default(),
        words,
        whispers: list("whispers"),
    }
}

#[derive(Clone)]
struct Edit {
    start: Instant,
    theme: String,
    combo: u8,
    maxed: bool,
    big: bool,
    seed: u32,
    pre: f32,
    len: f32,
    char_id: Option<u32>,
    /// Sounds: the last half-beat played (-1: the whoosh is still to come).
    half_done: i64,
}

impl Edit {
    fn t(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }
    fn total(&self) -> f32 {
        self.pre + self.len
    }
    fn side(&self) -> f64 {
        if self.seed & 1 == 0 { 1.0 } else { -1.0 }
    }
}

#[derive(Clone, Copy)]
struct Pose {
    pos: [f64; 3],
    yaw: f32,
    pitch: f32,
}

#[derive(Default)]
struct Client {
    cfg: Option<Ccfg>,
    cfg_mtime: Option<SystemTime>,
    cfg_at: Option<Instant>,
    edit: Option<Edit>,
    total: i64,
    pops: Vec<(String, u32, Instant)>,
    /// The berry characters we were told of: id → feet, refreshed each tick.
    ents: HashMap<u32, [f64; 3]>,
    cam_before: Option<CameraType>,
    pose: Option<Pose>,
    fov: f32,
}

static CLIENT: Mutex<Option<Client>> = Mutex::new(None);
/// The post chains this frame adds (read by the mixin on the frame's list).
static FX: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn client<R>(f: impl FnOnce(&mut Client) -> R) -> R {
    let mut guard = lock(&CLIENT);
    f(guard.get_or_insert_with(Client::default))
}

fn cfg(c: &mut Client) -> Ccfg {
    if c.cfg.is_none() {
        c.cfg = Some(load_ccfg());
        c.cfg_mtime = config_mtime();
    }
    c.cfg.clone().unwrap_or_else(load_ccfg)
}

pub fn post_chains() -> Vec<String> {
    lock(&FX).clone()
}

// ===================================================================================================
// Timeline
// ===================================================================================================

/// Where the edit is now: seconds since the drop (negative in the pre-roll), the beat, the phase
/// within it (0..1), the shot and when it began (seconds since the drop).
struct Now {
    t: f32,
    since_drop: f32,
    beat: i64,
    phase: f32,
    shot: Option<Shot>,
    shot_t: f32,
    intensity: f32,
    left: f32,
}

fn now_of(e: &Edit, beat_len: f32) -> Now {
    let t = e.t();
    let since_drop = t - e.pre;
    let b = if since_drop >= 0.0 { (since_drop / beat_len).floor() as i64 } else { -1 };
    let phase = if since_drop >= 0.0 { (since_drop / beat_len).fract() } else { 0.0 };
    let left = e.total() - t;
    let (shot, shot_t) = if e.big && since_drop >= 0.0 {
        if left < beat_len * 2.0 {
            let start = ((e.len - beat_len * 2.0) / beat_len).floor().max(0.0) * beat_len;
            (Some(Shot::Close), since_drop - start)
        } else {
            let n = (b / 2).max(0) as usize;
            (Some(PLAN[n % PLAN.len()]), since_drop - (n as f32) * beat_len * 2.0)
        }
    } else {
        (None, 0.0)
    };
    let intensity = if e.maxed { 2.0 } else { 1.0 + 0.25 * f32::from(e.combo.saturating_sub(1)) };
    Now { t, since_drop, beat: b, phase, shot, shot_t, intensity, left }
}

fn active(c: &mut Client) -> Option<(Edit, Now, Ccfg)> {
    let e = c.edit.clone()?;
    let k = cfg(c);
    let n = now_of(&e, k.beat);
    if n.t > e.total() {
        return None;
    }
    Some((e, n, k))
}

// ===================================================================================================
// The server's messages
// ===================================================================================================

pub struct Handler;

impl IPayloadHandler for Handler {
    fn handle(&self, payload: PayloadBytes, _context: &mut dyn IPayloadContext) -> Result<(), PayloadHandleError> {
        let Ok(v) = serde_json::from_slice::<Value>(&payload.0) else { return Ok(()) };
        message(&v);
        Ok(())
    }
}

fn message(v: &Value) {
    let int = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    let kind = v.get("k").and_then(Value::as_str).unwrap_or("");
    client(|c| match kind {
        "edit" => {
            let k = cfg(c);
            let theme = v.get("t").and_then(Value::as_str).unwrap_or("other").to_owned();
            let big = v.get("b").and_then(Value::as_bool).unwrap_or(false);
            let maxed = v.get("m").and_then(Value::as_bool).unwrap_or(false);
            let combo = int("c").clamp(0, 9) as u8;
            let delta = int("d");
            c.total = int("a");
            if delta != 0 {
                let color = if delta > 0 { 0xFFD54A } else { 0xFF5A5A };
                c.pops.push((format!("{delta:+} аура"), color, Instant::now()));
            }
            let running_big = c.edit.as_ref().is_some_and(|e| e.big && e.t() < e.total());
            if !big && running_big {
                if let Some(e) = c.edit.as_mut() {
                    e.combo = e.combo.max(combo);
                    e.maxed |= maxed;
                }
                return;
            }
            let len = if big { k.big_seconds } else if theme == "revenge" { 2.5 } else { 2.0 };
            c.edit = Some(Edit {
                start: Instant::now(),
                theme,
                combo,
                maxed,
                big,
                seed: int("s") as u32,
                pre: if big { 0.5 } else { 0.15 },
                len,
                char_id: None,
                half_done: -1,
            });
        }
        "char" => {
            let id = int("id") as u32;
            c.ents.entry(id).or_insert([f64::NAN; 3]);
            if let Some(e) = c.edit.as_mut() {
                e.char_id = Some(id);
            }
        }
        "aura" => {
            c.total = int("a");
            let delta = int("d");
            if delta != 0 {
                let color = if delta > 0 { 0xFFD54A } else { 0xFF5A5A };
                c.pops.push((format!("{delta:+} аура"), color, Instant::now()));
            }
        }
        _ => {}
    });
}

// ===================================================================================================
// Hooks (client)
// ===================================================================================================

/// The local player's feet and yaw.
fn me(host: &dyn ClientHost) -> Option<([f64; 3], f32)> {
    let reader: &dyn Host = host;
    let e = host.local_player()?.entity();
    Some((e.position(reader)?, e.rotation(reader).map_or(0.0, |r| r[0])))
}

/// `client_tick_post`: the characters' places (for the camera and the model's turn), the settings file.
pub fn tick(host: &mut dyn ClientHost) {
    let reader: &dyn ClientHost = &*host;
    let in_world = reader.local_player().is_some();
    let ids: Vec<u32> = client(|c| c.ents.keys().copied().collect());
    let mut ents = HashMap::new();
    if in_world {
        for id in ids {
            if let Some(at) = reader.entity(id).and_then(|e| e.position(reader as &dyn Host)) {
                ents.insert(id, at);
            } else {
                ents.insert(id, [f64::NAN; 3]);
            }
        }
    }
    client(|c| {
        // Ids the client has not seen for long are dropped with the edit that named them.
        let keep: Vec<u32> = c.edit.iter().filter_map(|e| e.char_id).collect();
        ents.retain(|id, at| at[0].is_finite() || keep.contains(id));
        c.ents = ents;
        let check = c.cfg_at.is_none_or(|a| a.elapsed().as_secs_f32() > 5.0);
        if check {
            c.cfg_at = Some(Instant::now());
            let m = config_mtime();
            if m != c.cfg_mtime {
                c.cfg = None;
            }
        }
    });
}

fn char_feet(c: &Client, id: Option<u32>) -> Option<[f64; 3]> {
    id.and_then(|id| c.ents.get(&id).copied()).filter(|p| p[0].is_finite())
}

/// A sound at the listener (the camera follows our pose, so play it there).
fn sound(host: &dyn ClientHost, at: [f64; 3], id: &str, volume: f32, pitch: f32) {
    if volume > 0.0 {
        let mut play = SoundPlay::at(id, SoundSource::Master, at);
        play.volume = volume;
        play.pitch = pitch;
        let _ = host.play_local_sound(&play, 0);
    }
}

/// `render_frame_pre`: the camera type, the post chains and the beat's sounds, once a frame.
pub fn frame(host: &mut dyn ClientHost) {
    let host: &dyn ClientHost = &*host;
    let Some((feet, _)) = me(host).filter(|_| !host.is_paused()) else {
        lock(&FX).clear();
        return;
    };
    let mut camera: Option<CameraType> = None;
    let mut fx: Vec<String> = Vec::new();
    let mut sounds: Vec<(&'static str, f32, f32)> = Vec::new();
    let mut at = [feet[0], feet[1] + 1.62, feet[2]];
    client(|c| {
        let Some((mut e, n, k)) = active(c) else {
            c.edit = None;
            return;
        };
        if let Some(p) = c.pose {
            at = p.pos;
        }
        if e.big && n.t >= e.pre - 0.05 && n.left > 0.05 {
            camera = Some(CameraType::ThirdPersonBack);
        }
        // Grey on "MOGGED", red all the close-up for fire; with `flashes` on, also red on the last
        // beat and on the off-beats of "НЕИГРАБЕЛЬНО" (full-screen pulses).
        let last_beat = n.left < k.beat;
        let pulse = k.flashes && ((e.big && last_beat) || (e.maxed && n.beat % 2 == 1));
        if n.shot == Some(Shot::Mogged) {
            fx = vec![FX_GREY.to_owned()];
        } else if pulse || (n.shot == Some(Shot::Close) && e.theme == "fire") {
            fx = vec![FX_RED.to_owned()];
        }
        // With the owner's track every vanilla sound of the edit follows `beat_volume` (0: the track alone).
        let v = if k.track.is_empty() { k.volume } else { k.volume * k.beat_volume };
        // -1: the whoosh is still to come; -2: it played, the beat has not started.
        if e.half_done == -1 {
            sounds.push(("minecraft:entity.breeze.wind_burst", 1.0 * v, 0.7));
            e.half_done = -2;
        }
        if n.since_drop >= 0.0 {
            let half = (n.since_drop / (k.beat / 2.0)).floor() as i64;
            // Only the newest half-beat: a long frame never stacks sounds.
            if half > e.half_done {
                let h = half;
                if h % 2 == 0 {
                    sounds.push(("minecraft:entity.warden.heartbeat", 1.0 * v, 1.1));
                    sounds.push(("minecraft:block.note_block.basedrum", 0.9 * v, 0.6));
                    if h % 4 == 2 {
                        sounds.push(("minecraft:block.note_block.snare", 0.7 * v, 1.0));
                    }
                }
                sounds.push(("minecraft:block.note_block.hat", if h % 2 == 1 { 0.45 } else { 0.25 } * v, 1.6));
                if e.big && h % 4 == 0 && n.shot != Some(Shot::Close) {
                    sounds.push(("minecraft:block.bell.use", 0.45 * v, 1.4));
                }
                if n.shot == Some(Shot::Mogged) && n.shot_t < k.beat / 2.0 {
                    sounds.push(("minecraft:entity.elder_guardian.curse", 0.6 * v, 1.0));
                }
                if e.big && last_beat && h % 2 == 0 {
                    sounds.push(("minecraft:entity.generic.explode", 0.7 * v, 1.0));
                }
                e.half_done = h;
            }
        }
        c.edit = Some(e);
    });
    for (id, volume, pitch) in sounds {
        sound(host, at, id, volume, pitch);
    }
    // The camera: third person through the big edit (the player's model shows, the hand doesn't),
    // the player's own one back after it.
    let before = client(|c| c.cam_before);
    match camera {
        Some(to) => {
            if before.is_none() {
                let was = host.camera_type();
                client(|c| c.cam_before = Some(was));
            }
            if host.camera_type() != to {
                host.set_camera_type(to);
            }
        }
        None => {
            if let Some(was) = before {
                if host.camera_type() != was {
                    host.set_camera_type(was);
                }
                client(|c| {
                    c.cam_before = None;
                    c.pose = None;
                });
            }
        }
    }
    *lock(&FX) = fx;
}

fn look_at(from: [f64; 3], to: [f64; 3]) -> (f32, f32) {
    let (dx, dy, dz) = (to[0] - from[0], to[1] - from[1], to[2] - from[2]);
    let h = (dx * dx + dz * dz).sqrt().max(1e-6);
    let yaw = (-dx).atan2(dz).to_degrees() as f32;
    let pitch = (-(dy.atan2(h))).to_degrees() as f32;
    (yaw, pitch.clamp(-89.0, 89.0))
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn scale(a: [f64; 3], k: f64) -> [f64; 3] {
    [a[0] * k, a[1] * k, a[2] * k]
}
/// The horizontal unit vector of a Minecraft yaw.
fn dir(yaw_deg: f64) -> [f64; 3] {
    let r = yaw_deg.to_radians();
    [-r.sin(), 0.0, r.cos()]
}
fn wrap(d: f32) -> f32 {
    let d = d.rem_euclid(360.0);
    if d > 180.0 { d - 360.0 } else { d }
}
fn ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// `camera_pose`: the edit's shots, cut on the beat, eased back into the player's camera.
pub fn camera_pose(host: &dyn ClientHost, event: &mut CameraPoseEvent<'_>) {
    let Some((feet, my_yaw)) = me(host) else { return };
    let head = add(feet, [0.0, 1.62, 0.0]);
    let pose = client(|c| {
        let (e, n, k) = active(c)?;
        if !e.big || n.since_drop < 0.0 {
            c.pose = None;
            return None;
        }
        let shot = n.shot?;
        let u = (n.shot_t / (k.beat * 2.0)).clamp(0.0, 1.0) as f64;
        let side = e.side();
        let ch_feet = char_feet(c, e.char_id);
        // The subject: the character when there is one, else the player.
        let subj = ch_feet.unwrap_or(feet);
        let subj_h = if ch_feet.is_some() { 2.4 } else { 1.7 };
        let to_me = {
            let d = [feet[0] - subj[0], 0.0, feet[2] - subj[2]];
            let l = (d[0] * d[0] + d[2] * d[2]).sqrt();
            if l > 0.3 { scale(d, 1.0 / l) } else { dir(f64::from(my_yaw)) }
        };
        let right = [to_me[2], 0.0, -to_me[0]];
        let my_dir = dir(f64::from(my_yaw));
        let (cam, target) = match shot {
            Shot::Face => {
                let d = 1.9 - 0.6 * u;
                (add(head, add(scale(my_dir, d), [0.0, -0.15, 0.0])), head)
            }
            Shot::Void => {
                let side_dir = [my_dir[2] * side, 0.0, -my_dir[0] * side];
                (add(head, add(scale(side_dir, 2.2 - 0.5 * u), [0.0, 0.25, 0.0])), add(feet, [0.0, 1.1, 0.0]))
            }
            Shot::Hero => {
                let cam = add(subj, add(scale(to_me, 2.3), [0.0, 0.2 + 0.3 * u, 0.0]));
                (add(cam, scale(right, 0.6 * side)), add(subj, [0.0, subj_h - 0.2, 0.0]))
            }
            Shot::Orbit => {
                let base = f64::from(look_at(subj, feet).0);
                let ang = base + side * (u * 150.0 - 40.0);
                let cam = add(subj, add(scale(dir(ang), 3.0), [0.0, 1.5, 0.0]));
                (cam, add(subj, [0.0, subj_h * 0.6, 0.0]))
            }
            Shot::Sky => {
                let cam = add(subj, add(scale(to_me, 1.3), [0.0, 0.25, 0.0]));
                (cam, add(subj, add(scale(to_me, -1.0), [0.0, 8.0, 0.0])))
            }
            Shot::Mogged => {
                // Over the character's shoulder, down at the player.
                let top = add(subj, [0.0, subj_h + 0.3, 0.0]);
                (add(top, add(scale(to_me, 0.4), scale(right, 0.6 * side))), add(feet, [0.0, 1.2, 0.0]))
            }
            Shot::Close => {
                let face = add(subj, [0.0, subj_h - 0.45, 0.0]);
                (add(face, add(scale(to_me, 1.25 - 0.35 * u), [0.0, 0.05, 0.0])), face)
            }
        };
        let (y, p) = look_at(cam, target);
        // Back into the player's camera over the last 0.3 s.
        let k_back = ease(n.left / 0.3);
        let pose = Pose { pos: cam, yaw: y, pitch: p };
        c.pose = Some(pose);
        Some((pose, k_back))
    });
    let Some((p, k)) = pose else { return };
    let kd = f64::from(k);
    let position = &mut *event.position;
    *position = [
        position[0] + (p.pos[0] - position[0]) * kd,
        position[1] + (p.pos[1] - position[1]) * kd,
        position[2] + (p.pos[2] - position[2]) * kd,
    ];
    event.yaw += wrap(p.yaw - event.yaw) * k;
    event.pitch += (p.pitch - event.pitch) * k;
}

/// `compute_fov`: pulled back in the pre-roll, punched in on each beat.
pub fn compute_fov(degrees: &mut f32) {
    let out = client(|c| {
        let Some((e, n, _)) = active(c) else {
            c.fov = *degrees;
            return None;
        };
        let mut d = *degrees;
        if n.since_drop < 0.0 {
            d *= 1.0 + 0.15 * (n.t / e.pre.max(0.01));
        } else {
            let punch = (-n.phase * 7.0).exp();
            d *= 1.0 - (0.16 * n.intensity * punch).min(0.45);
            d *= match n.shot {
                Some(Shot::Hero) | Some(Shot::Sky) => 1.15,
                Some(Shot::Close) => 0.85,
                _ => 1.0,
            };
        }
        let d = d.clamp(20.0, 140.0);
        c.fov = d;
        Some(d)
    });
    if let Some(d) = out {
        *degrees = d;
    }
}

/// `camera_shake`: a kick on each beat, a Dutch-angle roll.
pub fn camera_shake(shake: &mut Shake) {
    let s = client(|c| {
        let (_, n, _) = active(c)?;
        if n.since_drop < 0.0 {
            return None;
        }
        Some((n.intensity * (-n.phase * 6.0).exp(), n.since_drop))
    });
    let Some((a, t)) = s else { return };
    if a < 0.01 {
        return;
    }
    shake.pitch += a * 2.0 * (t * 53.0).sin();
    shake.yaw += a * 1.5 * (t * 41.0 + 1.0).sin();
    shake.roll += a * 3.0 * (t * 37.0 + 2.0).sin();
}

/// `render_fog`: the black void around the player in its shot.
pub fn render_fog(fog: &mut mod_api::client::fog::FogData) {
    let void = client(|c| active(c).is_some_and(|(_, n, _)| n.shot == Some(Shot::Void)));
    if !void {
        return;
    }
    fog.color = [0.0, 0.0, 0.0, 1.0];
    fog.environmental_start = 0.0;
    fog.environmental_end = 3.5;
    fog.render_distance_start = 0.0;
    fog.render_distance_end = 3.5;
    fog.sky_end = 0.0;
    fog.cloud_end = 0.0;
}

/// `menu_background`: a whip blur on each cut (vanilla's menu blur, no shader pack).
pub fn menu_background(radius: &mut Option<f32>) {
    let blur = client(|c| {
        let (e, n, _) = active(c)?;
        if !e.big {
            return None;
        }
        let cut = n.shot.is_some() && n.shot_t < 0.1 && n.since_drop > 0.05;
        let drop = n.since_drop < 0.0 && n.since_drop > -0.15;
        (cut || drop).then_some(9.0f32)
    });
    if let Some(r) = blur {
        *radius = Some(r);
    }
}

/// `music`: vanilla's music is quiet through an edit.
pub fn music(volume: &mut f32) {
    if client(|c| active(c).is_some()) {
        *volume = 0.0;
    }
}

/// `music_layers`: the owner's own track under every edit (a mini edit plays its drop), when one is set.
pub fn music_layers(layers: &mut Vec<MusicLayer>) {
    let track = client(|c| {
        let (_, n, k) = active(c)?;
        (n.since_drop >= -0.1 && !k.track.is_empty()).then(|| (k.track.clone(), k.volume.min(1.0)))
    });
    if let Some((sound, volume)) = track {
        layers.push(MusicLayer { sound, volume });
    }
}

// ===================================================================================================
// HUD
// ===================================================================================================

/// Centred text with a black outline, scaled about its centre.
fn outlined(b: &mut dyn DrawContext, s: &str, cx: f32, cy: f32, k: f32, rgb: u32) {
    let [width, _] = b.size();
    let w = b.text_width(s);
    let k = k.min(width * 0.92 / w.max(1.0));
    let (x, y) = ((cx - w / 2.0).round() as i32, (cy - 4.0).round() as i32);
    let mut draws = vec![GuiDraw::Push(vec![PoseOp::Translate(cx, cy), PoseOp::Scale(k, k), PoseOp::Translate(-cx, -cy)])];
    for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (1, 1), (-1, 1), (1, -1)] {
        draws.push(GuiDraw::Text { text: Text::literal(s), x: x + dx, y: y + dy, color: 0xFF00_0000, shadow: false });
    }
    draws.push(GuiDraw::Text { text: Text::literal(s), x, y, color: 0xFF00_0000 | (rgb & 0xFF_FFFF), shadow: false });
    draws.push(GuiDraw::Pop);
    let _ = b.gui_paint(&draws);
}

fn argb(alpha: f32, rgb: u32) -> u32 {
    ((alpha.clamp(0.0, 1.0) * 255.0) as u32) << 24 | (rgb & 0xFF_FFFF)
}

/// A world point on screen (GUI pixels) from our own camera pose and FOV.
fn project(p: Pose, fov: f32, w: f32, h: f32, at: [f64; 3]) -> Option<[f32; 2]> {
    let (yr, pr) = (f64::from(p.yaw).to_radians(), f64::from(p.pitch).to_radians());
    let fwd = [-yr.sin() * pr.cos(), -pr.sin(), yr.cos() * pr.cos()];
    let right = [-yr.cos(), 0.0, -yr.sin()];
    let up = [
        right[1] * fwd[2] - right[2] * fwd[1],
        right[2] * fwd[0] - right[0] * fwd[2],
        right[0] * fwd[1] - right[1] * fwd[0],
    ];
    let rel = [at[0] - p.pos[0], at[1] - p.pos[1], at[2] - p.pos[2]];
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let z = dot(rel, fwd);
    if z < 0.2 {
        return None;
    }
    let f = f64::from(h) / 2.0 / (f64::from(fov).to_radians() / 2.0).tan();
    let x = f64::from(w) / 2.0 - dot(rel, right) / z * f;
    let y = f64::from(h) / 2.0 - dot(rel, up) / z * f;
    Some([x as f32, y as f32])
}

pub fn hud(host: &dyn ClientHost, b: &mut dyn DrawContext) {
    if host.local_player().is_none() || host.is_paused() {
        return;
    }
    let [w, h] = b.size();
    let state = client(|c| {
        c.pops.retain(|p| p.2.elapsed().as_secs_f32() < 1.6);
        let pops = c.pops.clone();
        let ch = active(c).map(|(e, n, k)| {
            let feet = char_feet(c, e.char_id);
            (e, n, k, feet)
        });
        (c.total, pops, ch, c.pose, c.fov)
    });
    let (total, pops, ch, pose, fov) = state;
    if let Some((e, n, k, char_feet)) = ch {
        // Letterbox bars through the big edit.
        if e.big && n.since_drop >= 0.0 {
            let bar = (h * 0.1).round();
            b.fill([0.0, 0.0, w, bar], 0xFF00_0000);
            b.fill([0.0, h - bar, w, bar], 0xFF00_0000);
        }
        // A tint by theme; a red pulse on "НЕИГРАБЕЛЬНО" (with `flashes` on).
        let tint = match e.theme.as_str() {
            "fire" => Some(0xFF7A1A),
            "water" => Some(0x2A6AFF),
            "revenge" => Some(0xFFD54A),
            _ => None,
        };
        if let Some(rgb) = tint {
            b.fill([0.0, 0.0, w, h], argb(0.12, rgb));
        }
        if k.flashes && e.maxed && n.since_drop >= 0.0 {
            b.fill([0.0, 0.0, w, h], argb(0.25 * (-n.phase * 4.0).exp(), 0xFF0000));
        }
        // Flashes: on the drop and on each cut (at most every two beats), white.
        if k.flashes && n.since_drop >= 0.0 {
            let since_cut = if n.shot.is_some() { n.shot_t } else { n.since_drop };
            let strong = n.left < k.beat && e.big;
            let a = if since_cut < 0.12 { 0.55 * (1.0 - since_cut / 0.12) } else { 0.0 };
            let a = if strong && n.phase < 0.2 { a.max(0.85 * (1.0 - n.phase / 0.2)) } else { a };
            if a > 0.01 {
                b.fill([0.0, 0.0, w, h], argb(a, 0xFFFFFF));
            }
        }
        if n.since_drop >= 0.0 {
            let words = k.words.get(&e.theme).cloned().unwrap_or_default();
            let pop = 1.0 + 0.3 * (-n.phase * 10.0).exp();
            if n.shot == Some(Shot::Mogged) {
                b.fill([0.0, 0.0, w, h], argb(0.15, 0x808080));
                outlined(b, "MOGGED", w / 2.0, h * 0.5, 5.0 * pop, 0xE0E0E0);
            } else if !words.is_empty() {
                let word = &words[(n.beat.max(0) as usize + (e.seed as usize % 3)) % words.len()];
                let rgb = if e.theme == "revenge" { 0xFFD54A } else { 0xFFFFFF };
                outlined(b, word, w / 2.0, h * 0.24, 4.0 * pop, rgb);
            }
            // Whispers at the bottom on the off-beats.
            if n.beat % 2 == 1 && !k.whispers.is_empty() {
                let wsp = &k.whispers[(n.beat as usize / 2 + e.seed as usize) % k.whispers.len()];
                outlined(b, wsp, w / 2.0, h * 0.8, 1.6, 0xC8C8C8);
            }
            if e.maxed {
                outlined(b, "НЕИГРАБЕЛЬНО", w / 2.0, h * 0.68, 3.0 * pop, 0xFF3030);
            } else if e.combo > 1 {
                outlined(b, &format!("x{}", e.combo), w - 24.0, h * 0.14, 2.0, 0xFFD54A);
            }
            // The character marked, as edits do: a red frame round its head and "+1000 АУРА".
            if let (Some(feet), Some(p)) = (char_feet, pose)
                && matches!(n.shot, Some(Shot::Hero) | Some(Shot::Orbit) | Some(Shot::Close))
                && let Some([sx, sy]) = project(p, fov, w, h, add(feet, [0.0, 2.15, 0.0]))
            {
                let r = (24.0 * (1.0 + 0.15 * (-n.phase * 8.0).exp())).min(w * 0.2);
                let t = 2.0;
                b.fill([sx - r, sy - r, r * 2.0, t], 0xFFFF2020);
                b.fill([sx - r, sy + r - t, r * 2.0, t], 0xFFFF2020);
                b.fill([sx - r, sy - r, t, r * 2.0], 0xFFFF2020);
                b.fill([sx + r - t, sy - r, t, r * 2.0], 0xFFFF2020);
                let label = "+1000 АУРА";
                let lw = b.text_width(label) * 1.5 + 8.0;
                let (lx, ly) = (sx - lw / 2.0, sy - r - 22.0);
                b.fill([lx, ly, lw, 16.0], 0xFFFFFFFF);
                let tw = b.text_width(label);
                let _ = b.gui_paint(&[
                    GuiDraw::Push(vec![PoseOp::Translate(sx, ly + 8.0), PoseOp::Scale(1.5, 1.5), PoseOp::Translate(-sx, -(ly + 8.0))]),
                    GuiDraw::Text {
                        text: Text::literal(label),
                        x: (sx - tw / 2.0).round() as i32,
                        y: (ly + 4.0).round() as i32,
                        color: 0xFF00_0000,
                        shadow: false,
                    },
                    GuiDraw::Pop,
                ]);
            }
        }
    }
    // Aura pops, rising from the middle.
    for (i, (text, rgb, at)) in pops.iter().enumerate() {
        let s = at.elapsed().as_secs_f32();
        let y = h * 0.42 - s * 30.0 - i as f32 * 14.0;
        outlined(b, text, w / 2.0, y, 2.0, *rgb);
    }
    // The aura itself, small, top right.
    if total != 0 {
        let label = format!("аура {total}");
        let tw = b.text_width(&label);
        let rgb = if total > 0 { 0xFFD54A } else { 0xFF7070 };
        b.text(&label, [w - tw - 4.0, 4.0], 0xFF00_0000 | rgb, true);
    }
}

// ===================================================================================================
// The berry character's model
// ===================================================================================================

fn cube(draw: &mut dyn BlockEntityDrawContext, sprite: &str, from: [f32; 3], to: [f32; 3], tint: u32) {
    // Explicit UVs: the box's own run past 0..16 for our coordinates and sample the atlas' neighbours.
    let f = || Some(CubeFace::new(sprite).with_uv([0.0, 0.0, 16.0, 16.0]));
    let _ = draw.cube(from, to, [f(), f(), f(), f(), f(), f()], tint);
}

/// `render_living_pre`: the "Berry sigma" in place of its husk. Cubes in texels (16 a block), turns in blocks,
/// feet at 0, +z the way it faces. White body with berry stains, a square jaw, black shades, a crown of
/// berry bush; it turns to whoever looks and nods to the beat.
pub fn berry_model(host: &dyn ClientHost, event: &mut RenderLivingPreEvent<'_>) -> Flow {
    let mob = event.entity;
    if mob.custom_name != Some(BERRY_NAME) {
        return Flow::Continue;
    }
    let (pos, beat) = client(|c| {
        let beat = active(c).map(|(_, n, _)| n.phase);
        (c.ents.get(&mob.id).copied().filter(|p| p[0].is_finite()), beat)
    });
    let reader: &dyn Host = host;
    let viewer = host.local_player().and_then(|p| p.entity().position(reader));
    let draw = &mut *event.draw;
    draw.push();
    if let (Some(m), Some(me)) = (pos, viewer) {
        let want = look_at(m, me).0;
        let rel = wrap(want - mob.body_yaw);
        draw.rotate_about([0.0, 1.0, 0.0], -rel * FACE_SIGN, [0.0, 0.0, 0.0]);
    }
    draw.scale([1.15, 1.15, 1.15]);
    const WHITE: &str = "block/white_concrete";
    const BLACK: &str = "block/black_concrete";
    const RED: &str = "block/red_concrete";
    const LEAVES: &str = "block/oak_leaves";
    // Legs, body, arms.
    cube(draw, WHITE, [-4.0, 0.0, -2.0], [-0.1, 12.0, 2.0], 0xF4F4F4);
    cube(draw, WHITE, [0.1, 0.0, -2.0], [4.0, 12.0, 2.0], 0xF4F4F4);
    cube(draw, WHITE, [-4.0, 12.0, -2.0], [4.0, 24.0, 2.0], 0xFFFFFF);
    cube(draw, WHITE, [-8.0, 12.0, -2.0], [-4.0, 24.0, 2.0], 0xF0F0F0);
    cube(draw, WHITE, [4.0, 12.0, -2.0], [8.0, 24.0, 2.0], 0xF0F0F0);
    // Berry stains on the chest and an arm.
    for (x, y) in [(-2.5, 19.0), (0.5, 21.0), (1.5, 16.5), (-1.0, 14.0)] {
        cube(draw, RED, [x, y, 2.0], [x + 1.6, y + 1.6, 2.3], 0xD01830);
    }
    cube(draw, RED, [-7.0, 18.0, 2.0], [-5.5, 19.5, 2.3], 0xD01830);
    // The head nods on the beat.
    let nod = beat.map_or((mob.age * 0.15).sin() * 2.0, |ph| -8.0 * (-ph * 6.0).exp());
    draw.push();
    draw.rotate_about([1.0, 0.0, 0.0], nod, [0.0, 1.5, 0.0]);
    cube(draw, WHITE, [-4.0, 24.0, -4.0], [4.0, 32.0, 4.0], 0xFFFFFF);
    // The sigma jaw.
    cube(draw, WHITE, [-4.3, 23.0, 1.0], [4.3, 26.0, 4.6], 0xE8E8E8);
    // Shades.
    cube(draw, BLACK, [-4.4, 28.0, 4.0], [4.4, 29.8, 4.6], 0xFFFFFF);
    // The crown of berry bush and its berries.
    cube(draw, LEAVES, [-5.0, 31.0, -5.0], [5.0, 35.0, 5.0], 0x3E8E3E);
    cube(draw, LEAVES, [-5.0, 26.0, -5.0], [5.0, 31.0, -3.6], 0x3E8E3E);
    cube(draw, LEAVES, [-5.0, 27.0, -3.6], [-4.0, 31.0, 2.0], 0x3E8E3E);
    cube(draw, LEAVES, [4.0, 27.0, -3.6], [5.0, 31.0, 2.0], 0x3E8E3E);
    for (x, y, z) in [
        (-4.0, 34.6, -2.0),
        (-1.0, 35.0, 1.5),
        (2.5, 34.6, -1.0),
        (0.5, 34.8, -4.0),
        (-3.5, 33.0, 4.6),
        (2.8, 32.6, 4.6),
        (-5.4, 30.0, -1.0),
        (5.0, 29.0, 0.5),
        (-2.0, 28.0, -5.4),
        (2.0, 30.0, -5.4),
    ] {
        cube(draw, RED, [x, y, z], [x + 1.6, y + 1.6, z + 1.0], 0xE0203A);
    }
    draw.pop();
    draw.pop();
    Flow::Cancel
}
