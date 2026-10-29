//! The value types of the settings, each validated when it is made.
//!
//! Ranges, choices and limits come from the dashboard's controls
//! (`controller/static/dashboard.jsx`, `DeviceConfigForm`), `docs/configuration.md` and
//! `docs/post-afe-audio-architecture.md` §18.4; where a value goes to the device, the Go
//! firmware's own checks (`device/internal/config`, `device/internal/wakeword`) are the
//! floor. Each type's [`Default`] is the product default, with its source.

use std::collections::BTreeSet;
use std::fmt;

use echomuse_protocol::Sha256;
use echomuse_protocol::session::{RuleCombine, RuleProfile};

use crate::json::{Json, JsonObject};

/// Why a value is not a valid setting.
#[derive(Clone, Debug, PartialEq)]
pub enum ValueError {
    /// The JSON type is wrong.
    WrongType {
        /// What the setting takes.
        expected: &'static str,
    },
    /// A number outside its range.
    OutOfRange {
        /// The number.
        value: f64,
        /// The smallest allowed.
        min: f64,
        /// The largest allowed.
        max: f64,
    },
    /// A number with more than two decimals where the setting takes at most two.
    TooPrecise {
        /// The number.
        value: f64,
    },
    /// A list of the wrong length.
    WrongLength {
        /// The length required.
        expected: usize,
        /// The length given.
        found: usize,
    },
    /// A list or map longer than allowed.
    TooMany {
        /// The most allowed.
        max: usize,
        /// How many were given.
        found: usize,
    },
    /// An entry that repeats an earlier one.
    Duplicate {
        /// The repeated entry as given.
        entry: String,
    },
    /// A word that is not one of the choices.
    NotAChoice {
        /// What was given.
        value: String,
        /// The choices.
        choices: &'static [&'static str],
    },
    /// A rule of the setting that the value breaks.
    Rule {
        /// The rule, in words.
        rule: &'static str,
    },
    /// A field an object requires is missing.
    MissingField {
        /// The field.
        field: &'static str,
    },
    /// An object field the setting does not have.
    UnknownField {
        /// The field.
        field: String,
    },
    /// One part of the value is invalid.
    At {
        /// Which part: a list index or field name.
        at: String,
        /// What is wrong with it.
        error: Box<ValueError>,
    },
}

impl ValueError {
    fn at(at: impl fmt::Display, error: ValueError) -> Self {
        Self::At {
            at: at.to_string(),
            error: Box::new(error),
        }
    }
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongType { expected } => write!(f, "must be {expected}"),
            Self::OutOfRange { value, min, max } => {
                write!(f, "{value} is outside {min} to {max}")
            }
            Self::TooPrecise { value } => write!(f, "{value} has more than two decimals"),
            Self::WrongLength { expected, found } => {
                write!(f, "needs {expected} entries, has {found}")
            }
            Self::TooMany { max, found } => write!(f, "{found} entries, at most {max}"),
            Self::Duplicate { entry } => write!(f, "{entry:?} is listed twice"),
            Self::NotAChoice { value, choices } => {
                write!(f, "{value:?} is not one of {}", choices.join(", "))
            }
            Self::Rule { rule } => f.write_str(rule),
            Self::MissingField { field } => write!(f, "{field} is missing"),
            Self::UnknownField { field } => write!(f, "{field:?} is not a field of this setting"),
            Self::At { at, error } => write!(f, "{at}: {error}"),
        }
    }
}

impl std::error::Error for ValueError {}

/// A setting's value type: parsed from the JSON a document stores or an edit sends, and
/// written back as JSON in the stored shape.
pub trait SettingType: Clone + fmt::Debug + PartialEq + Sized {
    /// Parse and validate.
    fn from_json(value: &Json) -> Result<Self, ValueError>;
    /// The stored JSON form.
    fn to_json(&self) -> Json;
}

impl SettingType for bool {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        match value {
            Json::Bool(b) => Ok(*b),
            _ => Err(ValueError::WrongType {
                expected: "true or false",
            }),
        }
    }

    fn to_json(&self) -> Json {
        Json::Bool(*self)
    }
}

fn number(value: &Json) -> Result<f64, ValueError> {
    value.as_f64().ok_or(ValueError::WrongType {
        expected: "a number",
    })
}

fn in_range(value: f64, min: f64, max: f64) -> Result<f64, ValueError> {
    if value.is_finite() && (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(ValueError::OutOfRange { value, min, max })
    }
}

fn two_decimals(value: f64) -> Result<f64, ValueError> {
    let hundredths = value * 100.0;
    if (hundredths - hundredths.round()).abs() < 1e-6 {
        Ok(value)
    } else {
        Err(ValueError::TooPrecise { value })
    }
}

fn whole(value: &Json, min: i64, max: i64) -> Result<i64, ValueError> {
    let n = value.as_whole().ok_or(ValueError::WrongType {
        expected: "a whole number",
    })?;
    if (min..=max).contains(&n) {
        Ok(n)
    } else {
        Err(ValueError::OutOfRange {
            value: n as f64,
            min: min as f64,
            max: max as f64,
        })
    }
}

fn string(value: &Json) -> Result<&str, ValueError> {
    match value {
        Json::String(s) => Ok(s),
        _ => Err(ValueError::WrongType {
            expected: "a string",
        }),
    }
}

fn number_json(value: f64) -> Json {
    Json::number(value).expect("validated settings are finite")
}

