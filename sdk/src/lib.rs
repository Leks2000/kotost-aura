//! Kotost Aura («Аура»): damage turns the game into an edit about whoever (whatever) hurt you.
//! A sweet berry bush pricks a player: a cool "Berry sigma" character climbs out next to them (a no-AI
//! husk the clients draw with our own model), and for six seconds the victim's screen is a hardtekk
//! edit about it: a cinematic camera, beat zooms, shakes, captions word by word, a grey "MOGGED",
//! flashes. The victim loses aura; killing the character wins it back (revenge).
//!
//! `server`: damage and deaths are queued by the hooks and handled at the end of the server tick.
//! `edit`: the client half, timed from the server's message on `kotost_aura:aura`.
//! Settings: `config/kotost_aura.txt` in the instance folder (both halves); aura per player:
//! `config/kotost_aura_players.json`.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use mod_api::{
    CameraPoseEvent, CameraShakeEvent, ClientHost, ClientTickPostEvent, ComputeFovEvent, Flow, GuiRenderPostEvent,
    Hooks, Host, LevelRandomHost, LivingDamagePostEvent, LivingDeathEvent, ServerHost, MenuBackgroundEvent, MusicEvent, MusicLayersEvent,
    PayloadId, PlayerLoggedInEvent, RegisterCommandsEvent, Registrar, RegistrationContext, RenderFogEvent,
    RenderFramePreEvent, RenderLivingPreEvent, RunModCommandEvent, ServerTickEvent,
};

mod edit;
mod server;

/// One channel, server to client: `{"k": "edit" | "char" | "aura", …}` as JSON.
pub const CHANNEL: &str = "kotost_aura:aura";
/// The custom name the clients know the berry character by (and its name tag).
pub const BERRY_NAME: &str = "Ягода-сигма";

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn rand_u32() -> u32 {
    static SEED: Mutex<u64> = Mutex::new(0);
    let mut s = lock(&SEED);
    if *s == 0 {
        *s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
    }
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    (*s >> 32) as u32
}

// ===================================================================================================
// Themes and settings (both halves)
// ===================================================================================================

/// What hurt the player, by damage type.
pub fn theme_of(damage_type: &str) -> &'static str {
    let t = damage_type.strip_prefix("minecraft:").unwrap_or(damage_type);
    match t {
        "sweet_berry_bush" | "cactus" => "berry",
        "in_fire" | "on_fire" | "lava" | "hot_floor" | "campfire" | "fireball" | "unattributed_fireball" => "fire",
        "fall" | "stalagmite" | "fly_into_wall" | "falling_block" | "falling_anvil" | "falling_stalactite" => "fall",
        "drown" | "dry_out" | "freeze" => "water",
        "player_attack" | "player_explosion" => "player",
        "mob_attack" | "mob_attack_no_aggro" | "mob_projectile" | "arrow" | "trident" | "sting" | "sonic_boom" => "mob",
        "explosion" | "bad_respawn_point" | "firework" | "wind_charge" => "boom",
        _ => "other",
    }
}

pub const THEMES: [&str; 8] = ["berry", "fire", "fall", "water", "player", "mob", "boom", "other"];

static INSTANCE: OnceLock<PathBuf> = OnceLock::new();

/// Remembers the instance folder the first time a hook has a host.
pub fn note_instance(host: &dyn Host) {
    if INSTANCE.get().is_none() {
        let _ = INSTANCE.set(host.instance_dir());
    }
}

pub fn config_dir() -> PathBuf {
    INSTANCE.get().cloned().unwrap_or_default().join("config")
}

pub const DEFAULT_FILE: &str = "\
# Kotost Aura («Аура»): урон превращает игру в эдит про того, кто ударил.
# Сервер
enabled = 1
# урон меньше этого не считается (полсердца = 1)
min_damage = 0.5
edit_seconds = 6
# большой эдит (кино-камера) не чаще, секунд; между ними мини-эдит
big_edit_cooldown = 20
combo_max = 5
# после «НЕИГРАБЕЛЬНО» столько секунд без эдитов
quiet_seconds = 10
# сколько живёт персонаж урона
char_seconds = 30
aura_berry = -200
aura_fire = -300
aura_fall = -300
aura_water = -200
aura_player = -500
aura_mob = -100
aura_boom = -400
aura_other = -100
aura_death = -1000
aura_revenge = 1000
# Клиент (у каждого свой)
# 1: белые вспышки и красные пульсации на весь экран (по умолчанию выключены)
screen_flashes = 0
volume = 1.0
bpm = 150
# свой трек (звуковое событие ресурспака, stream; старт за 0,1 с до дропа); пусто: только бит из ванильных звуков
track =
# громкость ванильного бита под своим треком (0: выключить)
beat_volume = 1.0
# подписи по слову, через |
words_berry = ЯГОДА|МОГНУЛА|ТЕБЯ|ЭТО|СИГМА|+1000|АУРЫ|КУСТ|ТЫ|ПРОИГРАЛ
words_fire = ТЫ|ГОРИШЬ|А|ОН|НЕТ|ЛАВА|МОГ|ГОРЯЧО
words_fall = ГРАВИТАЦИЯ|ЗАБРАЛА|ТВОЮ|АУРУ|ЗЕМЛЯ|СИГМА
words_water = ТЫ|НЕ|ДЫШИШЬ|ЛАРП|ВОДА|ПОБЕДИЛА
words_player = ДРУГ|ПРЕДАЛ|МОГ|ДИЗРЕСПЕКТ|МИНУС|АУРА
words_mob = МОБ|ЗАБРАЛ|ТВОЮ|АУРУ|ЛАРП
words_boom = БУМ|АУРЫ|БОЛЬШЕ|НЕТ
words_other = МИНУС|АУРА|ЛАРП
words_revenge = РЕВАНШ|+1000|АУРЫ|ТЕПЕРЬ|ТЫ|СИГМА
whispers = мог...|ларп...|аура...|сигма...|фронт...|мьюинг...
";

