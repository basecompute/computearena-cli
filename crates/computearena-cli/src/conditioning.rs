//! Temperature-based pre-launch conditioning, not in-runtime phase control.
use crate::config::{
    CONDITIONING_FALLBACK_WAIT_SECONDS, CONDITIONING_MAXIMUM_WAIT_SECONDS,
    CONDITIONING_STABLE_WINDOW_SECONDS,
};
use crate::ui::TerminalUi;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::Components;

const MARGIN_C: f64 = 3.0;
const SLOPE_C_PER_SECOND: f64 = 0.1;
const POLL_SECONDS: f64 = 1.0;

trait Environment {
    fn now(&self) -> f64;
    fn read(&mut self) -> Option<f64>;
    fn sleep(&mut self, seconds: f64);
}

struct Sensors {
    started: Instant,
    components: Components,
    labels: Vec<String>,
}

impl Sensors {
    fn new() -> Self {
        let components = Components::new_with_refreshed_list();
        let labels = components
            .iter()
            .filter(|c| die_sensor(c.label()))
            .filter(|c| {
                c.temperature()
                    .is_some_and(|t| t.is_finite() && (0.0..130.0).contains(&t))
            })
            .map(|c| c.label().to_owned())
            .take(128)
            .collect();
        Self {
            started: Instant::now(),
            components,
            labels,
        }
    }
}

fn die_sensor(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    !["battery", "nand", "ssd", "ambient"]
        .iter()
        .any(|s| label.contains(s))
        && [
            "cpu", "gpu", "acc", "tdie", "soc", "package", "core", "tctl",
        ]
        .iter()
        .any(|s| label.contains(s))
}

impl Environment for Sensors {
    fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
    fn sleep(&mut self, seconds: f64) {
        thread::sleep(Duration::from_secs_f64(seconds.max(0.0)));
    }
    fn read(&mut self) -> Option<f64> {
        // A fixed sensor set prevents a disappearing hot sensor from looking cool.
        let mut peak: Option<f64> = None;
        for label in &self.labels {
            let sensor = self.components.iter_mut().find(|c| c.label() == label)?;
            sensor.refresh();
            let t = f64::from(sensor.temperature()?);
            if !t.is_finite() || !(0.0..130.0).contains(&t) {
                return None;
            }
            peak = Some(peak.map_or(t, |p| p.max(t)));
        }
        peak
    }
}

struct Controller<E> {
    environment: E,
    baseline: Option<f64>,
}

impl<E: Environment> Controller<E> {
    fn prepare(&mut self, mut progress: impl FnMut(f64, Option<f64>)) -> Value {
        let start = self.environment.now();
        let baseline_at_start = self.baseline;
        let mut window: VecDeque<(f64, f64)> = VecDeque::new();
        let mut samples = 0;
        let mut last_progress = 0.0;
        loop {
            let temperature = self.environment.read();
            samples += 1;
            let elapsed = self.environment.now() - start;
            let Some(t) = temperature else {
                // Missing sensors cannot establish thermal readiness. A bounded
                // fallback is an explicit timed rest, never "target reached".
                let until = (elapsed + CONDITIONING_FALLBACK_WAIT_SECONDS)
                    .min(CONDITIONING_MAXIMUM_WAIT_SECONDS);
                while self.environment.now() - start < until {
                    progress(self.environment.now() - start, None);
                    let remaining = until - (self.environment.now() - start);
                    self.environment.sleep(remaining.min(10.0));
                }
                return json!({"method":"timed_fallback","target_reached":false,"timed_out":false,
                    "waited_s":self.environment.now()-start,"sample_count":samples,
                    "baseline_c":baseline_at_start,"end_c":null,
                    "reason":"No usable fixed die-temperature sensor set; performed a timed rest"});
            };
            window.push_back((elapsed, t));
            while window.len() > 2 && window[1].0 <= elapsed - CONDITIONING_STABLE_WINDOW_SECONDS {
                window.pop_front();
            }
            let duration = elapsed - window[0].0;
            let mean = window.iter().map(|(_, t)| t).sum::<f64>() / window.len() as f64;
            let slope = if duration > 0.0 {
                (t - window[0].1) / duration
            } else {
                0.0
            };
            let target = self.baseline.unwrap_or(mean) + MARGIN_C;
            if duration >= CONDITIONING_STABLE_WINDOW_SECONDS
                && slope.abs() <= SLOPE_C_PER_SECOND
                && window.iter().all(|(_, t)| *t <= target)
            {
                if self.baseline.is_none() {
                    self.baseline = Some(mean);
                }
                return json!({"method":if baseline_at_start.is_some(){"temperature_reset"}else{"establish_baseline"},
                    "target_reached":true,"timed_out":false,"waited_s":elapsed,"sample_count":samples,
                    "baseline_c":self.baseline,"end_c":t,"slope_c_per_s":slope});
            }
            if elapsed >= CONDITIONING_MAXIMUM_WAIT_SECONDS {
                return json!({"method":"temperature_timeout","target_reached":false,"timed_out":true,
                    "waited_s":elapsed,"sample_count":samples,"baseline_c":self.baseline,"end_c":t,
                    "reason":"Thermal target was not reached before the maximum wait; continuing as disclosed"});
            }
            if elapsed - last_progress >= 10.0 {
                progress(elapsed, Some(t));
                last_progress = elapsed;
            }
            self.environment
                .sleep(POLL_SECONDS.min(CONDITIONING_MAXIMUM_WAIT_SECONDS - elapsed));
        }
    }
}

