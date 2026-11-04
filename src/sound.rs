//! The sounds that tell an action has finished, and whether it went well.
//!
//! A sound is declared per event in `[sounds]`, and nothing plays until one is.
//! `auto` plays the success sound or the error sound according to how the event ended.
//! A path plays that audio file whatever the outcome: WAV, MP3, FLAC, OGG Vorbis or AAC in an M4A.
//!
//! The built-in sounds are 2 short tones made here, so there is no asset to ship.
//! A sound plays on a thread of its own: it never holds up a turn, and a machine with no audio
//! device stays silent.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::de::{Deserializer, Error as _};
use serde::Deserialize;

/// How an action ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Error,
}

/// What an event plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sound {
    /// The sound of the outcome.
    Auto,
    Success,
    Error,
    /// An audio file, played whatever the outcome.
    File(PathBuf),
}

impl<'de> Deserialize<'de> for Sound {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        match text.as_str() {
            "auto" => Ok(Sound::Auto),
            "success" => Ok(Sound::Success),
            "error" => Ok(Sound::Error),
            "" => Err(D::Error::custom(
                "a sound is auto, success, error or an audio file",
            )),
            path => Ok(Sound::File(PathBuf::from(path))),
        }
    }
}

/// The events that can play a sound, as `[sounds]` names them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SoundsSection {
    /// When the model has answered, or the turn ended otherwise.
    pub turn_end: Option<Sound>,
    /// When a sub-agent has answered.
    pub agent_end: Option<Sound>,
    /// When the session waits for the person, as for an approval.
    pub notification: Option<Sound>,
    /// When a shell command ends, after it has run for `command-end-after` seconds.
    pub command_end: Option<Sound>,
    /// How many seconds a command must run to play `command-end`. Every command plays it when
    /// this is not written.
    pub command_end_after: Option<u64>,
}

impl SoundsSection {
    /// Both sides' sounds, where `over` has the last word.
    pub fn under(self, over: SoundsSection) -> SoundsSection {
        SoundsSection {
            turn_end: over.turn_end.or(self.turn_end),
            agent_end: over.agent_end.or(self.agent_end),
            notification: over.notification.or(self.notification),
            command_end: over.command_end.or(self.command_end),
            command_end_after: over.command_end_after.or(self.command_end_after),
        }
    }
}

/// The events that play, as the code names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    TurnEnd,
    AgentEnd,
    Notification,
    CommandEnd,
}

static DECLARED: OnceLock<SoundsSection> = OnceLock::new();

/// Fix the sounds of this process, read once from the configuration.
pub fn declare(sounds: SoundsSection) {
    let _ = DECLARED.set(sounds);
}

/// Play what `event` is declared to play, if anything.
pub fn play(event: Event, outcome: Outcome) {
    let Some(declared) = DECLARED.get() else {
        return;
    };
    let sound = match event {
        Event::TurnEnd => &declared.turn_end,
        Event::AgentEnd => &declared.agent_end,
        Event::Notification => &declared.notification,
        Event::CommandEnd => &declared.command_end,
    };
    if let Some(sound) = sound {
        start(sound.clone(), outcome);
    }
}

/// Play what the end of a command is declared to play, when it ran long enough.
pub fn command_ended(ran: std::time::Duration, outcome: Outcome) {
    let after = DECLARED
        .get()
        .and_then(|d| d.command_end_after)
        .unwrap_or(0);
    if ran.as_secs() >= after {
        play(Event::CommandEnd, outcome);
    }
}

/// One note of a built-in sound: when it starts, its pitch and how long it lasts.
#[cfg(any(feature = "sound", test))]
struct Note {
    start_ms: u64,
    hertz: f32,
    millis: u64,
}

/// The notes of a built-in sound.
///
/// Success rises by a fifth, and error falls by a fourth.
#[cfg(any(feature = "sound", test))]
fn notes(outcome: Outcome) -> &'static [Note] {
    const SUCCESS: [Note; 2] = [
        Note {
            start_ms: 0,
            hertz: 523.3,
            millis: 420,
        },
        Note {
            start_ms: 150,
            hertz: 784.0,
            millis: 650,
        },
    ];
    const ERROR: [Note; 2] = [
        Note {
            start_ms: 0,
            hertz: 392.0,
            millis: 450,
        },
        Note {
            start_ms: 170,
            hertz: 293.7,
            millis: 700,
        },
    ];
    match outcome {
        Outcome::Success => &SUCCESS,
        Outcome::Error => &ERROR,
    }
}

/// The voice of every note: a fundamental and 2 harmonics, as a ratio to the pitch and a gain.
#[cfg(feature = "sound")]
const PARTIALS: [(f32, f32); 3] = [(1.0, 1.0), (2.0, 0.14), (3.0, 0.08)];

/// How long a note takes to reach its full volume.
/// It then dies away in a straight line to its end.
#[cfg(feature = "sound")]
const ATTACK_MS: u64 = 30;

/// The volume of the player.
#[cfg(feature = "sound")]
const VOLUME: f32 = 0.45;