/// `config/kotost_aura.txt` as key = value (written with the defaults when missing).
pub fn read_config() -> HashMap<String, String> {
    let path = config_dir().join("kotost_aura.txt");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            if INSTANCE.get().is_some() {
                let _ = std::fs::create_dir_all(config_dir());
                let _ = std::fs::write(&path, DEFAULT_FILE);
            }
            DEFAULT_FILE.to_owned()
        }
    };
    let mut out = HashMap::new();
    for source in [DEFAULT_FILE, text.as_str()] {
        for line in source.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                out.insert(k.trim().to_owned(), v.trim().to_owned());
            }
        }
    }
    out
}

pub fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(config_dir().join("kotost_aura.txt")).and_then(|m| m.modified()).ok()
}

pub fn num(cfg: &HashMap<String, String>, key: &str, default: f64) -> f64 {
    cfg.get(key).and_then(|v| v.replace(',', ".").parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(default)
}

/// The RETURN mixin on the frame's post chain list (`src/mixins/post.rs`).
pub fn post_effects(mut value: Vec<String>) -> Vec<String> {
    value.extend(edit::post_chains());
    value
}

// ===================================================================================================
// Hooks
// ===================================================================================================

#[derive(Default)]
pub struct Mod;

#[mod_api::horizon_mod]
impl Hooks for Mod {
    fn register(&self, r: &mut dyn Registrar, _cx: &RegistrationContext) {
        r.payload_handlers()
            .registrar("1")
            .optional()
            .play_to_client(PayloadId(CHANNEL.into()), Some(Arc::new(edit::Handler)));
    }

    // --- server ------------------------------------------------------------------------------------
    fn register_commands(&self, _host: &mut dyn ServerHost, event: &mut RegisterCommandsEvent<'_>) {
        server::register_commands(event.commands);
    }

    fn run_mod_command(&self, host: &mut dyn ServerHost, event: &mut RunModCommandEvent<'_>) -> Flow {
        note_instance(&*host);
        server::run_command(host, event)
    }

    /// Only a hit that landed: after the shield, the hurt cooldown and every reduction. What it took
    /// from health and the absorption hearts; a fully blocked hit is 0 and costs nothing.
    fn living_damage_post(&self, _level: &mut dyn LevelRandomHost, event: &mut LivingDamagePostEvent<'_>) {
        let c = event.container;
        if c.should_cause_side_effects() {
            let landed = c.health_damage() + c.reduction(mod_api::Reduction::Absorption);
            server::incoming(event.entity, event.source.damage_type, landed);
        }
    }

    fn living_death(&self, _host: &mut dyn ServerHost, event: &mut LivingDeathEvent<'_>) -> Flow {
        server::living_death(event.entity, event.kind, event.source.causing);
        Flow::Continue
    }

    fn player_logged_in(&self, _host: &mut dyn ServerHost, event: &mut PlayerLoggedInEvent<'_>) {
        server::joined(event.player);
    }

    fn server_tick(&self, host: &mut dyn ServerHost, event: &mut ServerTickEvent<'_>) {
        if matches!(event.phase, mod_api::ServerTickPhase::ServerTickEnd { .. }) {
            note_instance(&*host);
            server::server_tick(host);
        }
    }

    // --- client ------------------------------------------------------------------------------------
    fn client_tick_post(&self, host: &mut dyn ClientHost, _event: &mut ClientTickPostEvent) {
        note_instance(&*host);
        edit::tick(host);
    }

    fn render_frame_pre(&self, host: &mut dyn ClientHost, _event: &mut RenderFramePreEvent) {
        edit::frame(host);
    }

    fn camera_pose(&self, host: &dyn ClientHost, event: &mut CameraPoseEvent<'_>) {
        edit::camera_pose(host, event);
    }

    fn compute_fov(&self, _host: &dyn ClientHost, event: &mut ComputeFovEvent) {
        edit::compute_fov(&mut event.degrees);
    }

    fn camera_shake(&self, _host: &dyn ClientHost, event: &mut CameraShakeEvent<'_>) {
        edit::camera_shake(event.shake);
    }

    fn render_fog(&self, _host: &dyn ClientHost, event: &mut RenderFogEvent<'_>) {
        edit::render_fog(event.fog);
    }

    fn menu_background(&self, _host: &dyn ClientHost, event: &mut MenuBackgroundEvent<'_>) {
        edit::menu_background(event.radius);
    }

    fn music(&self, _host: &dyn ClientHost, event: &mut MusicEvent<'_>) {
        edit::music(&mut event.volume);
    }

    fn music_layers(&self, _host: &dyn ClientHost, event: &mut MusicLayersEvent<'_>) {
        edit::music_layers(event.layers);
    }

    fn gui_render_post(&self, host: &dyn ClientHost, event: &mut GuiRenderPostEvent<'_>) {
        if !event.gui_hidden {
            edit::hud(host, event.batch);
        }
    }

    fn render_living_pre(&self, host: &dyn ClientHost, event: &mut RenderLivingPreEvent<'_>) -> Flow {
        edit::berry_model(host, event)
    }
}
