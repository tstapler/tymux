//! Pure rendering: results in, `(file name, contents)` out. No I/O here, so
//! the staleness check can compare byte for byte.

use crate::results::{Backend, Results};
use std::fmt::Write;

const W: f64 = 760.0;

// No CSS variables: librsvg and some viewers drop them.
fn style() -> String {
    let light = ["#0969da", "#1a7f37", "#6e7781", "#bf8700"];
    let dark = ["#58a6ff", "#3fb950", "#9198a1", "#d29922"];
    let series = |colors: &[&str; 4]| {
        SERIES
            .iter()
            .zip(colors)
            .map(|(c, col)| {
                format!(
                    ".{c} .ln,.{c} .p95,.{c} .whisk{{stroke:{col}}} .{c} .dot,.{c} .bar{{fill:{col}}} .{c} .lbl{{fill:{col}}}"
                )
            })
            .collect::<String>()
    };
    format!(
        r#"<style>
  .bg{{fill:#ffffff}} .fg{{fill:#1f2328}} .mut{{fill:#59636e}} .grid{{stroke:#d1d9e0;stroke-width:1}}
  .ln{{fill:none;stroke-width:2.5}} .p95{{fill:none;stroke-width:1.5;stroke-dasharray:4 3;opacity:.55}}
  .whisk{{fill:none;stroke-width:1.5;opacity:.9}} .lbl{{font-weight:600}}
  text{{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;font-size:13px}}
  .title{{font-size:16px;font-weight:600}} .sub{{font-size:12px}}
  {}
  @media (prefers-color-scheme:dark){{
    .bg{{fill:#0d1117}} .fg{{fill:#e6edf3}} .mut{{fill:#9198a1}} .grid{{stroke:#30363d}}
    {}
  }}
</style>"#,
        series(&light),
        series(&dark)
    )
}

const SERIES: [&str; 4] = ["s-tymux", "s-tymuxcli", "s-cli", "s-ctl"];

fn class_for(name: &str) -> &'static str {
    if name.starts_with("tymux (gRPC") {
        SERIES[0]
    } else if name.starts_with("tymux") {
        SERIES[1]
    } else if name.contains("control") {
        SERIES[3]
    } else {
        SERIES[2]
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn svg(w: f64, h: f64, title: &str, desc: &str, body: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\" role=\"img\" aria-labelledby=\"t d\"><title id=\"t\">{}</title><desc id=\"d\">{}</desc>{}<rect class=\"bg\" width=\"{w}\" height=\"{h}\"/>{body}</svg>\n",
        esc(title),
        esc(desc),
        style()
    )
}

/// A "nice" axis maximum and step covering `v` in about five steps.
fn nice_axis(v: f64) -> (f64, f64) {
    const STEPS: [f64; 12] = [
        0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0,
    ];
    for s in STEPS {
        if v <= s * 5.0 {
            return (s * 5.0, s);
        }
    }
    (v, v / 5.0)
}

fn tick(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn footer(r: &Results, y: f64) -> String {
    let e = &r.env;
    format!(
        "<text class=\"mut sub\" x=\"24\" y=\"{y}\">{}</text>",
        esc(&format!(
            "{} · {} · {} ({} cores) · tymux {}",
            e.tmux_version, e.kernel, e.cpu, e.cores, e.tymux_git_sha
        ))
    )
}

fn scaling(r: &Results) -> String {
    let (h, left, right, top, bottom) = (420.0, 64.0, 170.0, 70.0, 70.0);
    let (pw, ph) = (W - left - right, h - top - bottom);
    let ymax_raw = r
        .backends
        .iter()
        .flat_map(|b| b.create.iter().map(|c| c.stats.p95_ms))
        .fold(0.0, f64::max);
    let (ymax, step) = nice_axis(ymax_raw);
    let n = r.backends[0].create.len() as f64;
    let x = |i: usize| left + pw * (i as f64 + 0.5) / n;
    let y = |v: f64| top + ph * (1.0 - v / ymax);
    let mut o = String::new();
    o += "<text class=\"fg title\" x=\"24\" y=\"28\">Session create latency as sessions pile up</text>";
    o += "<text class=\"mut sub\" x=\"24\" y=\"48\">p50 (solid) and p95 (dashed) per bucket of live sessions; lower is better</text>";
    let mut v = 0.0;
    while v <= ymax + 1e-9 {
        let _ = write!(o, "<line class=\"grid\" x1=\"{left}\" x2=\"{:.1}\" y1=\"{:.1}\" y2=\"{:.1}\"/><text class=\"mut\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>", left + pw, y(v), y(v), left - 8.0, y(v) + 4.0, tick(v));
        v += step;
    }
    let _ = write!(o, "<text class=\"mut\" transform=\"translate(16 {:.1}) rotate(-90)\" text-anchor=\"middle\">create latency (ms)</text>", top + ph / 2.0);
    for (i, b) in r.backends[0].create.iter().enumerate() {
        let _ = write!(
            o,
            "<text class=\"mut\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}–{}</text>",
            x(i),
            top + ph + 20.0,
            b.live_from,
            b.live_to
        );
    }
    let _ = write!(o, "<text class=\"mut\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">live sessions when the create ran</text>", left + pw / 2.0, top + ph + 42.0);
    // Label positions, nudged apart so end-of-line labels never overlap.
    let mut labels: Vec<(f64, &Backend)> = r
        .backends
        .iter()
        .map(|b| (y(b.create.last().map_or(0.0, |c| c.stats.p50_ms)) + 4.0, b))
        .collect();
    labels.sort_by(|a, b| a.0.total_cmp(&b.0));
    for i in 1..labels.len() {
        if labels[i].0 - labels[i - 1].0 < 15.0 {
            labels[i].0 = labels[i - 1].0 + 15.0;
        }
    }
    for b in &r.backends {
        let cls = class_for(&b.name);
        let pts = |f: fn(&crate::results::Bucket) -> f64| {
            b.create
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{:.1},{:.1}", x(i), y(f(c))))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let dots: String = b
            .create
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "<circle class=\"dot\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"3.5\"/>",
                    x(i),
                    y(c.stats.p50_ms)
                )
            })
            .collect();
        let ly = labels
            .iter()
            .find(|(_, l)| l.name == b.name)
            .map_or(0.0, |l| l.0);
        let _ = write!(o, "<g class=\"{cls}\"><polyline class=\"p95\" points=\"{}\"/><polyline class=\"ln\" points=\"{}\"/>{dots}<text class=\"lbl\" x=\"{:.1}\" y=\"{ly:.1}\">{}</text></g>", pts(|c| c.stats.p95_ms), pts(|c| c.stats.p50_ms), x(r.backends[0].create.len() - 1) + 12.0, esc(&b.name));
    }
    o += &footer(r, h - 14.0);
    svg(
        W,
        h,
        "Session create latency vs live sessions",
        &format!(
            "Line chart of create latency p50 and p95 for each backend as live sessions grow to {}",
            r.env.sessions
        ),
        &o,
    )
}

struct Bar {
    value: f64,
    whisker: Option<f64>,
    note: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn bars(
    r: &Results,
    title: &str,
    subtitle: &str,
    unit: &str,
    decimals: usize,
    pick: impl Fn(&Backend) -> Option<Bar>,
) -> String {
    let rows: Vec<(&Backend, Bar)> = r
        .backends
        .iter()
        .filter_map(|b| pick(b).map(|v| (b, v)))
        .collect();
    let (left, right, top, row) = (190.0, 170.0, 70.0, 54.0);
    let h = top + row * rows.len() as f64 + 72.0;
    let pw = W - left - right;
    let top_v = rows
        .iter()
        .map(|(_, b)| b.whisker.unwrap_or(b.value))
        .fold(0.0, f64::max);
    let (xmax, step) = nice_axis(top_v);
    let x = |v: f64| left + pw * v / xmax;
    let mut o = String::new();
    let _ = write!(o, "<text class=\"fg title\" x=\"24\" y=\"28\">{}</text><text class=\"mut sub\" x=\"24\" y=\"48\">{}</text>", esc(title), esc(subtitle));
    let bottom = top + row * rows.len() as f64;
    let mut v = 0.0;
    while v <= xmax + 1e-9 {
        let _ = write!(o, "<line class=\"grid\" x1=\"{:.1}\" x2=\"{:.1}\" y1=\"{}\" y2=\"{}\"/><text class=\"mut\" x=\"{:.1}\" y=\"{}\" text-anchor=\"middle\">{}</text>", x(v), x(v), top - 8.0, bottom - 12.0, x(v), bottom + 6.0, tick(v));
        v += step;
    }
    let _ = write!(
        o,
        "<text class=\"mut\" x=\"{:.1}\" y=\"{}\" text-anchor=\"middle\">{}</text>",
        left + pw / 2.0,
        bottom + 26.0,
        esc(unit)
    );
    for (i, (b, bar)) in rows.iter().enumerate() {
        let cy = top + row * i as f64 + row / 2.0 - 14.0;
        let mut end = x(bar.value);
        let _ = write!(o, "<g class=\"{}\"><text class=\"fg\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{}</text><rect class=\"bar\" x=\"{left}\" y=\"{:.1}\" width=\"{:.1}\" height=\"22\" rx=\"2\"/>", class_for(&b.name), left - 12.0, cy + 5.0, esc(&b.name), cy - 11.0, (x(bar.value) - left).max(1.5));
        let mut label = format!("{:.*}", decimals, bar.value);
        if let Some(wv) = bar.whisker {
            let _ = write!(o, "<line class=\"whisk\" x1=\"{:.1}\" x2=\"{:.1}\" y1=\"{cy:.1}\" y2=\"{cy:.1}\"/><line class=\"whisk\" x1=\"{:.1}\" x2=\"{:.1}\" y1=\"{:.1}\" y2=\"{:.1}\"/>", end, x(wv), x(wv), x(wv), cy - 6.0, cy + 6.0);
            end = x(wv);
            let _ = write!(label, " (p95 {:.*})", decimals, wv);
        }
        if let Some(note) = &bar.note {
            let _ = write!(label, " · {note}");
        }
        let _ = write!(
            o,
            "<text class=\"fg\" x=\"{:.1}\" y=\"{:.1}\">{}</text></g>",
            end + 8.0,
            cy + 5.0,
            esc(&label)
        );
    }
    o += &footer(r, h - 14.0);
    svg(W, h, title, subtitle, &o)
}

fn latency(s: Option<crate::results::Stats>) -> Option<Bar> {
    s.map(|s| Bar {
        value: s.p50_ms,
        whisker: Some(s.p95_ms),
        note: None,
    })
}

fn cell(s: Option<crate::results::Stats>) -> String {
    s.map_or("–".into(), |s| format!("{:.2} ms", s.p50_ms))
}

pub fn table(r: &Results) -> String {
    let mut t = format!(
        "| Backend | Create p50 (first bucket) | Create p50 (last bucket) | Kill p50 | List p50 ({} sessions) | Capture p50 | Keystroke echo p50 | Cold start p50 | Server RSS per session |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|\n",
        r.env.sessions
    );
    for b in &r.backends {
        let _ = writeln!(
            t,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            b.name,
            cell(b.create.first().map(|c| c.stats)),
            cell(b.create.last().map(|c| c.stats)),
            cell(Some(b.kill)),
            cell(Some(b.list)),
            cell(b.capture),
            cell(b.echo),
            cell(b.cold_start),
            b.rss_per_session_kib
                .map_or("–".into(), |k| format!("{k:.0} KiB")),
        );
    }
    t
}

/// Every generated file in `docs/benchmarks/`, in a stable order.
pub fn render_all(r: &Results) -> Vec<(&'static str, String)> {
    vec![
        ("create-scaling.svg", scaling(r)),
        ("kill-latency.svg", bars(r, "Session kill latency", "p50 with p95 whisker; lower is better", "kill latency (ms)", 2, |b| latency(Some(b.kill)))),
        ("list-latency.svg", bars(r, &format!("List sessions with {} alive", r.env.sessions), "p50 with p95 whisker; lower is better", "list latency (ms)", 2, |b| latency(Some(b.list)))),
        ("capture-latency.svg", bars(r, "Read a pane's screen", "p50 with p95 whisker; tymux returns a structured cell grid, tmux returns text; lower is better", "capture latency (ms)", 2, |b| latency(b.capture))),
        ("keystroke-echo.svg", bars(r, "Keystroke to echoed output", "one key sent over a persistent connection until its echo arrives; lower is better", "round trip (ms)", 2, |b| latency(b.echo))),
        ("cold-start.svg", bars(r, "Server cold start", "launch until the server answers its first request; lower is better", "cold start (ms)", 1, |b| latency(b.cold_start))),
        ("reattach.svg", bars(r, "Reattach after a dropped client", "time until the client is streaming again; only tymux can also prove no output was lost; lower is better", "reattach latency (ms)", 2, |b| b.reattach.as_ref().map(|r| Bar {
            value: r.stats.p50_ms,
            whisker: Some(r.stats.p95_ms),
            note: Some(if r.gap_free_resumes > 0 {
                format!("gap-free {}/{}", r.gap_free_resumes, r.stats.n)
            } else {
                "no resume protocol".into()
            }),
        }))),
        ("memory-per-session.svg", bars(r, "Server memory per session", "resident memory of the server process ÷ sessions, at the maximum session count; lower is better", "KiB per session", 0, |b| b.rss_per_session_kib.map(|k| Bar { value: k, whisker: None, note: None }))),
        ("results-table.md", table(r)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Results {
        serde_json::from_str(include_str!("../testdata/sample.json")).unwrap()
    }

    #[test]
    fn render_should_be_deterministic() {
        let r = sample();
        assert_eq!(render_all(&r), render_all(&r));
    }

    #[test]
    fn render_should_emit_well_formed_looking_svg_for_every_chart() {
        for (name, body) in render_all(&sample()) {
            if name.ends_with(".svg") {
                assert!(
                    body.starts_with("<svg ") && body.trim_end().ends_with("</svg>"),
                    "{name}"
                );
                assert_eq!(body.matches("<svg ").count(), 1, "{name}");
            }
        }
    }

    #[test]
    fn bar_charts_should_skip_backends_without_the_metric() {
        let files = render_all(&sample());
        let echo = &files
            .iter()
            .find(|(n, _)| *n == "keystroke-echo.svg")
            .unwrap()
            .1;
        assert!(echo.contains("tymux (gRPC API)") && echo.contains("control mode"));
        assert!(!echo.contains("tmux (CLI)"), "CLI rows have no echo number");
    }

    #[test]
    fn text_should_be_escaped() {
        assert_eq!(esc("a<b&c>"), "a&lt;b&amp;c&gt;");
    }

    #[test]
    fn nice_axis_should_cover_the_value() {
        for v in [0.04, 0.9, 3.7, 12.0, 146.0] {
            let (max, step) = nice_axis(v);
            assert!(max >= v && step > 0.0, "{v}");
        }
    }
}
