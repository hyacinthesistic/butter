//! Glue between the three parsers and Discord: turns Roblox's console output
//! into the presence to show.
//!
//! Output goes in through [`Bridge::feed`], on whatever thread. Join/leave
//! comes from [`crate::activity`], the game's own `[BloxstrapRPC]` messages
//! from [`crate::rpc`], and the finished [`crate::discord::Activity`] goes to
//! a [`Sink`] (the real one is [`crate::discord::Presence`]). The game name
//! is looked up on a short-lived thread, so `feed` never waits on the
//! network; until it arrives the presence says "Playing Roblox".
//!
//! Not supported yet: images chosen by a game through `assetId`. The game
//! icon is shown instead, unless the game hides the large image.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::activity::{Activity as Tracker, Change};
use crate::discord::{debug, Activity as Shown, Presence};
use crate::rpc::{LineSplitter, Rpc, Text};

const MENU_IMAGE: &str = "roblox";
const MAX_CACHE: usize = 64;
const MAX_NAME_CHARS: usize = 100;

/// What we know about a game from Roblox's public API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameInfo {
    pub name: String,
    pub creator: Option<String>,
    pub icon_url: Option<String>,
}

/// Where finished presences go.
pub trait Sink: Send + Sync + 'static {
    fn set(&self, activity: Option<Shown>);
}

impl Sink for Presence {
    fn set(&self, activity: Option<Shown>) {
        Presence::set(self, activity)
    }
}

type Lookup = Arc<dyn Fn(u64) -> Option<GameInfo> + Send + Sync>;
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

struct State {
    splitter: LineSplitter,
    tracker: Tracker,
    rpc: Rpc,
    /// Unix seconds when the current game was joined.
    started: Option<u64>,
    info: Option<(u64, GameInfo)>,
    cache: HashMap<u64, GameInfo>,
    /// True while a Roblox session is alive.
    running: bool,
    /// Unix seconds since Roblox has been in its menus (not in a game).
    menu_since: Option<u64>,
    /// The last thing handed to the sink. Nothing is shown at the start.
    sent: Option<Shown>,
}

