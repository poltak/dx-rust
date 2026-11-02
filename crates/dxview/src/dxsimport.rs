//! Converts a saved game of the original Deus Ex (a Save/SaveNNNN folder of .dxs packages) into one of ours.
//!
//! `dxview <game folder> --import-save <save folder> [out.ron]`
//!
//! A .dxs file is an Unreal package: every object of the map as it was saved, each one holding only what differs from
//! its class defaults. So a value is the saved one, else the class default, and map actors keep the names they have in
//! the map file, which is how they're matched to `Level::actors`.
//!
//! Comes across: the player, inventory, skills, augmentations, flags, goals, notes, images, conversation history,
//! nanokeys; on each map which actors still exist, moved decorations, living pawns, doors and triggers.
//! Doesn't: bodies and items spawned in play (no map actor to match), alliances changed in play, hacked terminals,
//! security systems. Only reads the save folder, and never replaces a file.

use crate::ai::Orders;
use crate::consys::{ConSave, Goal, History};
use crate::save::{AugSave, Global, LevelSave, MoverSave, NpcSave, PlayerSave, SaveGame, VERSION};
use crate::triggers::TrigSave;
use crate::weapons::ModState;
use dx_level::actors::ActorInfo;
use dx_level::Level;
use imsim_core::flags::{FlagBase, FlagValue};
use imsim_core::inventory::{Inventory, Item, ItemDef};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use unreal_pkg::classes::{overlay, ClassDb};
use unreal_pkg::props::{self, find, Property, Value};
use unreal_pkg::{Package, Reader};

/// Class defaults and ancestry, kept: a map has thousands of objects of a few hundred classes.
struct Classes {
    db: ClassDb,
    defaults: HashMap<String, Vec<Property>>,
    ancestry: HashMap<String, Vec<String>>,
}

impl Classes {
    fn defaults(&mut self, class: &str) -> &[Property] {
        let db = &mut self.db;
        self.defaults.entry(class.to_string()).or_insert_with(|| db.defaults(class))
    }

    fn is_a(&mut self, class: &str, base: &str) -> bool {
        if class.eq_ignore_ascii_case(base) {
            return true;
        }
        let db = &mut self.db;
        self.ancestry.entry(class.to_string()).or_insert_with(|| db.ancestry(class)).iter().any(|c| c.eq_ignore_ascii_case(base))
    }
}

/// One saved object: `saved` holds object references, `all` the saved values over the class defaults.
struct Obj {
    class: String,
    saved: Vec<Property>,
    all: Vec<Property>,
}

struct Dxs {
    pkg: Package,
    /// Lower-cased object name -> object index.
    names: HashMap<String, i32>,
}

impl Dxs {
    fn open(path: &Path) -> Result<Dxs, String> {
        let pkg = Package::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let names = pkg.exports.iter().enumerate().map(|(i, e)| (e.name.to_ascii_lowercase(), i as i32 + 1)).collect();
        Ok(Dxs { pkg, names })
    }

    fn class(&self, o: i32) -> String {
        if o <= 0 {
            return String::new();
        }
        self.pkg.class_name(self.pkg.exports[o as usize - 1].class_index).to_string()
    }

    fn props(&self, o: i32) -> Vec<Property> {
        if o <= 0 {
            return Vec::new();
        }
        let e = &self.pkg.exports[o as usize - 1];
        let mut r = Reader::new(self.pkg.export_data(e), 0);
        if e.flags & props::RF_HAS_STACK != 0 && props::skip_state_frame(&mut r).is_err() {
            return Vec::new();
        }
        props::read_properties(&self.pkg, &mut r).unwrap_or_default()
    }

    fn obj(&self, cl: &mut Classes, o: i32) -> Obj {
        let class = self.class(o);
        let saved = self.props(o);
        let mut all = cl.defaults(&class).to_vec();
        overlay(&mut all, saved.clone());
        Obj { class, saved, all }
    }