/// A number setting within a range, written as an integer when whole.
macro_rules! bounded_number {
    ($(#[$meta:meta])* $name:ident, $min:expr, $max:expr, default $default:expr, $two:expr) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct $name(f64);

        impl $name {
            /// The smallest value.
            pub const MIN: f64 = $min;
            /// The largest value.
            pub const MAX: f64 = $max;

            /// The value, when it is in range.
            pub fn new(value: f64) -> Result<Self, ValueError> {
                let value = in_range(value, Self::MIN, Self::MAX)?;
                if $two {
                    two_decimals(value)?;
                }
                Ok(Self(value))
            }

            /// The number.
            pub fn get(self) -> f64 {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self($default)
            }
        }

        impl SettingType for $name {
            fn from_json(value: &Json) -> Result<Self, ValueError> {
                Self::new(number(value)?)
            }

            fn to_json(&self) -> Json {
                number_json(self.0)
            }
        }
    };
}

/// A whole-number setting within a range.
macro_rules! bounded_whole {
    ($(#[$meta:meta])* $name:ident, $ty:ty, $min:expr, $max:expr, default $default:expr) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub struct $name($ty);

        impl $name {
            /// The smallest value.
            pub const MIN: $ty = $min;
            /// The largest value.
            pub const MAX: $ty = $max;

            /// The value, when it is in range.
            pub fn new(value: $ty) -> Result<Self, ValueError> {
                if (Self::MIN..=Self::MAX).contains(&value) {
                    Ok(Self(value))
                } else {
                    Err(ValueError::OutOfRange {
                        value: f64::from(value),
                        min: f64::from(Self::MIN),
                        max: f64::from(Self::MAX),
                    })
                }
            }

            /// The number.
            pub fn get(self) -> $ty {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self($default)
            }
        }

        impl SettingType for $name {
            fn from_json(value: &Json) -> Result<Self, ValueError> {
                let n = whole(value, i64::from(Self::MIN), i64::from(Self::MAX))?;
                // In range of the type, checked just above.
                Ok(Self(n as $ty))
            }

            fn to_json(&self) -> Json {
                Json::int(i64::from(self.0))
            }
        }
    };
}

/// A closed vocabulary stored as its word.
macro_rules! word_choice {
    ($(#[$meta:meta])* $name:ident, default $default:ident, { $($(#[$vmeta:meta])* $variant:ident => $wire:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Every word, in the dashboard's order.
            pub const CHOICES: &'static [&'static str] = &[$($wire,)+];

            /// The stored word.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::$default
            }
        }

        impl SettingType for $name {
            fn from_json(value: &Json) -> Result<Self, ValueError> {
                match string(value)? {
                    $($wire => Ok(Self::$variant),)+
                    other => Err(ValueError::NotAChoice {
                        value: other.to_owned(),
                        choices: Self::CHOICES,
                    }),
                }
            }

            fn to_json(&self) -> Json {
                Json::String(self.as_str().to_owned())
            }
        }
    };
}

// ---- Playback -------------------------------------------------------------------------

/// `eqBands`: the eight equalizer faders in dB (125, 250, 500, 1k, 2k, 3.5k, 5.5k and
/// 8k Hz), each from −12 to +12. Default flat (configuration.md "Equalizer").
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EqBands([f64; 8]);

impl EqBands {
    /// The quietest fader position.
    pub const MIN_DB: f64 = -12.0;
    /// The loudest fader position.
    pub const MAX_DB: f64 = 12.0;

    /// The bands, when each is in range.
    pub fn new(bands: [f64; 8]) -> Result<Self, ValueError> {
        for (i, band) in bands.iter().enumerate() {
            in_range(*band, Self::MIN_DB, Self::MAX_DB).map_err(|e| ValueError::at(i, e))?;
        }
        Ok(Self(bands))
    }

    /// The gains in dB, lowest band first.
    pub fn db(&self) -> [f64; 8] {
        self.0
    }
}

impl SettingType for EqBands {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        let Json::Array(items) = value else {
            return Err(ValueError::WrongType {
                expected: "a list of eight numbers",
            });
        };
        if items.len() != 8 {
            return Err(ValueError::WrongLength {
                expected: 8,
                found: items.len(),
            });
        }
        let mut bands = [0.0; 8];
        for (i, item) in items.iter().enumerate() {
            bands[i] = number(item).map_err(|e| ValueError::at(i, e))?;
        }
        Self::new(bands)
    }

    fn to_json(&self) -> Json {
        Json::Array(self.0.iter().map(|b| number_json(*b)).collect())
    }
}

bounded_number!(
    /// `duckDb`: how far content drops under a voice response and during a provisional
    /// duck, −40 to 0 dB. Default −18 dB (configuration.md "Duck depth"; the firmware's
    /// `DefaultDuckDB`).
    DuckDb, -40.0, 0.0, default -18.0, false
);

// ---- Wake word ------------------------------------------------------------------------

/// The graph SHA-256 of the deployed BCResNet model (post-afe-audio-architecture.md
/// §5.1), the default `wakeModel` (configuration.md "Defaults at a glance").
pub const DEPLOYED_WAKE_GRAPH_SHA256: &str =
    "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f";

/// `wakeModel`: the graph SHA-256 of the selected registry model.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WakeModelChoice(pub Sha256);

impl Default for WakeModelChoice {
    fn default() -> Self {
        Self(Sha256::parse(DEPLOYED_WAKE_GRAPH_SHA256).expect("a valid SHA-256"))
    }
}

impl SettingType for WakeModelChoice {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        Sha256::parse(string(value)?)
            .map(Self)
            .ok_or(ValueError::Rule {
                rule: "must be a graph SHA-256: 64 lowercase hex digits",
            })
    }

    fn to_json(&self) -> Json {
        Json::String(self.0.as_str().to_owned())
    }
}

