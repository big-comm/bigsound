//! Automatic output routing: follow the physical output the user picked last.
//!
//! In Automatic mode `BigSound.output` used to follow WirePlumber's session
//! priority. That ignores the user's choice: with a USB headset (priority
//! 1109) and a Bluetooth speaker (1010) both present, selecting BigSound sent
//! the processed audio to the headset even when the user had just been
//! listening on the speaker. The only way to hear the speaker was to select
//! it directly, which bypasses BigSound.
//!
//! The daemon now keeps a list of the physical sinks the user wants, most
//! recent first, and Automatic routes BigSound through the newest connected
//! one. A sink moves to the front when the user selects it in the system
//! sound menu (`default.configured.audio.sink`) or when a Bluetooth output
//! connects. With no remembered sink available, WirePlumber priority applies.
//!
//! Only Bluetooth outputs take over on arrival. A USB device can be a
//! microphone with a headphone jack (the HyperX QuadCast S exposes one), and
//! HDMI outputs come and go with monitor power; neither should steal the
//! sound. Those are chosen in the sound menu instead.

/// Remembered physical sinks, most recent first.
pub const HISTORY_LIMIT: usize = 16;

/// Metadata key WirePlumber stores the user's chosen output under. Desktop
/// sound menus, `pactl set-default-sink` and `wpctl set-default` all write it.
const CONFIGURED_SINK_KEY: &str = "default.configured.audio.sink";

/// A sink `node.name` that can be routed to and passed to `pw-metadata` and
/// `pactl`: printable ASCII, bounded, never BigSound's own nodes. `"` and `\`
/// are rejected because the name is embedded in JSON; real names never use
/// them.
pub fn valid_sink_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name != "BigSound"
        && name != "BigSound.output"
        && name
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
}

/// Parse one `pw-metadata` line and return the sink name when it reports the
/// user's configured output. Both the initial listing and `--monitor` output
/// use this format:
///
/// ```text
/// update: id:0 key:'default.configured.audio.sink' value:'{ "name": "x" }' type:'Spa:String:JSON'
/// ```
///
/// Returns `None` for other keys, deletions and malformed values.
pub fn configured_sink_from_metadata_line(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("update: id:0 key:'")?;
    let rest = rest.strip_prefix(CONFIGURED_SINK_KEY)?;
    let rest = rest.strip_prefix("' value:'")?;
    let (value, _) = rest.rsplit_once("' type:")?;
    let json: serde_json::Value = serde_json::from_str(value).ok()?;
    let name = json.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Read WirePlumber's own record of past output choices from its state file
/// (`$XDG_STATE_HOME/wireplumber/default-nodes`, WirePlumber 0.5):
///
/// ```text
/// [default-nodes]
/// default.configured.audio.sink=bluez_output.00_1A_7D_DA_71_13.1
/// default.configured.audio.sink.0=BigSound
/// ```
///
/// The unsuffixed key is the current choice and `.0`, `.1`, … the earlier
/// ones. Used once to seed the history, so a choice made before this daemon
/// version ran is not forgotten. Returns physical sinks, most recent first.
pub fn history_from_wireplumber_state(text: &str) -> Vec<String> {
    let mut current = None;
    let mut older: Vec<(u32, String)> = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(suffix) = key.trim().strip_prefix(CONFIGURED_SINK_KEY) else {
            continue;
        };
        let value = value.trim().to_string();
        if suffix.is_empty() {
            current = Some(value);
        } else if let Some(index) = suffix.strip_prefix('.').and_then(|i| i.parse().ok()) {
            older.push((index, value));
        }
    }
    older.sort_by_key(|(index, _)| *index);
    let mut history = Vec::new();
    for name in current.into_iter().chain(older.into_iter().map(|(_, n)| n)) {
        remember(&mut history, &name, false);
    }
    history
}

