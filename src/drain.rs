//! Drain-trend evaluation: compare two backlog samples taken `window` apart to
//! answer "are we draining or growing, and when do we clear?".

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trend {
    /// Backlog is zero at both samples — nothing to clear.
    Empty,
    /// Backlog shrinking; an ETA to clear can be projected.
    Draining,
    /// Backlog growing; producers are outpacing consumers.
    Growing,
    /// Net change is negligible relative to the backlog — holding steady.
    Stable,
}

impl Trend {
    pub fn label(self) -> &'static str {
        match self {
            Trend::Empty => "empty",
            Trend::Draining => "draining",
            Trend::Growing => "growing",
            Trend::Stable => "stable",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DrainStats {
    pub trend: Trend,
    /// Backlog change over the window (second minus first). Negative = draining.
    pub delta: i64,
    /// Net messages per second (delta / window). Negative = draining.
    pub net_per_sec: f64,
    /// Seconds to clear at the current net drain rate. Only present when
    /// draining and backlog is non-zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<f64>,
}

/// Build drain stats from two backlog readings `window_secs` apart.
///
/// `stable_frac` is the fraction of the starting backlog below which a net
/// change is treated as noise rather than a real trend (default 1%). This keeps
/// a topic that drifts by a handful of messages out of the Growing/Draining
/// buckets.
pub fn evaluate_drain(
    backlog_first: i64,
    backlog_second: i64,
    window_secs: f64,
    stable_frac: f64,
) -> DrainStats {
    let delta = backlog_second - backlog_first;
    let net_per_sec = if window_secs > 0.0 {
        delta as f64 / window_secs
    } else {
        0.0
    };

    if backlog_first == 0 && backlog_second == 0 {
        return DrainStats {
            trend: Trend::Empty,
            delta,
            net_per_sec,
            eta_secs: None,
        };
    }

    // Treat tiny movement relative to the backlog as steady state.
    let noise_floor = (backlog_first as f64 * stable_frac).max(1.0);
    let trend = if (delta as f64).abs() < noise_floor {
        Trend::Stable
    } else if delta < 0 {
        Trend::Draining
    } else {
        Trend::Growing
    };

    let eta_secs = if trend == Trend::Draining && net_per_sec < 0.0 {
        Some(backlog_second as f64 / -net_per_sec)
    } else {
        None
    };

    DrainStats {
        trend,
        delta,
        net_per_sec,
        eta_secs,
    }
}

/// Human-friendly duration: "3m", "42h", "5.2d", or ">1y" for the absurd.
pub fn format_eta(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "—".to_string();
    }
    let minutes = secs / 60.0;
    if minutes < 1.0 {
        return "<1m".to_string();
    }
    if minutes < 90.0 {
        return format!("{:.0}m", minutes);
    }
    let hours = minutes / 60.0;
    if hours < 48.0 {
        return format!("{:.0}h", hours);
    }
    let days = hours / 24.0;
    if days < 365.0 {
        return format!("{:.1}d", days);
    }
    ">1y".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_projects_eta() {
        // 10000 -> 7000 over 30s = -100/s; 7000 / 100 = 70s to clear.
        let d = evaluate_drain(10_000, 7_000, 30.0, 0.01);
        assert_eq!(d.trend, Trend::Draining);
        assert_eq!(d.delta, -3000);
        assert!((d.net_per_sec + 100.0).abs() < 1e-9);
        assert!((d.eta_secs.unwrap() - 70.0).abs() < 1e-6);
    }

    #[test]
    fn growing_has_no_eta() {
        let d = evaluate_drain(10_000, 13_000, 30.0, 0.01);
        assert_eq!(d.trend, Trend::Growing);
        assert!(d.eta_secs.is_none());
        assert!(d.net_per_sec > 0.0);
    }

    #[test]
    fn small_drift_is_stable() {
        // 50 message move on a 10000 backlog (0.5%) is below the 1% floor.
        let d = evaluate_drain(10_000, 9_950, 30.0, 0.01);
        assert_eq!(d.trend, Trend::Stable);
        assert!(d.eta_secs.is_none());
    }

    #[test]
    fn empty_stays_empty() {
        let d = evaluate_drain(0, 0, 30.0, 0.01);
        assert_eq!(d.trend, Trend::Empty);
        assert!(d.eta_secs.is_none());
    }

    #[test]
    fn eta_formatting() {
        assert_eq!(format_eta(30.0), "<1m");
        assert_eq!(format_eta(300.0), "5m");
        assert_eq!(format_eta(3600.0), "60m");
        assert_eq!(format_eta(3600.0 * 10.0), "10h");
        assert_eq!(format_eta(86400.0 * 5.0), "5.0d");
        assert_eq!(format_eta(86400.0 * 400.0), ">1y");
    }
}
