//! What the backend says each request cost, kept by what the request was for.
//!
//! Every response that carries a loken measurement is recorded here, against the purpose
//! its caller gave: a step of a turn, a read guessed ahead, a second opinion, a repaired
//! reply, the probe at startup.
//! A turn's cost is what the totals moved by while it ran.
//! A backend that measures nothing leaves the totals at zero, and nothing is shown.

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::backend::wire::{LokenUsage, Share, Usage};

/// What a request is for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// A step of a turn: the request the model answers the person with.
    #[default]
    Step,
    /// A read guessed while the model was still writing.
    Guess,
    /// A second reading of a change before it is made.
    Review,
    /// A reply that did not parse, asked for again.
    Repair,
    /// The question asked at startup to learn what the backend can do.
    Probe,
    /// A question a tool server asked the model.
    Sampling,
}

impl Purpose {
    const ALL: [Purpose; 6] = [
        Purpose::Step,
        Purpose::Guess,
        Purpose::Review,
        Purpose::Repair,
        Purpose::Probe,
        Purpose::Sampling,
    ];

    fn at(self) -> usize {
        self as usize
    }

    /// What the purpose is called where a person reads it.
    pub fn label(self) -> &'static str {
        match self {
            Purpose::Step => "steps",
            Purpose::Guess => "guessed reads",
            Purpose::Review => "second opinions",
            Purpose::Repair => "repairs",
            Purpose::Probe => "startup probe",
            Purpose::Sampling => "tool servers",
        }
    }
}

/// Energy drawn by some requests, and how far it can be trusted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Spent {
    pub joules: f64,
    /// How many requests the energy was measured on.
    pub requests: u32,
    /// How many of those shared the server with other requests.
    pub shared: u32,
    /// How many of those had their processor part estimated.
    pub estimated: u32,
}

impl Spent {
    fn add(&mut self, usage: &LokenUsage) {
        let Some(e) = usage.energy else { return };
        self.joules += e.joules;
        self.requests += 1;
        self.shared += u32::from(e.share != Share::Alone);
        self.estimated += u32::from(e.estimated);
    }

    /// What was spent since `earlier`, a reading of the same totals.
    pub fn since(&self, earlier: &Spent) -> Spent {
        Spent {
            joules: (self.joules - earlier.joules).max(0.0),
            requests: self.requests.saturating_sub(earlier.requests),
            shared: self.shared.saturating_sub(earlier.shared),
            estimated: self.estimated.saturating_sub(earlier.estimated),
        }
    }

    pub fn plus(&self, other: &Spent) -> Spent {
        Spent {
            joules: self.joules + other.joules,
            requests: self.requests + other.requests,
            shared: self.shared + other.shared,
            estimated: self.estimated + other.estimated,
        }
    }

    /// Whether anything was measured at all.
    pub fn is_measured(&self) -> bool {
        self.requests > 0
    }

    /// The energy in the unit that reads best, marked `≈` when it was shared with other
    /// requests and `~` when part of it was estimated.
    pub fn describe(&self) -> String {
        let mark = match (self.estimated > 0, self.shared > 0) {
            (true, _) => "~",
            (false, true) => "≈",
            (false, false) => "",
        };
        format!("{mark}{}", watt_hours(self.joules))
    }
}

/// Joules as watt-hours, or milliwatt-hours below a hundredth of one.
pub fn watt_hours(joules: f64) -> String {
    let wh = joules / 3600.0;
    match wh >= 0.01 {
        true => format!("{wh:.2} Wh"),
        false => format!("{:.1} mWh", wh * 1000.0),
    }
}

/// The totals by purpose, as read at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    by: [Spent; Purpose::ALL.len()],
}

impl Totals {
    /// What each purpose spent since `earlier`.
    pub fn since(&self, earlier: &Totals) -> Totals {
        let mut out = Totals::default();
        for (i, slot) in out.by.iter_mut().enumerate() {
            *slot = self.by[i].since(&earlier.by[i]);
        }
        out
    }

    /// Everything, whatever it was for.
    pub fn sum(&self) -> Spent {
        self.by.iter().fold(Spent::default(), |acc, s| acc.plus(s))
    }

