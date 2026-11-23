//! Join/leave tracking from Roblox's log lines.

const HARD_LEAVE: &str = "[FLog::SingleSurfaceApp] leaveUGCGameInternal";
const SOFT_LEAVE: &str = "[FLog::Network] Time to disconnect replication data:";

const JOIN_REQUEST: &str = "GameJoinUtil::joinGamePost";
const JOIN_START: &str = "[FLog::GameJoinUtil] GameJoinUtil::joinGame";
const CONNECTED: &str = "[FLog::Network] serverId:";

/// A game the player is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub place_id: u64,
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Connected to a game (also reported when moving to another one).
    Joined(Game),
    Left,
}

#[derive(Debug, Default)]
struct Pending {
    place_id: Option<u64>,
    job_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct Activity {
    pending: Option<Pending>,
    current: Option<Game>,
    /// Kept across a plain disconnect so a reconnect can show it again.
    last: Option<Game>,
}

impl Activity {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current(&self) -> Option<&Game> {
        self.current.as_ref()
    }

    /// Look at one log line. Returns what changed, if anything.
    pub fn feed_line(&mut self, line: &str) -> Option<Change> {
        if line.contains(JOIN_REQUEST) {
            let place_id = number_after(line, "\"placeId\":");
            let pending = self.pending.get_or_insert_with(Pending::default);
            if place_id.is_some() {
                pending.place_id = place_id;
            }
            return None;
        }
        if line.ends_with(JOIN_START) {
            // A new attempt: forget the half-finished one.
            self.pending = Some(Pending::default());
            return None;
        }
        if let Some(pending) = &mut self.pending {
            if let Some(job) = quoted_id_after(line, "\"jobId\":\"") {
                pending.job_id = Some(job);
                return None;
            }
        }
        if line.contains(CONNECTED) {
            let game = match self.pending.take() {
                Some(pending) => Game {
                    place_id: pending.place_id?,
                    job_id: pending.job_id,
                },
                // No new join request: a reconnect after a plain disconnect.
                None if self.current.is_none() => self.last.clone()?,
                None => return None,
            };
            if self.current.as_ref() == Some(&game) {
                return None;
            }
            self.last = Some(game.clone());
            self.current = Some(game.clone());
            return Some(Change::Joined(game));
        }
        if line.contains(HARD_LEAVE) {
            self.pending = None;
            self.last = None;
            return self.current.take().map(|_| Change::Left);
        }
        if line.contains(SOFT_LEAVE) {
            // Don't touch `pending`: during a teleport the old game's disconnect
            // can arrive after the new join already started.
            return self.current.take().map(|_| Change::Left);
        }
        None
    }
}

fn number_after(line: &str, key: &str) -> Option<u64> {
    let at = line.find(key)? + key.len();
    let digits: String = line[at..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok().filter(|&n| n > 0)
}

/// A UUID-like value right after `key`, closed by a quote.
fn quoted_id_after(line: &str, key: &str) -> Option<String> {
    let at = line.find(key)? + key.len();
    let rest = &line[at..];
    let value: String = rest
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();
    let closed = rest[value.len()..].starts_with('"');
    (closed && (8..=64).contains(&value.len())).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOIN: &str = "2026-11-22T23:00:39.853Z,120.853806,e6e70000,6,Info [FLog::GameJoinUtil] GameJoinUtil::joinGame";
    const POST: &str = r#"2026-11-22T23:00:39.853Z,120.853935,e6e70000,6,Info [FLog::GameJoinUtil] GameJoinUtil::joinGamePostStandard: URL: https://gamejoin.roblox.com/v1/join-game BODY: {"placeId":189707,"gameJoinAttemptId":"6ebf5402-59d4-40ee-9"#;
    const RESPONSE: &str = r#"status code: {"jobId":"7e12d67f-dc50-410e-82dd-acf7fdf8975f","status":2,"statusData":null,"joinScriptUrl":"https://assetgame.roblox.com/Game/Join.ashx?ticketVersion=2&ticket=..."}"#;
    const UDMUX: &str = "2026-11-22T23:00:40.340Z,121.340073,e6ff0000,7 [FLog::Network] UDMUX Address = 128.116.50.33, Port = 52152 | RCC Server Address = 10.20.7.209, Port = 52152";
    const SERVER: &str = "2026-11-22T23:00:40.427Z,121.427864,e6ff0000,7 [FLog::Network] serverId: 128.116.50.33|52152";
    const SOFT: &str = "2026-11-22T23:05:00.000Z,400.0,e6ff0000,7 [FLog::Network] Time to disconnect replication data: 0.01";
    const HARD: &str = "2026-11-22T23:05:00.000Z,400.0,e6ff0000,6 [FLog::SingleSurfaceApp] leaveUGCGameInternal";

    fn feed_all(activity: &mut Activity, lines: &[&str]) -> Vec<Change> {
        lines.iter().filter_map(|l| activity.feed_line(l)).collect()
    }

    fn expected_game() -> Game {
        Game {
            place_id: 189707,
            job_id: Some("7e12d67f-dc50-410e-82dd-acf7fdf8975f".to_string()),
        }
    }

    #[test]
    fn real_join_sequence_yields_one_joined_event() {
        let mut a = Activity::new();
        let changes = feed_all(&mut a, &[JOIN, POST, RESPONSE, UDMUX, SERVER]);
        assert_eq!(changes, [Change::Joined(expected_game())]);
        assert_eq!(a.current(), Some(&expected_game()));
    }

    #[test]
    fn repeated_server_line_does_not_report_again() {
        let mut a = Activity::new();
        feed_all(&mut a, &[JOIN, POST, RESPONSE, SERVER]);
        assert_eq!(feed_all(&mut a, &[SERVER, UDMUX]), []);
    }

    #[test]
    fn connection_without_a_join_request_is_ignored() {
        // For example Butter attached mid-game and only saw the tail.
        let mut a = Activity::new();
        assert_eq!(feed_all(&mut a, &[UDMUX, SERVER]), []);
        assert_eq!(a.current(), None);
    }

    #[test]
    fn a_new_attempt_discards_the_failed_one() {
        let mut a = Activity::new();
        let other_post = POST.replace("189707", "555");
        // First attempt never connects, second one does.
        let changes = feed_all(&mut a, &[JOIN, &other_post, JOIN, POST, RESPONSE, SERVER]);
        assert_eq!(changes, [Change::Joined(expected_game())]);
    }

    #[test]
    fn leaving_is_reported_once() {
        let mut a = Activity::new();
        feed_all(&mut a, &[JOIN, POST, RESPONSE, SERVER]);
        assert_eq!(a.feed_line(SOFT), Some(Change::Left));
        assert_eq!(a.feed_line(SOFT), None);
        assert_eq!(a.current(), None);
    }

    #[test]
    fn teleport_reports_the_new_game() {
        let mut a = Activity::new();
        feed_all(&mut a, &[JOIN, POST, RESPONSE, SERVER]);
        let post2 = POST.replace("189707", "321");
        let response2 = RESPONSE.replace("7e12d67f", "aaaaaaaa");
        let changes = feed_all(&mut a, &[JOIN, &post2, &response2, SERVER]);
        assert_eq!(
            changes,
            [Change::Joined(Game {
                place_id: 321,
                job_id: Some("aaaaaaaa-dc50-410e-82dd-acf7fdf8975f".to_string()),
            })]
        );
    }

    #[test]
    fn job_id_is_optional() {
        let mut a = Activity::new();
        let changes = feed_all(&mut a, &[JOIN, POST, SERVER]);
        assert_eq!(
            changes,
            [Change::Joined(Game { place_id: 189707, job_id: None })]
        );
    }

    #[test]
    fn junk_is_ignored() {
        let mut a = Activity::new();
        let bad_place = POST.replace("189707", "abc");
        let bad_job = RESPONSE.replace("7e12d67f-dc50-410e-82dd-acf7fdf8975f", "zz");
        assert_eq!(feed_all(&mut a, &["", "hello", &bad_place, &bad_job, SERVER]), []);
        assert_eq!(a.current(), None);
    }

    #[test]
    fn job_id_outside_a_join_is_not_picked_up() {
        let mut a = Activity::new();
        // No pending join: the response alone must not start anything.
        assert_eq!(feed_all(&mut a, &[RESPONSE, SERVER]), []);
    }

    #[test]
    fn reconnect_after_a_plain_disconnect_shows_the_game_again() {
        let mut a = Activity::new();
        feed_all(&mut a, &[JOIN, POST, RESPONSE, SERVER]);
        assert_eq!(a.feed_line(SOFT), Some(Change::Left)); // plain disconnect
        assert_eq!(a.feed_line(SERVER), Some(Change::Joined(expected_game())));
    }

    #[test]
    fn a_real_leave_forgets_the_game() {
        let mut a = Activity::new();
        feed_all(&mut a, &[JOIN, POST, RESPONSE, SERVER]);
        assert_eq!(a.feed_line(HARD), Some(Change::Left));
        assert_eq!(a.feed_line(SERVER), None);
    }

    #[test]
    fn real_log_from_butter_keeps_the_game_across_the_mid_session_reconnect() {
        let join = "2026-11-23T05:43:11.140Z,4.140076,e6d30000,6,Info [FLog::GameJoinUtil] GameJoinUtil::joinGame";
        let post = r#"2026-11-23T05:43:11.140Z,4.140203,e6d30000,6,Info [FLog::GameJoinUtil] GameJoinUtil::joinGamePostSpecificUser: URL: https://gamejoin.roblox.com/v1/play-with-user BODY: {"userIdToFollow":4911849246,"placeId":79862723087324,"gameJoinAttemptId":"48678a36-236e-42fe-a6c6-737b93e94f01","joinOrigin":"peopleListInHomePage"}"#;
        let s1 = "2026-11-23T05:43:12.061Z,5.061123,e6ee0000,7 [FLog::Network] serverId: 128.116.48.33|49616";
        let d = "2026-11-23T05:43:18.614Z,11.614579,eeed0000,7 [FLog::Network] Time to disconnect replication data: 0.062794";
        let s2 = "2026-11-23T05:43:19.118Z,12.118072,e6ee0000,7 [FLog::Network] serverId: 128.116.44.33|63913";
        let l1 = "2026-11-23T06:10:13.331Z,1626.331787,e6d30000,6 [FLog::SingleSurfaceApp] leaveUGCGame: (stage:UGCGame) blocking=0.";
        let l2 = "2026-11-23T06:10:13.331Z,1626.331787,e6d30000,6 [FLog::SingleSurfaceApp] leaveUGCGameInternal";
        let l3 = "2026-11-23T06:10:13.421Z,1626.421997,e6ee0000,7 [FLog::Network] Time to disconnect replication data: 0.380874";
        let mut a = Activity::new();
        let g = Game { place_id: 79862723087324, job_id: None };
        let changes = feed_all(&mut a, &[join, post, s1, d, s2, l1, l2, l3]);
        assert_eq!(changes, [Change::Joined(g.clone()), Change::Left, Change::Joined(g), Change::Left]);
        assert_eq!(a.current(), None);
    }

    #[test]
    fn a_new_join_after_a_plain_disconnect_wins_over_the_remembered_game() {
        let mut a = Activity::new();
        feed_all(&mut a, &[JOIN, POST, RESPONSE, SERVER, SOFT]);
        let post2 = POST.replace("189707", "321");
        let changes = feed_all(&mut a, &[JOIN, &post2, SERVER]);
        assert_eq!(changes, [Change::Joined(Game { place_id: 321, job_id: None })]);
    }

    #[test]
    fn a_plain_disconnect_does_not_discard_a_join_in_progress() {
        let mut a = Activity::new();
        let changes = feed_all(&mut a, &[JOIN, POST, SOFT, SERVER]);
        assert_eq!(changes, [Change::Joined(Game { place_id: 189707, job_id: None })]);
    }
}