pub(crate) struct Cooldown(Controller<Sensors>);

impl Cooldown {
    pub(crate) fn new() -> Self {
        Self(Controller {
            environment: Sensors::new(),
            baseline: None,
        })
    }
    pub(crate) fn prepare(&mut self, workload: &str) -> Value {
        let ui = TerminalUi::detect();
        println!(
            "{}",
            ui.neutral(format!(
                "Conditioning: {workload} — waiting for thermal stability before model loading"
            ))
        );
        let mut result = self.0.prepare(|elapsed, temp| {
            let detail = temp
                .map(|t| format!("{t:.1}°C"))
                .unwrap_or_else(|| "sensorless timed rest".into());
            println!(
                "{}",
                ui.muted(format!(
                    "Conditioning: {workload} — waiting after {elapsed:.0}s, {detail}"
                ))
            );
        });
        let status = if result["target_reached"] == true {
            "ready"
        } else if result["timed_out"] == true {
            "maximum wait reached; continuing without confirmed recovery"
        } else {
            "timed rest complete; thermal recovery unverified"
        };
        println!(
            "{}",
            ui.neutral(format!(
                "Conditioning: {workload} — {status} after {:.1}s",
                result["waited_s"].as_f64().unwrap_or(0.0)
            ))
        );
        result["sensor_labels"] = json!(self.0.environment.labels);
        result
    }
}

pub(crate) fn policy() -> Value {
    json!({"schema":"computearena-conditioning/1","mode":"idle_reset_then_warm","cooldown_enabled":true,
        "scope":"before_each_workload_process","criterion":"fixed_die_temperature_sensors_only",
        "maximum_wait_s":CONDITIONING_MAXIMUM_WAIT_SECONDS,"fallback_wait_s":CONDITIONING_FALLBACK_WAIT_SECONDS,
        "stable_window_s":CONDITIONING_STABLE_WINDOW_SECONDS,"sample_interval_s":POLL_SECONDS,
        "temperature_margin_c":MARGIN_C,"temperature_slope_limit_c_per_s":SLOPE_C_PER_SECOND,
        "note":"Model loading and native warmup follow the wait; this is not in-runtime BaseRT conditioning"})
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        time: f64,
        readings: VecDeque<Option<f64>>,
        last: Option<f64>,
    }
    impl Environment for Fake {
        fn now(&self) -> f64 {
            self.time
        }
        fn sleep(&mut self, s: f64) {
            self.time += s;
        }
        fn read(&mut self) -> Option<f64> {
            self.readings.pop_front().unwrap_or(self.last)
        }
    }
    fn controller(t: Option<f64>) -> Controller<Fake> {
        Controller {
            environment: Fake {
                time: 0.0,
                readings: VecDeque::new(),
                last: t,
            },
            baseline: None,
        }
    }
    #[test]
    fn baseline_recovery_and_timeout_use_real_elapsed_windows() {
        let mut c = controller(Some(40.0));
        assert_eq!(c.prepare(|_, _| {})["waited_s"], 10.0);
        c.environment.last = Some(42.0);
        assert_eq!(c.prepare(|_, _| {})["target_reached"], true);
        c.environment.last = Some(50.0);
        let r = c.prepare(|_, _| {});
        assert_eq!(r["timed_out"], true);
        assert_eq!(r["waited_s"], 180.0);
    }
    #[test]
    fn missing_sensors_rest_but_never_claim_recovery() {
        let r = controller(None).prepare(|_, _| {});
        assert_eq!(r["waited_s"], 30.0);
        assert_eq!(r["target_reached"], false);
        assert_eq!(r["method"], "timed_fallback");
    }
    #[test]
    fn falling_temperature_does_not_establish_a_false_baseline() {
        let mut c = controller(Some(40.0));
        c.environment.readings = (0..20).map(|i| Some(60.0 - f64::from(i))).collect();
        let r = c.prepare(|_, _| {});
        assert!(r["waited_s"].as_f64().unwrap() >= 29.0);
        assert!(!die_sensor("battery"));
        assert!(die_sensor("PMU tdie0"));
    }
}
