//! A tiny Discord Rich Presence client over Discord's local IPC socket.
//!
//! Standard library plus `serde_json` only, so it adds no new crate. The
//! protocol is a Unix socket (`discord-ipc-0` ...) carrying frames of
//! `[opcode: u32 LE][length: u32 LE][JSON]`:
//!
//! * opcode 0, handshake: `{"v":1,"client_id":"..."}`, answered with a
//! `READY` dispatch;
//! * opcode 1, frame: commands such as `SET_ACTIVITY`;
//! * opcode 2, close: the other side hangs up.
//!
//! [`Client`] speaks the protocol on any stream. [`Presence`] owns a worker
//! thread that finds Discord, reconnects when it starts later, coalesces
//! bursts and rate-limits the updates, so the GTK thread never blocks on it.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const MAX_FRAME: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Discord limits details/state to 128 characters.
const MAX_TEXT_CHARS: usize = 128;

/// Diagnostics on stderr, only when BUTTER_DISCORD_DEBUG is set. Nothing else
/// reports why a presence does not show up, so this is the first thing to
/// turn on when it does not.
pub(crate) fn debug(message: impl FnOnce() -> String) {
    if std::env::var_os("BUTTER_DISCORD_DEBUG").is_some() {
        eprintln!("butter: discord: {}", message());
    }
}

/// What to show. Every field is optional.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Activity {
    pub details: Option<String>,
    pub state: Option<String>,
    /// Start of the elapsed timer, in Unix seconds.
    pub start: Option<u64>,
    /// An asset key from the developer portal or, on current Discord
    /// clients as far as I know, an https URL.
    pub large_image: Option<String>,
    pub large_text: Option<String>,
    pub small_image: Option<String>,
    pub small_text: Option<String>,
}

impl Activity {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        if let Some(text) = text(&self.details) {
            map.insert("details".into(), text.into());
        }
        if let Some(text) = text(&self.state) {
            map.insert("state".into(), text.into());
        }
        if let Some(start) = self.start {
            map.insert("timestamps".into(), json!({ "start": start }));
        }
        let mut assets = Map::new();
        for (key, value) in [
            ("large_image", &self.large_image),
            ("large_text", &self.large_text),
            ("small_image", &self.small_image),
            ("small_text", &self.small_text),
        ] {
            if let Some(text) = text(value) {
                assets.insert(key.into(), text.into());
            }
        }
        if !assets.is_empty() {
            map.insert("assets".into(), Value::Object(assets));
        }
        Value::Object(map)
    }
}

/// Trimmed, capped, and dropped when shorter than 2 characters, because as I
/// recall Discord rejects shorter details and state.
fn text(value: &Option<String>) -> Option<String> {
    let trimmed: String = value
        .as_deref()?
        .trim()
        .chars()
        .take(MAX_TEXT_CHARS)
        .collect();
    (trimmed.chars().count() >= 2).then_some(trimmed)
}

