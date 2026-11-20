//! Duration serialization helpers for configuration files

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::Duration;

/// Durations are written as whole seconds when they divide evenly and as
/// fractional seconds otherwise, so sub-second timeouts survive a round trip
/// through a config file or the web admin API.
fn seconds_of(duration: Duration) -> SecondsValue {
    if duration.subsec_nanos() == 0 {
        SecondsValue::Whole(duration.as_secs())
    } else {
        SecondsValue::Fractional(duration.as_secs_f64())
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum SecondsValue {
    Whole(u64),
    Fractional(f64),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SecondsInput {
    Whole(u64),
    Fractional(f64),
}

impl SecondsInput {
    fn into_duration<E: serde::de::Error>(self) -> Result<Duration, E> {
        match self {
            Self::Whole(secs) => Ok(Duration::from_secs(secs)),
            Self::Fractional(secs) if secs.is_finite() && secs >= 0.0 => {
                Ok(Duration::from_secs_f64(secs))
            }
            Self::Fractional(secs) => Err(E::custom(format!(
                "duration must be a finite, non-negative number of seconds, got {secs}"
            ))),
        }
    }
}

/// Helper for deserializing Duration from seconds
///
/// TOML/JSON configs typically specify durations in seconds, so we need
/// custom serde to convert from seconds to Duration.
pub mod duration_serde {
    use super::{
        Deserialize, Deserializer, Duration, SecondsInput, Serialize, Serializer, seconds_of,
    };

    /// Serialize `Duration` as seconds.
    ///
    /// # Errors
    /// Returns any serializer error from the underlying serde sink.
    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        seconds_of(*duration).serialize(serializer)
    }

    /// Deserialize `Duration` from seconds.
    ///
    /// # Errors
    /// Returns any deserializer error while reading the seconds value.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        SecondsInput::deserialize(deserializer)?.into_duration()
    }
}

/// Helper for deserializing `Option<Duration>` from seconds
pub mod option_duration_serde {
    use super::{Deserialize, Deserializer, Duration, SecondsInput, Serializer, seconds_of};

    /// Serialize `Option<Duration>` as optional seconds.
    ///
    /// # Errors
    /// Returns any serializer error from the underlying serde sink.
    pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match duration {
            Some(duration) => serializer.serialize_some(&seconds_of(*duration)),
            None => serializer.serialize_none(),
        }
    }

    /// Deserialize `Option<Duration>` from optional seconds.
    ///
    /// # Errors
    /// Returns any deserializer error while reading the optional seconds value.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<SecondsInput>::deserialize(deserializer)?
            .map(SecondsInput::into_duration)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct TestDuration {
        #[serde(with = "duration_serde")]
        timeout: Duration,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct TestOptionDuration {
        #[serde(with = "option_duration_serde")]
        timeout: Option<Duration>,
    }

    proptest! {
        /// Property: Duration serialization round-trips correctly for any valid second value
        #[test]
        fn prop_duration_serde_roundtrip(secs in 0u64..100_000) {
            let original = TestDuration {
                timeout: Duration::from_secs(secs),
            };
            let json = serde_json::to_string(&original).unwrap();
            let parsed: TestDuration = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(original, parsed);
        }

        /// Property: whole-second durations keep the plain JSON number form
        #[test]
        fn prop_duration_json_format(secs in 0u64..100_000) {
            let test = TestDuration {
                timeout: Duration::from_secs(secs),
            };
            let json = serde_json::to_string(&test).unwrap();
            let expected = format!(r#"{{"timeout":{secs}}}"#);
            prop_assert_eq!(json, expected);
        }

        /// Property: Option<Duration> with Some serializes to number
        #[test]
        fn prop_option_duration_some_roundtrip(secs in 0u64..100_000) {
            let original = TestOptionDuration {
                timeout: Some(Duration::from_secs(secs)),
            };
            let json = serde_json::to_string(&original).unwrap();
            let parsed: TestOptionDuration = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(original, parsed);
        }

        /// Property: TOML serialization round-trips correctly
        #[test]
        fn prop_duration_toml_roundtrip(secs in 0u64..100_000) {
            let original = TestDuration {
                timeout: Duration::from_secs(secs),
            };
            let toml = toml::to_string(&original).unwrap();
            let parsed: TestDuration = toml::from_str(&toml).unwrap();
            prop_assert_eq!(original, parsed);
        }
    }

    // Edge cases
    #[test]
    fn test_option_duration_none_roundtrip() {
        let original = TestOptionDuration { timeout: None };
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, r#"{"timeout":null}"#);

        let parsed: TestOptionDuration = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_option_duration_deserialize_null() {
        let json = r#"{"timeout":null}"#;
        let test: TestOptionDuration = serde_json::from_str(json).unwrap();
        assert_eq!(test.timeout, None);
    }

    #[test]
    fn test_option_duration_deserialize_missing_field() {
        let json = r"{}";
        let result: Result<TestOptionDuration, _> = serde_json::from_str(json);
        // serde will error on missing required field
        assert!(result.is_err());
    }

    #[test]
    fn test_subsecond_durations_survive_a_round_trip() {
        let original = TestDuration {
            timeout: Duration::from_millis(100),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(
            serde_json::from_str::<TestDuration>(&json).unwrap(),
            original
        );
        let toml = toml::to_string(&original).unwrap();
        assert_eq!(toml::from_str::<TestDuration>(&toml).unwrap(), original);

        let original = TestOptionDuration {
            timeout: Some(Duration::from_millis(250)),
        };
        let toml = toml::to_string(&original).unwrap();
        assert_eq!(
            toml::from_str::<TestOptionDuration>(&toml).unwrap(),
            original
        );
    }

    #[test]
    fn test_negative_duration_is_rejected() {
        assert!(toml::from_str::<TestDuration>("timeout = -1.0").is_err());
        assert!(serde_json::from_str::<TestDuration>(r#"{"timeout":-0.5}"#).is_err());
    }
}
