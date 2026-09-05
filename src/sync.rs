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
//! 3. **Correction** — the most-*delayed* player (maximum `Epoch`) is the
//!    reference; every other player is *ahead* of it by `refEpoch - Epoch` ms
//!    and is told to `strm 'p'` (timed pause) for that long. Squeezelite plays
//!    the pause as inserted silence, in full, without touching its buffered
//!    audio — so the paused player's buffer *grows* by the pause length, and
//!    `elapsed_ms` (which counts only real frames) stays a faithful content
//!    clock, keeping the model honest.
//!
//! Why pause-the-leaders instead of skip-the-laggards: for a *live* stream the
//! most-advanced player is by definition the one rendering closest to the live
//! write head, i.e. the one with the least audio buffered. Skipping the others
//! forward (`strm 'a'`) would drain their buffers down to the leader's — near
//! zero — so any WiFi hiccup underruns them, they fall behind, get skipped
//! again, and the group stutters forever instead of locking. (Squeezelite also
//! applies a skip only up to whatever is currently buffered, in one pass, so
//! large skips silently under-apply; a timed pause is honored exactly.)
//! Aligning to the laggard costs a little end-to-end latency and buys every
//! player the laggard's safety margin.
//!
//! The one place skip-ahead (`strm 'a'`) *is* used: a player parked far behind
//! the group after an output stall. Its stream kept flowing while its output
//! froze, so the missing audio is sitting in its buffer — skipping is exactly
//! draining that surplus, and is self-limiting for a player whose data really
//! is late (it skips only what it has). Pauses can only fix ahead-players;
//! without this path a stalled player stays seconds behind forever.

use crate::audio::AudioFormat;
use crate::slim;
use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Don't nudge a player unless it's off by more than this (audio-frame noise).
const DEADBAND_MS: f64 = 10.0;
/// Minimum gap between corrections to one player. Must exceed [`MAX_PAUSE_MS`]
/// so a pause fully plays out and we re-measure before deciding again.
const COOLDOWN_MS: u32 = 3000;
/// A player must have been playing at least this long before it's eligible, so
/// `elapsed_ms` is meaningful and its output pipeline has settled.
const MIN_ELAPSED_MS: u32 = 2000;
/// Cap a single pause; larger initial errors converge over successive rounds
/// rather than inserting one long audible silence.
const MAX_PAUSE_MS: u32 = 1000;
/// Pause for slightly less than the measured error. Squeezelite quantizes a
/// pause *up* to whole output callbacks, so an exact-length pause can overshoot
/// past the laggard — the corrected player then becomes the new reference a few
/// ms back, and the group ratchets backward through endless micro-pauses.
/// Undershooting lands the correction inside the deadband instead.
const PAUSE_UNDERSHOOT_MS: f64 = 8.0;
/// How many recent probes to keep per player for min-RTT offset selection.
const PROBE_WINDOW: usize = 8;
/// Ignore a player whose last report is older than this — a half-dead session
/// (e.g. a reconnect's abandoned predecessor) must not anchor the group.
const STALE_MS: u32 = 5000;
/// How far behind the most-advanced player the group is willing to follow a
/// laggard (by pausing everyone else). A player further behind than this is
/// parked — it stalled and got left back — and is skipped *forward* to the
/// group instead of the group sinking to it. Anchoring the band at the most-
/// advanced player means no number of co-stalled players can drag it down.
const FOLLOW_MAX_MS: f64 = 1500.0;
/// Cap a single catch-up skip. A skip is a content splice (not silence), so
/// larger jumps are fine, but capping lets measurements keep up round-by-round.
const MAX_SKIP_MS: u32 = 2000;
/// A player whose anchor moves faster than this is mid-stall or mid-correction,
/// not a steady room: a stalled player's `elapsed` freezes so its anchor
/// recedes at ~1000 ms/s, and a receding anchor must never be the group
/// reference — chasing it pauses every healthy player over and over. Healthy
/// playback moves the anchor at ppm rates; measurement jitter adds a few ms/s.
const STABLE_MAX_VEL_MS_PER_S: f64 = 100.0;

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
    mac: String,
    name: String,
    /// Write half of this player's SlimProto socket, for sending corrections.
    writer: Arc<Mutex<TcpStream>>,
    probes: Vec<Probe>,
    offset_ms: Option<i64>,
    /// Absolute byte position of the live head when this player's HTTP
    /// connection subscribed (`None` until the HTTP stream attaches). Advanced
    /// by any bytes the broadcast buffer later drops for this reader, so it
    /// always maps `elapsed_ms` to the true content position.
    b_start_bytes: Option<u64>,
    last_jiffies: u32,
    last_elapsed: u32,
    last_correction_ms: u32,
    /// Server clock when the last STMt arrived, for staleness gating.
    last_report_ms: u32,
    /// Previous (epoch, report time) sample, for anchor-velocity estimation.
    last_epoch_sample: Option<(f64, u32)>,
    /// How fast this player's anchor is moving, in ms of epoch per second.
    epoch_vel: f64,
    /// True from (re)starting the player's stream until the first STMt that is
    /// plausibly measured on the fresh stream. Squeezelite can emit one more
    /// report computed against the *old* stream right after a restart; pairing
    /// that stale `elapsed_ms` with the new anchor would fabricate a huge error.
    awaiting_fresh_stream: bool,
}