bounded_number!(
    /// `nearMissThreshold`: the near-miss floor, 0.05 to 0.60 with at most two decimals.
    /// Default 0.17 (configuration.md "Near-miss floor"; the dashboard rounds to two
    /// decimals because the controller takes no more).
    NearMissFloor, 0.05, 0.60, default 0.17, true
);

bounded_whole!(
    /// `wakeArbitrationMs`: the wake arbitration window, 0 (off) to 2000 ms. Default 700
    /// (spec §4.2, configuration.md "Arbitration window").
    ArbitrationWindowMs, u16, 0, 2000, default 700
);

bounded_number!(
    /// A wake rule's threshold: 0.30 to 0.99 with at most two decimals
    /// (configuration.md "Open rules").
    RuleThreshold, 0.30, 0.99, default 0.95, true
);

/// One `wakeOpenRules` or `wakeShadowRules` entry: `{profile, windows, combine,
/// threshold}`, stored with the keys in that order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WakeRule {
    /// The profile whose hops the rule looks at.
    pub profile: RuleProfile,
    /// How many of the latest raw scores it combines, 1 to 3.
    pub windows: RuleWindows,
    /// Whether their mean or every one must reach the threshold.
    pub combine: RuleCombine,
    /// The threshold.
    pub threshold: RuleThreshold,
}

bounded_whole!(
    /// A wake rule's window count, 1 to 3 (the device's `SmoothingWindows`).
    RuleWindows, u8, 1, 3, default 2
);

impl WakeRule {
    const FIELDS: [&'static str; 4] = ["profile", "windows", "combine", "threshold"];

    fn from_json(value: &Json) -> Result<Self, ValueError> {
        let Json::Object(object) = value else {
            return Err(ValueError::WrongType {
                expected: "an object {profile, windows, combine, threshold}",
            });
        };
        reject_unknown_fields(object, &Self::FIELDS)?;
        let field = |name: &'static str| {
            object
                .get(name)
                .ok_or(ValueError::MissingField { field: name })
        };
        let profile_word = string(field("profile")?).map_err(|e| ValueError::at("profile", e))?;
        let profile = RuleProfile::from_wire(profile_word).ok_or_else(|| {
            ValueError::at(
                "profile",
                ValueError::NotAChoice {
                    value: profile_word.to_owned(),
                    choices: &["idle", "playback"],
                },
            )
        })?;
        let windows =
            RuleWindows::from_json(field("windows")?).map_err(|e| ValueError::at("windows", e))?;
        let combine_word = string(field("combine")?).map_err(|e| ValueError::at("combine", e))?;
        let combine = RuleCombine::from_wire(combine_word).ok_or_else(|| {
            ValueError::at(
                "combine",
                ValueError::NotAChoice {
                    value: combine_word.to_owned(),
                    choices: &["mean", "all"],
                },
            )
        })?;
        let threshold = RuleThreshold::from_json(field("threshold")?)
            .map_err(|e| ValueError::at("threshold", e))?;
        Ok(Self {
            profile,
            windows,
            combine,
            threshold,
        })
    }

    fn to_json(self) -> Json {
        let mut object = JsonObject::new();
        object.insert("profile", Json::String(self.profile.as_str().to_owned()));
        object.insert("windows", self.windows.to_json());
        object.insert("combine", Json::String(self.combine.as_str().to_owned()));
        object.insert("threshold", self.threshold.to_json());
        Json::Object(object)
    }

    fn describe(&self) -> String {
        format!(
            "{} {}-window {} >= {:.2}",
            self.profile,
            self.windows.get(),
            self.combine,
            self.threshold.get()
        )
    }

    /// Whether the rule is one of a model's two baseline rules: the three-window mean at
    /// that profile's threshold.
    pub fn is_baseline(&self, idle: f64, playback: f64) -> bool {
        let threshold = match self.profile {
            RuleProfile::Idle => idle,
            RuleProfile::Playback => playback,
        };
        self.windows.get() == 3
            && self.combine == RuleCombine::Mean
            && (self.threshold.get() - threshold).abs() < 1e-9
    }
}

fn reject_unknown_fields(object: &JsonObject, fields: &[&str]) -> Result<(), ValueError> {
    match object.iter().find(|(k, _)| !fields.contains(k)) {
        Some((field, _)) => Err(ValueError::UnknownField {
            field: field.to_owned(),
        }),
        None => Ok(()),
    }
}

fn rules_from_json(value: &Json, max: usize) -> Result<Vec<WakeRule>, ValueError> {
    let Json::Array(items) = value else {
        return Err(ValueError::WrongType {
            expected: "a list of rules",
        });
    };
    let rules = items
        .iter()
        .enumerate()
        .map(|(i, item)| WakeRule::from_json(item).map_err(|e| ValueError::at(i, e)))
        .collect::<Result<Vec<_>, _>>()?;
    check_rules(&rules, max)?;
    Ok(rules)
}

fn check_rules(rules: &[WakeRule], max: usize) -> Result<(), ValueError> {
    if rules.len() > max {
        return Err(ValueError::TooMany {
            max,
            found: rules.len(),
        });
    }
    for (i, rule) in rules.iter().enumerate() {
        if rules[..i].contains(rule) {
            return Err(ValueError::Duplicate {
                entry: rule.describe(),
            });
        }
    }
    Ok(())
}

/// `wakeOpenRules`: live open rules added after the model's two baseline rules, at most
/// four, no duplicates. Default none (configuration.md "Open rules"). A rule equal to a
/// baseline rule depends on the selected model, so the settings service checks that.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtraOpenRules(Vec<WakeRule>);

