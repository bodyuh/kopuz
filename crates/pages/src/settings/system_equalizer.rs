use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use dioxus::prelude::*;

const FREQUENCIES: [u32; 6] = [60, 150, 400, 1_000, 2_400, 15_000];
const FILTER_SHAPES: [(&str, f64); 6] = [
    ("LSC", std::f64::consts::FRAC_1_SQRT_2),
    ("PK", 1.0),
    ("PK", 1.0),
    ("PK", 1.0),
    ("PK", 1.0),
    ("HSC", std::f64::consts::FRAC_1_SQRT_2),
];
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
                // Spotify stores the band gains as signed 32-bit values scaled to ±12 dB.
                let raw: i32 = value.trim_matches('"').parse().ok()?;
                profile.gains[index] = (raw as f32 / i32::MAX as f32) * 12.0;
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

        let _ = writeln!(config, "Preamp: {:.1} dB", -self.headroom_db());
        for ((frequency, gain), (kind, q)) in
            FREQUENCIES.into_iter().zip(self.gains).zip(FILTER_SHAPES)
        {
            if gain != 0.0 {
                let _ = writeln!(
                    config,
                    "Filter: ON {kind} Fc {frequency} Hz Gain {gain:.3} dB Q {q:.6}"
                );
            }
        }
        config
    }

    fn headroom_db(self) -> f64 {
        let mut peak = 0.0_f64;
        // Sample the combined response at common Windows output rates, including
        // the edges of the low and high shelves. Sum responses, not slider gains.
        for sample_rate in [32_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let nyquist = sample_rate / 2.0;
            let centres = FREQUENCIES.map(|hz| prewarp(f64::from(hz), sample_rate));
            for point in 0..=384 {
                let frequency = (nyquist - 1.0).powf(f64::from(point) / 384.0);
                let warped = prewarp(frequency, sample_rate);
                let response: f64 = FILTER_SHAPES
                    .into_iter()
                    .zip(centres)
                    .zip(self.gains)
                    .map(|(((kind, q), centre), gain)| {
                        filter_response_db(kind, q, warped / centre, gain)
                    })
                    .sum();
                peak = peak.max(response);
            }
        }
        if peak < 0.01 {
            return 0.0;
        }
        (peak * 10.0).ceil() / 10.0 + 0.1
    }
}

fn prewarp(hz: f64, sample_rate: f64) -> f64 {
    (std::f64::consts::PI * hz / sample_rate).tan()
}

fn filter_response_db(kind: &str, q: f64, frequency_ratio: f64, gain: f32) -> f64 {
    let a = 10.0_f64.powf(f64::from(gain) / 40.0);
    let t = frequency_ratio;
    let square = t * t;
    let energy = |real: f64, imaginary: f64| real * real + imaginary * imaginary;
    let shelf_imaginary = a.sqrt() * t / q;
    let power_ratio = match kind {
        "LSC" => {
            a * a * energy(a - square, shelf_imaginary) / energy(1.0 - a * square, shelf_imaginary)
        }
        "HSC" => {
            a * a * energy(1.0 - a * square, shelf_imaginary) / energy(a - square, shelf_imaginary)
        }
        _ => energy(1.0 - square, a * t / q) / energy(1.0 - square, t / (a * q)),
    };
    10.0 * power_ratio.log10()
}

fn parse_gain(value: &str) -> Option<f32> {
    let gain: f32 = value.trim_matches('"').parse().ok()?;
    (-12.0..=12.0).contains(&gain).then_some(gain)
}

fn profile_path() -> Option<PathBuf> {
    let dir = windows_registry::LOCAL_MACHINE
        .open(r"SOFTWARE\EqualizerAPO")
        .and_then(|key| key.get_string("ConfigPath"))
        .ok()?;
    Some(PathBuf::from(dir).join("KopuzEQ").join("profile.txt"))
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
    let path = profile_path();
    let mut saved = use_signal(|| {
        fs::read_to_string(path.as_ref()?)
            .ok()
            .and_then(|text| Profile::parse(text.lines().next()?))
    });
    let mut draft = use_signal(|| saved.peek().unwrap_or_default());
    let mut status = use_signal(String::new);
    let settings = *draft.read();
    let dirty = *saved.read() != Some(settings);
    let ready = path.as_ref().is_some_and(|path| path.is_file());

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
                        span { {format!("{label} · {:+.1} dB", settings.gains[index])} }
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
                                dialog = dialog.set_directory(PathBuf::from(appdata).join("Spotify").join("Users"));
                            }
                            if let Some(file) = dialog.pick_file().await {
                                match fs::read_to_string(file.path()) {
                                    Ok(text) => match Profile::from_spotify(&text) {
                                    Some(profile) => {
                                            draft.set(profile);
                                            status.set("Spotify settings imported. Select Apply EQ to use them.".into());
                                        }
                                        None => status.set("Select the prefs file inside your Spotify account folder under Spotify/Users.".into()),
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
                    disabled: !dirty || !ready,
                    onclick: move |_| {
                        let Some(path) = path.as_ref() else { return; };
                        match save_profile(path, settings) {
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
                    "Install Equalizer APO, enable it on your headphones or speakers, then run setup-kopuz-eq.ps1 from this build as administrator."
                }
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
            "audio.equalizer.low_shelf_gain_v2=536870912\naudio.equalizer.high_shelf_gain_v2=357913941",
        )
        .unwrap();
        let output = profile.apo_config();
        assert!((3.0..3.3).contains(&profile.headroom_db()));
        assert!(output.contains("Preamp: -3."));
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

    #[test]
    fn overlapping_boosts_get_headroom_without_summing_all_slider_values() {
        let profile = Profile {
            enabled: true,
            gains: [12.0; 6],
        };
        assert!((12.0..36.0).contains(&profile.headroom_db()));
        assert_eq!(Profile::default().headroom_db(), 0.0);
        assert!((filter_response_db("PK", 1.0, 1.0, 6.0) - 6.0).abs() < 1e-10);
    }
}