    /// The object has been destroyed: killed pawns, smashed crates and the like stay in the package, marked.
    fn destroyed(&self, o: i32) -> bool {
        flag(&self.props(o), "bDeleteMe", false)
    }

    /// The saved object of a map actor that still exists: same name, same class, not destroyed.
    fn actor(&self, a: &ActorInfo) -> Option<i32> {
        let o = *self.names.get(&a.name.to_ascii_lowercase())?;
        (self.class(o).eq_ignore_ascii_case(&a.class) && !self.destroyed(o)).then_some(o)
    }

    /// Objects linked one to the next by `next` (a guard stops a broken save looping).
    fn chain(&self, first: i32, next: &str) -> Vec<i32> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut o = first;
        while o > 0 && seen.insert(o) {
            out.push(o);
            o = link(&self.props(o), next);
        }
        out
    }

    fn objects_of(&self, class: &str) -> Vec<i32> {
        (1..=self.pkg.exports.len() as i32).filter(|&o| self.class(o).eq_ignore_ascii_case(class)).collect()
    }
}

fn link(p: &[Property], n: &str) -> i32 {
    match find(p, n) {
        Some(Value::Object(o)) => *o,
        _ => 0,
    }
}
fn int(p: &[Property], n: &str, or: i32) -> i32 {
    match find(p, n) {
        Some(Value::Int(v)) => *v,
        Some(Value::Byte(v)) => *v as i32,
        _ => or,
    }
}
fn float(p: &[Property], n: &str, or: f32) -> f32 {
    match find(p, n) {
        Some(Value::Float(v)) => *v,
        Some(Value::Int(v)) => *v as f32,
        _ => or,
    }
}
fn flag(p: &[Property], n: &str, or: bool) -> bool {
    match find(p, n) {
        Some(Value::Bool(v)) => *v,
        _ => or,
    }
}
fn text(p: &[Property], n: &str) -> String {
    match find(p, n) {
        Some(Value::Str(v)) | Some(Value::Name(v)) => v.clone(),
        _ => String::new(),
    }
}
fn vector(p: &[Property], n: &str) -> [f64; 3] {
    match find(p, n) {
        Some(Value::Vector(v)) => [v[0] as f64, v[1] as f64, v[2] as f64],
        _ => [0.0; 3],
    }
}
fn rotator(p: &[Property], n: &str) -> [i32; 3] {
    match find(p, n) {
        Some(Value::Rotator(v)) => *v,
        _ => [0; 3],
    }
}

/// DeusExWeapon.AmmoLeftInClip: ClipCount counts the rounds fired from this clip, and AmmoAmount includes the clip.
fn clip_rounds(reload_count: i32, fired: i32, pool: u32) -> u32 {
    ((reload_count - fired).max(0) as u32).min(pool)
}

/// ViewRotation (pitch, yaw in Unreal units) as our camera's angles: radians, yaw measured the way main.rs does.
fn camera_angles(view: [i32; 3]) -> (f32, f32) {
    let signed = |v: i32| {
        let v = v.rem_euclid(65536);
        if v >= 32768 {
            v - 65536
        } else {
            v
        }
    };
    let deg = |v: i32| v as f32 * 360.0 / 65536.0;
    let yaw = deg(view[1].rem_euclid(65536));
    ((-yaw - 90.0).to_radians(), deg(signed(view[0])).to_radians())
}

/// Saved orders ours has too, as Npc::set_orders reads them. The rest (Attacking, Leaving, Idle...) are reactions or
/// the work of scripts, and a pawn loaded with them goes back to the map's orders.
fn orders(name: &str) -> Option<Orders> {
    Some(match name.to_ascii_lowercase().as_str() {
        "standing" => Orders::Standing,
        "patrolling" => Orders::Patrolling,
        "wandering" => Orders::Wandering,
        "sitting" => Orders::Sitting,
        "runningto" | "following" => Orders::RunningTo,
        "goingto" => Orders::GoingTo,
        "waitingfor" => Orders::WaitingFor,
        _ => return None,
    })
}