/// Record `name` in the history. `front = true` marks it as the newest choice;
/// `front = false` appends it as an older one (used while seeding). Invalid
/// names and BigSound itself are ignored. Returns whether the history changed.
pub fn remember(history: &mut Vec<String>, name: &str, front: bool) -> bool {
    if !valid_sink_name(name) {
        return false;
    }
    let existing = history.iter().position(|n| n == name);
    match (existing, front) {
        (Some(0), true) | (Some(_), false) => return false,
        (Some(i), true) => {
            let entry = history.remove(i);
            history.insert(0, entry);
        }
        (None, true) => history.insert(0, name.to_string()),
        (None, false) => history.push(name.to_string()),
    }
    history.truncate(HISTORY_LIMIT);
    true
}

/// The sink Automatic should route to: the most recently chosen one that is
/// currently available. `None` leaves the choice to WirePlumber priority.
pub fn pick_auto_output<'a>(history: &'a [String], available: &[String]) -> Option<&'a str> {
    history
        .iter()
        .find(|name| available.contains(name))
        .map(String::as_str)
}

/// Bluetooth outputs are named `bluez_output.<address>.<n>` by WirePlumber.
pub fn is_bluetooth_sink(name: &str) -> bool {
    name.starts_with("bluez_output.")
}

/// A sink that disappears and comes back within this window is treated as
/// the same connection (Bluetooth profile/codec switches during calls,
/// suspend of the adapter), not as the user connecting a device.
pub const RECONNECT_GRACE_MS: u64 = 15_000;

/// Detects Bluetooth outputs that have just connected, from successive
/// snapshots of the available sinks.
#[derive(Debug, Default)]
pub struct ArrivalTracker {
    known: Option<Vec<String>>,
    /// Sinks that left, with the time they left.
    departed: Vec<(String, u64)>,
}

impl ArrivalTracker {
    /// Feed the sinks present at `now_ms`; returns Bluetooth sinks that
    /// connected since the previous call. The first call only records the
    /// starting state: devices present when the daemon starts are not
    /// arrivals.
    pub fn update(&mut self, now_ms: u64, available: &[String]) -> Vec<String> {
        let Some(known) = self.known.replace(available.to_vec()) else {
            return Vec::new();
        };
        for name in &known {
            if !available.contains(name) {
                self.departed.retain(|(n, _)| n != name);
                self.departed.push((name.clone(), now_ms));
            }
        }
        self.departed
            .retain(|(_, left)| now_ms.saturating_sub(*left) <= RECONNECT_GRACE_MS);
        let mut arrivals = Vec::new();
        for name in available {
            if known.contains(name) || !is_bluetooth_sink(name) {
                continue;
            }
            if let Some(i) = self.departed.iter().position(|(n, _)| n == name) {
                self.departed.remove(i);
                continue;
            }
            arrivals.push(name.clone());
        }
        arrivals
    }
}

/// Sink names from `pactl list short sinks` (tab separated, name second).
pub fn sink_names_from_pactl_short(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split('\t').nth(1))
        .map(str::trim)
        .filter(|name| valid_sink_name(name))
        .map(str::to_string)
        .collect()
}

/// History file format: one sink name per line, most recent first.
pub fn parse_history_file(text: &str) -> Vec<String> {
    let mut history = Vec::new();
    for line in text.lines() {
        remember(&mut history, line.trim(), false);
    }
    history
}