impl ExtraOpenRules {
    /// The most extra rules.
    pub const MAX: usize = 4;

    /// The rules, when there are at most four and none repeats.
    pub fn new(rules: Vec<WakeRule>) -> Result<Self, ValueError> {
        check_rules(&rules, Self::MAX)?;
        Ok(Self(rules))
    }

    /// The rules in order.
    pub fn rules(&self) -> &[WakeRule] {
        &self.0
    }
}

impl SettingType for ExtraOpenRules {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        rules_from_json(value, Self::MAX).map(Self)
    }

    fn to_json(&self) -> Json {
        Json::Array(self.0.iter().map(|r| r.to_json()).collect())
    }
}

/// `wakeShadowRules`: rules the device evaluates without acting on, at most eight, no
/// duplicates. Default the five idle rules configuration.md "Shadow rules" lists.
#[derive(Clone, Debug, PartialEq)]
pub struct ShadowRules(Vec<WakeRule>);

impl ShadowRules {
    /// The most shadow rules.
    pub const MAX: usize = 8;

    /// The rules, when there are at most eight and none repeats.
    pub fn new(rules: Vec<WakeRule>) -> Result<Self, ValueError> {
        check_rules(&rules, Self::MAX)?;
        Ok(Self(rules))
    }

    /// The rules in order.
    pub fn rules(&self) -> &[WakeRule] {
        &self.0
    }
}

impl Default for ShadowRules {
    fn default() -> Self {
        let rule = |windows, combine, threshold| WakeRule {
            profile: RuleProfile::Idle,
            windows: RuleWindows(windows),
            combine,
            threshold: RuleThreshold(threshold),
        };
        Self(vec![
            rule(2, RuleCombine::Mean, 0.90),
            rule(2, RuleCombine::Mean, 0.95),
            rule(2, RuleCombine::All, 0.90),
            rule(2, RuleCombine::All, 0.95),
            rule(1, RuleCombine::Mean, 0.97),
        ])
    }
}

impl SettingType for ShadowRules {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        rules_from_json(value, Self::MAX).map(Self)
    }

    fn to_json(&self) -> Json {
        Json::Array(self.0.iter().map(|r| r.to_json()).collect())
    }
}

// ---- Speech ---------------------------------------------------------------------------

word_choice!(
    /// Who transcribes a request at its pauses.
    PauseAsrEngine, default Kroko, {
        /// The controller's own streaming Kroko model.
        Kroko => "kroko",
        /// A Wyoming speech-to-text server.
        Wyoming => "wyoming",
    }
);

/// `pauseAsr`: `{engine, host, port, model, language}`, stored with the keys in that
/// order. Default `{"kroko", "", 10300, "", "en"}` (post-afe-audio-architecture.md §18.4,
/// configuration.md "Pause transcription"). `wyoming` needs a host name or IP address;
/// an empty model or language leaves the choice to the server.
#[derive(Clone, Debug, PartialEq)]
pub struct PauseAsr {
    engine: PauseAsrEngine,
    host: String,
    port: u16,
    model: String,
    language: String,
}

impl Default for PauseAsr {
    fn default() -> Self {
        Self {
            engine: PauseAsrEngine::Kroko,
            host: String::new(),
            port: 10300,
            model: String::new(),
            language: "en".to_owned(),
        }
    }
}

impl PauseAsr {
    const FIELDS: [&'static str; 5] = ["engine", "host", "port", "model", "language"];

    /// A pause transcription choice, when it is complete and valid.
    pub fn new(
        engine: PauseAsrEngine,
        host: String,
        port: u16,
        model: String,
        language: String,
    ) -> Result<Self, ValueError> {
        if port == 0 {
            return Err(ValueError::at(
                "port",
                ValueError::OutOfRange {
                    value: 0.0,
                    min: 1.0,
                    max: 65535.0,
                },
            ));
        }
        if !host.is_empty() && !is_host(&host) {
            return Err(ValueError::at(
                "host",
                ValueError::Rule {
                    rule: "must be a host name or IP address",
                },
            ));
        }
        if engine == PauseAsrEngine::Wyoming && host.is_empty() {
            return Err(ValueError::at(
                "host",
                ValueError::Rule {
                    rule: "a Wyoming server needs a host name or IP address",
                },
            ));
        }
        for (field, text) in [("model", &model), ("language", &language)] {
            if text.chars().any(char::is_control) {
                return Err(ValueError::at(
                    field,
                    ValueError::Rule {
                        rule: "must not contain control characters",
                    },
                ));
            }
        }
        Ok(Self {
            engine,
            host,
            port,
            model,
            language,
        })
    }

    /// The engine.
    pub fn engine(&self) -> PauseAsrEngine {
        self.engine
    }

    /// The Wyoming server's host; empty for Kroko.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The Wyoming server's port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The model to ask for; empty for the server's default.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The language to ask for; empty for the server's default.
    pub fn language(&self) -> &str {
        &self.language
    }
}

fn is_host(host: &str) -> bool {
    host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

impl SettingType for PauseAsr {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        let Json::Object(object) = value else {
            return Err(ValueError::WrongType {
                expected: "an object {engine, host, port, model, language}",
            });
        };
        reject_unknown_fields(object, &Self::FIELDS)?;
        let field = |name: &'static str| {
            object
                .get(name)
                .ok_or(ValueError::MissingField { field: name })
        };
        let engine =
            PauseAsrEngine::from_json(field("engine")?).map_err(|e| ValueError::at("engine", e))?;
        let text = |name: &'static str| -> Result<String, ValueError> {
            string(field(name)?)
                .map(str::to_owned)
                .map_err(|e| ValueError::at(name, e))
        };
        let port = whole(field("port")?, 1, 65535).map_err(|e| ValueError::at("port", e))?;
        Self::new(
            engine,
            text("host")?,
            // In range, checked just above.
            port as u16,
            text("model")?,
            text("language")?,
        )
    }

    fn to_json(&self) -> Json {
        let mut object = JsonObject::new();
        object.insert("engine", self.engine.to_json());
        object.insert("host", Json::String(self.host.clone()));
        object.insert("port", Json::int(i64::from(self.port)));
        object.insert("model", Json::String(self.model.clone()));
        object.insert("language", Json::String(self.language.clone()));
        Json::Object(object)
    }
}

