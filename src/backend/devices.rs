//! What a loken server says about its accelerators: the power they draw now.
//!
//! Read from the device listing the server already serves to its own interface. Shown as it
//! is said, never integrated into an energy figure: the energy of a request is the server's
//! measurement, carried with the request.

use std::time::Duration;

use serde_json::Value;

/// Where a loken server lists its devices.
const LISTING: &str = "/api/distributed/devices";

/// How long one reading may take before it is given up.
const READ_FOR: Duration = Duration::from_secs(2);

/// How often the power is read while a turn runs: as often as loken's own interface does.
const EVERY: Duration = Duration::from_secs(2);

/// How many readings in a row may fail before a server is taken to have no listing.
const MISSES: u32 = 3;

/// The power the server's accelerators draw, summed over those that report it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Power {
    pub watts: f64,
    /// Their power limits summed, when every one of them states one.
    pub limit: Option<f64>,
    /// How busy the busiest of them is, in per cent.
    pub busy_percent: Option<u8>,
    /// How many devices reported power.
    pub devices: usize,
}

/// Read the power of the server at `base` while `busy` says a turn runs, and send it on.
///
/// Between turns it waits on `busy` and nothing wakes it. It ends when `stop` fires, when
/// nobody is listening any more, or when the server has failed to answer several times in
/// a row, which is a server with no such listing.
pub async fn follow(
    http: reqwest::Client,
    base: String,
    mut busy: tokio::sync::watch::Receiver<bool>,
    out: tokio::sync::mpsc::Sender<Power>,
    stop: tokio_util::sync::CancellationToken,
) {
    let mut misses = 0u32;
    loop {
        while !*busy.borrow() {
            tokio::select! {
                _ = stop.cancelled() => return,
                changed = busy.changed() => if changed.is_err() { return },
            }
        }
        match read(&http, &base).await {
            Some(power) => {
                misses = 0;
                if out.send(power).await.is_err() {
                    return;
                }
            }
            None => {
                misses += 1;
                if misses >= MISSES {
                    return;
                }
            }
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(EVERY) => {}
        }
    }
}

/// One reading from the server at `base`, or `None` when it has no listing or no device
/// in it reports power.
pub async fn read(http: &reqwest::Client, base: &str) -> Option<Power> {
    let url = format!("{}{LISTING}", base.trim_end_matches('/'));
    let resp = http.get(&url).timeout(READ_FOR).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    parse(&body)
}

/// The power a device listing reports, or `None` when no device reports any.
pub fn parse(body: &Value) -> Option<Power> {
    let devices = body.get("devices")?.as_array()?;
    let mut power = Power::default();
    let mut every_limit = true;
    let mut limit = 0.0;
    for d in devices {
        let Some(watts) = d.get("power_watts").and_then(Value::as_f64) else {
            continue;
        };
        power.watts += watts;
        power.devices += 1;
        match d.get("power_limit_watts").and_then(Value::as_f64) {
            Some(l) => limit += l,
            None => every_limit = false,
        }
        if let Some(busy) = d.get("utilization_gpu_percent").and_then(Value::as_f64) {
            let busy = busy.clamp(0.0, 100.0) as u8;
            power.busy_percent = Some(power.busy_percent.map_or(busy, |b| b.max(busy)));
        }
    }
    if power.devices == 0 {
        return None;
    }
    power.limit = every_limit.then_some(limit);
    Some(power)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The devices that report power are summed, and the others are left out.
    #[test]
    fn the_power_of_every_device_is_summed() {
        let body = json!({"devices": [
            {"name": "a", "power_watts": 280.5, "power_limit_watts": 450.0, "utilization_gpu_percent": 87},
            {"name": "b", "power_watts": 31.5, "power_limit_watts": 450.0, "utilization_gpu_percent": 12},
            {"name": "vulkan", "power_watts": null, "power_limit_watts": null}
        ]});
        let p = parse(&body).expect("power");
        assert_eq!(p.watts, 312.0);
        assert_eq!(p.limit, Some(900.0));
        assert_eq!(p.busy_percent, Some(87));
        assert_eq!(p.devices, 2);
    }

    /// A listing in which no device reports power says nothing, rather than zero watts.
    #[test]
    fn no_reported_power_is_no_reading() {
        assert_eq!(parse(&json!({"devices": [{"power_watts": null}]})), None);
        assert_eq!(parse(&json!({"error": "not here"})), None);
    }

    /// A limit is only stated when every device states its own.
    #[test]
    fn a_partial_limit_is_not_a_limit() {
        let body = json!({"devices": [
            {"power_watts": 100.0, "power_limit_watts": 300.0},
            {"power_watts": 50.0}
        ]});
        assert_eq!(parse(&body).and_then(|p| p.limit), None);
    }
}
