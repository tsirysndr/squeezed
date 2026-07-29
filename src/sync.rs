//! Multiroom synchronization engine.
//!
//! Goal: every connected player renders the *same* absolute audio sample at the
//! same wall-clock instant, and stays that way despite each device running on
//! its own crystal. The mechanism (verified against squeezelite's source) is:
//!
//! 1. **Clock offset** — periodically send `strm 't'`; the player answers with a
//!    `STMt` carrying its clock (`jiffies`) and echoing our timestamp. That's an
//!    NTP-style round trip: `offset = (S0+S1)/2 - jiffies`, kept from the
//!    lowest-RTT probe so network jitter doesn't poison it.
//!
//! 2. **Absolute playhead** — the player reports `elapsed_ms` (audio played
//!    since its HTTP stream began). We recorded the absolute byte position of
//!    the live write head when it connected (`b_start`), so its absolute content
//!    position is `H = b_start/bytes_per_ms + elapsed_ms`. Its sync **anchor**
//!    is `Epoch = (jiffies + offset) - H` — the server-clock time at which it
//!    would have played content position 0. Equal anchors == perfectly aligned.
//!
//! 3. **Correction** — the most-advanced player (minimum `Epoch`) is the
//!    reference frontier; every other player is behind by `Epoch - refEpoch` ms
//!    and is told to `strm 'a'` skip-ahead by that much. Skip-ahead is the only
//!    primitive used: squeezelite advances `frames_played` by the skipped amount,
//!    so `elapsed_ms` stays a faithful content clock and the model never drifts
//!    from reality (a `pause` would not have that property).

use crate::audio::AudioFormat;
use crate::slim;
use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Don't nudge a player unless it's off by more than this (audio-frame noise).
const DEADBAND_MS: f64 = 10.0;
/// Minimum gap between corrections to one player, so a skip fully applies and
/// we re-measure before deciding again.
const COOLDOWN_MS: u32 = 3000;
/// A player must have been playing at least this long before it's eligible, so
/// `elapsed_ms` is meaningful and its output pipeline has settled.
const MIN_ELAPSED_MS: u32 = 2000;
/// Cap a single skip; larger initial errors converge over successive rounds
/// rather than making one big audible jump.
const MAX_SKIP_MS: u32 = 1000;
/// How many recent probes to keep per player for min-RTT offset selection.
const PROBE_WINDOW: usize = 8;

/// Server clock in milliseconds — the same base we stamp into `strm 't'`
/// probes and compare against player `jiffies` (via the measured offset).
pub fn server_ms() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

struct Probe {
    rtt: u32,
    offset: i64,
}

struct Player {
    name: String,
    join_seq: u64,
    /// Write half of this player's SlimProto socket, for sending corrections.
    writer: Arc<Mutex<TcpStream>>,
    probes: Vec<Probe>,
    offset_ms: Option<i64>,
    /// Absolute byte position of the live head when this player's HTTP
    /// connection subscribed (`None` until the HTTP stream attaches).
    b_start_bytes: Option<u64>,
    last_jiffies: u32,
    last_elapsed: u32,
    last_correction_ms: u32,
}

impl Player {
    /// The player's sync anchor, if it has everything needed to compute one.
    fn epoch(&self, bytes_per_ms: f64) -> Option<f64> {
        let offset = self.offset_ms?;
        let b_start = self.b_start_bytes?;
        if self.last_elapsed < MIN_ELAPSED_MS {
            return None; // not playing steadily yet
        }
        let h = b_start as f64 / bytes_per_ms + self.last_elapsed as f64;
        Some((self.last_jiffies as i64 + offset) as f64 - h)
    }
}

/// Shared, thread-safe registry of players plus the correction logic.
pub struct SyncManager {
    players: Mutex<HashMap<String, Player>>,
    bytes_per_ms: f64,
    enabled: bool,
    join_counter: AtomicU64,
}