/// An `elapsed_ms` below this after a stream (re)start is taken as the fresh
/// stream's counter (which starts from 0); anything larger is a stale report
/// from the previous stream and is ignored.
const FRESH_STREAM_MAX_ELAPSED_MS: u32 = 10_000;

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
///
/// Keyed by a per-connection **session id** (not the MAC): two squeezelite
/// instances sharing a MAC, or a reconnecting player racing its own half-dead
/// old session, must never write into each other's timing state — interleaved
/// reports fabricate an ever-receding phantom laggard that the whole group
/// then chases with endless pauses.
pub struct SyncManager {
    players: Mutex<HashMap<u64, Player>>,
    bytes_per_ms: f64,
    enabled: bool,
}

impl SyncManager {
    pub fn new(format: AudioFormat, enabled: bool) -> Arc<Self> {
        Arc::new(SyncManager {
            players: Mutex::new(HashMap::new()),
            bytes_per_ms: format.byte_rate() as f64 / 1000.0,
            enabled,
        })
    }

    /// Register a player session at HELO time. `sid` is the unique id of this
    /// SlimProto connection; it is embedded in the HTTP request URL so the
    /// HTTP side can correlate the two connections.
    pub fn add_player(&self, sid: u64, mac: String, name: String, writer: Arc<Mutex<TcpStream>>) {
        let mut players = self.players.lock().unwrap();
        if players.values().any(|p| p.mac == mac) {
            tracing::info!(
                "sync: another session already uses MAC {mac} (reconnect in progress, or \
                 two squeezelite instances sharing a MAC — consider distinct `-m` values)"
            );
        }
        players.insert(
            sid,
            Player {
                mac,
                name,
                writer,
                probes: Vec::new(),
                offset_ms: None,
                b_start_bytes: None,
                last_jiffies: 0,
                last_elapsed: 0,
                last_correction_ms: 0,
                last_report_ms: 0,
                last_epoch_sample: None,
                epoch_vel: 0.0,
                awaiting_fresh_stream: true,
            },
        );
    }

    pub fn remove_player(&self, sid: u64) {
        self.players.lock().unwrap().remove(&sid);
    }

    /// Record where in the global stream this player's HTTP playback begins.
    pub fn set_http_start(&self, sid: u64, b_start_bytes: u64) {
        if let Some(p) = self.players.lock().unwrap().get_mut(&sid) {
            p.b_start_bytes = Some(b_start_bytes);
            p.last_epoch_sample = None; // anchor moved; old velocity meaningless
            tracing::info!(
                "sync: {} ({}) HTTP attached at byte {b_start_bytes}",
                p.name,
                p.mac
            );
        }
    }

    /// The player's HTTP stream dropped (DSCO). Forget its anchor — and treat
    /// it as not playing — until a fresh stream attaches and re-anchors it.
    pub fn clear_http_start(&self, sid: u64) {
        if let Some(p) = self.players.lock().unwrap().get_mut(&sid) {
            p.b_start_bytes = None;
            p.last_elapsed = 0;
            p.last_epoch_sample = None;
            p.epoch_vel = 0.0;
            p.awaiting_fresh_stream = true;
        }
    }

