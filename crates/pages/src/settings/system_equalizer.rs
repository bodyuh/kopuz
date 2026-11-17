use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use dioxus::prelude::*;

const FREQUENCIES: [u32; 6] = [60, 150, 400, 1_000, 2_400, 15_000];
const LABELS: [&str; 6] = ["60 Hz", "150 Hz", "400 Hz", "1 kHz", "2.4 kHz", "15 kHz"];
const PRESETS: [(&str, [f32; 6]); 5] = [
    ("Flat", [0.0; 6]),
    ("Bass boost", [6.0, 4.0, 1.0, 0.0, 0.0, 0.0]),
    ("Clear vocals", [-2.0, -1.0, 0.0, 2.0, 3.0, 0.0]),
    ("Treble boost", [0.0, 0.0, 0.0, 0.0, 3.0, 5.0]),
    ("Loudness", [4.0, 2.0, 0.0, -1.0, 1.0, 4.0]),
];
const PROFILE_HEADER: &str = "# Kopuz system EQ v1 ";

#[derive(Clone, Copy, Default, PartialEq)]
struct Profile {
    enabled: bool,
    gains: [f32; 6],
}

impl Profile {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.strip_prefix(PROFILE_HEADER)?.split_whitespace();
        let enabled = fields.next()?.parse().ok()?;
        let mut gains = [0.0; 6];
        for gain in &mut gains {
            *gain = parse_gain(fields.next()?)?;
        }
        Some(Self { enabled, gains })
    }

    fn from_spotify(prefs: &str) -> Option<Self> {
        let keys = [
            "low_shelf",
            "low_peak",
            "low_mid_peak",
            "high_mid_peak",
            "high_peak",
            "high_shelf",
        ];
        let mut profile = Self {
            enabled: true,
            ..Self::default()
        };
        let mut found = false;
        for (index, key) in keys.iter().enumerate() {
            let prefix = format!("audio.equalizer.{key}_gain_v2");
            if let Some(value) = prefs.lines().find_map(|line| {
                let (name, value) = line.split_once('=')?;
                (name.trim() == prefix).then_some(value.trim())
            }) {
                profile.gains[index] = parse_gain(value)?;
                found = true;
            }
        }
        found.then_some(profile)
    }

    fn apo_config(self) -> String {
        let mut config = format!("{PROFILE_HEADER}{}", self.enabled);
        for gain in self.gains {
            let _ = write!(config, " {gain:.3}");
        }
        config.push('\n');
        if !self.enabled {
            return config;
        }

        // Reserve enough headroom even where multiple filter boosts overlap.
        let boost: f32 = self.gains.into_iter().map(|gain| gain.max(0.0)).sum();
        let _ = writeln!(config, "Preamp: {:.3} dB", -boost);
        let shape = [
            ("LSC", 0.707_107),
            ("PK", 1.0),
            ("PK", 1.0),
            ("PK", 1.0),
            ("PK", 1.0),
            ("HSC", 0.707_107),
        ];
        for ((frequency, gain), (kind, q)) in FREQUENCIES.into_iter().zip(self.gains).zip(shape) {
            if gain != 0.0 {
                let _ = writeln!(
                    config,
                    "Filter: ON {kind} Fc {frequency} Hz Gain {gain:.3} dB Q {q:.6}"
                );
            }
        }
        config
    }
}

fn parse_gain(value: &str) -> Option<f32> {
    let gain: f32 = value.trim_matches('"').parse().ok()?;
    (-12.0..=12.0).contains(&gain).then_some(gain)
}

fn profile_path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("PUBLIC")?).join("Documents");
    Some(dir.join("Kopuz System EQ.txt"))
}

