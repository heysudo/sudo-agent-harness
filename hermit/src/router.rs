//! Fast-path intent router (spec §4.2).
//!
//! Device commands are matched by a precompiled regex table and executed directly.
//! They NEVER touch the LLM. The whole match is a handful of regex scans over a
//! short string — microseconds, against a 50 ms budget that is really dominated by
//! the mpv/librespot IPC round trip downstream.
//!
//! Ordering matters: more specific patterns are tried first ("stop the music"
//! before bare "stop", "play X on spotify" before bare "play X").

use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Handle locally, no LLM.
    Device(DeviceCommand),
    /// Hand to the orchestrator.
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceCommand {
    Pause,
    Resume,
    /// Stop playback entirely (as opposed to pause).
    StopMusic,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    /// Absolute volume, 0–100.
    VolumeSet(u8),
    Mute,
    Unmute,
    /// Play a free-text query on Spotify.
    PlaySpotify(String),
    /// Play a named internet-radio station.
    PlayStation(String),
    /// Local clock — answering this from the LLM would be absurd.
    TimeOfDay,
    /// What is currently playing.
    NowPlaying,
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: Lazy<Regex> = Lazy::new(|| Regex::new($pat).expect("static regex"));
    };
}

// Leading filler we strip before matching: wake-word residue and politeness.
re!(
    FILLER,
    r"^(?:hey |ok |okay |hermit[, ]+|computer[, ]+|please |could you |can you |would you |will you )+"
);

re!(
    R_PAUSE,
    r"^(?:pause|hold on|hold up|wait)(?: the)?(?: music| song| track| playback)?$"
);
re!(
    R_RESUME,
    r"^(?:resume|unpause|continue|keep going|play)(?: the)?(?: music| song| track| playback)?$"
);
re!(
    R_STOP,
    r"^(?:stop|kill|shut off|turn off|quit)(?: the)?(?: music| song| track| playback| radio| audio)$"
);
re!(
    R_STOP_BARE,
    r"^(?:stop|shut up|be quiet|silence|nevermind|never mind|cancel)$"
);
// Built compositionally rather than as a flat alternation: a flat list like
// `(?:skip|skip this song)` anchored with `$` fails on the longer form because the
// shorter branch matches first and the anchor then rejects the remainder.
re!(
    R_NEXT,
    r"^(?:next|skip|forward)(?: (?:this|it|the))?(?: (?:song|track|one))?$"
);
re!(
    R_PREV,
    r"^(?:previous|prev|back|go back|last)(?: (?:this|the))?(?: (?:song|track|one))?$"
);
re!(
    R_VOL_UP,
    r"^(?:volume up|turn (?:it |the volume )?up|louder|crank it|raise the volume)$"
);
re!(
    R_VOL_DOWN,
    r"^(?:volume down|turn (?:it |the volume )?down|quieter|softer|lower the volume)$"
);
re!(
    R_VOL_SET,
    r"^(?:set )?volume(?: to| at)? (\d{1,3})(?: percent)?$"
);
re!(R_MUTE, r"^(?:mute|mute (?:it|the music|yourself))$");
re!(R_UNMUTE, r"^(?:unmute|unmute (?:it|the music))$");
re!(
    R_TIME,
    r"^(?:what(?:'s| is) the time|what time is it|time please|got the time|the time)$"
);
re!(
    R_NOWPLAYING,
    r"^(?:what(?:'s| is) (?:playing|this|this song)|now playing|what song is this|who(?:'s| is) this)$"
);

// "play <query> on spotify"  /  "play <query> on the radio|station"
re!(
    R_PLAY_SPOTIFY,
    r"^play (.+?) on (?:spotify|spotify connect)$"
);
re!(
    R_PLAY_STATION,
    r"^play (?:the )?(.+?) (?:on (?:the )?)?(?:radio|station|stream)$"
);
// "play <station name>" where the name resolves against stations.toml — handled by
// the caller via `station_names`, since we cannot know the map here.
re!(R_PLAY_ANY, r"^play (?:some |the )?(.+)$");

/// Normalize an utterance for matching: lowercase, strip filler and trailing
/// punctuation, collapse internal whitespace.
pub fn normalize(input: &str) -> String {
    let lowered = input.trim().to_lowercase();
    let stripped = FILLER.replace(&lowered, "");
    let no_punct = stripped.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '.' | '!' | '?' | ',' | ';' | ':' | '"' | '\'')
    });
    let mut out = String::with_capacity(no_punct.len());
    let mut prev_space = false;
    for ch in no_punct.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

