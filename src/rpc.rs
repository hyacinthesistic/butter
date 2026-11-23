//! BloxstrapRPC receiver.
//!
//! Roblox games send messages by printing a line like
//!
//! ```text
//! [BloxstrapRPC] {"command":"SetRichPresence","data":{"details":"hi"}}
//! ```
//!
//! This module turns raw console/log bytes into those messages and keeps the
//! sticky presence state the protocol describes. It knows nothing about
//! Discord, GTK or Butter, so it can be unit-tested on its own.
//!
//! Trust model: the text comes from whatever game the player joined, so
//! everything is length-capped and stripped of control characters.

use serde_json::Value;

/// Marker that precedes the JSON payload.
pub const MARKER: &str = "[BloxstrapRPC]";

/// Longest line we will look at. Longer lines are skipped entirely.
const MAX_LINE: usize = 16 * 1024;
/// Discord limits details/state to 128 characters.
const MAX_TEXT_CHARS: usize = 128;
/// Conservative cap for launch data.
const MAX_LAUNCH_DATA_CHARS: usize = 200;

/// A text field of the presence.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Text {
    /// Not set by the game: use the launcher's default.
    #[default]
    Default,
    /// The game erased it (empty string).
    Hidden,
    /// The game set it.
    Set(String),
}

/// One of the two presence images.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Image {
    /// `clear = true` hides the image until a `reset = true`.
    pub hidden: bool,
    /// Roblox asset id, if the game chose one.
    pub asset_id: Option<u64>,
    pub hover_text: Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Presence {
    pub details: Text,
    pub state: Text,
    pub large_image: Image,
    pub small_image: Image,
}

/// Parser plus the state it maintains.
#[derive(Debug, Default)]
pub struct Rpc {
    presence: Presence,
    launch_data: Option<String>,
}

impl Rpc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn presence(&self) -> &Presence {
        &self.presence
    }

    pub fn launch_data(&self) -> Option<&str> {
        self.launch_data.as_deref()
    }

    /// Forget everything, e.g. when the player leaves the game.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Look at one output line. Returns true if the state changed.
    pub fn feed_line(&mut self, line: &str) -> bool {
        if line.len() > MAX_LINE {
            return false;
        }
        let Some(at) = line.find(MARKER) else {
            return false;
        };
        let rest = line[at + MARKER.len()..].trim();
        // Tolerate trailing junk after the JSON value.
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        let Some(Ok(message)) = values.next() else {
            return false;
        };
        let Some(command) = message.get("command").and_then(Value::as_str) else {
            return false;
        };
        let data = message.get("data").unwrap_or(&Value::Null);
        match command {
            "SetRichPresence" => apply_presence(&mut self.presence, data),
            "SetLaunchData" => match data.as_str() {
                Some(text) => {
                    let new = Some(clean(text, MAX_LAUNCH_DATA_CHARS));
                    let changed = new != self.launch_data;
                    self.launch_data = new;
                    changed
                }
                None => false,
            },
            _ => false,
        }
    }
}

fn apply_presence(presence: &mut Presence, data: &Value) -> bool {
    let Some(fields) = data.as_object() else {
        return false;
    };
    let before = presence.clone();
    set_text(fields.get("details"), &mut presence.details);
    set_text(fields.get("state"), &mut presence.state);
    if let Some(image) = fields.get("largeImage") {
        apply_image(image, &mut presence.large_image);
    }
    if let Some(image) = fields.get("smallImage") {
        apply_image(image, &mut presence.small_image);
    }
    *presence != before
}

fn apply_image(value: &Value, image: &mut Image) {
    // Lua serialises an empty table as `[]`, so a non-object is ignored.
    let Some(fields) = value.as_object() else {
        return;
    };
    if fields.get("reset").and_then(Value::as_bool) == Some(true) {
        *image = Image::default();
    }
    if fields.get("clear").and_then(Value::as_bool) == Some(true) {
        image.hidden = true;
    }
    if let Some(id) = fields.get("assetId").and_then(asset_id) {
        image.asset_id = Some(id);
    }
    set_text(fields.get("hoverText"), &mut image.hover_text);
}

/// Non-string or missing values leave the field alone (sticky state).
fn set_text(value: Option<&Value>, slot: &mut Text) {
    let Some(Value::String(raw)) = value else {
        return;
    };
    let text = clean(raw, MAX_TEXT_CHARS);
    *slot = if text == "<reset>" {
        Text::Default
    } else if text.is_empty() {
        Text::Hidden
    } else {
        Text::Set(text)
    };
}

/// Accepts a number (also the float form Lua may produce), "123" or
/// "rbxassetid://123".
fn asset_id(value: &Value) -> Option<u64> {
    let id = match value {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 1.0 && *f < 1.8e19 && f.fract() == 0.0)
                .map(|f| f as u64)
        }),
        Value::String(s) => {
            let s = s.trim();
            s.strip_prefix("rbxassetid://").unwrap_or(s).parse().ok()
        }
        _ => None,
    };
    id.filter(|&id| id > 0)
}