impl SyncManager {
    pub fn new(format: AudioFormat, enabled: bool) -> Arc<Self> {
        Arc::new(SyncManager {
            players: Mutex::new(HashMap::new()),
            bytes_per_ms: format.byte_rate() as f64 / 1000.0,
            enabled,
            join_counter: AtomicU64::new(0),
        })
    }

    /// Register a player at HELO time. `mac` correlates its SlimProto and HTTP
    /// connections (we embed it in the HTTP request URL).
    pub fn add_player(&self, mac: String, name: String, writer: Arc<Mutex<TcpStream>>) {
        let join_seq = self.join_counter.fetch_add(1, Ordering::AcqRel);
        self.players.lock().unwrap().insert(
            mac,
            Player {
                name,
                join_seq,
                writer,
                probes: Vec::new(),
                offset_ms: None,
                b_start_bytes: None,
                last_jiffies: 0,
                last_elapsed: 0,
                last_correction_ms: 0,
            },
        );
    }

    pub fn remove_player(&self, mac: &str) {
        self.players.lock().unwrap().remove(mac);
    }

    /// Record where in the global stream this player's HTTP playback begins.
    pub fn set_http_start(&self, mac: &str, b_start_bytes: u64) {
        if let Some(p) = self.players.lock().unwrap().get_mut(mac) {
            p.b_start_bytes = Some(b_start_bytes);
            tracing::info!(
                "sync: {} ({mac}) HTTP attached at byte {b_start_bytes}",
                p.name
            );
        }
    }

    /// Feed in a `STMt` report. `server_ts` is the echoed probe timestamp (0 for
    /// unprompted heartbeats); `recv_ms` is our clock when the report arrived.
    pub fn on_stmt(&self, mac: &str, jiffies: u32, elapsed_ms: u32, server_ts: u32, recv_ms: u32) {
        let mut players = self.players.lock().unwrap();
        let Some(p) = players.get_mut(mac) else {
            return;
        };

        p.last_jiffies = jiffies;
        p.last_elapsed = elapsed_ms;

        // A non-zero echo means this STMt answers one of our `strm 't'` probes.
        if server_ts != 0 {
            let rtt = recv_ms.wrapping_sub(server_ts);
            // Midpoint of send/recv in server time, minus the player's clock.
            let midpoint = server_ts.wrapping_add(rtt / 2) as i64;
            let offset = midpoint - jiffies as i64;
            p.probes.push(Probe { rtt, offset });
            if p.probes.len() > PROBE_WINDOW {
                p.probes.remove(0);
            }
            // Best estimate = the offset from the lowest-RTT probe.
            p.offset_ms = p.probes.iter().min_by_key(|pr| pr.rtt).map(|pr| pr.offset);
        }

        if self.enabled {
            self.evaluate(&mut players);
        }
    }

    /// Compute anchors and skip-correct every player that's behind the frontier.
    fn evaluate(&self, players: &mut HashMap<String, Player>) {
        let snaps: Vec<Snapshot> = players
            .iter()
            .filter_map(|(mac, p)| {
                p.epoch(self.bytes_per_ms).map(|epoch| Snapshot {
                    mac: mac.clone(),
                    join_seq: p.join_seq,
                    epoch,
                    last_correction_ms: p.last_correction_ms,
                })
            })
            .collect();

        let now = server_ms();
        for decision in decide_skips(&snaps, now) {
            let Some(p) = players.get_mut(&decision.mac) else {
                continue;
            };
            let mut stream = p.writer.lock().unwrap();
            match slim::send_strm_command(&mut stream, b'a', decision.skip_ms) {
                Ok(()) => {
                    drop(stream);
                    p.last_correction_ms = now;
                    tracing::info!(
                        "sync: {} behind by {:.0}ms → skip-ahead {}ms",
                        p.name,
                        decision.error_ms,
                        decision.skip_ms
                    );
                }
                Err(e) => tracing::debug!("sync: skip send to {} failed: {e}", p.name),
            }
        }
    }
}

