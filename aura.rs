// Kotost Aura («Аура»): damage turns the game into an edit about whoever (whatever) hurt you.
// A sweet berry bush pricks a player: a cool "Berry sigma" character climbs out next to them (a no-AI
// husk the clients draw with our own model, `kotost_aura_edit`), and for six seconds the victim's
// screen is a hardtekk edit about it: a cinematic camera, beat zooms, shakes, captions word by word,
// a grey "MOGGED", flashes. The victim loses aura; killing the character wins it back (revenge).
//
// Server half: damage (`living_damage_post`, read only) and deaths are queued by the hooks and
// handled at the end of the server tick; characters are spawned and removed in `level_tick` for the
// level in place. Aura per player: config/kotost_aura_players.json. Settings: config/kotost_aura.txt
// (shared with the client half: flashes, volume, bpm, words).
use super::Game;
use horizon_client::mod_api::{NativeCommand, NativeServerPlayer, ServerTickPhase};
use horizon_client::text::Text;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const CH_EDIT: &str = "kotost_aura:edit";
pub const CH_CHAR: &str = "kotost_aura:char";
pub const CH_AURA: &str = "kotost_aura:aura";
/// The custom name the clients know the berry character by (and its name tag).
pub const BERRY_NAME: &str = "Ягода-сигма";
const CHAR_KIND: &str = "minecraft:husk";

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