// ---- Ring -----------------------------------------------------------------------------

word_choice!(
    /// `ledScene`: the LED ring's colour scene. Default `standard`
    /// (configuration.md "Ring").
    LedScene, default Standard, {
        /// Green.
        Standard => "standard",
        /// Pale sky blue.
        Airy => "airy",
        /// Deep crimson with an ember spinner.
        Malevolent => "malevolent",
        /// Rainbow.
        Pride => "pride",
        /// The listening and thinking colours below.
        Custom => "custom",
    }
);

/// A ring colour, `#rrggbb` (an HTML colour input's value), stored as given.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LedColor(String);

impl LedColor {
    /// The colour, when it is `#` and six hex digits.
    pub fn new(text: &str) -> Result<Self, ValueError> {
        let valid = text.len() == 7
            && text.starts_with('#')
            && text[1..].bytes().all(|b| b.is_ascii_hexdigit());
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(ValueError::Rule {
                rule: "must be a colour #rrggbb",
            })
        }
    }

    /// The red, green and blue components.
    pub fn rgb(&self) -> [u8; 3] {
        let byte = |i: usize| u8::from_str_radix(&self.0[i..i + 2], 16).expect("validated hex");
        [byte(1), byte(3), byte(5)]
    }

    /// The colour as stored.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SettingType for LedColor {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        Self::new(string(value)?)
    }

    fn to_json(&self) -> Json {
        Json::String(self.0.clone())
    }
}

/// `ledListenColor`: the solid listening ring of the custom scene. Default `#00b400`
/// (configuration.md "Ring").
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ListenColor(pub LedColor);

impl Default for ListenColor {
    fn default() -> Self {
        Self(LedColor("#00b400".to_owned()))
    }
}

/// `ledThinkColor`: the thinking spinner of the custom scene. Default `#00c800`
/// (configuration.md "Ring").
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ThinkColor(pub LedColor);

impl Default for ThinkColor {
    fn default() -> Self {
        Self(LedColor("#00c800".to_owned()))
    }
}

macro_rules! color_setting {
    ($name:ident) => {
        impl SettingType for $name {
            fn from_json(value: &Json) -> Result<Self, ValueError> {
                LedColor::from_json(value).map(Self)
            }

            fn to_json(&self) -> Json {
                self.0.to_json()
            }
        }
    };
}
color_setting!(ListenColor);
color_setting!(ThinkColor);

bounded_number!(
    /// `meterAttack`: how fast the speaking meter rises, 0.05 to 1. Default 0.6
    /// (configuration.md "Meter response").
    MeterAttack, 0.05, 1.0, default 0.6, false
);
bounded_number!(
    /// `meterDecay`: how fast the speaking meter falls, 0.02 to 1. Default 0.30.
    MeterDecay, 0.02, 1.0, default 0.30, false
);
bounded_number!(
    /// `meterFloor`: brightness during silence, 0 to 0.6. Default 0.06.
    MeterFloor, 0.0, 0.6, default 0.06, false
);
bounded_number!(
    /// `meterGamma`: the meter's contrast, 1 to 3.5. Default 2.2.
    MeterGamma, 1.0, 3.5, default 2.2, false
);
bounded_number!(
    /// `meterRef`: the speaker level shown at full brightness, 0.02 to 1. Default 0.22.
    MeterRef, 0.02, 1.0, default 0.22, false
);
bounded_number!(
    /// `meterCurve`: below 1 lifts quiet sound into view, 0.3 to 2. Default 0.7.
    MeterCurve, 0.3, 2.0, default 0.7, false
);

// ---- Button ---------------------------------------------------------------------------

bounded_whole!(
    /// `buttonMultiTapMs`: the window grouping taps into single, double or triple, 0
    /// (off) to 600 ms. Default 0 (configuration.md "Multi-tap window").
    MultiTapMs, u16, 0, 600, default 0
);

// ---- Timers & alarms ------------------------------------------------------------------

/// `timerSound` and `alarmSound`: a sound catalog entry, or the default sound when
/// stored empty. Default empty (configuration.md "Sounds"). The id is the catalog's
/// (`[A-Za-z0-9_.-]`, at most 64 characters, as the dashboard names uploads); whether
/// the catalog has it is not a validity rule, since an unresolvable sound stays stored
/// and rings the built-in tone.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub enum SoundChoice {
    /// The catalog's `default` sound, otherwise the device's built-in tone.
    #[default]
    Default,
    /// A catalog sound by id.
    Named(String),
}

impl SoundChoice {
    /// A choice from its stored text.
    pub fn new(text: &str) -> Result<Self, ValueError> {
        if text.is_empty() {
            return Ok(Self::Default);
        }
        let valid = text.len() <= 64
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if valid {
            Ok(Self::Named(text.to_owned()))
        } else {
            Err(ValueError::Rule {
                rule: "must be empty or a sound id of letters, digits, '_', '.' and '-', at most 64",
            })
        }
    }