    /// The broadcast buffer dropped `bytes` for this player's reader (it lagged
    /// past retention). Those bytes never reach the player, so shift its stream
    /// origin forward to keep `b_start + elapsed` truthful.
    pub fn advance_http_start(&self, sid: u64, bytes: u64) {
        if let Some(p) = self.players.lock().unwrap().get_mut(&sid) {
            if let Some(b) = p.b_start_bytes.as_mut() {
                *b += bytes;
                p.last_epoch_sample = None; // anchor moved; old velocity meaningless
                tracing::warn!(
                    "sync: {} lagged past retention, {bytes} bytes dropped from its stream",
                    p.name
                );
            }
        }
    }

    /// Feed in a `STMt` report. `server_ts` is the echoed probe timestamp (0 for
    /// unprompted heartbeats); `recv_ms` is our clock when the report arrived.
    pub fn on_stmt(&self, sid: u64, jiffies: u32, elapsed_ms: u32, server_ts: u32, recv_ms: u32) {
        let mut players = self.players.lock().unwrap();
        let Some(p) = players.get_mut(&sid) else {
            return;
        };

        p.last_jiffies = jiffies;
        p.last_report_ms = recv_ms;
        if p.awaiting_fresh_stream {
            // Only a young counter can belong to the just-started stream; a
            // large one is a leftover report against the previous stream.
            if elapsed_ms < FRESH_STREAM_MAX_ELAPSED_MS {
                p.awaiting_fresh_stream = false;
                p.last_elapsed = elapsed_ms;
            }
        } else {
            p.last_elapsed = elapsed_ms;
        }

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

        // Anchor velocity from this player's own successive reports: ~0 for
        // steady playback, ~1000 ms/s while stalled (elapsed frozen, clock not).
        if let Some(epoch) = p.epoch(self.bytes_per_ms) {
            if let Some((prev_epoch, prev_ms)) = p.last_epoch_sample {
                let dt_s = recv_ms.wrapping_sub(prev_ms) as f64 / 1000.0;
                if dt_s >= 0.25 {
                    p.epoch_vel = (epoch - prev_epoch) / dt_s;
                    p.last_epoch_sample = Some((epoch, recv_ms));
                }
            } else {
                p.last_epoch_sample = Some((epoch, recv_ms));
            }
        }

        if self.enabled {
            self.evaluate(&mut players);
        }
    }

    /// Compute anchors and pause every player running ahead of the laggard.
    fn evaluate(&self, players: &mut HashMap<u64, Player>) {
        let now = server_ms();
        let snaps: Vec<Snapshot> = players
            .iter()
            .filter(|(_, p)| now.wrapping_sub(p.last_report_ms) <= STALE_MS)
            .filter_map(|(&sid, p)| {
                p.epoch(self.bytes_per_ms).map(|epoch| Snapshot {
                    sid,
                    epoch,
                    stable: p.epoch_vel.abs() <= STABLE_MAX_VEL_MS_PER_S,
                    last_correction_ms: p.last_correction_ms,
                })
            })
            .collect();

        if snaps.len() >= 2 {
            let lo = snaps.iter().map(|s| s.epoch).fold(f64::INFINITY, f64::min);
            let hi = snaps
                .iter()
                .map(|s| s.epoch)
                .fold(f64::NEG_INFINITY, f64::max);
            tracing::debug!("sync: {} eligible, spread {:.1}ms", snaps.len(), hi - lo);
        }

        for decision in decide_corrections(&snaps, now) {
            let Some(p) = players.get_mut(&decision.sid) else {
                continue;
            };
            let (command, amount, verb) = match decision.action {
                Action::Pause(ms) => (b'p', ms, "ahead"),
                Action::Skip(ms) => (b'a', ms, "behind"),
            };
            let mut stream = p.writer.lock().unwrap();
            match slim::send_strm_command(&mut stream, command, amount) {
                Ok(()) => {
                    drop(stream);
                    p.last_correction_ms = now;
                    tracing::info!(
                        "sync: {} {verb} by {:.0}ms → {} {amount}ms",
                        p.name,
                        decision.error_ms.abs(),
                        if command == b'p' { "pause" } else { "skip-ahead" },
                    );
                }
                Err(e) => tracing::debug!("sync: correction send to {} failed: {e}", p.name),
            }
        }
    }
}