impl State {
    fn emit(&mut self, sink: &dyn Sink) {
        let now = render(self);
        if now != self.sent {
            self.sent = now.clone();
            sink.set(now);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub struct Bridge {
    state: Arc<Mutex<State>>,
    sink: Arc<dyn Sink>,
    lookup: Lookup,
    clock: Clock,
}

impl Bridge {
    pub fn new(
        sink: Arc<dyn Sink>,
        lookup: impl Fn(u64) -> Option<GameInfo> + Send + Sync + 'static,
    ) -> Bridge {
        Bridge {
            state: Arc::new(Mutex::new(State {
                splitter: LineSplitter::new(),
                tracker: Tracker::new(),
                rpc: Rpc::new(),
                started: None,
                info: None,
                cache: HashMap::new(),
                running: false,
                menu_since: None,
                sent: None,
            })),
            sink,
            lookup: Arc::new(lookup),
            clock: Arc::new(unix_now),
        }
    }

    /// Replace the clock, for tests.
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Bridge {
        self.clock = Arc::new(clock);
        self
    }

    /// Raw console output, in chunks of any size.
    pub fn feed(&self, bytes: &[u8]) {
        let mut wanted = None;
        {
            let mut guard = lock(&self.state);
            let state = &mut *guard;
            let mut lines = Vec::new();
            state.splitter.push(bytes, |line| lines.push(line.to_string()));
            for line in lines {
                match state.tracker.feed_line(&line) {
                    Some(Change::Joined(game)) => {
                        debug(|| format!("joined place {}", game.place_id));
                        state.rpc.reset();
                        state.menu_since = None;
                        state.started = Some((self.clock)());
                        state.info = state
                            .cache
                            .get(&game.place_id)
                            .map(|info| (game.place_id, info.clone()));
                        wanted = state.info.is_none().then_some(game.place_id);
                    }
                    Some(Change::Left) => {
                        debug(|| "left the game".to_string());
                        state.rpc.reset();
                        state.started = None;
                        state.info = None;
                        state.menu_since = state.running.then(|| (self.clock)());
                        wanted = None;
                    }
                    None => {}
                }
                if state.rpc.feed_line(&line) {
                    debug(|| "applied a BloxstrapRPC message".to_string());
                }
            }
            state.emit(&*self.sink);
        }
        if let Some(place_id) = wanted {
            self.spawn_lookup(place_id);
        }
    }

    /// Forget everything and show nothing, e.g. when Roblox is closed.
    pub fn clear(&self) {
        let mut guard = lock(&self.state);
        let state = &mut *guard;
        state.splitter = LineSplitter::new();
        state.tracker = Tracker::new();
        state.rpc.reset();
        state.started = None;
        state.info = None;
        state.running = false;
        state.menu_since = None;
        state.emit(&*self.sink);
    }

    /// Roblox is running. Until a game is joined, shows the menu presence.
    pub fn set_running(&self) {
        let mut guard = lock(&self.state);
        let state = &mut *guard;
        if state.running {
            return;
        }
        state.running = true;
        if state.tracker.current().is_none() {
            state.menu_since = Some((self.clock)());
        }
        state.emit(&*self.sink);
    }

    fn spawn_lookup(&self, place_id: u64) {
        let state = self.state.clone();
        let sink = self.sink.clone();
        let lookup = self.lookup.clone();
        std::thread::spawn(move || {
            let Some(info) = lookup(place_id) else {
                debug(|| format!("lookup of place {place_id} failed"));
                return;
            };
            debug(|| format!("place {place_id} is {:?}", info.name));
            let mut guard = lock(&state);
            let state = &mut *guard;
            if state.cache.len() >= MAX_CACHE {
                state.cache.clear();
            }
            state.cache.insert(place_id, info.clone());
            // Only if the player is still in that game.
            if state.tracker.current().map(|game| game.place_id) == Some(place_id) {
                state.info = Some((place_id, info));
                state.emit(&*sink);
            }
        });
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn pick(text: &Text, default: Option<String>) -> Option<String> {
    match text {
        Text::Default => default,
        Text::Hidden => None,
        Text::Set(text) => Some(text.clone()),
    }
}

/// The presence for the current state, or None to show nothing.
fn render(state: &State) -> Option<Shown> {
    let Some(game) = state.tracker.current() else {
        return render_menu(state);
    };
    let info = state
        .info
        .as_ref()
        .filter(|(place_id, _)| *place_id == game.place_id)
        .map(|(_, info)| info);
    let presence = state.rpc.presence();

    let details = pick(
        &presence.details,
        Some(info.map_or_else(|| "Playing Roblox".to_string(), |info| info.name.clone())),
    );
    let by = info
        .and_then(|info| info.creator.as_ref())
        .map(|creator| format!("by {creator}"));
    let large_image = if presence.large_image.hidden {
        None
    } else {
        info.and_then(|info| info.icon_url.clone())
    };
    let large_text = large_image
        .as_ref()
        .and_then(|_| pick(&presence.large_image.hover_text, info.map(|info| info.name.clone())));
    Some(Shown {
        details,
        state: pick(&presence.state, by),
        start: state.started,
        large_image,
        large_text,
        small_image: None,
        small_text: None,
    })
}

/// Roblox is open but no game is joined.
fn render_menu(state: &State) -> Option<Shown> {
    if !state.running {
        return None;
    }
    Some(Shown {
        details: Some("In the Roblox app".to_string()),
         state: None,
        start: state.menu_since,
        large_image: Some(MENU_IMAGE.to_string()),
        large_text: Some("Roblox".to_string()),
        small_image: None,
        small_text: None,
    })
}

/// Look a game up on Roblox's public API. `fetch` returns the body of a GET,
/// so the network stays outside this module. Only a numeric place id goes
/// into the URLs.
pub fn lookup_game(place_id: u64, fetch: &dyn Fn(&str) -> Option<String>) -> Option<GameInfo> {
    let universe = parse_universe_id(&fetch(&format!(
        "https://apis.roblox.com/universes/v1/places/{place_id}/universe"
    ))?)?;
    let (name, creator) = parse_game(&fetch(&format!(
        "https://games.roblox.com/v1/games?universeIds={universe}"
    ))?)?;
    let icon_url = fetch(&format!(
        "https://thumbnails.roblox.com/v1/games/icons?universeIds={universe}&returnPolicy=PlaceHolder&size=512x512&format=Png&isCircular=false"
    ))
    .and_then(|body| parse_icon(&body));
    Some(GameInfo {
        name,
        creator,
        icon_url,
    })
}

fn parse_universe_id(body: &str) -> Option<u64> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("universeId")?
        .as_u64()
        .filter(|&id| id > 0)
}

fn parse_game(body: &str) -> Option<(String, Option<String>)> {
    let value: Value = serde_json::from_str(body).ok()?;
    let game = value.get("data")?.get(0)?;
    let name = clean(game.get("name")?.as_str()?)?;
    let creator = game
        .pointer("/creator/name")
        .and_then(Value::as_str)
        .and_then(clean);
    Some((name, creator))
}

/// Accepts only a finished thumbnail served from Roblox's image CDN.
fn parse_icon(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let entry = value.get("data")?.get(0)?;
    if entry.get("state").and_then(Value::as_str) != Some("Completed") {
        return None;
    }
    let url = entry.get("imageUrl")?.as_str()?;
    let host = url.strip_prefix("https://")?.split('/').next()?;
    (host == "rbxcdn.com" || host.ends_with(".rbxcdn.com")).then(|| url.to_string())
}

fn clean(text: &str) -> Option<String> {
    let text: String = text
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const JOIN: &str = "2026-11-22T23:00:39.853Z,120.853806,e6e70000,6,Info [FLog::GameJoinUtil] GameJoinUtil::joinGame";
    const POST: &str = r#"2026-11-22T23:00:39.853Z,120.853935,e6e70000,6,Info [FLog::GameJoinUtil] GameJoinUtil::joinGamePostStandard: URL: https://gamejoin.roblox.com/v1/join-game BODY: {"placeId":189707,"gameJoinAttemptId":"6ebf5402-59d4-40ee-9"#;
    const RESPONSE: &str = r#"status code: {"jobId":"7e12d67f-dc50-410e-82dd-acf7fdf8975f","status":2,"statusData":null}"#;
    const SERVER: &str = "2026-11-22T23:00:40.427Z,121.427864,e6ff0000,7 [FLog::Network] serverId: 128.116.50.33|52152";
    const LEAVE: &str = "2026-11-22T23:10:49.793Z,730.793640,e6e70000,6 [FLog::SingleSurfaceApp] leaveUGCGameInternal";
    const START: u64 = 1_700_000_000;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Option<Shown>>>);

    impl Sink for Recorder {
        fn set(&self, activity: Option<Shown>) {
            self.0.lock().unwrap().push(activity);
        }
    }

    impl Recorder {
        fn events(&self) -> Vec<Option<Shown>> {
            self.0.lock().unwrap().clone()
        }

        fn wait_for(&self, count: usize) -> Vec<Option<Shown>> {
            for _ in 0..300 {
                let events = self.events();
                if events.len() >= count {
                    return events;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("only got {:?}", self.events());
        }
    }

    fn info() -> GameInfo {
        GameInfo {
            name: "Test Game".into(),
            creator: Some("Someone".into()),
            icon_url: Some("https://tr.rbxcdn.com/abc/512/512/Image/Png".into()),
        }
    }

    fn lines(list: &[&str]) -> Vec<u8> {
        let mut text = list.join("\r\n");
        text.push_str("\r\n");
        text.into_bytes()
    }

    fn bridge_with(lookup: impl Fn(u64) -> Option<GameInfo> + Send + Sync + 'static) -> (Bridge, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let bridge = Bridge::new(recorder.clone(), lookup).with_clock(|| START);
        (bridge, recorder)
    }

    fn rpc_line(json: &str) -> String {
        format!("2026-11-22T23:01:00.000Z,1.0,e6ff0000,6 [FLog::Output] [BloxstrapRPC] {json}")
    }

    #[test]
    fn join_shows_a_generic_presence_then_the_game_name() {
        let (bridge, recorder) = bridge_with(|place| {
            assert_eq!(place, 189707);
            Some(info())
        });
        bridge.feed(&lines(&[JOIN, POST, RESPONSE, SERVER]));
        let events = recorder.wait_for(2);
        let first = events[0].as_ref().unwrap();
        assert_eq!(first.details.as_deref(), Some("Playing Roblox"));
        assert_eq!(first.start, Some(START));
        let second = events[1].as_ref().unwrap();
        assert_eq!(second.details.as_deref(), Some("Test Game"));
        assert_eq!(second.state.as_deref(), Some("by Someone"));
        assert_eq!(second.large_image.as_deref(), Some("https://tr.rbxcdn.com/abc/512/512/Image/Png"));
        assert_eq!(second.large_text.as_deref(), Some("Test Game"));
        assert_eq!(second.start, Some(START));
    }

    #[test]
    fn game_messages_override_text_and_can_hide_the_image() {
        let (bridge, recorder) = bridge_with(|_| Some(info()));
        bridge.feed(&lines(&[JOIN, POST, RESPONSE, SERVER]));
        recorder.wait_for(2);
        bridge.feed(&lines(&[&rpc_line(r#"{"command":"SetRichPresence","data":{"details":"Wave 5","state":"Alive"}}"#)]));
        let events = recorder.wait_for(3);
        let shown = events[2].as_ref().unwrap();
        assert_eq!(shown.details.as_deref(), Some("Wave 5"));
        assert_eq!(shown.state.as_deref(), Some("Alive"));
        assert!(shown.large_image.is_some());

        bridge.feed(&lines(&[&rpc_line(r#"{"command":"SetRichPresence","data":{"largeImage":{"clear":true},"state":"<reset>"}}"#)]));
        let events = recorder.wait_for(4);
        let shown = events[3].as_ref().unwrap();
        assert_eq!(shown.large_image, None);
        assert_eq!(shown.large_text, None);
        assert_eq!(shown.state.as_deref(), Some("by Someone"));
    }

    #[test]
    fn messages_before_a_join_are_ignored_and_repeats_are_not_resent() {
        let (bridge, recorder) = bridge_with(|_| Some(info()));
        let message = rpc_line(r#"{"command":"SetRichPresence","data":{"details":"Hello"}}"#);
        bridge.feed(&lines(&[&message]));
        assert_eq!(recorder.events(), []);
        bridge.feed(&lines(&[JOIN, POST, RESPONSE, SERVER]));
        recorder.wait_for(2);
        bridge.feed(&lines(&[&message]));
        let count = recorder.wait_for(3).len();
        bridge.feed(&lines(&[&message]));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(recorder.events().len(), count);
    }

    #[test]
    fn leaving_clears_the_presence_and_a_late_lookup_is_ignored() {
        let (bridge, recorder) = bridge_with(|_| {
            std::thread::sleep(Duration::from_millis(150));
            Some(info())
        });
        bridge.feed(&lines(&[JOIN, POST, RESPONSE, SERVER]));
        bridge.feed(&lines(&[LEAVE]));
        std::thread::sleep(Duration::from_millis(400));
        let events = recorder.events();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].as_ref().unwrap().details.as_deref(), Some("Playing Roblox"));
        assert_eq!(events[1], None);
    }

    #[test]
    fn rejoining_a_known_place_does_not_look_it_up_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let (bridge, recorder) = bridge_with(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Some(info())
        });
        bridge.feed(&lines(&[JOIN, POST, RESPONSE, SERVER]));
        recorder.wait_for(2);
        bridge.feed(&lines(&[LEAVE]));
        recorder.wait_for(3);
        bridge.feed(&lines(&[JOIN, POST, RESPONSE, SERVER]));
        let events = recorder.wait_for(4);
        assert_eq!(events[3].as_ref().unwrap().details.as_deref(), Some("Test Game"));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn replaying_history_that_ends_outside_a_game_shows_nothing() {
        let (bridge, recorder) = bridge_with(|_| panic!("no lookup expected"));
        bridge.feed(&lines(&[JOIN, POST, RESPONSE, SERVER, LEAVE]));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(recorder.events(), []);
    }

    #[test]
    fn works_when_output_arrives_one_byte_at_a_time() {
        let (bridge, recorder) = bridge_with(|_| Some(info()));
        for byte in lines(&[JOIN, POST, RESPONSE, SERVER]) {
            bridge.feed(&[byte]);
        }
        let events = recorder.wait_for(2);
        assert_eq!(events[1].as_ref().unwrap().details.as_deref(), Some("Test Game"));
    }

    #[test]
    fn clear_shows_nothing_and_does_not_repeat() {
        let (bridge, recorder) = bridge_with(|_| None);
        bridge.feed(&lines(&[JOIN, POST, RESPONSE, SERVER]));
        recorder.wait_for(1);
        bridge.clear();
        bridge.clear();
        assert_eq!(recorder.events().len(), 2);
        assert_eq!(recorder.events()[1], None);
    }

    fn fake_api(url: &str) -> Option<String> {
        match url {
            "https://apis.roblox.com/universes/v1/places/189707/universe" => {
                Some(r#"{"universeId":77}"#.into())
            }
            "https://games.roblox.com/v1/games?universeIds=77" => Some(
                r#"{"data":[{"id":77,"rootPlaceId":189707,"name":"  Test\u0007 Game ","creator":{"id":1,"name":"Someone","type":"User"}}]}"#.into(),
            ),
            u if u.starts_with("https://thumbnails.roblox.com/v1/games/icons?universeIds=77&") => Some(
                r#"{"data":[{"targetId":77,"state":"Completed","imageUrl":"https://tr.rbxcdn.com/abc/512/512/Image/Png"}]}"#.into(),
            ),
            _ => None,
        }
    }

    #[test]
    fn lookup_reads_the_three_api_answers() {
        let found = lookup_game(189707, &fake_api).unwrap();
        assert_eq!(found, info());
    }

    #[test]
    fn lookup_needs_the_name_but_not_the_icon() {
        let no_icon = |url: &str| {
            if url.contains("thumbnails") { None } else { fake_api(url) }
        };
        let found = lookup_game(189707, &no_icon).unwrap();
        assert_eq!(found.icon_url, None);
        assert_eq!(lookup_game(5, &fake_api), None);
        let no_games = |url: &str| if url.contains("games.roblox") { None } else { fake_api(url) };
        assert_eq!(lookup_game(189707, &no_games), None);
    }

    #[test]
    fn icons_must_be_finished_https_rbxcdn_urls() {
        let body = |state: &str, url: &str| {
            format!(r#"{{"data":[{{"state":"{state}","imageUrl":"{url}"}}]}}"#)
        };
        assert!(parse_icon(&body("Completed", "https://tr.rbxcdn.com/x")).is_some());
        assert_eq!(parse_icon(&body("Pending", "https://tr.rbxcdn.com/x")), None);
        assert_eq!(parse_icon(&body("Completed", "http://tr.rbxcdn.com/x")), None);
        assert_eq!(parse_icon(&body("Completed", "https://evil.test/rbxcdn.com")), None);
        assert_eq!(parse_icon(&body("Completed", "https://rbxcdn.com.evil.test/x")), None);
    }

    #[test]
    fn bad_api_answers_are_rejected() {
        assert_eq!(parse_universe_id("{}"), None);
        assert_eq!(parse_universe_id(r#"{"universeId":0}"#), None);
        assert_eq!(parse_universe_id("nope"), None);
        assert_eq!(parse_game(r#"{"data":[]}"#), None);
        assert_eq!(parse_game(r#"{"data":[{"name":"  "}]}"#), None);
    }

    #[test]
    fn menu_presence_while_roblox_runs_outside_a_game() {
        let (bridge, recorder) = bridge_with(|_| Some(info()));
        bridge.set_running();
        let menu = recorder.events()[0].clone().unwrap();
        assert_eq!(menu.details.as_deref(), Some("In the Roblox app"));
        assert_eq!(menu.start, Some(START));

        bridge.feed(&lines(&[JOIN, POST, RESPONSE, SERVER]));
        let events = recorder.wait_for(3);
        assert_eq!(events[1].as_ref().unwrap().details.as_deref(), Some("Playing Roblox"));

        bridge.feed(&lines(&[LEAVE]));
        let events = recorder.wait_for(4);
        assert_eq!(events[3].as_ref().unwrap().details.as_deref(), Some("In the Roblox app"));

        bridge.clear();
        let events = recorder.events();
        assert_eq!(events.len(), 5);
        assert_eq!(events[4], None);
    }
}