    /// The stored text.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Default => "",
            Self::Named(id) => id,
        }
    }
}

impl SettingType for SoundChoice {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        Self::new(string(value)?)
    }

    fn to_json(&self) -> Json {
        Json::String(self.as_str().to_owned())
    }
}

/// `timerSound`.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct TimerSound(pub SoundChoice);

/// `alarmSound`.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct AlarmSound(pub SoundChoice);

macro_rules! sound_setting {
    ($name:ident) => {
        impl SettingType for $name {
            fn from_json(value: &Json) -> Result<Self, ValueError> {
                SoundChoice::from_json(value).map(Self)
            }

            fn to_json(&self) -> Json {
                self.0.to_json()
            }
        }
    };
}
sound_setting!(TimerSound);
sound_setting!(AlarmSound);

bounded_whole!(
    /// `timerRingSeconds`: how long an unanswered timer rings, 30 to 1800 s. Default 900
    /// (configuration.md "Timer ring limit").
    TimerRingSeconds, u16, 30, 1800, default 900
);

bounded_number!(
    /// `timerRingGapSeconds`: silence between loops of the timer sound, 0 to 10 s.
    /// Default 2 (configuration.md "Gap between repeats").
    TimerRingGapSeconds, 0.0, 10.0, default 2.0, false
);

// ---- Transcript correction ------------------------------------------------------------

word_choice!(
    /// `transcriptFix`: the transcript correction mode. Default `shadow`
    /// (configuration.md "Mode").
    TranscriptFixMode, default Shadow, {
        /// No correction.
        Off => "off",
        /// Work out a correction and record it without sending it.
        Shadow => "shadow",
        /// Send a correction that passes every check in time.
        On => "on",
    }
);

/// One preferred term or phrase: whitespace collapsed to single spaces, at most 120
/// characters, with at least one letter or digit.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FixEntry(String);

impl FixEntry {
    /// The most characters in an entry.
    pub const MAX_CHARS: usize = 120;

    /// The entry with its whitespace collapsed, when it is valid.
    pub fn new(text: &str) -> Result<Self, ValueError> {
        let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.chars().count() > Self::MAX_CHARS {
            return Err(ValueError::Rule {
                rule: "an entry has at most 120 characters",
            });
        }
        if !collapsed.chars().any(char::is_alphanumeric) {
            return Err(ValueError::Rule {
                rule: "an entry needs a letter or digit",
            });
        }
        Ok(Self(collapsed))
    }

    /// The entry.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The identity duplicates share: its letters and digits, lowercased.
    fn identity(&self) -> String {
        self.0
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }
}

/// `transcriptFixTerms` and `transcriptFixPhrases`: a ranked list, highest preference
/// first, at most 200 entries; entries whose letters and digits match ignoring case are
/// duplicates (post-afe-audio-architecture.md §18.4). Default empty.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct FixList(Vec<FixEntry>);

impl FixList {
    /// The most entries.
    pub const MAX: usize = 200;

    /// The list, when it is short enough and has no duplicates.
    pub fn new(entries: Vec<FixEntry>) -> Result<Self, ValueError> {
        if entries.len() > Self::MAX {
            return Err(ValueError::TooMany {
                max: Self::MAX,
                found: entries.len(),
            });
        }
        let mut seen = BTreeSet::new();
        for entry in &entries {
            if !seen.insert(entry.identity()) {
                return Err(ValueError::Duplicate {
                    entry: entry.0.clone(),
                });
            }
        }
        Ok(Self(entries))
    }

    /// The entries, highest preference first.
    pub fn entries(&self) -> &[FixEntry] {
        &self.0
    }
}

impl SettingType for FixList {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        let Json::Array(items) = value else {
            return Err(ValueError::WrongType {
                expected: "a list of strings",
            });
        };
        let entries = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                string(item)
                    .and_then(FixEntry::new)
                    .map_err(|e| ValueError::at(i, e))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(entries)
    }

    fn to_json(&self) -> Json {
        Json::Array(self.0.iter().map(|e| Json::String(e.0.clone())).collect())
    }
}

/// `transcriptFixTerms`.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct FixTerms(pub FixList);

/// `transcriptFixPhrases`.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct FixPhrases(pub FixList);

macro_rules! fix_list_setting {
    ($name:ident) => {
        impl SettingType for $name {
            fn from_json(value: &Json) -> Result<Self, ValueError> {
                FixList::from_json(value).map(Self)
            }

            fn to_json(&self) -> Json {
                self.0.to_json()
            }
        }
    };
}
fix_list_setting!(FixTerms);
fix_list_setting!(FixPhrases);

/// ARPAbet's 39 phonemes, plus AX, which counts as AH.
const PHONEMES: [&str; 40] = [
    "AA", "AE", "AH", "AO", "AW", "AY", "B", "CH", "D", "DH", "EH", "ER", "EY", "F", "G", "HH",
    "IH", "IY", "JH", "K", "L", "M", "N", "NG", "OW", "OY", "P", "R", "S", "SH", "T", "TH", "UH",
    "UW", "V", "W", "Y", "Z", "ZH", "AX",
];

/// One pronunciation: a word and its ARPAbet phonemes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Pronunciation {
    word: String,
    phonemes: String,
}

impl Pronunciation {
    /// The most characters in a word.
    pub const MAX_WORD_CHARS: usize = 40;
    /// The most phonemes in a pronunciation.
    pub const MAX_PHONEMES: usize = 30;