/// A player's sync state at one instant, for the (pure) decision function.
struct Snapshot {
    sid: u64,
    epoch: f64,
    /// Anchor is moving at a steady-playback rate (not mid-stall/correction).
    stable: bool,
    last_correction_ms: u32,
}

/// How to bring one player into alignment.
enum Action {
    /// Insert this much silence — for a player running ahead of the reference.
    Pause(u32),
    /// Discard this much buffered audio — for a player parked behind the group
    /// after a stall. Self-limiting: squeezelite only skips what it has
    /// buffered, so a player that is behind because data hasn't *arrived* yet
    /// simply skips almost nothing and stays where it is.
    Skip(u32),
}

/// One correction to apply.
struct Correction {
    sid: u64,
    action: Action,
    error_ms: f64,
}

/// Decide corrections, given every eligible player's anchor.
///
/// Pure and deterministic (no clock, no I/O) so the alignment logic can be
/// unit-tested. The reference is the most-delayed player within
/// [`FOLLOW_MAX_MS`] of the most-advanced one — the slowest *reasonable* room.
/// Players ahead of the reference are paused; players behind it (parked after
/// a stall, beyond the follow band) are skipped forward. Only *stable* players
/// participate: a stalling player's receding anchor is a phantom laggard, and
/// a player mid-measurement-flux shouldn't be nudged on garbage numbers.
fn decide_corrections(snaps: &[Snapshot], now: u32) -> Vec<Correction> {
    if snaps.len() < 2 {
        return Vec::new();
    }
    let stable = || snaps.iter().filter(|s| s.stable);
    let Some(band_floor) = stable().map(|s| s.epoch).min_by(f64::total_cmp) else {
        return Vec::new();
    };
    // The slowest room still within the follow band (contains band_floor).
    let ref_epoch = stable()
        .map(|s| s.epoch)
        .filter(|&e| e <= band_floor + FOLLOW_MAX_MS)
        .max_by(f64::total_cmp)
        .unwrap();

    let mut out = Vec::new();
    for s in stable() {
        if now.wrapping_sub(s.last_correction_ms) < COOLDOWN_MS {
            continue;
        }
        let error = ref_epoch - s.epoch; // >0: ahead of the reference; <0: behind
        let action = if error > DEADBAND_MS {
            Action::Pause(((error - PAUSE_UNDERSHOOT_MS).round() as u32).min(MAX_PAUSE_MS))
        } else if -error > DEADBAND_MS {
            Action::Skip(((-error - PAUSE_UNDERSHOOT_MS).round() as u32).min(MAX_SKIP_MS))
        } else {
            continue;
        };
        match action {
            Action::Pause(0) | Action::Skip(0) => continue,
            _ => {}
        }
        out.push(Correction {
            sid: s.sid,
            action,
            error_ms: error,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(sid: u64, epoch: f64, last_corr: u32) -> Snapshot {
        Snapshot {
            sid,
            epoch,
            stable: true,
            last_correction_ms: last_corr,
        }
    }

    fn unstable(sid: u64, epoch: f64) -> Snapshot {
        Snapshot {
            sid,
            epoch,
            stable: false,
            last_correction_ms: 0,
        }
    }

    fn pause_of(c: &Correction) -> u32 {
        match c.action {
            Action::Pause(ms) => ms,
            Action::Skip(_) => panic!("expected a pause, got a skip"),
        }
    }

    fn skip_of(c: &Correction) -> u32 {
        match c.action {
            Action::Skip(ms) => ms,
            Action::Pause(_) => panic!("expected a skip, got a pause"),
        }
    }

    #[test]
    fn single_player_never_corrected() {
        let snaps = vec![snap(1, 1000.0, 0)];
        assert!(decide_corrections(&snaps, 100_000).is_empty());
    }

    #[test]
    fn ahead_player_pauses_by_error_and_laggard_untouched() {
        // 1's epoch is 50ms smaller => 1 is 50ms ahead of the laggard (2).
        let snaps = vec![snap(1, 1000.0, 0), snap(2, 1050.0, 0)];
        let d = decide_corrections(&snaps, 100_000);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].sid, 1);
        assert_eq!(pause_of(&d[0]), 42); // error minus the undershoot
    }

    #[test]
    fn within_deadband_no_correction() {
        let snaps = vec![snap(1, 1000.0, 0), snap(2, 1005.0, 0)];
        assert!(decide_corrections(&snaps, 100_000).is_empty());
    }

    #[test]
    fn cooldown_suppresses_repeat() {
        // 1 ahead by 50ms but corrected 500ms ago (< COOLDOWN) => suppressed.
        let now = 100_000;
        let snaps = vec![snap(1, 1000.0, now - 500), snap(2, 1050.0, 0)];
        assert!(decide_corrections(&snaps, now).is_empty());
    }

    #[test]
    fn large_error_capped() {
        // 2 is the in-band laggard 1400ms behind 1; 1's pause is capped.
        let snaps = vec![snap(1, 0.0, 0), snap(2, 1400.0, 0)];
        let d = decide_corrections(&snaps, 100_000);
        assert_eq!(d.len(), 1);
        assert_eq!(pause_of(&d[0]), MAX_PAUSE_MS);
    }

    #[test]
    fn three_players_two_ahead_of_the_laggard() {
        let snaps = vec![
            snap(1, 1000.0, 0), // 80ms ahead
            snap(2, 1050.0, 0), // 30ms ahead
            snap(3, 1080.0, 0), // laggard (reference)
        ];
        let d = decide_corrections(&snaps, 100_000);
        assert_eq!(d.len(), 2);
        let by_sid: std::collections::HashMap<_, _> =
            d.iter().map(|x| (x.sid, pause_of(x))).collect();
        assert_eq!(by_sid[&1], 72);
        assert_eq!(by_sid[&2], 22);
    }

    #[test]
    fn parked_player_is_skipped_forward_not_followed() {
        // 3 is 20s behind — parked after a stall, far beyond the follow band.
        // The group aligns to the in-band laggard (2); 3 is skipped forward.
        let snaps = vec![
            snap(1, 1000.0, 0),
            snap(2, 1050.0, 0),
            snap(3, 21_050.0, 0),
        ];
        let d = decide_corrections(&snaps, 100_000);
        assert_eq!(d.len(), 2);
        let one = d.iter().find(|x| x.sid == 1).unwrap();
        let three = d.iter().find(|x| x.sid == 3).unwrap();
        assert_eq!(pause_of(one), 42);
        assert_eq!(skip_of(three), MAX_SKIP_MS);
    }

    #[test]
    fn two_players_parked_one_gets_skipped_home() {
        // The healthy player is never dragged down; the parked one skips up.
        let snaps = vec![snap(1, 1000.0, 0), snap(2, 4000.0, 0)];
        let d = decide_corrections(&snaps, 100_000);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].sid, 2);
        assert_eq!(skip_of(&d[0]), MAX_SKIP_MS);
    }

    #[test]
    fn co_stalled_pair_cannot_drag_the_band_down() {
        // 3 and 4 parked together, corroborating each other — the band is
        // anchored at the most-advanced player, so they are still outliers:
        // both get skipped forward, and the healthy pair syncs among itself.
        let snaps = vec![
            snap(1, 1000.0, 0),
            snap(2, 1050.0, 0),
            snap(3, 10_000.0, 0),
            snap(4, 10_100.0, 0),
        ];
        let d = decide_corrections(&snaps, 100_000);
        assert_eq!(d.len(), 3);
        let by_sid: std::collections::HashMap<_, _> =
            d.iter().map(|x| (x.sid, x)).collect();
        assert_eq!(pause_of(by_sid[&1]), 42);
        assert_eq!(skip_of(by_sid[&3]), MAX_SKIP_MS);
        assert_eq!(skip_of(by_sid[&4]), MAX_SKIP_MS);
    }

    #[test]
    fn stalling_players_never_anchor_the_group() {
        // 3 and 4 are mid-stall (anchors still receding): no reference role,
        // no corrections — wait until they stabilize, then skip them home.
        let snaps = vec![
            snap(1, 1000.0, 0),
            snap(2, 1050.0, 0),
            unstable(3, 10_000.0),
            unstable(4, 10_100.0),
        ];
        let d = decide_corrections(&snaps, 100_000);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].sid, 1);
        assert_eq!(pause_of(&d[0]), 42);
    }

    #[test]
    fn no_stable_players_no_corrections() {
        let snaps = vec![unstable(1, 1000.0), unstable(2, 2000.0)];
        assert!(decide_corrections(&snaps, 100_000).is_empty());
    }
}
