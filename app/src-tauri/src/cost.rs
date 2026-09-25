//! Money shown to the user: the low-balance gate before Start, and the cost
//! bar while a GPU is billing. Pure functions; lib.rs feeds them.

use serde::Serialize;

use crate::sidecar::Deadlines;

/// Warn when credit covers less than this much runtime.
pub const LOW_RUNWAY_S: f64 = 3600.0;
/// Show an upcoming automatic shutdown this far ahead.
const SHUTDOWN_NOTICE_S: f64 = 10.0 * 60.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "lowercase")]
pub enum Gate {
    Ok,
    Warn(String),
    Refuse(String),
}

fn dollars(x: f64) -> String {
    format!("${x:.2}")
}

pub fn duration_words(secs: f64) -> String {
    // Tiny epsilon: 0.9 / 1.0 * 3600 is 3239.999...
    let m = (secs.max(0.0) / 60.0 + 1e-6).floor() as u64;
    match (m / 60, m % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// Before Start. `hourly` and `download` come from the best current offer,
/// when known.
pub fn gate(credit: f64, floor: f64, hourly: Option<f64>, download: f64) -> Gate {
    if !credit.is_finite() || credit < floor {
        return Gate::Refuse(format!(
            "You have {} of Vast credit. SlopTweak won't start below {} \
             (you can change this in Settings). Add credit on Vast first.",
            dollars(credit.max(0.0)),
            dollars(floor)
        ));
    }
    if let Some(h) = hourly.filter(|h| *h > 0.0) {
        let left = credit - download;
        if left < h * LOW_RUNWAY_S / 3600.0 {
            return Gate::Warn(format!(
                "Your {} of credit covers only about {} on this GPU. \
                 Vast stops the GPU when credit runs out.",
                dollars(credit),
                duration_words(left.max(0.0) / h * 3600.0)
            ));
        }
    }
    Gate::Ok
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostBar {
    pub hourly: f64,
    /// Seconds since the instance was created (billing starts then).
    pub elapsed_s: f64,
    /// Estimated spend this session, incl. the one-off model download.
    pub spent: f64,
    pub download_cost: f64,
    /// Last known Vast credit.
    pub credit: Option<f64>,
    /// How long the credit lasts at this rate.
    pub runway_s: Option<f64>,
    pub low: bool,
    /// Upcoming automatic shutdown, e.g. "idle", within the notice window.
    pub shutdown: Option<(String, f64)>,
}

pub struct BarInputs<'a> {
    pub hourly: f64,
    pub download_cost: f64,
    pub started_unix: u64,
    pub now_unix: u64,
    pub credit: Option<f64>,
    pub deadlines: Option<&'a Deadlines>,
}

pub fn bar(i: &BarInputs) -> CostBar {
    let elapsed_s = i.now_unix.saturating_sub(i.started_unix) as f64;
    let spent = i.hourly * elapsed_s / 3600.0 + i.download_cost;
    let runway_s = match i.credit {
        Some(c) if i.hourly > 0.0 => Some((c.max(0.0) / i.hourly) * 3600.0),
        _ => None,
    };
    let shutdown = i.deadlines.and_then(|d| {
        [
            ("idle", d.idle_s),
            ("max_session", d.max_session_s),
            ("heartbeat_lost", d.heartbeat_s),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k.to_string(), v)))
        // Heartbeat deadlines refresh every minute; only surface real trouble.
        .filter(|(k, v)| *v <= SHUTDOWN_NOTICE_S && (k != "heartbeat_lost" || *v <= 120.0))
        .min_by(|a, b| a.1.total_cmp(&b.1))
    });
    CostBar {
        hourly: i.hourly,
        elapsed_s,
        spent,
        download_cost: i.download_cost,
        credit: i.credit,
        runway_s,
        low: runway_s.is_some_and(|r| r < LOW_RUNWAY_S),
        shutdown,
    }
}