    /// A pronunciation, the word lowercased and the phonemes uppercased with single
    /// spaces, when it is valid: the word one token of letters and digits with an
    /// apostrophe allowed inside, at most 40 characters; one to 30 phonemes from
    /// ARPAbet's 39 plus AX, each optionally with a stress digit 0–2.
    pub fn new(word: &str, phonemes: &str) -> Result<Self, ValueError> {
        let word: String = word.chars().flat_map(char::to_lowercase).collect();
        let chars: Vec<char> = word.chars().collect();
        let inner_apostrophe = |i: usize| i > 0 && i + 1 < chars.len();
        let valid_word = !chars.is_empty()
            && chars.len() <= Self::MAX_WORD_CHARS
            && chars
                .iter()
                .enumerate()
                .all(|(i, c)| c.is_alphanumeric() || (*c == '\'' && inner_apostrophe(i)));
        if !valid_word {
            return Err(ValueError::Rule {
                rule: "a word is one token of letters and digits (an apostrophe inside is fine), at most 40 characters",
            });
        }
        let symbols: Vec<String> = phonemes
            .split_whitespace()
            .map(str::to_ascii_uppercase)
            .collect();
        if symbols.is_empty() || symbols.len() > Self::MAX_PHONEMES {
            return Err(ValueError::Rule {
                rule: "a pronunciation has 1 to 30 phonemes",
            });
        }
        for symbol in &symbols {
            let base = symbol.trim_end_matches(['0', '1', '2']);
            let stress = symbol.len() - base.len();
            if stress > 1 || !PHONEMES.contains(&base) {
                return Err(ValueError::Rule {
                    rule: "phonemes are ARPAbet symbols (AX allowed), each with at most one stress digit",
                });
            }
        }
        Ok(Self {
            word,
            phonemes: symbols.join(" "),
        })
    }

    /// The word, lowercased.
    pub fn word(&self) -> &str {
        &self.word
    }

    /// The phonemes, space separated.
    pub fn phonemes(&self) -> &str {
        &self.phonemes
    }
}

/// `transcriptFixPronunciations`: `{word: "ARPAbet"}`, at most 200 words, each word once
/// regardless of case (post-afe-audio-architecture.md §18.4). Default empty. Stored in
/// the order given.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct Pronunciations(Vec<Pronunciation>);

impl Pronunciations {
    /// The most words.
    pub const MAX: usize = 200;

    /// The pronunciations, when there are at most 200 and no word repeats.
    pub fn new(entries: Vec<Pronunciation>) -> Result<Self, ValueError> {
        if entries.len() > Self::MAX {
            return Err(ValueError::TooMany {
                max: Self::MAX,
                found: entries.len(),
            });
        }
        let mut seen = BTreeSet::new();
        for entry in &entries {
            if !seen.insert(entry.word.as_str()) {
                return Err(ValueError::Duplicate {
                    entry: entry.word.clone(),
                });
            }
        }
        Ok(Self(entries))
    }

    /// The pronunciations in stored order.
    pub fn entries(&self) -> &[Pronunciation] {
        &self.0
    }
}

impl SettingType for Pronunciations {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        let Json::Object(object) = value else {
            return Err(ValueError::WrongType {
                expected: "an object {word: phonemes}",
            });
        };
        let entries = object
            .iter()
            .map(|(word, phonemes)| {
                string(phonemes)
                    .and_then(|p| Pronunciation::new(word, p))
                    .map_err(|e| ValueError::at(word, e))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(entries)
    }

    fn to_json(&self) -> Json {
        Json::Object(
            self.0
                .iter()
                .map(|p| (p.word.clone(), Json::String(p.phonemes.clone())))
                .collect(),
        )
    }
}

// ---- Screen ---------------------------------------------------------------------------

/// The firmware's default kiosk API root (`DefaultScreenWakeURL`).
pub const DEFAULT_KIOSK_URL: &str = "http://127.0.0.1:2323";

/// `screenWakeUrl`: the kiosk REST API's root, `http://` or `https://`, or empty for the
/// firmware's default. Default `http://127.0.0.1:2323` (the firmware's default and the
/// dashboard's placeholder).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct KioskUrl(String);

impl KioskUrl {
    /// The URL, when it is empty or an http(s) URL without whitespace.
    pub fn new(text: &str) -> Result<Self, ValueError> {
        let valid = text.is_empty()
            || ((text.starts_with("http://") || text.starts_with("https://"))
                && text.len() <= 2048
                && !text.chars().any(|c| c.is_whitespace() || c.is_control()));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(ValueError::Rule {
                rule: "must be empty or an http:// or https:// URL",
            })
        }
    }

    /// The URL as stored; empty means the firmware's default.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for KioskUrl {
    fn default() -> Self {
        Self(DEFAULT_KIOSK_URL.to_owned())
    }
}

impl SettingType for KioskUrl {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        Self::new(string(value)?)
    }

    fn to_json(&self) -> Json {
        Json::String(self.0.clone())
    }
}

/// `screenWakePassword`: Fully Kiosk's Remote Administration password. Default empty.
/// Its `Debug` never shows it.
#[derive(Clone, Default, Eq, Hash, PartialEq)]
pub struct KioskPassword(String);

impl KioskPassword {
    /// The password; any text without control characters.
    pub fn new(text: &str) -> Result<Self, ValueError> {
        if text.chars().any(char::is_control) {
            return Err(ValueError::Rule {
                rule: "must not contain control characters",
            });
        }
        Ok(Self(text.to_owned()))
    }