fn write_frame(out: &mut impl Write, opcode: u32, payload: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(payload).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(8 + body.len());
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    out.write_all(&frame)
}
fn read_frame(input: &mut impl Read) -> io::Result<(u32, Value)> {
    let mut head = [0u8; 8];
    input.read_exact(&mut head)?;
    let opcode = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let length = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; length];
    input.read_exact(&mut body)?;
    Ok((opcode, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

/// Turn a close frame or an ERROR event into an error.
fn check_reply(opcode: u32, reply: &Value) -> io::Result<()> {
    let message = || {
        reply
            .pointer("/data/message")
            .or_else(|| reply.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("no message")
            .to_string()
    };
    if opcode == OP_CLOSE {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("Discord closed the connection: {}", message()),
        ));
    }
    if reply.get("evt").and_then(Value::as_str) == Some("ERROR") {
        return Err(io::Error::other(format!("Discord refused: {}", message())));
    }
    Ok(())
}

/// The protocol on an already connected stream.
pub struct Client<S> {
    stream: S,
    nonce: u64,
}

impl<S: Read + Write> Client<S> {
    pub fn handshake(mut stream: S, client_id: &str) -> io::Result<Self> {
        write_frame(
            &mut stream,
            OP_HANDSHAKE,
            &json!({ "v": 1, "client_id": client_id }),
        )?;
        let (opcode, reply) = read_frame(&mut stream)?;
        check_reply(opcode, &reply)?;
        Ok(Client { stream, nonce: 0 })
    }

    /// Show `activity`, or clear the presence with `None`.
    pub fn set_activity(&mut self, activity: Option<&Activity>, pid: u32) -> io::Result<()> {
        self.nonce += 1;
        let activity = activity
            .map(Activity::to_json)
            .filter(|json| json.as_object().is_some_and(|map| !map.is_empty()));
        write_frame(
            &mut self.stream,
            OP_FRAME,
            &json!({
                "cmd": "SET_ACTIVITY",
                "args": { "pid": pid, "activity": activity },
                "nonce": self.nonce.to_string(),
            }),
        )?;
        let (opcode, reply) = read_frame(&mut self.stream)?;
        check_reply(opcode, &reply)
    }
}

/// Where Discord's socket may be: the runtime dir (native Discord), its
/// Flatpak and Snap subfolders, then the temp dirs other tools also try.
pub fn socket_candidates() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        bases.push(runtime.clone());
        bases.push(runtime.join("app/com.discordapp.Discord"));
        bases.push(runtime.join("snap.discord"));
    }
    for var in ["TMPDIR", "TMP", "TEMP"] {
        if let Some(dir) = std::env::var_os(var) {
            bases.push(PathBuf::from(dir));
        }
    }
    bases.push(PathBuf::from("/tmp"));
    let mut paths = Vec::new();
    for base in bases {
        for n in 0..10 {
            let path = base.join(format!("discord-ipc-{n}"));
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn connect(settings: &Settings) -> io::Result<Client<UnixStream>> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no Discord socket found");
    for path in &settings.candidates {
        let stream = match UnixStream::connect(path) {
            Ok(stream) => stream,
            Err(e) => {
                last = e;
                continue;
            }
        };
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        match Client::handshake(stream, &settings.client_id) {
            Ok(client) => return Ok(client),
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub client_id: String,
    pub candidates: Vec<PathBuf>,
    /// Least time between two updates. Discord rate-limits presence updates
    /// (about 5 per 20 seconds as I remember it).
    pub min_interval: Duration,
    /// How often to look for Discord again after a failure.
    pub retry: Duration,
}
impl Settings {
    pub fn new(client_id: impl Into<String>) -> Self {
        Settings {
            client_id: client_id.into(),
            candidates: socket_candidates(),
            min_interval: Duration::from_secs(5),
            retry: Duration::from_secs(10),
        }
    }
}

enum Message {
    Set(Option<Activity>),
    Quit,
}

/// Handle to the worker thread. Cheap to call from the UI thread.
pub struct Presence {
    sender: Sender<Message>,
    thread: Option<JoinHandle<()>>,
}

impl Presence {
    pub fn start(settings: Settings) -> Presence {
        let (sender, receiver) = mpsc::channel();
        let thread = thread::spawn(move || worker(settings, receiver));
        Presence {
            sender,
            thread: Some(thread),
        }
    }

    /// Ask for this presence to be shown, or cleared with `None`. Only the
    /// latest request matters.
    pub fn set(&self, activity: Option<Activity>) {
        let _ = self.sender.send(Message::Set(activity));
    }

    /// Clear the presence and wait for the worker to finish. Dropping the
    /// handle does the same without waiting.
    pub fn shutdown(mut self) {
        let _ = self.sender.send(Message::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        let _ = self.sender.send(Message::Quit);
    }
}

fn worker(settings: Settings, receiver: mpsc::Receiver<Message>) {
    let pid = std::process::id();
    let mut desired: Option<Activity> = None;
    // What Discord shows on the current connection; None while unknown.
    let mut shown: Option<Option<Activity>> = None;
    let mut client: Option<Client<UnixStream>> = None;
    let mut last_send: Option<Instant> = None;
    let mut next_connect = Instant::now();

    loop {
        let in_sync = match (&client, &shown) {
            (None, _) => desired.is_none(),
            (Some(_), Some(shown)) => *shown == desired,
            (Some(_), None) => false,
        };
        let message = if in_sync {
            receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            let due = match (&client, last_send) {
                (Some(_), Some(at)) => at + settings.min_interval,
                (Some(_), None) => Instant::now(),
                (None, _) => next_connect,
            };
            receiver.recv_timeout(due.saturating_duration_since(Instant::now()))
        };
        match message {
            Ok(Message::Set(activity)) => desired = activity,
            Ok(Message::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        // Take everything already queued: only the latest request counts.
        while let Ok(more) = receiver.try_recv() {
            match more {
                Message::Set(activity) => desired = activity,
                Message::Quit => {
                    clear_on_exit(&mut client, &shown, pid);
                    return;
                }
            }
        }

        let now = Instant::now();
        if client.is_none() && desired.is_some() && now >= next_connect {
            match connect(&settings) {
                // A fresh connection starts without a presence.
                Ok(connected) => {
                    debug(|| "connected to Discord".to_string());
                    client = Some(connected);
                    shown = Some(None);
                }
                Err(e) => {
                    debug(|| format!("could not connect: {e}"));
                    next_connect = now + settings.retry;
                }
            }
        }
        if let Some(connected) = &mut client {
            let allowed = last_send.map_or(true, |at| now >= at + settings.min_interval);
            if allowed && shown.as_ref() != Some(&desired) {
                match connected.set_activity(desired.as_ref(), pid) {
                    Ok(()) => {
                        debug(|| {
                            format!(
                                "presence {}",
                                if desired.is_some() { "set" } else { "cleared" }
                            )
                        });
                        shown = Some(desired.clone());
                        last_send = Some(Instant::now());
                    }
                    Err(e) => {
                        debug(|| format!("update failed: {e}"));
                        client = None;
                        shown = None;
                        next_connect = Instant::now() + settings.retry;
                    }
                }
            }
        }
    }
    clear_on_exit(&mut client, &shown, pid);
}
fn clear_on_exit(
    client: &mut Option<Client<UnixStream>>,
    shown: &Option<Option<Activity>>,
    pid: u32,
) {
    if let Some(connected) = client {
        if *shown != Some(None) {
            let _ = connected.set_activity(None, pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Received = mpsc::Receiver<(u32, Value)>;
    const WAIT: Duration = Duration::from_secs(3);

    fn temp_socket() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("dtest-{}-{n}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Plays Discord on one stream: answers the handshake with READY and every
    /// command with a success reply, and reports what it received.
    fn serve(mut stream: UnixStream, report: Sender<(u32, Value)>, error_on_handshake: bool) {
        while let Ok((opcode, value)) = read_frame(&mut stream) {
            let reply = if opcode == OP_HANDSHAKE && error_on_handshake {
                json!({"evt": "ERROR", "data": {"code": 4000, "message": "Invalid Client ID"}})
            } else if opcode == OP_HANDSHAKE {
                json!({"cmd": "DISPATCH", "evt": "READY", "data": {}})
            } else {
                json!({"cmd": value["cmd"], "evt": null, "nonce": value["nonce"], "data": {}})
            };
            let _ = report.send((opcode, value));
            if write_frame(&mut stream, OP_FRAME, &reply).is_err() {
                break;
            }
        }
    }
    fn fake_discord(path: &Path) -> Received {
        let listener = UnixListener::bind(path).unwrap();
        let (report, received) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let report = report.clone();
                thread::spawn(move || serve(stream, report, false));
            }
        });
        received
    }

    fn fast_settings(path: &Path) -> Settings {
        Settings {
            client_id: "123".into(),
            candidates: vec![path.to_path_buf()],
            min_interval: Duration::from_millis(300),
            retry: Duration::from_millis(30),
        }
    }

    fn activity(details: &str) -> Activity {
        Activity {
            details: Some(details.into()),
            ..Activity::default()
        }
    }

    fn next(received: &Received) -> (u32, Value) {
        received.recv_timeout(WAIT).expect("no frame arrived")
    }

    #[test]
    fn client_handshakes_and_sets_then_clears_an_activity() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (report, received) = mpsc::channel();
        thread::spawn(move || serve(theirs, report, false));

        let mut client = Client::handshake(ours, "999").unwrap();
        let (opcode, hello) = next(&received);
        assert_eq!(
            (opcode, &hello["v"], &hello["client_id"]),
            (0, &json!(1), &json!("999"))
        );
        let shown = Activity {
            details: Some("Playing Obby".into()),
            state: Some("Stage 3".into()),
            start: Some(1_700_000_000),
            large_image: Some("https://example.test/a.png".into()),
            large_text: Some("Obby".into()),
            ..Activity::default()
        };
        client.set_activity(Some(&shown), 42).unwrap();
        let (opcode, command) = next(&received);
        assert_eq!(opcode, 1);
        assert_eq!(command["cmd"], "SET_ACTIVITY");
        assert_eq!(command["nonce"], "1");
        assert_eq!(command["args"]["pid"], 42);
        let sent = &command["args"]["activity"];
        assert_eq!(sent["details"], "Playing Obby");
        assert_eq!(sent["state"], "Stage 3");
        assert_eq!(sent["timestamps"]["start"], 1_700_000_000u64);
        assert_eq!(sent["assets"]["large_image"], "https://example.test/a.png");
        assert_eq!(sent["assets"]["large_text"], "Obby");
        assert!(sent["assets"].get("small_image").is_none());

        client.set_activity(None, 42).unwrap();
        let (_, command) = next(&received);
        assert_eq!(command["nonce"], "2");
        assert!(command["args"]["activity"].is_null());
    }

    #[test]
    fn short_or_blank_text_is_dropped_and_empty_activity_clears() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (report, received) = mpsc::channel();
        thread::spawn(move || serve(theirs, report, false));
        let mut client = Client::handshake(ours, "1").unwrap();
        next(&received);

        let weak = Activity {
            details: Some("x".into()),
            state: Some(" ".into()),
            ..Activity::default()
        };
        client.set_activity(Some(&weak), 1).unwrap();
        assert!(next(&received).1["args"]["activity"].is_null());
        let long = Activity {
            details: Some("é".repeat(300)),
            ..Activity::default()
        };
        client.set_activity(Some(&long), 1).unwrap();
        let sent = next(&received).1;
        assert_eq!(
            sent["args"]["activity"]["details"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            128
        );
    }

    #[test]
    fn handshake_error_from_discord_is_an_error() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (report, _received) = mpsc::channel();
        thread::spawn(move || serve(theirs, report, true));
        let error = Client::handshake(ours, "bad").err().expect("should fail");
        assert!(error.to_string().contains("Invalid Client ID"), "{error}");
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(MAX_FRAME as u32 + 1).to_le_bytes());
        let error = read_frame(&mut bytes.as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn frames_round_trip() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, 7, &json!({"a": [1, 2]})).unwrap();
        let (opcode, value) = read_frame(&mut buffer.as_slice()).unwrap();
        assert_eq!((opcode, value), (7, json!({"a": [1, 2]})));
    }

    #[test]
    fn candidates_cover_the_runtime_dir_and_flatpak_discord() {
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        let paths = socket_candidates();
        assert_eq!(paths[0], PathBuf::from("/run/user/1000/discord-ipc-0"));
        assert!(paths.contains(&PathBuf::from(
            "/run/user/1000/app/com.discordapp.Discord/discord-ipc-3"
        )));
        assert!(paths.contains(&PathBuf::from("/tmp/discord-ipc-9")));
    }
    #[test]
    fn worker_shows_updates_and_clears_on_shutdown() {
        let path = temp_socket();
        let received = fake_discord(&path);
        let presence = Presence::start(fast_settings(&path));
        presence.set(Some(activity("first one")));
        assert_eq!(next(&received).0, 0);
        assert_eq!(
            next(&received).1["args"]["activity"]["details"],
            "first one"
        );

        presence.set(None);
        assert!(next(&received).1["args"]["activity"].is_null());
        presence.set(Some(activity("second one")));
        assert_eq!(
            next(&received).1["args"]["activity"]["details"],
            "second one"
        );

        presence.shutdown();
        assert!(next(&received).1["args"]["activity"].is_null());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bursts_are_coalesced_to_the_latest_request() {
        let path = temp_socket();
        let received = fake_discord(&path);
        let presence = Presence::start(fast_settings(&path));
        presence.set(Some(activity("one one")));
        next(&received); // handshake
        next(&received); // one one
        presence.set(Some(activity("two two")));
        presence.set(Some(activity("three three")));
        assert_eq!(
            next(&received).1["args"]["activity"]["details"],
            "three three"
        );
        assert!(received.recv_timeout(Duration::from_millis(500)).is_err());
        presence.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn worker_connects_once_discord_appears() {
        let path = temp_socket();
        let presence = Presence::start(fast_settings(&path));
        presence.set(Some(activity("waiting up")));
        thread::sleep(Duration::from_millis(150));
        let received = fake_discord(&path);
        assert_eq!(next(&received).0, 0);
        assert_eq!(
            next(&received).1["args"]["activity"]["details"],
            "waiting up"
        );
        presence.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn worker_does_not_connect_just_to_clear() {
        let path = temp_socket();
        let received = fake_discord(&path);
        let presence = Presence::start(fast_settings(&path));
        presence.set(None);
        assert!(received.recv_timeout(Duration::from_millis(300)).is_err());
        presence.shutdown();
        let _ = std::fs::remove_file(&path);
    }
}