impl CostBar {
    /// For the Invoke window's title bar, which can't show app UI.
    pub fn title(&self) -> String {
        let mut t = format!(
            "SlopTweak — Invoke · ${:.3}/hr · {} · ≈{} so far",
            self.hourly,
            duration_words(self.elapsed_s),
            dollars(self.spent)
        );
        if let Some(c) = self.credit {
            t.push_str(&format!(" · {} left", dollars(c)));
        }
        if let Some(r) = self.runway_s {
            if self.low {
                t.push_str(&format!(" (only ~{}!)", duration_words(r)));
            }
        }
        if let Some((why, s)) = &self.shutdown {
            let what = match why.as_str() {
                "idle" => "idle shutdown",
                "max_session" => "session limit",
                _ => "shutdown",
            };
            t.push_str(&format!(" · {what} in {}", duration_words(*s)));
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_refuses_below_floor() {
        assert!(matches!(gate(0.99, 1.0, None, 0.0), Gate::Refuse(_)));
        assert!(matches!(gate(f64::NAN, 1.0, None, 0.0), Gate::Refuse(_)));
        assert_eq!(gate(1.0, 1.0, None, 0.0), Gate::Ok);
        // Floor 0 still refuses negative (owed) credit.
        assert!(matches!(gate(-0.5, 0.0, None, 0.0), Gate::Refuse(_)));
    }

    #[test]
    fn gate_warns_under_an_hour() {
        // $1.20 credit, $0.30 download leaves $0.90; at $1/hr that's 54 min.
        let Gate::Warn(m) = gate(1.20, 1.0, Some(1.0), 0.30) else {
            panic!()
        };
        assert!(m.contains("54 min"), "{m}");
        assert_eq!(gate(11.41, 1.0, Some(0.11), 0.03), Gate::Ok);
        // Unknown price: can't judge the runway, don't warn.
        assert_eq!(gate(1.2, 1.0, None, 0.0), Gate::Ok);
    }

    #[test]
    fn bar_math() {
        let b = bar(&BarInputs {
            hourly: 0.12,
            download_cost: 0.03,
            started_unix: 1000,
            now_unix: 1000 + 1800,
            credit: Some(0.06),
            deadlines: None,
        });
        assert!((b.spent - 0.09).abs() < 1e-9);
        assert!((b.runway_s.unwrap() - 1800.0).abs() < 1e-6);
        assert!(b.low);
        assert!(b.title().contains("$0.120/hr"));
        assert!(b.title().contains("30 min"));
        assert!(b.title().contains("only ~30 min"));

        let b = bar(&BarInputs {
            hourly: 0.12,
            download_cost: 0.0,
            started_unix: 1000,
            now_unix: 900, // clock skew: never negative
            credit: None,
            deadlines: None,
        });
        assert_eq!(b.elapsed_s, 0.0);
        assert!(!b.low);
        assert_eq!(b.runway_s, None);
    }

    #[test]
    fn bar_shows_near_shutdowns_only() {
        let d = Deadlines {
            heartbeat_s: Some(170.0),
            idle_s: Some(400.0),
            max_session_s: Some(9000.0),
        };
        let mk = |d: &Deadlines| {
            bar(&BarInputs {
                hourly: 0.1,
                download_cost: 0.0,
                started_unix: 0,
                now_unix: 60,
                credit: Some(5.0),
                deadlines: Some(d),
            })
        };
        let b = mk(&d);
        assert_eq!(b.shutdown, Some(("idle".into(), 400.0)));
        assert!(b.title().contains("idle shutdown in 6 min"));
        let calm = Deadlines {
            heartbeat_s: Some(170.0),
            idle_s: Some(1200.0),
            max_session_s: Some(9000.0),
        };
        assert_eq!(mk(&calm).shutdown, None);
        let lost = Deadlines {
            heartbeat_s: Some(60.0),
            ..calm
        };
        assert_eq!(mk(&lost).shutdown, Some(("heartbeat_lost".into(), 60.0)));
    }

    #[test]
    fn durations() {
        assert_eq!(duration_words(59.0), "0 min");
        assert_eq!(duration_words(3600.0), "1 h");
        assert_eq!(duration_words(3600.0 * 105.5), "105 h 30 min");
    }
}