fn save_profile(path: &Path, profile: Profile) -> io::Result<()> {
    match fs::read_to_string(path) {
        Ok(text) if !text.starts_with(PROFILE_HEADER) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the target file is not a Kopuz EQ profile",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(profile.apo_config().as_bytes())?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[component]
pub(super) fn SystemEqualizer() -> Element {
    let Some(path) = profile_path() else {
        return rsx! {};
    };
    let mut saved = use_signal(|| {
        fs::read_to_string(&path)
            .ok()
            .and_then(|text| Profile::parse(text.lines().next()?))
    });
    let mut draft = use_signal(|| saved.peek().unwrap_or_default());
    let mut status = use_signal(String::new);
    let settings = *draft.read();
    let dirty = *saved.read() != Some(settings);
    let include = format!("Include: {}", path.display());

    rsx! {
        div { class: "app-card flex flex-col gap-4 p-4",
            h3 { class: "font-semibold", "Spotify-style system equalizer" }
            p { class: "text-sm text-slate-400",
                "Applies to Spotify, SoundCloud and other audio on the Windows output configured in Equalizer APO. Turn off the built-in Kopuz EQ below to avoid applying both equalizers."
            }
            label { class: "flex items-center gap-2 text-sm",
                input {
                    r#type: "checkbox",
                    checked: settings.enabled,
                    onchange: move |event| draft.write().enabled = event.checked(),
                }
                "Enable system EQ"
            }
            div { class: "flex flex-wrap gap-2",
                for (name, gains) in PRESETS {
                    button {
                        r#type: "button",
                        class: "app-button-tonal rounded-lg px-3 py-2 text-sm",
                        onclick: move |_| {
                            draft.set(Profile { enabled: true, gains });
                        },
                        "{name}"
                    }
                }
            }
            div { class: "grid grid-cols-2 gap-4 md:grid-cols-3",
                for (index, label) in LABELS.into_iter().enumerate() {
                    label { class: "flex min-w-0 flex-col gap-1 text-sm",
                        span { "{label} · {settings.gains[index]:+.1} dB" }
                        input {
                            r#type: "range",
                            min: "-12",
                            max: "12",
                            step: "0.1",
                            value: settings.gains[index].to_string(),
                            oninput: move |event| {
                                if let Some(gain) = parse_gain(&event.value()) {
                                    draft.write().gains[index] = gain;
                                }
                            },
                        }
                    }
                }
            }
            div { class: "flex flex-wrap gap-2",
                button {
                    r#type: "button",
                    class: "app-button-tonal rounded-lg px-3 py-2 text-sm",
                    onclick: move |_| {
                        spawn(async move {
                            let mut dialog = rfd::AsyncFileDialog::new().set_title("Select Spotify's prefs file");
                            if let Some(appdata) = std::env::var_os("APPDATA") {
                                dialog = dialog.set_directory(PathBuf::from(appdata).join("Spotify"));
                            }
                            if let Some(file) = dialog.pick_file().await {
                                match fs::read_to_string(file.path()) {
                                    Ok(text) => match Profile::from_spotify(&text) {
                                        Some(profile) => {
                                            draft.set(profile);
                                            status.set("Spotify settings imported. Select Apply EQ to use them.".into());
                                        }
                                        None => status.set("No supported EQ settings found in this Spotify prefs file.".into()),
                                    },
                                    Err(error) => status.set(format!("Could not read Spotify prefs: {error}")),
                                }
                            }
                        });
                    },
                    "Import Spotify EQ"
                }
                button {
                    r#type: "button",
                    class: "app-button-tonal rounded-lg px-3 py-2 text-sm",
                    disabled: !dirty,
                    onclick: move |_| {
                        match save_profile(&path, settings) {
                            Ok(()) => {
                                saved.set(Some(settings));
                                status.set("Profile saved.".into());
                            }
                            Err(error) => status.set(format!("Could not save the EQ profile: {error}")),
                        }
                    },
                    "Apply EQ"
                }
            }
            p { role: "status", class: "text-sm", "{status}" }
            details { class: "text-sm text-slate-400",
                summary { "Set up Equalizer APO once" }
                p {
                    "Install Equalizer APO and enable it on your headphones or speakers. Select Apply EQ to create the profile, then add this line in Equalizer APO's Configuration Editor."
                }
                code { class: "select-all", "{include}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spotify_import_round_trips_with_headroom_and_shelves() {
        let profile = Profile::from_spotify(
            "audio.equalizer.low_shelf_gain_v2=3.0\naudio.equalizer.high_shelf_gain_v2=2.0",
        )
        .unwrap();
        let output = profile.apo_config();
        assert!(output.contains("Preamp: -5.000 dB"));
        assert!(output.contains("Filter: ON LSC Fc 60 Hz"));
        assert!(output.contains("Filter: ON HSC Fc 15000 Hz"));
        assert!(Profile::parse(output.lines().next().unwrap()) == Some(profile));
        assert!(Profile::from_spotify("audio.equalizer.low_shelf_gain_v2=NaN").is_none());
    }

    #[test]
    fn disabled_profile_contains_no_filters() {
        let profile = Profile {
            enabled: false,
            gains: [4.0; 6],
        };
        let output = profile.apo_config();
        assert_eq!(output.lines().count(), 1);
        assert!(Profile::parse(output.trim_end()) == Some(profile));
    }
}
