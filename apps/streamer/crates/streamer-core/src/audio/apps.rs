//! Pure parts of app listing and routing: grouping streams into apps, browser detection,
//! which streams to move or restore, and the version check.

use std::collections::{BTreeMap, BTreeSet};

use super::{AppInfo, AudioError};

/// One `Stream/Output/Audio` node with the properties routing needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamNode {
    /// Registry id; the metadata subject.
    pub id: u32,
    pub serial: u64,
    pub app_name: Option<String>,
    pub binary: Option<String>,
    pub node_name: Option<String>,
    /// The node's own `target.object` property (not the metadata entry).
    pub target_object: Option<String>,
}

/// A `target.object` entry of the `default` metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaValue {
    pub type_: Option<String>,
    pub value: String,
}

/// The app a stream belongs to: `application.name`, else the binary, else `node.name`.
pub fn app_key(n: &StreamNode) -> Option<String> {
    [&n.app_name, &n.binary, &n.node_name]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .cloned()
}

/// Groups streams by [`app_key`], sorted by name. Streams named `own_app_name` (the
/// streamer's own output) are left out.
pub fn group_apps(nodes: &[StreamNode], own_app_name: &str) -> Vec<AppInfo> {
    let mut apps: BTreeMap<String, AppInfo> = BTreeMap::new();
    for n in nodes {
        if n.app_name.as_deref() == Some(own_app_name) {
            continue;
        }
        let Some(name) = app_key(n) else { continue };
        let binary = n.binary.clone().filter(|b| !b.is_empty());
        let entry = apps.entry(name.clone()).or_insert_with(|| AppInfo {
            name,
            binary: None,
            is_browser: false,
            streams: 0,
        });
        entry.streams += 1;
        if entry.binary.is_none() {
            entry.binary = binary;
        }
    }
    apps.into_values()
        .map(|mut a| {
            a.is_browser = is_browser(&a.name, a.binary.as_deref());
            a
        })
        .collect()
}

/// Browser names as lowercase word sequences.
const BROWSERS: [&[&str]; 11] = [
    &["firefox"],
    &["chrome"],
    &["chromium"],
    &["google", "chrome"],
    &["microsoft", "edge"],
    &["msedge"],
    &["brave"],
    &["opera"],
    &["vivaldi"],
    &["librewolf"],
    &["zen"],
];