    /// The password, for the device's `config` message only.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether one is set.
    pub fn is_set(&self) -> bool {
        !self.0.is_empty()
    }
}

impl fmt::Debug for KioskPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() {
            "KioskPassword(unset)"
        } else {
            "KioskPassword(…)"
        })
    }
}

impl SettingType for KioskPassword {
    fn from_json(value: &Json) -> Result<Self, ValueError> {
        Self::new(string(value)?)
    }

    fn to_json(&self) -> Json {
        Json::String(self.0.clone())
    }
}

bounded_whole!(
    /// `kioskPort`: Kiosk Satellite's remote API port, 1 to 65535. Default 2324 (the
    /// firmware's `DefaultKioskPort`).
    KioskPort, u16, 1, 65535, default 2324
);

bounded_number!(
    /// `turnViewLinger`: seconds the turn view stays up after a turn, 0 to 60. Default 6
    /// (the firmware's `DefaultTurnViewLinger` and the dashboard's slider).
    TurnViewLinger, 0.0, 60.0, default 6.0, false
);

// ---- Device state ---------------------------------------------------------------------

bounded_whole!(
    /// `startupVolume`: the device's remembered volume, 0 to 127 on every platform
    /// (protocol-v1.md). Default 85 (configuration.md "Volume"; the firmware's
    /// `DefaultStartupVolume`).
    StartupVolume, u8, 0, 127, default 85
);

#[cfg(test)]
mod tests {
    use super::*;

    fn parse<T: SettingType>(text: &str) -> Result<T, ValueError> {
        T::from_json(&Json::parse(text).unwrap())
    }

    #[test]
    fn numbers_check_range_and_decimals() {
        assert_eq!(parse::<NearMissFloor>("0.17").unwrap().get(), 0.17);
        assert!(matches!(
            parse::<NearMissFloor>("0.61"),
            Err(ValueError::OutOfRange { .. })
        ));
        assert!(matches!(
            parse::<NearMissFloor>("0.175"),
            Err(ValueError::TooPrecise { .. })
        ));
        assert_eq!(parse::<ArbitrationWindowMs>("700.0").unwrap().get(), 700);
        assert!(parse::<ArbitrationWindowMs>("700.5").is_err());
        assert!(parse::<DuckDb>("\"-18\"").is_err());
    }

    #[test]
    fn rules_reject_duplicates_and_bad_fields() {
        let rule = r#"{"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.9}"#;
        assert!(parse::<ShadowRules>(&format!("[{rule}]")).is_ok());
        assert!(matches!(
            parse::<ShadowRules>(&format!("[{rule}, {rule}]")),
            Err(ValueError::Duplicate { .. })
        ));
        assert!(
            parse::<ExtraOpenRules>(
                r#"[{"profile": "idle", "windows": 4, "combine": "mean", "threshold": 0.9}]"#
            )
            .is_err()
        );
        assert!(parse::<ExtraOpenRules>(
            r#"[{"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.9, "x": 1}]"#
        )
        .is_err());
        let five = [rule; 5].join(", ");
        assert!(matches!(
            parse::<ExtraOpenRules>(&format!("[{five}]")),
            Err(ValueError::TooMany { max: 4, found: 5 })
        ));
    }

    #[test]
    fn pause_asr_needs_a_host_for_wyoming() {
        assert!(
            parse::<PauseAsr>(
                r#"{"engine": "wyoming", "host": "", "port": 10300, "model": "", "language": "en"}"#
            )
            .is_err()
        );
        let ok = parse::<PauseAsr>(
            r#"{"engine": "wyoming", "host": "stt.local", "port": 10300, "model": "", "language": "en"}"#,
        )
        .unwrap();
        assert_eq!(ok.host(), "stt.local");
        assert_eq!(
            PauseAsr::default().to_json().to_python_text(),
            r#"{"engine": "kroko", "host": "", "port": 10300, "model": "", "language": "en"}"#
        );
    }

    #[test]
    fn transcript_lists_and_pronunciations() {
        assert!(matches!(
            parse::<FixList>(r#"["YouTube TV", "youtube  t.v."]"#),
            Err(ValueError::Duplicate { .. })
        ));
        assert!(parse::<FixList>(r#"["..."]"#).is_err());
        let p = parse::<Pronunciations>(r#"{"Plex": "p l eh1 k s"}"#).unwrap();
        assert_eq!(p.entries()[0].word(), "plex");
        assert_eq!(p.entries()[0].phonemes(), "P L EH1 K S");
        assert!(parse::<Pronunciations>(r#"{"plex": "P L QQ"}"#).is_err());
        assert!(parse::<Pronunciations>(r#"{"'plex": "P L"}"#).is_err());
        assert!(parse::<Pronunciations>(r#"{"plex": "P", "PLEX": "P"}"#).is_err());
    }

    #[test]
    fn small_types() {
        assert!(parse::<LedColor>("\"#00B400\"").is_ok());
        assert!(parse::<LedColor>("\"00b400\"").is_err());
        assert_eq!(parse::<SoundChoice>("\"\"").unwrap(), SoundChoice::Default);
        assert!(parse::<SoundChoice>("\"../x\"").is_err());
        assert!(parse::<KioskUrl>("\"ftp://x\"").is_err());
        assert!(parse::<KioskUrl>("\"\"").is_ok());
        assert!(parse::<LedScene>("\"neon\"").is_err());
        assert_eq!(
            format!("{:?}", KioskPassword::new("hunter2").unwrap()),
            "KioskPassword(…)"
        );
    }
}