pub fn format_history_file(history: &[String]) -> String {
    history.iter().map(|name| format!("{name}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Node names from the workstation where the bug was observed
    // (2026-10-19): HyperX QuadCast S USB headset jack (priority 1109) and an
    // EDIFIER R1700BT Bluetooth speaker (priority 1010; placeholder address).
    const HYPERX: &str = "alsa_output.usb-Kingston_HyperX_QuadCast_S_4100-00.analog-stereo";
    const EDIFIER: &str = "bluez_output.00_1A_7D_DA_71_13.1";
    const HDMI: &str = "alsa_output.pci-0000_01_00.1.hdmi-stereo";

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_configured_sink_from_real_pw_metadata_output() {
        let line = format!(
            "update: id:0 key:'default.configured.audio.sink' value:'{{ \"name\": \"{EDIFIER}\" }}' type:'Spa:String:JSON'"
        );
        assert_eq!(
            configured_sink_from_metadata_line(&line).as_deref(),
            Some(EDIFIER)
        );
        // Compact JSON as written by WirePlumber for the effective default.
        assert_eq!(
            configured_sink_from_metadata_line(
                "update: id:0 key:'default.configured.audio.sink' value:'{\"name\":\"BigSound\"}' type:'Spa:String:JSON'"
            )
            .as_deref(),
            Some("BigSound")
        );
    }

    #[test]
    fn ignores_other_metadata_keys_deletions_and_garbage() {
        for line in [
            "Found \"default\" metadata 41",
            "update: id:0 key:'default.audio.sink' value:'{\"name\":\"x\"}' type:'Spa:String:JSON'",
            "update: id:0 key:'default.configured.audio.source' value:'{\"name\":\"x\"}' type:'Spa:String:JSON'",
            "update: id:0 key:'default.configured.audio.sink.0' value:'{\"name\":\"x\"}' type:'Spa:String:JSON'",
            "update: id:35 key:'target.object' value:'901' type:'Spa:Id'",
            "delete: id:0 key:'default.configured.audio.sink'",
            "update: id:0 key:'default.configured.audio.sink' value:'not json' type:'Spa:String:JSON'",
            "update: id:0 key:'default.configured.audio.sink' value:'{\"name\":\"\"}' type:'Spa:String:JSON'",
            "",
        ] {
            assert_eq!(configured_sink_from_metadata_line(line), None, "{line}");
        }
    }

    #[test]
    fn seeds_history_from_real_wireplumber_state_file() {
        // Exact state found on the workstation: the speaker selected directly
        // (bypassing BigSound) after BigSound had been selected before.
        let state = format!(
            "[default-nodes]\n\
             default.configured.audio.source=alsa_input.usb-Kingston_HyperX_QuadCast_S_4100-00.analog-stereo\n\
             default.configured.audio.sink={EDIFIER}\n\
             default.configured.audio.sink.0=BigSound\n"
        );
        assert_eq!(history_from_wireplumber_state(&state), names(&[EDIFIER]));
    }

    #[test]
    fn wireplumber_history_keeps_numeric_order_and_skips_bigsound() {
        let state = format!(
            "[default-nodes]\n\
             default.configured.audio.sink.10={HDMI}\n\
             default.configured.audio.sink.1={HYPERX}\n\
             default.configured.audio.sink=BigSound\n\
             default.configured.audio.sink.0={EDIFIER}\n\
             default.configured.audio.sink.2={EDIFIER}\n"
        );
        assert_eq!(
            history_from_wireplumber_state(&state),
            names(&[EDIFIER, HYPERX, HDMI])
        );
        assert!(history_from_wireplumber_state("").is_empty());
    }

    #[test]
    fn newest_choice_moves_to_front_without_duplicates() {
        let mut history = names(&[HYPERX, EDIFIER]);
        assert!(remember(&mut history, EDIFIER, true));
        assert_eq!(history, names(&[EDIFIER, HYPERX]));
        assert!(!remember(&mut history, EDIFIER, true));
        assert!(!remember(&mut history, "BigSound", true));
        assert!(!remember(&mut history, "BigSound.output", true));
        assert!(!remember(&mut history, "bad\"name", true));
        assert_eq!(history, names(&[EDIFIER, HYPERX]));
    }

    #[test]
    fn history_is_bounded() {
        let mut history = Vec::new();
        for i in 0..40 {
            remember(&mut history, &format!("sink{i}"), true);
        }
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history[0], "sink39");
    }

    #[test]
    fn automatic_follows_last_chosen_device_over_higher_priority_one() {
        // The reported bug: user listened on the speaker, then selected
        // BigSound. Both devices are connected; the headset has the higher
        // WirePlumber priority, but the speaker was chosen last.
        let history = names(&[EDIFIER]);
        let available = names(&[HYPERX, EDIFIER, HDMI]);
        assert_eq!(pick_auto_output(&history, &available), Some(EDIFIER));
    }

    #[test]
    fn automatic_falls_back_when_the_chosen_device_disconnects() {
        let history = names(&[EDIFIER, HYPERX]);
        // Speaker off: the previous choice takes over.
        assert_eq!(
            pick_auto_output(&history, &names(&[HYPERX, HDMI])),
            Some(HYPERX)
        );
        // Nothing remembered is connected: WirePlumber priority decides.
        assert_eq!(pick_auto_output(&history, &names(&[HDMI])), None);
        assert_eq!(pick_auto_output(&[], &names(&[HDMI])), None);
    }

    #[test]
    fn parses_real_pactl_short_sink_list() {
        let text = format!(
            "35\tBigSound\tPipeWire\tfloat32le 2ch 48000Hz\tSUSPENDED\n\
             67\t{HYPERX}\tPipeWire\ts16le 2ch 48000Hz\tSUSPENDED\n\
             894\t{EDIFIER}\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n"
        );
        assert_eq!(
            sink_names_from_pactl_short(&text),
            names(&[HYPERX, EDIFIER])
        );
    }

    #[test]
    fn bluetooth_outputs_are_recognised_by_name() {
        assert!(is_bluetooth_sink(EDIFIER));
        assert!(!is_bluetooth_sink(HYPERX));
        assert!(!is_bluetooth_sink(HDMI));
        assert!(!is_bluetooth_sink("bluez_input.00_1A_7D_DA_71_13.0"));
    }

    #[test]
    fn devices_present_at_startup_are_not_arrivals() {
        let mut tracker = ArrivalTracker::default();
        assert!(tracker.update(0, &names(&[HYPERX, EDIFIER])).is_empty());
        assert!(tracker.update(3_000, &names(&[HYPERX, EDIFIER])).is_empty());
    }

    #[test]
    fn a_bluetooth_speaker_turned_on_is_an_arrival() {
        // Laptop: internal speakers only, then the user turns on a speaker.
        let laptop = "alsa_output.pci-0000_00_1f.3.analog-stereo";
        let mut tracker = ArrivalTracker::default();
        tracker.update(0, &names(&[laptop]));
        assert_eq!(
            tracker.update(60_000, &names(&[laptop, EDIFIER])),
            names(&[EDIFIER])
        );
        assert!(tracker
            .update(63_000, &names(&[laptop, EDIFIER]))
            .is_empty());
    }

    #[test]
    fn usb_and_hdmi_outputs_never_take_over_on_arrival() {
        // A USB microphone with a headphone jack, or a monitor waking up.
        let mut tracker = ArrivalTracker::default();
        tracker.update(0, &names(&[EDIFIER]));
        assert!(tracker
            .update(3_000, &names(&[EDIFIER, HYPERX, HDMI]))
            .is_empty());
    }

    #[test]
    fn a_quick_bluetooth_reconnect_is_not_a_new_arrival() {
        // Profile switch during a call: the node goes away for a moment.
        let mut tracker = ArrivalTracker::default();
        tracker.update(0, &names(&[HYPERX, EDIFIER]));
        tracker.update(3_000, &names(&[HYPERX]));
        assert!(tracker.update(6_000, &names(&[HYPERX, EDIFIER])).is_empty());
        // Switched off for longer than the grace period, then on again.
        tracker.update(9_000, &names(&[HYPERX]));
        tracker.update(12_000, &names(&[HYPERX]));
        assert_eq!(
            tracker.update(
                9_000 + RECONNECT_GRACE_MS + 3_000,
                &names(&[HYPERX, EDIFIER])
            ),
            names(&[EDIFIER])
        );
    }

    #[test]
    fn history_file_round_trips_and_rejects_bad_lines() {
        let history = names(&[EDIFIER, HYPERX]);
        let text = format_history_file(&history);
        assert_eq!(parse_history_file(&text), history);
        let hostile = format!("{EDIFIER}\nBigSound\nwith space\n\n{EDIFIER}\n{HYPERX}\n");
        assert_eq!(parse_history_file(&hostile), history);
    }
}
