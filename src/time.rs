use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeQuality {
    Exact,
    Inferred,
    ObservedOnly,
    Unknown,
}

impl TimeQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Inferred => "inferred",
            Self::ObservedOnly => "observed_only",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_stored(value: &str) -> Self {
        match value {
            "exact" => Self::Exact,
            "inferred" => Self::Inferred,
            "observed_only" => Self::ObservedOnly,
            _ => Self::Unknown,
        }
    }

    pub fn enters_timeline(self) -> bool {
        matches!(self, Self::Exact | Self::Inferred)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeInfo {
    pub quality: TimeQuality,
    pub occurred_at_sort: Option<i64>,
    pub observed_at_sort: Option<i64>,
}

pub fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

/// Microseconds since the Unix epoch. Unlike RFC3339 text, this key orders
/// equivalent offsets and fractional-second forms by their actual UTC instant.
pub fn utc_sort_key(value: &str) -> Option<i64> {
    parse_rfc3339(value).map(|value| value.timestamp_micros())
}

pub fn time_info(occurred_at: Option<&str>, observed_at: Option<&str>, inferred: bool) -> TimeInfo {
    let occurred_at_sort = occurred_at.and_then(utc_sort_key);
    let observed_at_sort = observed_at.and_then(utc_sort_key);
    let quality = match occurred_at {
        Some(_) if occurred_at_sort.is_none() => TimeQuality::Unknown,
        Some(_) if inferred => TimeQuality::Inferred,
        Some(_) => TimeQuality::Exact,
        None if observed_at_sort.is_some() => TimeQuality::ObservedOnly,
        None => TimeQuality::Unknown,
    };
    TimeInfo {
        quality,
        occurred_at_sort,
        observed_at_sort,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_offsets_have_one_utc_order() {
        let utc = "2026-08-23T09:00:00Z";
        let offset = "2026-08-23T18:00:00+09:00";
        assert_eq!(utc_sort_key(utc), utc_sort_key(offset));
        assert_eq!(
            parse_rfc3339(offset).unwrap().to_rfc3339(),
            "2026-08-23T09:00:00+00:00"
        );
        assert!(
            utc_sort_key("2026-08-23T10:00:00+01:00").unwrap()
                < utc_sort_key("2026-08-23T10:00:00Z").unwrap()
        );
    }

    #[test]
    fn fractional_seconds_and_invalid_values_are_explicit() {
        assert!(
            utc_sort_key("2026-08-23T09:00:00.000001Z").unwrap()
                > utc_sort_key("2026-08-23T09:00:00Z").unwrap()
        );
        assert_eq!(utc_sort_key("not-a-time"), None);
        assert_eq!(utc_sort_key("2026-08-23 09:00:00"), None);
    }

    #[test]
    fn time_quality_does_not_turn_missing_occurrence_into_exact_time() {
        let observed = time_info(None, Some("2026-08-23T09:00:00Z"), false);
        assert_eq!(observed.quality, TimeQuality::ObservedOnly);
        assert_eq!(observed.occurred_at_sort, None);
        assert!(!observed.quality.enters_timeline());

        assert_eq!(time_info(None, None, false).quality, TimeQuality::Unknown);
        assert_eq!(
            time_info(Some("bad"), Some("2026-08-23T09:00:00Z"), false).quality,
            TimeQuality::Unknown
        );
        assert_eq!(
            time_info(Some("2026-08-23T09:00:00Z"), None, true).quality,
            TimeQuality::Inferred
        );
    }

    #[test]
    fn clock_rollback_does_not_change_numeric_order() {
        let first = "2026-11-01T01:30:00-04:00";
        let second = "2026-11-01T01:15:00-05:00";
        assert!(utc_sort_key(second).unwrap() > utc_sort_key(first).unwrap());
    }
}
