//! Regression gate over a benchmark run. Rules are tymux/tmux latency *ratios*
//! with wide margins, because CI runners (and busy workstations) swing
//! absolute times and even small ratios run to run, while a real regression
//! moves the ratio by multiples:
//!
//! - the API must beat tmux's one-process-per-command CLI (ratio below 1),
//!   which is the claim the README makes;
//! - everything else is a tripwire at several times today's ratio.

use crate::results::{Backend, Results, Stats};

const TYMUX_API: &str = "tymux (gRPC API)";
const TMUX_CONTROL: &str = "tmux (control mode)";
const TYMUX_CLI: &str = "tymux (CLI)";
const TMUX_CLI: &str = "tmux (CLI)";

pub struct Row {
    pub label: String,
    pub ratio: f64,
    pub max: f64,
    pub ok: bool,
}

type Pick = fn(&Backend) -> Option<Stats>;

struct Rule {
    metric: &'static str,
    pick: Pick,
    num: &'static str,
    den: &'static str,
    max_ratio: f64,
}

fn first_create(b: &Backend) -> Option<Stats> {
    b.create.first().map(|c| c.stats)
}

fn rules() -> Vec<Rule> {
    let mut v = Vec::new();
    let metrics: [(&str, Pick); 5] = [
        ("create", first_create),
        ("kill", |b| Some(b.kill)),
        ("capture", |b| b.capture),
        ("keystroke echo", |b| b.echo),
        ("cold start", |b| b.cold_start),
    ];
    for (metric, pick) in metrics {
        // Claim: the persistent API is faster than forking a tmux per command.
        if metric != "keystroke echo" {
            v.push(Rule {
                metric,
                pick,
                num: TYMUX_API,
                den: TMUX_CLI,
                max_ratio: 1.0,
            });
        }
        // Tripwires. Capture is excluded against control mode: with few
        // sessions tmux's text capture on an open connection is genuinely
        // several times faster than a gRPC call returning a cell grid.
        if metric != "capture" {
            v.push(Rule {
                metric,
                pick,
                num: TYMUX_API,
                den: TMUX_CONTROL,
                max_ratio: 10.0,
            });
        }
        if matches!(metric, "create" | "kill" | "capture") {
            v.push(Rule {
                metric,
                pick,
                num: TYMUX_CLI,
                den: TMUX_CLI,
                max_ratio: 8.0,
            });
        }
    }
    v
}

pub fn evaluate(r: &Results) -> Vec<Row> {
    rules()
        .into_iter()
        .filter_map(|rule| {
            let n = (rule.pick)(r.backend(rule.num)?)?.p50_ms;
            let d = (rule.pick)(r.backend(rule.den)?)?.p50_ms;
            (d > 0.0).then(|| Row {
                label: format!("{} / {}: {}", rule.num, rule.den, rule.metric),
                ratio: n / d,
                max: rule.max_ratio,
                ok: n / d <= rule.max_ratio,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Results {
        serde_json::from_str(include_str!("../testdata/sample.json")).unwrap()
    }

    fn api(r: &mut Results) -> &mut Backend {
        r.backends.iter_mut().find(|b| b.name == TYMUX_API).unwrap()
    }

    #[test]
    fn gate_should_pass_on_the_sample_run() {
        let rows = evaluate(&sample());
        assert!(!rows.is_empty());
        let failed: Vec<_> = rows.iter().filter(|r| !r.ok).map(|r| &r.label).collect();
        assert!(failed.is_empty(), "{failed:?}");
    }

    #[test]
    fn gate_should_fail_when_the_api_stops_beating_the_tmux_cli() {
        let mut slow = sample();
        api(&mut slow).kill.p50_ms = 5.0; // tmux CLI kill is 1.5 ms
        let rows = evaluate(&slow);
        let row = rows
            .iter()
            .find(|r| r.label == format!("{TYMUX_API} / {TMUX_CLI}: kill"))
            .unwrap();
        assert!(!row.ok && row.ratio > 1.0);
    }

    #[test]
    fn gate_should_fail_on_a_catastrophic_regression_against_control_mode() {
        let mut slow = sample();
        api(&mut slow).echo.as_mut().unwrap().p50_ms = 5.0; // control mode is 0.12 ms
        assert!(evaluate(&slow)
            .iter()
            .any(|r| !r.ok && r.label.ends_with("keystroke echo")));
    }

    #[test]
    fn gate_should_skip_rules_whose_backends_lack_the_metric() {
        let rows = evaluate(&sample());
        assert!(!rows
            .iter()
            .any(|r| r.label == format!("{TYMUX_API} / {TMUX_CLI}: keystroke echo")));
    }
}