/// Route an utterance. `station_names` comes from `stations.toml` so that a bare
/// "play jazz24" resolves to a station instead of falling through to Spotify.
pub fn route(input: &str, station_names: &[String]) -> Route {
    let s = normalize(input);
    if s.is_empty() {
        return Route::Agent;
    }

    // Order is load-bearing. Specific → general.
    if R_PAUSE.is_match(&s) {
        return Route::Device(DeviceCommand::Pause);
    }
    if R_STOP.is_match(&s) || R_STOP_BARE.is_match(&s) {
        return Route::Device(DeviceCommand::StopMusic);
    }
    if R_NEXT.is_match(&s) {
        return Route::Device(DeviceCommand::Next);
    }
    if R_PREV.is_match(&s) {
        return Route::Device(DeviceCommand::Previous);
    }
    if R_VOL_UP.is_match(&s) {
        return Route::Device(DeviceCommand::VolumeUp);
    }
    if R_VOL_DOWN.is_match(&s) {
        return Route::Device(DeviceCommand::VolumeDown);
    }
    if let Some(c) = R_VOL_SET.captures(&s)
        && let Ok(v) = c[1].parse::<u32>()
    {
        return Route::Device(DeviceCommand::VolumeSet(v.min(100) as u8));
    }
    if R_MUTE.is_match(&s) {
        return Route::Device(DeviceCommand::Mute);
    }
    if R_UNMUTE.is_match(&s) {
        return Route::Device(DeviceCommand::Unmute);
    }
    if R_TIME.is_match(&s) {
        return Route::Device(DeviceCommand::TimeOfDay);
    }
    if R_NOWPLAYING.is_match(&s) {
        return Route::Device(DeviceCommand::NowPlaying);
    }
    if let Some(c) = R_PLAY_SPOTIFY.captures(&s) {
        return Route::Device(DeviceCommand::PlaySpotify(c[1].trim().to_string()));
    }
    if let Some(c) = R_PLAY_STATION.captures(&s) {
        return Route::Device(DeviceCommand::PlayStation(c[1].trim().to_string()));
    }
    // Bare "resume"/"play" with no object means resume playback.
    if R_RESUME.is_match(&s) {
        return Route::Device(DeviceCommand::Resume);
    }
    if let Some(c) = R_PLAY_ANY.captures(&s) {
        let target = c[1].trim();
        // Only claim it as a station if the name is actually configured; otherwise
        // this is "play something relaxing", which needs the LLM.
        if let Some(name) = match_station(target, station_names) {
            return Route::Device(DeviceCommand::PlayStation(name));
        }
        // A concrete-looking "play X" goes to Spotify; vague ones go to the agent.
        if looks_like_media_title(target) {
            return Route::Device(DeviceCommand::PlaySpotify(target.to_string()));
        }
    }

    Route::Agent
}

/// Case-insensitive exact-or-contains station match, longest name first so
/// "npr news" beats "npr".
fn match_station(target: &str, names: &[String]) -> Option<String> {
    let t = target.trim().to_lowercase();
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort_by_key(|n| std::cmp::Reverse(n.len()));
    sorted
        .into_iter()
        .find(|n| {
            let n = n.to_lowercase();
            t == n || t == format!("{n} radio") || t.starts_with(&format!("{n} "))
        })
        .cloned()
}

/// Heuristic: does this look like a specific thing to play, rather than a mood?
/// Vague requests ("something chill") benefit from the LLM picking; named ones
/// should not pay a round trip.
fn looks_like_media_title(s: &str) -> bool {
    const VAGUE: &[&str] = &[
        "something",
        "anything",
        "some music",
        "a song",
        "music",
        "songs",
        "whatever",
        "stuff",
        "tunes",
        "a playlist",
    ];
    let low = s.to_lowercase();
    if VAGUE
        .iter()
        .any(|v| low == *v || low.starts_with(&format!("{v} ")))
    {
        return false;
    }
    // "X by Y" and multi-word proper-ish names are concrete enough.
    low.contains(" by ") || s.split_whitespace().count() <= 6
}

// ---------------------------------------------------------------------------
// Classification helpers used by the orchestrator
// ---------------------------------------------------------------------------

re!(
    R_RESEARCH,
    r"(?:^|\b)(?:research|deep dive|deep-dive|dig into|investigate|comprehensive|thorough(?:ly)?|write me a report|full report|analyz[e]? in depth|in depth)\b"
);

/// Should this query use `reasoning_effort=medium` and be eligible for the
/// background research path (spec §4.3, §5)?
pub fn is_research(input: &str) -> bool {
    R_RESEARCH.is_match(&input.to_lowercase())
}

re!(
    R_LOOKUP,
    r"^(?:what|what's|whats|who|who's|whos|when|where|which|how much|how many|latest|current|price|score|news|weather|is there|are there|did|does|has|show me|tell me about|look up|search)\b"
);