fn body_parts(p: &[Property], health: f32) -> [f32; 6] {
    ["HealthHead", "HealthTorso", "HealthArmLeft", "HealthArmRight", "HealthLegLeft", "HealthLegRight"].map(|n| float(p, n, health))
}

// ------------------------------------------------------------------------------------------------ the player

fn flags(s: &Dxs, cl: &mut Classes, base: i32, skipped: &mut Vec<String>) -> FlagBase {
    let mut out = FlagBase::default();
    out.set_default_expiration(int(&s.props(base), "defaultFlagExpiration", 0));
    for class in ["FlagBool", "FlagInt", "FlagByte", "FlagName", "FlagFloat", "FlagVector", "FlagRotator"] {
        for o in s.objects_of(class) {
            let f = s.obj(cl, o);
            if link(&f.saved, "FlagBase") != base {
                continue;
            }
            let name = text(&f.all, "FlagName");
            let value = match class {
                "FlagBool" => FlagValue::Bool(flag(&f.all, "bValue", false)),
                "FlagInt" => FlagValue::Int(int(&f.all, "intValue", 0)),
                "FlagByte" => FlagValue::Int(int(&f.all, "byteValue", 0)),
                "FlagName" => FlagValue::Name(text(&f.all, "nameValue")),
                _ => {
                    skipped.push(format!("flag {name} ({class})"));
                    continue;
                }
            };
            out.set(&name, value, int(&f.all, "expiration", 0));
        }
    }
    out
}

/// What the player carries: the inventory, rounds loaded, ammo types loaded and weapon mods.
struct Carried {
    inventory: Inventory,
    clips: Vec<(String, u32)>,
    loaded_ammo: Vec<(String, String)>,
    mods: Vec<(String, ModState)>,
}