    /// The purposes that spent anything, with what they spent.
    pub fn by_purpose(&self) -> Vec<(Purpose, Spent)> {
        Purpose::ALL
            .iter()
            .map(|p| (*p, self.by[p.at()]))
            .filter(|(_, s)| s.is_measured())
            .collect()
    }
}

/// The last request that came back with a measurement, as the rail shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Latest {
    pub purpose: Purpose,
    pub prompt_tokens: u32,
    pub cached_tokens: u32,
    pub accepted_tokens: u32,
    pub rejected_tokens: u32,
    pub loken: LokenUsage,
}

#[derive(Default)]
struct Inner {
    totals: Totals,
    latest: Option<Latest>,
}

/// The process's record of what the backend measured.
#[derive(Default)]
pub struct Ledger {
    inner: Mutex<Inner>,
}

impl Ledger {
    /// Note what a response says it cost, against what it was for.
    pub fn record(&self, purpose: Purpose, usage: &Usage) {
        let Some(loken) = usage.loken else { return };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.totals.by[purpose.at()].add(&loken);
        inner.latest = Some(Latest {
            purpose,
            prompt_tokens: usage.prompt_tokens,
            cached_tokens: usage.prompt_tokens_details.map_or(0, |d| d.cached_tokens),
            accepted_tokens: usage
                .completion_tokens_details
                .map_or(0, |d| d.accepted_prediction_tokens),
            rejected_tokens: usage
                .completion_tokens_details
                .map_or(0, |d| d.rejected_prediction_tokens),
            loken,
        });
    }

    pub fn totals(&self) -> Totals {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).totals
    }

    pub fn latest(&self) -> Option<Latest> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).latest
    }
}

static LEDGER: OnceLock<Ledger> = OnceLock::new();

/// The ledger every backend in this process records into.
pub fn ledger() -> &'static Ledger {
    LEDGER.get_or_init(Ledger::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::wire::Energy;

    fn usage(joules: f64, share: Share, estimated: bool) -> Usage {
        Usage {
            prompt_tokens: 100,
            loken: Some(LokenUsage {
                energy: Some(Energy {
                    joules,
                    gpu_joules: None,
                    cpu_joules: None,
                    estimated,
                    share,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Each request counts against its purpose, and a turn is what the totals moved by.
    #[test]
    fn a_turn_is_what_the_totals_moved_by() {
        let l = Ledger::default();
        l.record(Purpose::Step, &usage(100.0, Share::Alone, false));
        let start = l.totals();
        l.record(Purpose::Step, &usage(36.0, Share::Alone, false));
        l.record(Purpose::Guess, &usage(7.2, Share::Work, false));
        let turn = l.totals().since(&start);
        let sum = turn.sum();
        assert_eq!(sum.requests, 2);
        assert!((sum.joules - 43.2).abs() < 1e-9);
        assert_eq!(
            turn.by_purpose()
                .iter()
                .map(|(p, _)| *p)
                .collect::<Vec<_>>(),
            vec![Purpose::Step, Purpose::Guess]
        );
    }

    /// A response with no measurement changes nothing.
    #[test]
    fn a_response_that_measured_nothing_counts_for_nothing() {
        let l = Ledger::default();
        l.record(Purpose::Step, &Usage::default());
        assert!(!l.totals().sum().is_measured());
        assert!(l.latest().is_none());
    }

    /// The energy says how far it can be trusted.
    #[test]
    fn energy_is_marked_by_how_it_was_measured() {
        let mut s = Spent::default();
        s.add(&usage(1512.0, Share::Alone, false).loken.unwrap());
        assert_eq!(s.describe(), "0.42 Wh");
        s.add(&usage(0.0, Share::Work, false).loken.unwrap());
        assert_eq!(s.describe(), "≈0.42 Wh");
        s.add(&usage(0.0, Share::Alone, true).loken.unwrap());
        assert_eq!(s.describe(), "~0.42 Wh");
        assert_eq!(watt_hours(18.0), "5.0 mWh");
    }
}