/// Speculative prefetch gate (spec §5): fire a provisional Turbo search before
/// end-of-speech when the interim transcript looks like a lookup and is long
/// enough to be worth a request. At $0.001/req a wasted call costs nothing.
pub fn should_prefetch(interim: &str) -> bool {
    let s = normalize(interim);
    let words = s.split_whitespace().count();
    words >= 4 && R_LOOKUP.is_match(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stations() -> Vec<String> {
        vec!["npr".into(), "bbc world service".into(), "jazz24".into()]
    }

    fn dev(s: &str) -> DeviceCommand {
        match route(s, &stations()) {
            Route::Device(d) => d,
            Route::Agent => panic!("expected device command for {s:?}, got Agent"),
        }
    }

    #[test]
    fn transport_commands() {
        assert_eq!(dev("pause"), DeviceCommand::Pause);
        assert_eq!(dev("Pause the music."), DeviceCommand::Pause);
        assert_eq!(dev("next"), DeviceCommand::Next);
        assert_eq!(dev("skip this song"), DeviceCommand::Next);
        assert_eq!(dev("previous track"), DeviceCommand::Previous);
        assert_eq!(dev("stop the music"), DeviceCommand::StopMusic);
        assert_eq!(dev("resume"), DeviceCommand::Resume);
    }

    #[test]
    fn wake_residue_and_politeness_are_stripped() {
        assert_eq!(dev("Hey Hermit, pause"), DeviceCommand::Pause);
        assert_eq!(dev("computer, next"), DeviceCommand::Next);
        assert_eq!(
            dev("could you please pause the music?"),
            DeviceCommand::Pause
        );
    }

    #[test]
    fn volume_commands() {
        assert_eq!(dev("volume up"), DeviceCommand::VolumeUp);
        assert_eq!(dev("turn it up"), DeviceCommand::VolumeUp);
        assert_eq!(dev("louder"), DeviceCommand::VolumeUp);
        assert_eq!(dev("turn the volume down"), DeviceCommand::VolumeDown);
        assert_eq!(dev("set volume to 40"), DeviceCommand::VolumeSet(40));
        assert_eq!(dev("volume 90 percent"), DeviceCommand::VolumeSet(90));
    }

    #[test]
    fn volume_is_clamped_not_wrapped() {
        assert_eq!(dev("set volume to 300"), DeviceCommand::VolumeSet(100));
    }

    #[test]
    fn play_targets() {
        assert_eq!(
            dev("play miles davis on spotify"),
            DeviceCommand::PlaySpotify("miles davis".into())
        );
        assert_eq!(dev("play npr"), DeviceCommand::PlayStation("npr".into()));
        assert_eq!(
            dev("play jazz24 radio"),
            DeviceCommand::PlayStation("jazz24".into())
        );
        assert_eq!(
            dev("play kind of blue by miles davis"),
            DeviceCommand::PlaySpotify("kind of blue by miles davis".into())
        );
    }

    #[test]
    fn vague_play_requests_go_to_the_agent() {
        // "play something relaxing" needs judgement; don't guess on the fast path.
        assert_eq!(route("play something relaxing", &stations()), Route::Agent);
        assert_eq!(
            route("play some music that fits a rainy afternoon", &stations()),
            Route::Agent
        );
    }

    #[test]
    fn time_is_answered_locally() {
        assert_eq!(dev("what time is it"), DeviceCommand::TimeOfDay);
        assert_eq!(dev("what's the time?"), DeviceCommand::TimeOfDay);
    }

    #[test]
    fn real_questions_go_to_the_agent() {
        for q in [
            "what's the weather in Oslo",
            "who won the match last night",
            "explain how a heat pump works",
            "remind me what I said about the boiler",
            "what time does the pharmacy close", // NOT the local-clock command
        ] {
            assert_eq!(
                route(q, &stations()),
                Route::Agent,
                "{q} should reach the LLM"
            );
        }
    }

    #[test]
    fn barge_in_cancel_words_stop_playback() {
        assert_eq!(dev("stop"), DeviceCommand::StopMusic);
        assert_eq!(dev("nevermind"), DeviceCommand::StopMusic);
    }

    #[test]
    fn research_classification() {
        assert!(is_research("do a deep dive on solid state batteries"));
        assert!(is_research("Research the history of the Aral Sea"));
        assert!(!is_research("what's the capital of Peru"));
    }

    #[test]
    fn prefetch_gate_requires_lookup_shape_and_length() {
        assert!(should_prefetch("what is the current price of"));
        assert!(should_prefetch("who won the world cup"));
        assert!(!should_prefetch("what is"), "too short");
        assert!(
            !should_prefetch("tell me a joke about cats"),
            "not a lookup"
        );
        assert!(!should_prefetch(""), "empty");
    }

    #[test]
    fn empty_input_is_not_a_device_command() {
        assert_eq!(route("", &stations()), Route::Agent);
        assert_eq!(route("   ", &stations()), Route::Agent);
    }

    #[test]
    fn routing_is_fast() {
        // Not a real benchmark, but catches accidental O(n) blowups in the table.
        let s = stations();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            let _ = route("play kind of blue by miles davis on spotify", &s);
        }
        let per_call_us = start.elapsed().as_micros() as f64 / 1000.0;
        // Generous: this catches an accidental O(n) blowup in the table, not
        // microsecond drift on a busy machine.
        assert!(per_call_us < 500.0, "routing took {per_call_us}us/call");
    }
}