/// A player's sync state at one instant, for the (pure) decision function.
struct Snapshot {
    mac: String,
    join_seq: u64,
    epoch: f64,
    last_correction_ms: u32,
}

/// One correction to apply.
struct SkipDecision {
    mac: String,
    skip_ms: u32,
    error_ms: f64,
}

/// Decide which players to skip-ahead, given every eligible player's anchor.
///
/// Pure and deterministic (no clock, no I/O) so the alignment logic can be
/// unit-tested. The frontier (minimum `epoch`) is the reference; any player
/// behind it by more than the deadband, and off cooldown, is skipped forward by
/// the error (capped). Ties in `epoch` never produce a correction on the leader.
fn decide_skips(snaps: &[Snapshot], now: u32) -> Vec<SkipDecision> {
    if snaps.len() < 2 {
        return Vec::new();
    }
    let ref_epoch = snaps.iter().map(|s| s.epoch).fold(f64::INFINITY, f64::min);

    let mut ordered: Vec<&Snapshot> = snaps.iter().collect();
    ordered.sort_by_key(|s| s.join_seq);

    let mut out = Vec::new();
    for s in ordered {
        let error = s.epoch - ref_epoch; // >= 0: behind the frontier by `error` ms
        if error <= DEADBAND_MS {
            continue;
        }
        if now.wrapping_sub(s.last_correction_ms) < COOLDOWN_MS {
            continue;
        }
        let skip = (error.round() as u32).min(MAX_SKIP_MS);
        if skip == 0 {
            continue;
        }
        out.push(SkipDecision {
            mac: s.mac.clone(),
            skip_ms: skip,
            error_ms: error,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(mac: &str, join: u64, epoch: f64, last_corr: u32) -> Snapshot {
        Snapshot {
            mac: mac.into(),
            join_seq: join,
            epoch,
            last_correction_ms: last_corr,
        }
    }

    #[test]
    fn single_player_never_corrected() {
        let snaps = vec![snap("a", 0, 1000.0, 0)];
        assert!(decide_skips(&snaps, 100_000).is_empty());
    }

    #[test]
    fn behind_player_skips_by_error_and_leader_untouched() {
        // b's epoch is 50ms greater => b is 50ms behind the frontier (a).
        let snaps = vec![snap("a", 0, 1000.0, 0), snap("b", 1, 1050.0, 0)];
        let d = decide_skips(&snaps, 100_000);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].mac, "b");
        assert_eq!(d[0].skip_ms, 50);
    }

    #[test]
    fn within_deadband_no_correction() {
        let snaps = vec![snap("a", 0, 1000.0, 0), snap("b", 1, 1005.0, 0)];
        assert!(decide_skips(&snaps, 100_000).is_empty());
    }

    #[test]
    fn cooldown_suppresses_repeat() {
        // b behind by 50ms but corrected 500ms ago (< COOLDOWN) => suppressed.
        let now = 100_000;
        let snaps = vec![snap("a", 0, 1000.0, 0), snap("b", 1, 1050.0, now - 500)];
        assert!(decide_skips(&snaps, now).is_empty());
    }

    #[test]
    fn large_error_capped() {
        let snaps = vec![snap("a", 0, 0.0, 0), snap("b", 1, 5000.0, 0)];
        let d = decide_skips(&snaps, 100_000);
        assert_eq!(d[0].skip_ms, MAX_SKIP_MS);
    }

    #[test]
    fn three_players_two_behind_the_frontier() {
        let snaps = vec![
            snap("a", 0, 1000.0, 0), // frontier
            snap("b", 1, 1030.0, 0), // 30ms behind
            snap("c", 2, 1080.0, 0), // 80ms behind
        ];
        let d = decide_skips(&snaps, 100_000);
        assert_eq!(d.len(), 2);
        let by_mac: std::collections::HashMap<_, _> =
            d.iter().map(|x| (x.mac.as_str(), x.skip_ms)).collect();
        assert_eq!(by_mac["b"], 30);
        assert_eq!(by_mac["c"], 80);
    }
}