/// Drop control characters, trim, and cap the length in characters.
fn clean(text: &str, max_chars: usize) -> String {
    let filtered: String = text.chars().filter(|c| !c.is_control()).collect();
    filtered.trim().chars().take(max_chars).collect()
}

/// Turns arbitrary chunks of bytes into lines. A chunk may end mid-line, and
/// terminals add `\r`.
#[derive(Debug, Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
    skipping: bool,
}

impl LineSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8], mut each: impl FnMut(&str)) {
        for &byte in bytes {
            if byte == b'\n' {
                if !self.skipping {
                    let text = String::from_utf8_lossy(&self.buf);
                    each(text.trim_end_matches('\r'));
                }
                self.buf.clear();
                self.skipping = false;
            } else if !self.skipping {
                if self.buf.len() >= MAX_LINE {
                    // Too long to be ours: drop it up to the next newline.
                    self.buf.clear();
                    self.skipping = true;
                } else {
                    self.buf.push(byte);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(s: &str) -> Text {
        Text::Set(s.to_string())
    }

    #[test]
    fn parses_set_rich_presence_from_a_log_line() {
        let mut rpc = Rpc::new();
        let line = r#"2026-11-22T15:04:07.100Z,12.3,abc,6 [FLog::Output] [BloxstrapRPC] {"command":"SetRichPresence","data":{"details":"Wave 5","state":"Alive","largeImage":{"assetId":10630555127,"hoverText":"Boss"}}}"#;
        assert!(rpc.feed_line(line));
        let p = rpc.presence();
        assert_eq!(p.details, set("Wave 5"));
        assert_eq!(p.state, set("Alive"));
        assert_eq!(p.large_image.asset_id, Some(10630555127));
        assert_eq!(p.large_image.hover_text, set("Boss"));
    }

    #[test]
    fn state_is_sticky_between_commands() {
        let mut rpc = Rpc::new();
        rpc.feed_line(
            r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"details":"A","state":"B"}}"#,
        );
        assert!(
            rpc.feed_line(r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"state":"C"}}"#)
        );
        assert_eq!(rpc.presence().details, set("A"));
        assert_eq!(rpc.presence().state, set("C"));
    }