#[cfg(feature = "sound")]
fn start(sound: Sound, outcome: Outcome) {
    std::thread::spawn(move || {
        if let Err(why) = emit(&sound, outcome) {
            // Said once for each sound, so that a broken one does not fill the log.
            static SAID: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
                OnceLock::new();
            let said = SAID.get_or_init(Default::default);
            if said.lock().unwrap().insert(format!("{sound:?}")) {
                tracing::warn!(%why, ?sound, "sound not played");
            }
        }
    });
}

#[cfg(not(feature = "sound"))]
fn start(_sound: Sound, _outcome: Outcome) {}

#[cfg(feature = "sound")]
fn emit(sound: &Sound, outcome: Outcome) -> Result<(), String> {
    use rodio::source::{SineWave, Source};
    use std::time::Duration;

    let device = rodio::DeviceSinkBuilder::open_default_sink().map_err(|e| e.to_string())?;
    let player = rodio::Player::connect_new(device.mixer());
    player.set_volume(VOLUME);
    match sound {
        Sound::File(path) => {
            let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let source = rodio::Decoder::try_from(file).map_err(|e| e.to_string())?;
            player.append(source);
        }
        Sound::Auto | Sound::Success | Sound::Error => {
            let outcome = match sound {
                Sound::Success => Outcome::Success,
                Sound::Error => Outcome::Error,
                _ => outcome,
            };
            let gains: f32 = PARTIALS.iter().map(|(_, gain)| gain).sum();
            let mut end = 0;
            for note in notes(outcome) {
                let length = Duration::from_millis(note.millis);
                for (ratio, gain) in PARTIALS {
                    device.mixer().add(
                        SineWave::new(note.hertz * ratio)
                            .amplify(gain / gains)
                            .take_duration(length)
                            .fade_in(Duration::from_millis(ATTACK_MS))
                            .fade_out(length)
                            .delay(Duration::from_millis(note.start_ms)),
                    );
                }
                end = end.max(note.start_ms + note.millis);
            }
            std::thread::sleep(Duration::from_millis(end + 50));
            return Ok(());
        }
    }
    player.sleep_until_end();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> SoundsSection {
        toml::from_str(text).unwrap()
    }

    /// An audio file is read by its content, whatever its name says.
    #[cfg(feature = "sound")]
    #[test]
    fn a_wav_file_is_decoded() {
        let samples: Vec<i16> = (0..800).map(|i| ((i % 40) as i16 - 20) * 500).collect();
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend((36 + samples.len() as u32 * 2).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(8000u32.to_le_bytes());
        wav.extend(16000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend((samples.len() as u32 * 2).to_le_bytes());
        for s in samples {
            wav.extend(s.to_le_bytes());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ding.mp3");
        std::fs::write(&path, wav).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert!(rodio::Decoder::try_from(file).is_ok());
    }

    #[test]
    fn a_sound_is_a_word_or_a_file() {
        let s = read("turn-end = \"auto\"\nagent-end = \"error\"\nnotification = \"ding.wav\"\n");
        assert_eq!(s.turn_end, Some(Sound::Auto));
        assert_eq!(s.agent_end, Some(Sound::Error));
        assert_eq!(s.notification, Some(Sound::File("ding.wav".into())));
        assert!(toml::from_str::<SoundsSection>("turn-end = \"\"\n").is_err());
        assert!(toml::from_str::<SoundsSection>("turn_start = \"auto\"\n").is_err());
    }

    #[test]
    fn nothing_is_declared_until_the_file_says_so() {
        assert_eq!(read(""), SoundsSection::default());
    }

    #[test]
    fn the_last_word_goes_to_the_side_that_declares() {
        let user = read("turn-end = \"auto\"\nnotification = \"success\"\n");
        let project = read("turn-end = \"error\"\n");
        let merged = user.under(project);
        assert_eq!(merged.turn_end, Some(Sound::Error));
        assert_eq!(merged.notification, Some(Sound::Success));
        assert_eq!(merged.agent_end, None);
        let timed = read("command-end = \"auto\"\ncommand-end-after = 30\n");
        assert_eq!(timed.command_end_after, Some(30));
        assert_eq!(merged.under(timed).command_end_after, Some(30));
    }

    #[test]
    fn success_and_error_do_not_sound_alike() {
        let pitches = |o| {
            notes(o)
                .iter()
                .map(|n| n.hertz.to_bits())
                .collect::<Vec<_>>()
        };
        assert_ne!(pitches(Outcome::Success), pitches(Outcome::Error));
        assert!(notes(Outcome::Success)[1].hertz > notes(Outcome::Success)[0].hertz);
        assert!(notes(Outcome::Error)[1].hertz < notes(Outcome::Error)[0].hertz);
        // The notes overlap, so the first one still rings when the second starts.
        for outcome in [Outcome::Success, Outcome::Error] {
            let [first, second] = notes(outcome) else {
                panic!("a sound has 2 notes");
            };
            assert!(second.start_ms > first.start_ms);
            assert!(second.start_ms < first.start_ms + first.millis);
        }
    }
}