/// Whether the app is a web browser (spec §9.3, C9), by app name or binary. Matches whole
/// words, case-insensitive: `chromium-browser` matches, `Zenity` does not.
pub fn is_browser(name: &str, binary: Option<&str>) -> bool {
    std::iter::once(name).chain(binary).any(|text| {
        let lower = text.to_lowercase();
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        BROWSERS
            .iter()
            .any(|browser| words.windows(browser.len()).any(|w| w == *browser))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteAction {
    /// Point the stream at the sink.
    Move { node_id: u32 },
    /// Give the stream its previous target back.
    Restore { node_id: u32 },
}

/// Moves the streams of `selected` apps that are not moved yet, and restores moved streams
/// whose app is no longer selected. Moved ids that are not in `nodes` (vanished) are ignored.
pub fn routing_actions(
    selected: &[String],
    nodes: &[StreamNode],
    moved: &BTreeSet<u32>,
) -> Vec<RouteAction> {
    nodes
        .iter()
        .filter_map(|n| {
            let is_selected = app_key(n).is_some_and(|key| selected.contains(&key));
            match (is_selected, moved.contains(&n.id)) {
                (true, false) => Some(RouteAction::Move { node_id: n.id }),
                (false, true) => Some(RouteAction::Restore { node_id: n.id }),
                _ => None,
            }
        })
        .collect()
}

/// Moved streams whose `target.object` entry (`targets`, by subject) no longer holds the sink
/// serial: the user or another tool moved them away or deleted the entry. Their choice wins,
/// so the streamer neither moves them back nor restores them.
pub fn moved_away(
    moved: &BTreeSet<u32>,
    targets: &BTreeMap<u32, MetaValue>,
    sink_serial: &str,
) -> Vec<u32> {
    moved
        .iter()
        .copied()
        .filter(|id| targets.get(id).is_none_or(|t| t.value != sink_serial))
        .collect()
}

/// PipeWire 1.0 and WirePlumber 0.5 or later; no WirePlumber is an error.
pub fn check_versions(pipewire: &str, wireplumber: Option<&str>) -> Result<(), AudioError> {
    let at_least = |version: &str, min: (u32, u32, u32)| parse_version(version) >= Some(min);
    match wireplumber {
        Some(wp) if at_least(pipewire, (1, 0, 0)) && at_least(wp, (0, 5, 0)) => Ok(()),
        _ => Err(AudioError::UnsupportedVersions),
    }
}

/// `major.minor[.patch]` with optional suffixes (`1.2.3-dev`); `None` when unparsable.
fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.trim().split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, app: Option<&str>, binary: Option<&str>, name: Option<&str>) -> StreamNode {
        StreamNode {
            id,
            serial: 1000 + u64::from(id),
            app_name: app.map(str::to_string),
            binary: binary.map(str::to_string),
            node_name: name.map(str::to_string),
            target_object: None,
        }
    }

    fn app(name: &str, binary: Option<&str>, is_browser: bool, streams: usize) -> AppInfo {
        AppInfo {
            name: name.to_string(),
            binary: binary.map(str::to_string),
            is_browser,
            streams,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn groups_by_app_name_then_binary_then_node_name() {
        let nodes = [
            node(1, Some("Spotify"), Some("spotify"), Some("spotify")),
            node(2, None, Some("mpv"), Some("mpv-node")),
            node(3, Some("Spotify"), None, Some("spotify")),
            node(4, None, None, Some("alsa_playback.aplay")),
            node(5, None, None, None),
            node(6, Some(""), Some("vlc"), None),
        ];
        assert_eq!(
            group_apps(&nodes, "Beatsync Streamer"),
            vec![
                app("Spotify", Some("spotify"), false, 2),
                app("alsa_playback.aplay", None, false, 1),
                app("mpv", Some("mpv"), false, 1),
                app("vlc", Some("vlc"), false, 1),
            ]
        );
    }

    #[test]
    fn own_nodes_are_excluded() {
        let nodes = [
            node(
                1,
                Some("Beatsync Streamer"),
                Some("beatsync-streamer"),
                None,
            ),
            node(2, Some("beatsync-test-tone"), Some("pw-cat"), None),
        ];
        assert_eq!(
            group_apps(&nodes, "Beatsync Streamer"),
            vec![app("beatsync-test-tone", Some("pw-cat"), false, 1)]
        );
    }

    #[test]
    fn browser_detection() {
        assert!(is_browser("Firefox", None));
        assert!(!is_browser("Spotify", Some("spotify")));
        assert!(is_browser("Chromium input", Some("chromium-browser")));
        assert!(is_browser("Some App", Some("chromium-browser")));
        assert!(is_browser("Google Chrome", None));
        assert!(is_browser("Microsoft Edge", None));
        // Whole words only: these merely contain a browser name.
        assert!(!is_browser("Zenity", Some("zenity")));
        assert!(!is_browser("Operator", None));
    }

    #[test]
    fn new_stream_of_selected_app_is_moved() {
        let nodes = [
            node(10, Some("Spotify"), None, None),
            node(11, Some("Spotify"), None, None),
        ];
        let moved = BTreeSet::from([10]);
        assert_eq!(
            routing_actions(&names(&["Spotify"]), &nodes, &moved),
            vec![RouteAction::Move { node_id: 11 }]
        );
    }

    #[test]
    fn stream_of_unselected_app_is_not_moved() {
        let nodes = [
            node(10, Some("Spotify"), None, None),
            node(11, Some("Firefox"), Some("firefox"), None),
            node(12, None, None, None),
        ];
        assert_eq!(
            routing_actions(&names(&["Spotify"]), &nodes, &BTreeSet::new()),
            vec![RouteAction::Move { node_id: 10 }]
        );
    }

    #[test]
    fn unselecting_an_app_restores_its_moved_streams() {
        let nodes = [
            node(10, Some("Spotify"), None, None),
            node(11, Some("Firefox"), None, None),
            node(12, Some("Firefox"), None, None),
        ];
        // 99 vanished while moved: nothing to restore on it.
        let moved = BTreeSet::from([10, 11, 12, 99]);
        assert_eq!(
            routing_actions(&names(&["Spotify"]), &nodes, &moved),
            vec![
                RouteAction::Restore { node_id: 11 },
                RouteAction::Restore { node_id: 12 },
            ]
        );
    }

    #[test]
    fn streams_the_user_moved_away_are_left_alone() {
        let entry = |value: &str| MetaValue {
            type_: Some("Spa:Id".to_string()),
            value: value.to_string(),
        };
        let targets = BTreeMap::from([
            (10, entry("500")),
            // pavucontrol's "back to the default sink".
            (11, entry("-1")),
            // 12: the user deleted the entry.
            // 13 points at the sink but was not moved by the streamer.
            (13, entry("500")),
            (14, entry("77")),
        ]);
        let moved = BTreeSet::from([10, 11, 12, 14]);
        assert_eq!(moved_away(&moved, &targets, "500"), vec![11, 12, 14]);
    }

    #[test]
    fn versions() {
        assert_eq!(check_versions("1.6.11", Some("0.5.19")), Ok(()));
        assert_eq!(check_versions("1.0.0", Some("0.5.0")), Ok(()));
        assert_eq!(
            check_versions("0.3.80", Some("0.5.19")),
            Err(AudioError::UnsupportedVersions)
        );
        assert_eq!(
            check_versions("1.0.0", Some("0.4.17")),
            Err(AudioError::UnsupportedVersions)
        );
        assert_eq!(
            check_versions("1.6.11", None),
            Err(AudioError::UnsupportedVersions)
        );
        assert_eq!(
            AudioError::UnsupportedVersions.to_string(),
            "PipeWire 1.0 and WirePlumber 0.5 or later required"
        );
    }
}
