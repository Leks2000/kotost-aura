//! The server half: damage (read only) and deaths are queued by the hooks and handled at the end of the
//! server tick, where the characters are spawned and sent home. Aura per player in
//! `config/kotost_aura_players.json`.
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use mod_api::serde_json::{self, Value, json};
use mod_api::{
    Command, CommandArgumentValue, CommandSource, EntityRef, Flow, GameMode, Host, LevelRef, MobRef, PayloadBytes,
    PayloadId, PermissionLevel, PlayerRef, RunModCommandEvent, ServerHost, SpawnReason, Text,
};

use crate::{BERRY_NAME, CHANNEL, THEMES, config_dir, config_mtime, lock, num, rand_u32, read_config, theme_of};

const CHAR_KIND: &str = "minecraft:husk";

static TICK: AtomicU64 = AtomicU64::new(0);
fn now() -> u64 {
    TICK.load(Ordering::Relaxed)
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

#[derive(Clone, Copy)]
struct Hit {
    entity: i32,
    theme: &'static str,
    test: bool,
}

enum Ev {
    Hit(Hit),
    Death { entity: u32, attacker: i32, player: bool },
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
    owner: u128,
    until: u64,
}

struct Spawn {
    owner: u128,
    level: LevelRef,
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

/// What the server knows of a player, read once per use.
struct Seen {
    player: PlayerRef,
    uuid: u128,
    entity: u32,
    name: String,
    spectator: bool,
    position: [f64; 3],
    yaw: f32,
    level: Option<LevelRef>,
}

fn seen(host: &dyn ServerHost, player: PlayerRef) -> Seen {
    let reader: &dyn Host = host;
    let e = player.entity();
    Seen {
        player,
        uuid: player.uuid(),
        entity: e.id(),
        name: player.name(reader).unwrap_or_default(),
        spectator: matches!(player.game_mode(reader), Some(GameMode::Spectator)),
        position: e.position(reader).unwrap_or([0.0; 3]),
        yaw: e.rotation(reader).map_or(0.0, |r| r[0]),
        level: player.level(reader),
    }
}

fn send(host: &mut dyn ServerHost, player: PlayerRef, v: Value) {
    let _ = player.send_payload(host, &PayloadId(CHANNEL.into()), PayloadBytes(v.to_string().into_bytes()));
}

// ===================================================================================================
// Hooks
// ===================================================================================================

pub fn register_commands(commands: &mut Vec<Command>) {
    commands.push(Command::new("aura"));
    commands.push(Command::new("aura off"));
    commands.push(Command::new("aura on"));
    commands.push(Command::op("aura test <theme:word>", 2));
    commands.push(Command::op("aura give <player:word> <count:integer>", 2));
}

pub fn run_command(host: &mut dyn ServerHost, event: &mut RunModCommandEvent<'_>) -> Flow {
    let inv = event.command;
    if inv.literals.first().map(String::as_str) != Some("aura") {
        return Flow::Continue;
    }
    let CommandSource::Player { player, permission } = event.source.clone() else {
        let _ = host.commands().feedback(Text::literal("Аура: только для игроков"));
        return Flow::Cancel;
    };
    let me = seen(host, player);
    let op = matches!(permission, PermissionLevel::Gamemasters | PermissionLevel::Admins | PermissionLevel::Owners);
    let word = |name: &str| match inv.argument(name) {
        Some(CommandArgumentValue::String(s)) => Some(s.clone()),
        _ => None,
    };
    let sub = inv.literals.get(1).map(String::as_str);
    let mut sends: Vec<(PlayerRef, Value)> = Vec::new();
    let reply = {
        let mut guard = lock(&SERVER);
        let s = guard.get_or_insert_with(load_server);
        match sub {
            Some("off") => {
                let p = s.players.entry(me.uuid).or_default();
                p.off = true;
                p.name = me.name.clone();
                s.dirty = true;
                "Аура: эдиты выключены (/aura on, чтобы вернуть)".to_owned()
            }
            Some("on") => {
                let p = s.players.entry(me.uuid).or_default();
                p.off = false;
                p.name = me.name.clone();
                s.dirty = true;
                "Аура: эдиты включены".to_owned()
            }
            Some("test") if op => {
                let want = word("theme").unwrap_or_else(|| "berry".to_owned());
                let theme = THEMES.iter().find(|t| **t == want).copied().unwrap_or("berry");
                push(Ev::Hit(Hit { entity: me.entity as i32, theme, test: true }));
                format!("Аура: тест «{theme}»")
            }
            Some("give") if op => {
                let who = word("player").unwrap_or_default();
                let n = match inv.argument("count") {
                    Some(CommandArgumentValue::Integer(n)) => i64::from(*n),
                    _ => 1000,
                };
                let found = host.players().server_players().into_iter().find(|p| {
                    let reader: &dyn Host = &*host;
                    p.name(reader).is_some_and(|name| name.eq_ignore_ascii_case(&who))
                });
                match found {
                    Some(p) => {
                        let target = seen(host, p);
                        let per = s.players.entry(target.uuid).or_default();
                        per.aura += n;
                        per.name = target.name.clone();
                        let total = per.aura;
                        s.dirty = true;
                        sends.push((p, json!({"k": "aura", "a": total, "d": n})));
                        format!("Аура: {} {:+} (стало {})", target.name, n, total)
                    }
                    None => format!("Аура: игрока «{who}» нет"),
                }
            }
            _ => {
                let mine = s.players.get(&me.uuid).map_or(0, |p| p.aura);
                let mut top: Vec<(&String, i64)> = s.players.values().map(|p| (&p.name, p.aura)).collect();
                top.sort_by(|a, b| b.1.cmp(&a.1));
                let list: Vec<String> = top.iter().take(5).map(|(n, a)| format!("{n} {a}")).collect();
                sends.push((player, json!({"k": "aura", "a": mine})));
                format!("Аура: {mine}. Топ: {}", list.join(", "))
            }
        }
    };
    for (p, v) in sends {
        send(host, p, v);
    }
    let _ = me.player.tell(host, Text::literal(reply.as_str()));
    Flow::Cancel
}

/// `living_incoming_damage`: read only, queued for the tick's end.
pub fn incoming(entity: i32, damage_type: &str, amount: f32) {
    if amount < *lock(&MIN_DAMAGE) {
        return;
    }
    if lock(&CHAR_IDS).contains(&(entity as u32)) {
        return;
    }
    push(Ev::Hit(Hit { entity, theme: theme_of(damage_type), test: false }));
}

pub fn living_death(entity: u32, kind: &str, causing: i32) {
    let player = kind == "minecraft:player";
    if player || lock(&CHAR_IDS).contains(&entity) {
        push(Ev::Death { entity, attacker: causing, player });
    }
}

pub fn joined(player: u128) {
    push(Ev::Joined(player));
}

pub fn server_tick(host: &mut dyn ServerHost) {
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
    let has_chars = !s.spawns.is_empty() || !s.chars.is_empty();
    if events.is_empty() && !has_chars && t % 200 != 0 {
        return;
    }
    let players: Vec<Seen> = host.players().server_players().into_iter().map(|p| seen(host, p)).collect();
    let by_entity = |id: i32| players.iter().find(|p| p.entity as i32 == id);
    let mut sends: Vec<(PlayerRef, Value)> = Vec::new();
    for ev in events {
        match ev {
            Ev::Joined(uuid) => {
                let total = s.players.get(&uuid).map_or(0, |p| p.aura);
                if let Some(p) = players.iter().find(|p| p.uuid == uuid) {
                    sends.push((p.player, json!({"k": "aura", "a": total})));
                }
            }
            Ev::Hit(hit) => {
                let Some(p) = by_entity(hit.entity) else { continue };
                if !hit.test && (!s.cfg.enabled || p.spectator) {
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
                sends.push((
                    p.player,
                    json!({"k": "edit", "t": hit.theme, "c": combo, "m": maxed, "b": big, "d": delta, "a": total, "s": rand_u32()}),
                ));
                if big
                    && hit.theme == "berry"
                    && s.spawns.len() < 16
                    && let Some(level) = p.level
                {
                    // In front of the player, two blocks along their look, at their feet.
                    let yaw = f64::from(p.yaw).to_radians();
                    let at = [p.position[0] - yaw.sin() * 2.2, p.position[1], p.position[2] + yaw.cos() * 2.2];
                    s.spawns.push(Spawn { owner: p.uuid, level, at });
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
                    sends.push((p.player, json!({"k": "aura", "a": total, "d": delta})));
                    continue;
                }
                let Some(c) = s.chars.remove(&entity) else { continue };
                lock(&CHAR_IDS).retain(|id| *id != entity);
                let Some(killer) = (attacker > 0).then(|| by_entity(attacker)).flatten() else { continue };
                let delta = s.cfg.aura.get("revenge").copied().unwrap_or(0);
                let per = s.players.entry(killer.uuid).or_default();
                per.aura += delta;
                let total = per.aura;
                s.dirty = true;
                let own = c.owner == killer.uuid;
                sends.push((
                    killer.player,
                    json!({"k": "edit", "t": "revenge", "c": 1, "m": false, "b": false, "d": delta, "a": total, "s": rand_u32(), "own": own}),
                ));
            }
        }
    }
    // The characters: spawned where the player stands, sent home when their time is up.
    for sp in std::mem::take(&mut s.spawns) {
        let Some(e) = sp.level.spawn_mob(host, CHAR_KIND, sp.at, SpawnReason::Triggered) else { continue };
        let mob = MobRef::new(e);
        mob.set_no_ai(host, true);
        mob.set_persistence_required(host);
        e.set_silent(host, true);
        let _ = e.set_custom_name(host, Some(Text::literal(BERRY_NAME)));
        let id = e.id();
        s.chars.insert(id, Char { owner: sp.owner, until: t + s.cfg.char_ticks });
        lock(&CHAR_IDS).push(id);
        if let Some(p) = players.iter().find(|p| p.uuid == sp.owner) {
            sends.push((p.player, json!({"k": "char", "id": id})));
        }
    }
    let gone: Vec<u32> = s.chars.iter().filter(|(_, c)| t >= c.until).map(|(id, _)| *id).collect();
    for id in gone {
        s.chars.remove(&id);
        lock(&CHAR_IDS).retain(|x| *x != id);
        if let Some(e) = host.entity(id) {
            let _: EntityRef = e;
            e.discard(host);
        }
    }
    if t % 200 == 0 {
        save(s);
    }
    drop(guard);
    for (p, v) in sends {
        send(host, p, v);
    }
}