fn carried(s: &Dxs, cl: &mut Classes, player: &Obj, skipped: &mut Vec<String>) -> Carried {
    let mut inv = Inventory { credits: int(&player.all, "Credits", 0).max(0) as u32, ..Default::default() };
    let mut pool: BTreeMap<String, u32> = BTreeMap::new();
    let mut weapons: Vec<Obj> = Vec::new();
    for o in s.chain(link(&player.saved, "Inventory"), "Inventory") {
        let it = s.obj(cl, o);
        let class = it.class.clone();
        if cl.is_a(&class, "Ammo") {
            if class != "AmmoNone" {
                *pool.entry(class).or_default() += int(&it.all, "AmmoAmount", 0).max(0) as u32;
            }
            continue;
        }
        if class == "NanoKeyRing" || cl.is_a(&class, "DataVaultImage") {
            continue;
        }
        if !cl.is_a(&class, "Inventory") {
            skipped.push(format!("carried {class}"));
            continue;
        }
        let def: ItemDef = crate::pickups::item_def(&class, cl.defaults(&class));
        let uid = if inv.items.iter().any(|i| i.def.class == class) {
            inv.next_uid += 1;
            inv.next_uid
        } else {
            0
        };
        let (x, y) = (int(&it.all, "invPosX", -1), int(&it.all, "invPosY", -1));
        let mut augs: Vec<_> = it.saved.iter().filter(|p| p.name.eq_ignore_ascii_case("AddAugs")).collect();
        augs.sort_by_key(|p| p.index);
        let data = augs
            .iter()
            .filter_map(|p| match &p.value {
                Value::Name(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        inv.items.push(Item {
            def,
            count: int(&it.all, "NumCopies", 1).max(1) as u32,
            pos: (x >= 0 && y >= 0).then_some((x as u32, y as u32)),
            uid,
            data,
        });
        let slot = int(&it.all, "beltPos", -1);
        if (0..10).contains(&slot) {
            inv.belt[slot as usize] = Some(inv.items.len() - 1);
        }
        if cl.is_a(&class, "DeusExWeapon") {
            weapons.push(it);
        }
    }

    let (mut clips, mut loaded_ammo, mut mods) = (Vec::new(), Vec::new(), Vec::new());
    for w in &weapons {
        let d = cl.defaults(&w.class).to_vec();
        if let Some(i) = find(&w.saved, "AmmoName").and(Some(link(&w.saved, "AmmoName"))).filter(|i| *i != 0) {
            loaded_ammo.push((w.class.clone(), s.pkg.object_name(i)));
        }
        let m = ModState {
            accuracy: float(&w.all, "ModBaseAccuracy", 0.0),
            clip: float(&w.all, "ModReloadCount", 0.0),
            range: float(&w.all, "ModAccurateRange", 0.0),
            reload: float(&w.all, "ModReloadTime", 0.0),
            recoil: float(&w.all, "ModRecoilStrength", 0.0),
            scope: flag(&w.all, "bHasScope", false) && !flag(&d, "bHasScope", false),
            silencer: flag(&w.all, "bHasSilencer", false) && !flag(&d, "bHasSilencer", false),
            laser: flag(&w.all, "bHasLaser", false) && !flag(&d, "bHasLaser", false),
        };
        if m.accuracy != 0.0 || m.clip != 0.0 || m.range != 0.0 || m.reload != 0.0 || m.recoil != 0.0 || m.scope || m.silencer || m.laser {
            mods.push((w.class.clone(), m));
        }
        // Ours keeps loaded rounds out of the ammo pool: a reload takes them from it.
        let reload_count = int(&w.all, "ReloadCount", 0);
        if reload_count <= 0 || flag(&w.all, "bHandToHand", false) || clips.iter().any(|(c, _)| *c == w.class) {
            continue;
        }
        let ammo = match link(&w.saved, "AmmoName") {
            0 => match find(&d, "AmmoName") {
                Some(Value::Path(p)) => p.rsplit('.').next().unwrap_or(p).to_string(),
                _ => continue,
            },
            i => s.pkg.object_name(i),
        };
        let Some(left) = pool.get_mut(&ammo) else { continue };
        let rounds = clip_rounds(reload_count, int(&w.all, "ClipCount", 0), *left);
        *left -= rounds;
        clips.push((w.class.clone(), rounds));
    }
    inv.ammo = pool.into_iter().collect();
    Carried { inventory: inv, clips, loaded_ammo, mods }
}

fn conversations(s: &Dxs, player: &Obj) -> ConSave {
    let goals = s
        .chain(link(&player.saved, "FirstGoal"), "next")
        .into_iter()
        .map(|o| {
            let p = s.props(o);
            Goal {
                name: text(&p, "goalName"),
                text: text(&p, "Text"),
                primary: flag(&p, "bPrimaryGoal", false),
                completed: flag(&p, "bCompleted", false),
            }
        })
        .collect();
    let notes = s.chain(link(&player.saved, "FirstNote"), "next").into_iter().map(|o| text(&s.props(o), "Text")).collect();
    let images = s.chain(link(&player.saved, "FirstImage"), "nextImage").into_iter().map(|o| s.class(o)).collect();
    let history = s
        .chain(link(&player.saved, "ConHistory"), "next")
        .into_iter()
        .map(|o| {
            let p = s.props(o);
            let lines = s
                .chain(link(&p, "firstEvent"), "next")
                .into_iter()
                .map(|e| {
                    let l = s.props(e);
                    (text(&l, "conSpeaker"), text(&l, "Speech"))
                })
                .collect();
            History {
                owner: text(&p, "conOwnerName"),
                location: text(&p, "strLocation"),
                description: text(&p, "strDescription"),
                infolink: flag(&p, "bInfoLink", false),
                lines,
            }
        })
        .collect();
    ConSave { goals, notes, images, history }
}

fn global(s: &Dxs, cl: &mut Classes, player: &Obj, skipped: &mut Vec<String>) -> (Global, Vec<(String, u32)>) {
    let p = &player.all;
    let c = carried(s, cl, player, skipped);
    let view = rotator(p, "ViewRotation");
    let (yaw, pitch) = camera_angles(view);
    let hp = float(p, "Health", 100.0);
    let name = text(p, "TruePlayerName");
    let skills = s
        .chain(link(&s.props(link(&player.saved, "SkillSystem")), "FirstSkill"), "Next")
        .into_iter()
        .map(|o| {
            let k = s.obj(cl, o);
            (k.class, int(&k.all, "CurrentLevel", 0).max(0) as usize)
        })
        .collect();
    let augs = s
        .chain(link(&s.props(link(&player.saved, "AugmentationSystem")), "FirstAug"), "next")
        .into_iter()
        .map(|o| {
            let a = s.obj(cl, o);
            AugSave {
                has: flag(&a.all, "bHasIt", false),
                active: flag(&a.all, "bIsActive", false),
                level: int(&a.all, "CurrentLevel", 0).max(0) as usize,
                boosted: flag(&a.all, "bBoosted", false),
                hotkey: int(&a.all, "HotKeyNum", 0).max(0),
                class: a.class,
            }
        })
        .collect();
    // The ring's chain is newest first; ours appends.
    let mut keys: Vec<(String, String)> = s
        .chain(link(&player.saved, "KeyList"), "NextKey")
        .into_iter()
        .map(|o| {
            let k = s.props(o);
            (text(&k, "KeyID"), text(&k, "Description"))
        })
        .collect();
    keys.reverse();
    let held = match link(&player.saved, "inHand") {
        0 => None,
        o => Some(s.class(o)).filter(|c| c != "NanoKeyRing" && c.starts_with("Weapon") || c == "Binoculars"),
    };
    let g = Global {
        name: if name.is_empty() { "JC Denton".to_string() } else { name },
        skin: int(p, "PlayerSkin", 0).max(0) as u8,
        difficulty: float(p, "CombatDifficulty", 1.0),
        female: player.class == "JCDentonFemale" || flag(p, "bMadeFemale", false),
        retro_morpheus: flag(p, "bRetroMorpheus", false),
        male_interactions: flag(p, "bFemaleUsesMaleInteractions", false),
        player: PlayerSave {
            location: vector(p, "Location"),
            yaw,
            pitch,
            crouched: flag(p, "bIsCrouching", false),
            health: body_parts(p, hp),
            hp,
            energy: float(p, "Energy", 100.0),
            poison: (int(p, "poisonCounter", 0).max(0) as u32, float(p, "poisonTimer", 0.0), float(p, "poisonDamage", 0.0)),
            drug_timer: float(p, "drugEffectTimer", 0.0),
        },
        inventory: c.inventory,
        clips: c.clips.clone(),
        loaded_ammo: c.loaded_ammo,
        mods: c.mods,
        held,
        skills,
        points: (int(p, "SkillPointsAvail", 0), int(p, "SkillPointsTotal", 0)),
        augs,
        flags: flags(s, cl, link(&player.saved, "FlagBase"), skipped),
        keys,
        con: conversations(s, player),
        ..Default::default()
    };
    (g, c.clips)
}

// ------------------------------------------------------------------------------------------------ a map

/// Saved orders and where they lead. No goal means the player, as a pawn's order with no tag does in the original
/// (FindTaggedActor) and in ours. The target is the saved actor, not its tag: a PatrolPoint isn't in Level::actors. It
/// is taken as a fixed spot, which is also what Npc::set_orders does with a target that moves.
fn order_target(s: &Dxs, ordered: Option<Orders>, tag: &str, target: i32) -> (Option<Orders>, Option<[f64; 3]>) {
    if target > 0 && !s.destroyed(target) {
        let player = s.class(target).starts_with("JCDenton");
        return (ordered, (!player).then(|| vector(&s.props(target), "Location")));
    }
    match ordered {
        // The target has gone: state RunningTo ends in Standing there. Ours would run to the player instead.
        Some(Orders::RunningTo | Orders::GoingTo) if !tag.is_empty() => (Some(Orders::Standing), None),
        _ => (ordered, None),
    }
}

fn npc(s: &Dxs, cl: &mut Classes, actor: usize, a: &ActorInfo, o: i32) -> NpcSave {
    let p = s.obj(cl, o);
    let (mut arsenal, mut ammo_left) = (Vec::new(), Vec::new());
    for i in s.chain(link(&p.saved, "Inventory"), "Inventory") {
        let it = s.obj(cl, i);
        if cl.is_a(&it.class, "Ammo") {
            ammo_left.push((it.class.clone(), int(&it.all, "AmmoAmount", 0)));
        } else if cl.is_a(&it.class, "DeusExWeapon") {
            arsenal.push(it.class);
        }
    }
    let health = float(&p.all, "Health", 50.0);
    let use_home = flag(&p.all, "bUseHome", false);
    let start = a.location();
    let start = [start[0] as f64, start[1] as f64, start[2] as f64];
    let in_world = flag(&p.all, "bInWorld", true);
    let turn = std::f64::consts::TAU / 65536.0;
    let ordered = orders(&text(&p.all, "Orders"));
    let order_tag = text(&p.all, "OrderTag");
    let (ordered_now, order_goal) = order_target(s, ordered, &order_tag, link(&p.saved, "OrderActor"));
    let location = vector(&p.all, "Location");
    NpcSave {
        actor,
        location,
        yaw: rotator(&p.all, "Rotation")[1] as f64 * turn,
        hidden: flag(&p.all, "bHidden", false) && in_world,
        in_world,
        parts: body_parts(&p.all, health),
        health,
        alliance: text(&p.all, "Alliance"),
        orders: ordered_now,
        order_tag,
        order_goal,
        // As Npc::set_orders: orders that end in Standing stand here.
        home: if ordered_now != ordered {
            location
        } else if use_home {
            vector(&p.all, "HomeLoc")
        } else {
            start
        },
        home_yaw: rotator(&a.props, "Rotation")[1] as f64 * turn,
        use_home: true,
        arsenal,
        ammo_left,
        ..Default::default()
    }
}

fn mover(s: &Dxs, cl: &mut Classes, index: usize, info: &dx_level::movers::MoverInfo, actor: &ActorInfo) -> MoverSave {
    let at = [info.location[0] as f64, info.location[1] as f64, info.location[2] as f64];
    let Some(o) = s.actor(actor) else {
        return MoverSave { index, location: at, rotation: info.rotation, key_num: info.key_num, prev_key_num: info.key_num, destroyed: true, ..Default::default() };
    };
    let m = s.obj(cl, o);
    let key = int(&m.all, "KeyNum", 0) as u8;
    let moving = flag(&m.all, "bInterpolating", false);
    let k = (key as usize).min(7);
    let (location, rotation) = if moving {
        // As a save of ours: a door caught moving is put where it's heading.
        let at = |c: usize| info.base_pos[c] as f64 + info.key_pos[k][c] as f64;
        ([at(0), at(1), at(2)], std::array::from_fn(|c| info.base_rot[c] + info.key_rot[k][c]))
    } else {
        (vector(&m.all, "Location"), rotator(&m.all, "Rotation"))
    };
    MoverSave {
        index,
        location,
        rotation,
        key_num: key,
        prev_key_num: if moving { key } else { int(&m.all, "PrevKeyNum", 0) as u8 },
        opening: flag(&m.all, "bOpening", false),
        locked: flag(&m.all, "bLocked", false),
        lock_strength: float(&m.all, "lockStrength", 0.0) as f64,
        pickable: flag(&m.all, "bPickable", true),
        door_strength: float(&m.all, "doorStrength", 0.0) as f64,
        destroyed: flag(&m.all, "bDestroyed", false),
    }
}

fn level_save(s: &Dxs, cl: &mut Classes, level: &Level, player: i32, carried_keys: &[(String, String)]) -> LevelSave {
    let mut l = LevelSave { placed: level.actors.len(), ..Default::default() };
    let mut gone = Vec::new();
    let mut mesh: Vec<(usize, [i32; 3])> = level.mesh_actors.iter().map(|m| (m.actor, m.rotation)).collect();
    mesh.sort_by_key(|m| m.0);
    mesh.dedup_by_key(|m| m.0);
    for (a, rot) in mesh {
        let info = &level.actors[a];
        let Some(o) = s.actor(info) else {
            gone.push(info.name.clone());
            continue;
        };
        let saved = s.obj(cl, o);
        // Something the player or a pawn took keeps its name, but it's carried now.
        if cl.is_a(&info.class, "Inventory") && link(&saved.saved, "Owner") != 0 {
            gone.push(info.name.clone());
            continue;
        }
        l.present.push(a);
        let pawn = cl.is_a(&info.class, "ScriptedPawn");
        if pawn {
            l.npcs.push(npc(s, cl, a, info, o));
        } else {
            let (from, to) = (info.location(), vector(&saved.all, "Location"));
            let far = (0..3).any(|c| (from[c] as f64 - to[c]).abs() > 1.0);
            let turned = rotator(&saved.all, "Rotation");
            if far || turned != rot {
                l.moved.push((a, to, turned));
            }
        }
        if flag(&saved.all, "bHidden", false) || !flag(&saved.all, "bInWorld", true) {
            l.hidden_actors.push(a);
        }
    }
    for (index, info) in level.movers.iter().enumerate() {
        l.movers.push(mover(s, cl, index, info, &level.actors[info.actor]));
    }
    for (actor, info) in level.actors.iter().enumerate() {
        if !cl.is_a(&info.class, "Trigger") {
            continue;
        }
        l.triggers.push(match s.actor(info) {
            None => TrigSave { actor, active: false, collide: false, destroyed: true, touching: false, state: String::new() },
            Some(o) => {
                let t = s.obj(cl, o);
                TrigSave {
                    actor,
                    active: flag(&t.all, "bInitiallyActive", true),
                    collide: flag(&t.all, "bCollideActors", true),
                    destroyed: false,
                    // A trigger the player stands in mustn't fire again on load.
                    touching: player != 0 && t.saved.iter().any(|p| p.name.eq_ignore_ascii_case("Touching") && matches!(p.value, Value::Object(o) if o == player)),
                    state: text(&t.all, "InitialState"),
                }
            }
        });
    }
    println!("  {}: gone since the map began: {}", level.name, gone.join(" "));
    l.keys_carried = level.nano_keys.iter().filter(|k| !carried_keys.iter().any(|c| c.0.eq_ignore_ascii_case(&k.1))).cloned().collect();
    l
}

// ------------------------------------------------------------------------------------------------ the save

pub fn run(root: &Path, folder: &Path, out: Option<&Path>) -> Result<PathBuf, String> {
    let path = out.map(Path::to_path_buf).unwrap_or_else(crate::save::new_slot);
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let info_path = folder.join("SaveInfo.dxs");
    let info = Dxs::open(&info_path)?;
    let mut cl = Classes { db: ClassDb::open(&root.join("System")), defaults: HashMap::new(), ancestry: HashMap::new() };
    let info = info.props(*info.names.get("mydeusexsaveinfo").ok_or("SaveInfo.dxs has no save information")?);
    let map = text(&info, "MapName");
    if map.is_empty() {
        return Err("SaveInfo.dxs names no map".to_string());
    }
    let s = Dxs::open(&folder.join(format!("{map}.dxs")))?;
    let player = ["JCDentonMale", "JCDentonFemale"]
        .iter()
        .flat_map(|c| s.objects_of(c))
        .find(|&o| !s.destroyed(o))
        .ok_or_else(|| format!("{map}.dxs holds no player"))?;
    let po = s.obj(&mut cl, player);
    // Save/Current holds maps the player has left: their copy of him has no flags, skills or augmentations.
    if link(&po.saved, "FlagBase") == 0 || link(&po.saved, "SkillSystem") == 0 {
        return Err(format!("the player in {map}.dxs has no flags or skills: this isn't a finished save"));
    }

    let mut skipped = Vec::new();
    let (global, clips) = global(&s, &mut cl, &po, &mut skipped);
    let file = format!("{map}.dx");
    let level = dx_level::load_level(root, &file)?;
    let this_level = level_save(&s, &mut cl, &level, player, &global.keys);
    let level_info = level.actors.iter().find(|a| a.class == "DeusExLevelInfo");
    let mission = level_info.map_or(0, |a| int(&a.props, "missionNumber", 0));

    let mut visited = BTreeMap::new();
    let rd = std::fs::read_dir(folder).map_err(|e| format!("cannot read {}: {e}", folder.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        let Some(stem) = p.file_stem().and_then(|x| x.to_str()) else { continue };
        if p.extension().and_then(|x| x.to_str()) != Some("dxs") || stem == "SaveInfo" || stem.eq_ignore_ascii_case(&map) {
            continue;
        }
        let other = dx_level::load_level(root, &format!("{stem}.dx"))?;
        let dxs = Dxs::open(&p)?;
        visited.insert(other.name.to_ascii_lowercase(), level_save(&dxs, &mut cl, &other, 0, &global.keys));
    }

    let saved_unix = std::fs::metadata(&info_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let game = SaveGame {
        version: VERSION,
        description: format!("{} (imported)", text(&info, "Description")),
        map: level.name.clone(),
        location: text(&info, "MissionLocation"),
        mission,
        saved_unix,
        play_time: float(&po.all, "saveTime", int(&info, "saveTime", 0) as f32) as f64,
        save_count: int(&info, "saveCount", 0).max(0) as u32,
        global,
        level: this_level,
        visited,
    };
    crate::save::write(&path, &game).map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    let g = &game.global;
    println!("Imported \"{}\" on {} into {}", game.description, game.map, path.display());
    println!("  health {} energy {:.0} credits {} skill points {} of {}", g.player.hp, g.player.energy, g.inventory.credits, g.points.0, g.points.1);
    println!("  {} items, {} ammo types, {} clips, {} weapons with mods", g.inventory.items.len(), g.inventory.ammo.len(), clips.len(), g.mods.len());
    println!("  {} skills, {} of {} augmentations", g.skills.len(), g.augs.iter().filter(|a| a.has).count(), g.augs.len());
    println!("  {} flags, {} goals, {} notes, {} images, {} conversations, {} keys", g.flags.len(), g.con.goals.len(), g.con.notes.len(), g.con.images.len(), g.con.history.len(), g.keys.len());
    let l = &game.level;
    println!("  this map: {} of {} placed actors, {} pawns, {} moved, {} doors, {} triggers", l.present.len(), l.placed, l.npcs.len(), l.moved.len(), l.movers.len(), l.triggers.len());
    println!("  other maps: {}", game.visited.keys().cloned().collect::<Vec<_>>().join(", "));
    if !skipped.is_empty() {
        println!("  left out: {}", skipped.join(", "));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_rounds_follow_ammo_left_in_clip() {
        assert_eq!(clip_rounds(6, 2, 278), 4);
        assert_eq!(clip_rounds(30, 0, 12), 12);
        assert_eq!(clip_rounds(6, 9, 278), 0);
    }

    #[test]
    fn orders_ours_lacks_keep_the_map_orders() {
        assert_eq!(orders("RunningTo"), Some(Orders::RunningTo));
        assert_eq!(orders("Following"), Some(Orders::RunningTo));
        assert_eq!(orders("Attacking"), None);
        assert_eq!(orders(""), None);
    }

    #[test]
    fn view_rotation_becomes_camera_angles() {
        let (yaw, pitch) = camera_angles([62096, 183863, 0]);
        assert!((pitch + 0.3298).abs() < 0.001, "{pitch}");
        let (north, _) = camera_angles([0, 0, 0]);
        assert!((north + std::f32::consts::FRAC_PI_2).abs() < 0.001);
        assert!(yaw < 0.0);
    }
}
