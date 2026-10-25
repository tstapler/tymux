//! Converts a run into benchdiff's canonical JSON (schema 1) so the shared
//! benchdiff Action can compare it with a baseline. One metric per
//! (backend, scenario, p50/p95), plus server RSS per session.

use crate::results::{Backend, Results, Stats};
use serde_json::{json, Value};

fn metric(group: &str, name: &str, measure: &str, unit: &str, value: f64) -> Value {
    json!({
        "group": group,
        "name": name,
        "measure": measure,
        "unit": unit,
        "better": "lower",
        "value": value,
    })
}

fn latency(out: &mut Vec<Value>, group: &str, name: &str, s: &Stats) {
    out.push(metric(group, name, "p50", "ms", s.p50_ms));
    out.push(metric(group, name, "p95", "ms", s.p95_ms));
}

fn backend_metrics(out: &mut Vec<Value>, b: &Backend) {
    let g = b.name.as_str();
    for c in &b.create {
        let name = format!("create@live {}-{}", c.live_from, c.live_to);
        latency(out, g, &name, &c.stats);
    }
    latency(out, g, "kill", &b.kill);
    latency(out, g, "list", &b.list);
    let optional = [
        ("capture", b.capture),
        ("keystroke echo", b.echo),
        ("cold start", b.cold_start),
        ("reattach", b.reattach.as_ref().map(|r| r.stats)),
    ];
    for (name, stats) in optional {
        if let Some(s) = stats {
            latency(out, g, name, &s);
        }
    }
    if let Some(kib) = b.rss_per_session_kib {
        out.push(metric(g, "memory per session", "rss", "KiB", kib));
    }
}

pub fn to_benchdiff(r: &Results) -> Value {
    let mut metrics = Vec::new();
    for b in &r.backends {
        backend_metrics(&mut metrics, b);
    }
    json!({
        "schema": 1,
        "env": {
            "tymux_git_sha": r.env.tymux_git_sha,
            "tmux_version": r.env.tmux_version,
            "kernel": r.env.kernel,
            "cpu": r.env.cpu,
            "cores": r.env.cores.to_string(),
            "sessions": r.env.sessions.to_string(),
        },
        "metrics": metrics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed() -> Results {
        serde_json::from_str(include_str!("../../../docs/benchmarks/results.json")).unwrap()
    }

    fn find<'a>(doc: &'a Value, group: &str, name: &str, measure: &str) -> &'a Value {
        doc["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["group"] == group && m["name"] == name && m["measure"] == measure)
            .unwrap_or_else(|| panic!("missing {group}/{name}/{measure}"))
    }

    #[test]
    fn export_should_use_the_canonical_envelope() {
        let doc = to_benchdiff(&committed());
        assert_eq!(doc["schema"], 1);
        assert_eq!(doc["env"]["tymux_git_sha"], "c2b6035");
        assert_eq!(doc["env"]["sessions"], "600");
    }

    #[test]
    fn export_should_map_each_scenario_to_p50_and_p95() {
        let doc = to_benchdiff(&committed());
        let api = "tymux (gRPC API)";
        let p50 = find(&doc, api, "create@live 0-100", "p50");
        assert_eq!(p50["value"], 0.437658);
        assert_eq!(p50["unit"], "ms");
        assert_eq!(p50["better"], "lower");
        assert_eq!(find(&doc, api, "kill", "p95")["value"], 0.159788);
        assert_eq!(find(&doc, api, "list", "p50")["value"], 0.889583);
        assert_eq!(find(&doc, api, "reattach", "p50")["value"], 0.17105);
    }

    #[test]
    fn export_should_report_memory_in_kib_and_skip_absent_scenarios() {
        let r = committed();
        let doc = to_benchdiff(&r);
        let rss = find(&doc, "tmux (CLI)", "memory per session", "rss");
        assert_eq!(rss["unit"], "KiB");
        let has = |g: &str, n: &str| {
            doc["metrics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["group"] == g && m["name"] == n)
        };
        assert!(!has("tymux (CLI)", "keystroke echo"));
        assert!(!has("tymux (CLI)", "memory per session"));
    }

    #[test]
    fn export_should_have_unique_metric_identities() {
        let doc = to_benchdiff(&committed());
        let mut ids: Vec<String> = doc["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| format!("{}|{}|{}", m["group"], m["name"], m["measure"]))
            .collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total);
    }
}