    #[test]
    fn same_command_twice_reports_no_change() {
        let mut rpc = Rpc::new();
        let line = r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"details":"A"}}"#;
        assert!(rpc.feed_line(line));
        assert!(!rpc.feed_line(line));
    }

    #[test]
    fn empty_string_erases_and_reset_marker_restores_default() {
        let mut rpc = Rpc::new();
        rpc.feed_line(r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"details":"A"}}"#);
        rpc.feed_line(r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"details":""}}"#);
        assert_eq!(rpc.presence().details, Text::Hidden);
        rpc.feed_line(
            r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"details":"<reset>"}}"#,
        );
        assert_eq!(rpc.presence().details, Text::Default);
    }

    #[test]
    fn asset_ids_accept_number_float_string_and_rbxassetid() {
        for raw in ["123", "123.0", "1.23e2", "\"123\"", "\"rbxassetid://123\""] {
            let mut rpc = Rpc::new();
            let line = format!(
                r#"[BloxstrapRPC] {{"command":"SetRichPresence","data":{{"largeImage":{{"assetId":{raw}}}}}}}"#
            );
            assert!(rpc.feed_line(&line), "{raw}");
            assert_eq!(rpc.presence().large_image.asset_id, Some(123), "{raw}");
        }
        let mut rpc = Rpc::new();
        rpc.feed_line(
            r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"largeImage":{"assetId":-5}}}"#,
        );
        assert_eq!(rpc.presence().large_image.asset_id, None);
    }

    #[test]
    fn image_clear_hides_until_reset() {
        let mut rpc = Rpc::new();
        rpc.feed_line(
            r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"smallImage":{"assetId":7}}}"#,
        );
        rpc.feed_line(
            r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"smallImage":{"clear":true}}}"#,
        );
        assert!(rpc.presence().small_image.hidden);
        // Setting an asset does not unhide it.
        rpc.feed_line(
            r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"smallImage":{"assetId":8}}}"#,
        );
        assert!(rpc.presence().small_image.hidden);
        rpc.feed_line(
            r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"smallImage":{"reset":true}}}"#,
        );
        assert_eq!(rpc.presence().small_image, Image::default());
    }

    #[test]
    fn empty_lua_table_image_is_ignored() {
        let mut rpc = Rpc::new();
        assert!(!rpc
            .feed_line(r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"largeImage":[]}}"#));
    }

    #[test]
    fn launch_data_is_stored_and_capped() {
        let mut rpc = Rpc::new();
        assert!(rpc.feed_line(r#"[BloxstrapRPC] {"command":"SetLaunchData","data":"party=42"}"#));
        assert_eq!(rpc.launch_data(), Some("party=42"));
        let long = "x".repeat(1000);
        rpc.feed_line(&format!(
            r#"[BloxstrapRPC] {{"command":"SetLaunchData","data":"{long}"}}"#
        ));
        assert_eq!(
            rpc.launch_data().unwrap().chars().count(),
            MAX_LAUNCH_DATA_CHARS
        );
    }

    #[test]
    fn text_is_capped_and_control_characters_removed() {
        let mut rpc = Rpc::new();
        let long = "é".repeat(500);
        rpc.feed_line(&format!(
          r#"[BloxstrapRPC] {{"command":"SetRichPresence","data":{{"details":"{long}","state":"a\u001b[31mb\nc"}}}}"#
      ));
        let Text::Set(details) = &rpc.presence().details else {
            panic!()
        };
        assert_eq!(details.chars().count(), MAX_TEXT_CHARS);
        assert_eq!(rpc.presence().state, set("a[31mbc"));
    }

    #[test]
    fn garbage_and_unknown_commands_are_ignored() {
        let mut rpc = Rpc::new();
        for line in [
            "nothing to see here",
            "[BloxstrapRPC] not json",
            "[BloxstrapRPC] {\"command\":\"Nope\",\"data\":{}}",
            "[BloxstrapRPC] {\"data\":{}}",
            "[BloxstrapRPC] {\"command\":\"SetRichPresence\",\"data\":5}",
            "[BloxstrapRPC] {\"command\":\"SetRichPresence\",\"data\":{\"details\":5}}",
        ] {
            assert!(!rpc.feed_line(line), "{line}");
        }
        assert_eq!(*rpc.presence(), Presence::default());
    }

    #[test]
    fn trailing_junk_after_the_json_is_tolerated() {
        let mut rpc = Rpc::new();
        assert!(rpc.feed_line(
            r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"details":"A"}} trailing"#
        ));
    }

    #[test]
    fn oversized_lines_are_skipped() {
        let mut rpc = Rpc::new();
        let line = format!(
            r#"[BloxstrapRPC] {{"command":"SetRichPresence","data":{{"details":"{}"}}}}"#,
            "a".repeat(MAX_LINE)
        );
        assert!(!rpc.feed_line(&line));
    }

    #[test]
    fn reset_forgets_everything() {
        let mut rpc = Rpc::new();
        rpc.feed_line(r#"[BloxstrapRPC] {"command":"SetRichPresence","data":{"details":"A"}}"#);
        rpc.feed_line(r#"[BloxstrapRPC] {"command":"SetLaunchData","data":"x"}"#);
        rpc.reset();
        assert_eq!(*rpc.presence(), Presence::default());
        assert_eq!(rpc.launch_data(), None);
    }

    fn collect(splitter: &mut LineSplitter, chunk: &[u8], out: &mut Vec<String>) {
        splitter.push(chunk, |l| out.push(l.to_string()));
    }

    #[test]
    fn splitter_handles_chunks_crlf_and_partial_lines() {
        let mut s = LineSplitter::new();
        let mut out = Vec::new();
        collect(&mut s, b"first li", &mut out);
        assert!(out.is_empty());
        collect(&mut s, b"ne\r\nsecond\n\nthi", &mut out);
        collect(&mut s, b"rd\r\n", &mut out);
        assert_eq!(out, ["first line", "second", "", "third"]);
    }

    #[test]
    fn splitter_splits_a_multibyte_char_across_chunks() {
        let mut s = LineSplitter::new();
        let mut out = Vec::new();
        let bytes = "héllo\n".as_bytes();
        collect(&mut s, &bytes[..2], &mut out); // cuts "é" in half
        collect(&mut s, &bytes[2..], &mut out);
        assert_eq!(out, ["héllo"]);
    }

    #[test]
    fn splitter_drops_overlong_lines_but_recovers() {
        let mut s = LineSplitter::new();
        let mut out = Vec::new();
        collect(&mut s, &vec![b'a'; MAX_LINE + 10], &mut out);
        collect(&mut s, b"tail\nok\n", &mut out);
        assert_eq!(out, ["ok"]);
    }

    #[test]
    fn end_to_end_from_chunks() {
        let mut s = LineSplitter::new();
        let mut rpc = Rpc::new();
        let data = b"noise\r\n2026-11-22T15:00:00.000Z,1,2,6 [FLog::Output] [BloxstrapRPC] {\"command\":\"SetRichPresence\",\"data\":{\"state\":\"Lobby\"}}\r\n";
        let mut changed = false;
        for chunk in data.chunks(7) {
            s.push(chunk, |l| changed |= rpc.feed_line(l));
        }
        assert!(changed);
        assert_eq!(rpc.presence().state, set("Lobby"));
    }
}
