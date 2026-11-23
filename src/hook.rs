//! Butter-specific glue: attaches the Discord presence to Roblox sessions.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::bridge::{lookup_game, Bridge};
use crate::discord::{debug, Presence, Settings};
use crate::sessions::{Session, Sessions};

/// The Application ID of your own app from Discord's developer portal. Its
/// name is what Discord shows after "Playing", so call the app "Roblox".
const DISCORD_CLIENT_ID: &str = "1555928317109080124";

/// How often the log tailer looks for new bytes.
const POLL: Duration = Duration::from_millis(500);

/// When the tailer wakes up it ignores log files not written to in this long,
/// so a log left over from an old (maybe crashed) game cannot show a presence.
const STALE_AFTER: Duration = Duration::from_secs(120);

/// `install` may be reached more than once (a second window); only the first
/// call does anything, so there is never a second Discord worker.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Call once, right after `Sessions::new()`, for example in `ui/window.rs`
/// next to `sessions: Sessions::new()`.
///
/// Opt-in until there is a real setting: it does nothing unless the
/// environment variable BUTTER_DISCORD_PRESENCE is set.
pub fn install(sessions: &Sessions) {
    if std::env::var_os("BUTTER_DISCORD_PRESENCE").is_none() {
        return;
    }
    if DISCORD_CLIENT_ID.starts_with("PASTE") {
        eprintln!("butter: Discord presence is on, but DISCORD_CLIENT_ID in hook.rs is not set");
        return;
    }
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    debug(|| "presence enabled".to_string());
    let presence = Arc::new(Presence::start(Settings::new(DISCORD_CLIENT_ID)));
    let bridge = Bridge::new(presence, |place_id| lookup_game(place_id, &fetch));
    let hooked: Rc<RefCell<HashSet<u64>>> = Rc::default();
    // Bottles that already have a log tailer, and whether a Roblox session is
    // currently alive (the tailers only poll while it is).
    let tailed: Rc<RefCell<HashSet<String>>> = Rc::default();
    let active = Arc::new(AtomicBool::new(false));

    // Looks at every Roblox session: follows the new ones, and clears the
    // presence once none is left running.
    let sweep = {
        let sessions = sessions.clone();
        move || {
            let mut running = false;
            for bottle in crate::bottle::list() {
                for session in sessions.of(&bottle.id) {
                    debug(|| {
                        format!("session {} named {:?} in bottle {}", session.id, session.name, bottle.id)
                    });
                    if !is_roblox(&session) {
                        continue;
                    }
                    running |= session.is_alive();

                    // One log tailer per bottle, however many sessions.
                    let bottle_id = bottle.id.to_string();
                    if tailed.borrow_mut().insert(bottle_id.clone()) {
                        let users = users_dir(&bottle_id);
                        debug(|| format!("will follow Roblox logs under {}", users.display()));
                        tail_logs(bridge.clone(), users, active.clone());
                    }

                    if hooked.borrow_mut().insert(session.id) {
                        debug(|| format!("following session {}", session.id));
                        // What was already printed first, so starting Butter
                        // in the middle of a game still works.
                        bridge.feed(&session.output());
                        let bridge = bridge.clone();
                        session.watch(move |data| {
                            bridge.feed(data);
                            true
                        });
                    }
                }
            }
            active.store(running, Ordering::Relaxed);
            if running {
                bridge.set_running();
            } else {
                bridge.clear();
            }
        }
    };
    sweep();
    sessions.connect_changed(move || {
        sweep();
        true
    });
}

/// Programs are named after their app, so Roblox.app is "Roblox".
fn is_roblox(session: &Session) -> bool {
    session.name.eq_ignore_ascii_case("roblox")
}

/// `<data dir>/butter/bottles/<id>/prefix/root/Users`. Inside the Flatpak the
/// data dir is ~/.var/app/io.github.microlift.butter/data. If `bottle.rs`
/// already has a helper for the prefix path, use that instead.
fn users_dir(bottle_id: &str) -> PathBuf {
    gtk::glib::user_data_dir()
        .join("butter/bottles")
        .join(bottle_id)
        .join("prefix/root/Users")
}

/// The newest `*_Player_*.log` written to since `since`, across every
/// `Users/*/Library/Logs/Roblox` folder. The `_CrashHandler_` logs are not
/// the game's, so they are skipped.
fn newest_player_log(users: &Path, since: SystemTime) -> Option<PathBuf> {
    std::fs::read_dir(users)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|user| user.path().join("Library/Logs/Roblox"))
        .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten())
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.contains("_Player_") || name.contains("CrashHandler") || !name.ends_with(".log") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            (modified >= since).then(|| (modified, entry.path()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

/// Follows the newest Roblox player log of a bottle on a background thread
/// and passes whatever gets appended to the bridge. A file seen for the first
/// time is read from its start, so attaching in the middle of a game works
/// (the tracker ignores everything before the last join anyway). Does
/// nothing while `active` is false.
fn tail_logs(bridge: Bridge, users: PathBuf, active: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut current: Option<(PathBuf, u64)> = None;
        let mut since: Option<SystemTime> = None;
        loop {
            std::thread::sleep(POLL);
            if !active.load(Ordering::Relaxed) {
                // Roblox is not running: start over next time.
                since = None;
                current = None;
                continue;
            }
            let floor = *since.get_or_insert_with(|| {
                SystemTime::now().checked_sub(STALE_AFTER).unwrap_or(UNIX_EPOCH)
            });
            let Some(path) = newest_player_log(&users, floor) else {
                continue;
            };
            let mut offset = match &current {
                Some((known, at)) if *known == path => *at,
                _ => {
                    debug(|| format!("following log {}", path.display()));
                    0
                }
            };
            let Ok(mut file) = File::open(&path) else {
                continue;
            };
            let len = file.metadata().map_or(0, |meta| meta.len());
            if len < offset {
                // Truncated or replaced: begin again.
                offset = 0;
            }
            if len > offset {
                let mut chunk = Vec::new();
                if file.seek(SeekFrom::Start(offset)).is_ok()
                    && file.by_ref().take(len - offset).read_to_end(&mut chunk).is_ok()
                {
                    offset += chunk.len() as u64;
                    bridge.feed(&chunk);
                }
            }
            current = Some((path, offset));
        }
    });
}

/// GET a URL and return the body. Written from my memory of ureq 3's API
/// (your Cargo.toml has ureq 3 without the "json" feature, hence the plain
/// string): fix it if the compiler disagrees. The timeout keeps a stuck
/// request from lingering.
fn fetch(url: &str) -> Option<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .build()
        .into();
    let mut response = agent.get(url).call().ok()?;
    response.body_mut().read_to_string().ok()
}