static TICK: AtomicU64 = AtomicU64::new(0);
fn now() -> u64 {
    TICK.load(Ordering::Relaxed)
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

/// What hurt the player, by `damage_type`.
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

fn config_dir() -> std::path::PathBuf {
    horizon_client::logging::game_home().join("config")
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
            let _ = std::fs::create_dir_all(config_dir());
            let _ = std::fs::write(&path, DEFAULT_FILE);
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

#[derive(Clone)]
struct Cfg {
    enabled: bool,
    min_damage: f32,
    edit_ticks: u64,
    big_cooldown: u64,
    combo_max: u8,
    quiet_ticks: u64,
    char_ticks: u64,
    aura: HashMap<String, i64>,
}

fn load_cfg() -> Cfg {
    let c = read_config();
    let mut aura = HashMap::new();
    for t in THEMES.iter().chain(["death", "revenge"].iter()) {
        aura.insert((*t).to_owned(), num(&c, &format!("aura_{t}"), 0.0) as i64);
    }
    Cfg {
        enabled: num(&c, "enabled", 1.0) != 0.0,
        min_damage: num(&c, "min_damage", 0.5) as f32,
        edit_ticks: (num(&c, "edit_seconds", 6.0).clamp(1.0, 30.0) * 20.0) as u64,
        big_cooldown: (num(&c, "big_edit_cooldown", 20.0).clamp(0.0, 600.0) * 20.0) as u64,
        combo_max: num(&c, "combo_max", 5.0).clamp(1.0, 9.0) as u8,
        quiet_ticks: (num(&c, "quiet_seconds", 10.0).clamp(0.0, 600.0) * 20.0) as u64,
        char_ticks: (num(&c, "char_seconds", 30.0).clamp(5.0, 600.0) * 20.0) as u64,
        aura,
    }
}

// ===================================================================================================
// Server state
// ===================================================================================================

#[derive(Clone, Copy)]
struct Hit {
    entity: i32,
    theme: &'static str,
    amount: f32,
    test: bool,
}

enum Ev {
    Hit(Hit),
    Death { entity: u32, attacker: u32, player: bool },
    Joined(u128),
}

static EVENTS: Mutex<Vec<Ev>> = Mutex::new(Vec::new());

fn push(ev: Ev) {
    let mut e = lock(&EVENTS);
    if e.len() < 512 {
        e.push(ev);
    }
}

#[derive(Default)]
struct Per {
    name: String,
    aura: i64,
    off: bool,
    /// The tick the last edit ends.
    edit_until: u64,
    combo: u8,
    last_level: u64,
    big_ready: u64,
    quiet_until: u64,
}

struct Char {
    dim: String,
    owner: u128,
    until: u64,
}

struct Spawn {
    owner: u128,
    dim: String,
    at: [f64; 3],
}

struct Server {
    cfg: Cfg,
    cfg_mtime: Option<std::time::SystemTime>,
    players: HashMap<u128, Per>,
    chars: HashMap<u32, Char>,
    spawns: Vec<Spawn>,
    dirty: bool,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// Characters by entity id, for the damage and death hooks (which never lock SERVER).
static CHAR_IDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static MIN_DAMAGE: Mutex<f32> = Mutex::new(0.5);

fn save_path() -> std::path::PathBuf {
    config_dir().join("kotost_aura_players.json")
}

fn load_server() -> Server {
    let mut players = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(save_path())
        && let Ok(v) = serde_json::from_str::<Value>(&text)
        && let Some(map) = v.get("players").and_then(Value::as_object)
    {
        for (k, p) in map {
            if let Ok(uuid) = u128::from_str_radix(k, 16) {
                players.insert(
                    uuid,
                    Per {
                        name: p.get("name").and_then(Value::as_str).unwrap_or("").to_owned(),
                        aura: p.get("aura").and_then(Value::as_i64).unwrap_or(0),
                        off: p.get("off").and_then(Value::as_bool).unwrap_or(false),
                        ..Default::default()
                    },
                );
            }
        }
    }
    let cfg = load_cfg();
    *lock(&MIN_DAMAGE) = cfg.min_damage;
    Server { cfg, cfg_mtime: config_mtime(), players, chars: HashMap::new(), spawns: Vec::new(), dirty: false }
}

fn save(s: &mut Server) {
    if !s.dirty {
        return;
    }
    s.dirty = false;
    let mut map = serde_json::Map::new();
    for (uuid, p) in &s.players {
        map.insert(format!("{uuid:032x}"), json!({"name": p.name, "aura": p.aura, "off": p.off}));
    }
    let _ = std::fs::create_dir_all(config_dir());
    let tmp = save_path().with_extension("json.tmp");
    if std::fs::write(&tmp, json!({"players": map}).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, save_path());
    }
}

// ===================================================================================================
// Hooks (server)
// ===================================================================================================

pub fn register_commands(commands: &mut Vec<NativeCommand>) {
    commands.push(NativeCommand::new("aura"));
    commands.push(NativeCommand::new("aura off"));
    commands.push(NativeCommand::new("aura on"));
    commands.push(NativeCommand::op("aura test <theme:word>", 2));
    commands.push(NativeCommand::op("aura give <player:word> <count:integer>", 2));
}

pub fn perform_command(game: &mut Game<'_>, player: &NativeServerPlayer, command: &str, ran: &mut bool) {
    let mut words = command.split_whitespace();
    if words.next() != Some("aura") {
        return;
    }
    *ran = true;
    let rest: Vec<&str> = words.collect();
    let op = player.permission >= 2;
    let reply = {
        let mut guard = lock(&SERVER);
        let s = guard.get_or_insert_with(load_server);
        match rest.first().copied() {
            Some("off") => {
                let p = s.players.entry(player.uuid).or_default();
                p.off = true;
                p.name = player.name.clone();
                s.dirty = true;
                "Аура: эдиты выключены (/aura on, чтобы вернуть)".to_owned()
            }
            Some("on") => {
                let p = s.players.entry(player.uuid).or_default();
                p.off = false;
                p.name = player.name.clone();
                s.dirty = true;
                "Аура: эдиты включены".to_owned()
            }
            Some("test") if op => {
                let want = rest.get(1).copied().unwrap_or("berry");
                let theme = THEMES.iter().find(|t| **t == want).copied().unwrap_or("berry");
                push(Ev::Hit(Hit { entity: player.entity_id, theme, amount: 1.0, test: true }));
                format!("Аура: тест «{theme}»")
            }
            Some("give") if op => {
                let who = rest.get(1).copied().unwrap_or("");
                let n = rest.get(2).and_then(|x| x.parse::<i64>().ok()).unwrap_or(1000);
                let found = game.native_server_players().into_iter().find(|p| p.name.eq_ignore_ascii_case(who));
                match found {
                    Some(p) => {
                        let per = s.players.entry(p.uuid).or_default();
                        per.aura += n;
                        per.name = p.name.clone();
                        let total = per.aura;
                        s.dirty = true;
                        let _ = game.native_net_send_to_player(p.uuid, CH_AURA, json!({"a": total, "d": n}).to_string().as_bytes());
                        format!("Аура: {} {:+} (стало {})", p.name, n, total)
                    }
                    None => format!("Аура: игрока «{who}» нет"),
                }
            }
            _ => {
                let mine = s.players.get(&player.uuid).map_or(0, |p| p.aura);
                let mut top: Vec<(&String, i64)> = s.players.values().map(|p| (&p.name, p.aura)).collect();
                top.sort_by(|a, b| b.1.cmp(&a.1));
                let list: Vec<String> = top.iter().take(5).map(|(n, a)| format!("{n} {a}")).collect();
                let _ = game.native_net_send_to_player(player.uuid, CH_AURA, json!({"a": mine}).to_string().as_bytes());
                format!("Аура: {mine}. Топ: {}", list.join(", "))
            }
        }
    };
    let _ = game.native_tell(player.uuid, Text::literal(reply.as_str()));
}

/// `living_damage_post` (v1): only what came off the health (after the shield, the hurt cooldown
/// and every reduction; a fully blocked hit is 0 and costs nothing), queued for the tick's end.
pub fn incoming(entity: i32, damage_type: &str, _causing: i32, _direct: i32, amount: f32) {
    if !(amount >= *lock(&MIN_DAMAGE)) {
        return;
    }
    if lock(&CHAR_IDS).contains(&(entity as u32)) {
        return;
    }
    push(Ev::Hit(Hit { entity, theme: theme_of(damage_type), amount, test: false }));
}

pub fn living_death(entity: u32, kind: &str, _damage_type: &str, attacker: u32, _cancel: &mut bool) {
    let player = kind == "minecraft:player";
    if player || lock(&CHAR_IDS).contains(&entity) {
        push(Ev::Death { entity, attacker, player });
    }
}

pub fn joined(player: u128) {
    push(Ev::Joined(player));
}

pub fn server_tick_hook(game: &mut Game<'_>, phase: ServerTickPhase) {
    if matches!(phase, ServerTickPhase::ServerTickEnd { .. }) && game.lan_acting.is_none() {
        server_tick(game);
    }
}

fn send(game: &mut Game<'_>, uuid: u128, channel: &str, v: Value) {
    let _ = game.native_net_send_to_player(uuid, channel, v.to_string().as_bytes());
}

fn server_tick(game: &mut Game<'_>) {
    TICK.fetch_add(1, Ordering::Relaxed);
    let t = now();
    let events: Vec<Ev> = std::mem::take(&mut *lock(&EVENTS));
    let mut guard = lock(&SERVER);
    let s = guard.get_or_insert_with(load_server);
    if t % 100 == 1 {
        let m = config_mtime();
        if m != s.cfg_mtime {
            s.cfg_mtime = m;
            s.cfg = load_cfg();
            *lock(&MIN_DAMAGE) = s.cfg.min_damage;
        }
    }
    if events.is_empty() && t % 200 != 0 {
        return;
    }
    let players = game.native_server_players();
    let by_entity = |id: i32| players.iter().find(|p| p.entity_id == id);
    for ev in events {
        match ev {
            Ev::Joined(uuid) => {
                let total = s.players.get(&uuid).map_or(0, |p| p.aura);
                send(game, uuid, CH_AURA, json!({"a": total}));
            }
            Ev::Hit(hit) => {
                let Some(p) = by_entity(hit.entity) else { continue };
                if !hit.test && (!s.cfg.enabled || p.game_mode == 3) {
                    continue;
                }
                let cfg = s.cfg.clone();
                let per = s.players.entry(p.uuid).or_default();
                per.name = p.name.clone();
                if per.off || (!hit.test && t < per.quiet_until) {
                    continue;
                }
                if !hit.test && t.saturating_sub(per.last_level) < 20 {
                    // Fire and magma tick fast: a level a second at most; the edit runs on.
                    continue;
                }
                per.last_level = t;
                let running = t < per.edit_until;
                per.combo = if running { (per.combo + 1).min(cfg.combo_max) } else { 1 };
                let big = hit.test || t >= per.big_ready;
                if big {
                    per.big_ready = t + cfg.big_cooldown;
                }
                per.edit_until = t + cfg.edit_ticks;
                let maxed = per.combo >= cfg.combo_max && cfg.combo_max > 1;
                if maxed {
                    per.quiet_until = per.edit_until + cfg.quiet_ticks;
                }
                let delta = if hit.test { 0 } else { cfg.aura.get(hit.theme).copied().unwrap_or(0) };
                per.aura += delta;
                let total = per.aura;
                let combo = per.combo;
                if maxed {
                    per.combo = 0;
                }
                s.dirty |= delta != 0;
                let _ = hit.amount;
                send(
                    game,
                    p.uuid,
                    CH_EDIT,
                    json!({"t": hit.theme, "c": combo, "m": maxed, "b": big, "d": delta, "a": total, "s": rand_u32()}),
                );
                if big && hit.theme == "berry" && s.spawns.len() < 16 {
                    // In front of the player, two blocks along their look, at their feet.
                    let yaw = f64::from(p.yaw).to_radians();
                    let at = [p.position[0] - yaw.sin() * 2.2, p.position[1], p.position[2] + yaw.cos() * 2.2];
                    s.spawns.push(Spawn { owner: p.uuid, dim: p.dimension.clone(), at });
                }
            }
            Ev::Death { entity, attacker, player } => {
                if player {
                    let Some(p) = by_entity(entity as i32) else { continue };
                    let delta = s.cfg.aura.get("death").copied().unwrap_or(0);
                    let per = s.players.entry(p.uuid).or_default();
                    per.aura += delta;
                    per.edit_until = 0;
                    per.combo = 0;
                    let total = per.aura;
                    s.dirty = true;
                    send(game, p.uuid, CH_AURA, json!({"a": total, "d": delta}));
                    continue;
                }
                let Some(c) = s.chars.remove(&entity) else { continue };
                lock(&CHAR_IDS).retain(|id| *id != entity);
                let Some(killer) = (attacker != 0).then(|| by_entity(attacker as i32)).flatten() else { continue };
                let delta = s.cfg.aura.get("revenge").copied().unwrap_or(0);
                let per = s.players.entry(killer.uuid).or_default();
                per.aura += delta;
                let total = per.aura;
                let off = per.off;
                s.dirty = true;
                let own = c.owner == killer.uuid;
                // The balance always; the edit only for a player who has not turned it off.
                if off {
                    send(game, killer.uuid, CH_AURA, json!({"a": total, "d": delta}));
                } else {
                    send(
                        game,
                        killer.uuid,
                        CH_EDIT,
                        json!({"t": "revenge", "c": 1, "m": false, "b": false, "d": delta, "a": total, "s": rand_u32(), "own": own}),
                    );
                }
            }
        }
    }
    if t % 200 == 0 {
        save(s);
    }
}

/// `level_tick_post`: the characters of this level, spawned and sent home.
pub fn level_tick(game: &mut Game<'_>, dimension: &str) {
    let t = now();
    let mut guard = lock(&SERVER);
    let Some(s) = guard.as_mut() else { return };
    if s.spawns.is_empty() && s.chars.is_empty() {
        return;
    }
    let (here, rest): (Vec<Spawn>, Vec<Spawn>) = std::mem::take(&mut s.spawns).into_iter().partition(|x| x.dim == dimension);
    s.spawns = rest;
    for sp in here {
        let opts = super::native_mobs::NativeSpawn {
            name: Some(BERRY_NAME.to_owned()),
            health: Some(20.0),
            no_ai: true,
            silent: true,
            ..Default::default()
        };
        let Some(id) = game.native_spawn_mob(dimension, CHAR_KIND, sp.at, &opts) else { continue };
        if let Some(m) = game.mobs.iter_mut().find(|m| m.id == id) {
            m.persistent = true;
            m.ai.target = None;
        }
        s.chars.insert(id, Char { dim: dimension.to_owned(), owner: sp.owner, until: t + s.cfg.char_ticks });
        lock(&CHAR_IDS).push(id);
        let _ = game.native_net_send_to_player(sp.owner, CH_CHAR, json!({"id": id}).to_string().as_bytes());
    }
    let gone: Vec<u32> = s.chars.iter().filter(|(_, c)| c.dim == dimension && t >= c.until).map(|(id, _)| *id).collect();
    for id in gone {
        s.chars.remove(&id);
        lock(&CHAR_IDS).retain(|x| *x != id);
        let _ = game.native_remove_mob(dimension, id);
    }
}
