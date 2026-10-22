#!/usr/bin/env python3
"""Render docs/benchmarks/*.svg and the results table from results.json.

Stdlib only. Usage: scripts/render_bench.py [results.json] [outdir]
Each SVG carries its own light/dark styles so it reads on GitHub in both.
"""
import json
import sys
from html import escape
from pathlib import Path

SERIES_CLASS = ["s-tymux", "s-cli", "s-ctl"]  # tymux first, then the tmux flavors

# No CSS variables: librsvg and some viewers drop them.
def _series_css(colors):
    return "".join(
        f".{c} .ln,.{c} .p95,.{c} .whisk{{stroke:{col}}} .{c} .dot,.{c} .bar{{fill:{col}}} .{c} .lbl{{fill:{col}}}"
        for c, col in zip(SERIES_CLASS, colors)
    )


STYLE = (
    """
<style>
  .bg{fill:#ffffff} .fg{fill:#1f2328} .mut{fill:#59636e} .grid{stroke:#d1d9e0;stroke-width:1}
  .ln{fill:none;stroke-width:2.5} .p95{fill:none;stroke-width:1.5;stroke-dasharray:4 3;opacity:.55}
  .whisk{fill:none;stroke-width:1.5;stroke-dasharray:none;opacity:.9} .lbl{font-weight:600}
  text{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;font-size:13px}
  .title{font-size:16px;font-weight:600} .sub{font-size:12px}
  """
    + _series_css(["#0969da", "#6e7781", "#bf8700"])
    + """
  @media (prefers-color-scheme:dark){
    .bg{fill:#0d1117} .fg{fill:#e6edf3} .mut{fill:#9198a1} .grid{stroke:#30363d}
    """
    + _series_css(["#58a6ff", "#9198a1", "#d29922"])
    + """
  }
</style>"""
)


def svg(w, h, body, title, desc):
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" '
        f'role="img" aria-labelledby="t d"><title id="t">{escape(title)}</title><desc id="d">{escape(desc)}</desc>'
        f'{STYLE}<rect class="bg" width="{w}" height="{h}"/>{body}</svg>\n'
    )


def nice_max(v):
    for step in (0.5, 1, 2, 2.5, 5, 10, 25, 50, 100, 250, 500):
        if v <= step * 5:
            return step * 5, step
    return v, v / 5


def footer(env, y, w):
    note = f'{env["tmux_version"]} · {env["kernel"]} · {env["cpu"]} ({env["cores"]} cores) · tymux {env["tymux_git_sha"]}'
    return f'<text class="mut sub" x="24" y="{y}">{escape(note)}</text>'


def scaling_chart(res):
    env, backends = res["env"], res["backends"]
    w, h, left, right, top, bottom = 760, 420, 64, 150, 70, 70
    pw, ph = w - left - right, h - top - bottom
    ymax, step = nice_max(max(b["p95_ms"] for r in backends for b in r["create"]))
    n = len(backends[0]["create"])
    x = lambda i: left + pw * (i + 0.5) / n
    y = lambda v: top + ph * (1 - v / ymax)
    out = [
        '<text class="fg title" x="24" y="28">Session create latency as sessions pile up</text>',
        '<text class="mut sub" x="24" y="48">p50 (solid) and p95 (dashed) per 100 live sessions; lower is better</text>',
    ]
    v = 0.0
    while v <= ymax + 1e-9:
        out.append(f'<line class="grid" x1="{left}" x2="{left + pw}" y1="{y(v):.1f}" y2="{y(v):.1f}"/>')
        out.append(f'<text class="mut" x="{left - 8}" y="{y(v) + 4:.1f}" text-anchor="end">{v:g}</text>')
        v += step
    out.append(f'<text class="mut" transform="translate(16 {top + ph / 2}) rotate(-90)" text-anchor="middle">create latency (ms)</text>')
    for i, b in enumerate(backends[0]["create"]):
        out.append(f'<text class="mut" x="{x(i):.1f}" y="{top + ph + 20}" text-anchor="middle">{b["live_from"]}–{b["live_to"]}</text>')
    out.append(f'<text class="mut" x="{left + pw / 2}" y="{top + ph + 42}" text-anchor="middle">live sessions when the create ran</text>')
    for r, cls in zip(backends, SERIES_CLASS):
        p50 = " ".join(f"{x(i):.1f},{y(b['p50_ms']):.1f}" for i, b in enumerate(r["create"]))
        p95 = " ".join(f"{x(i):.1f},{y(b['p95_ms']):.1f}" for i, b in enumerate(r["create"]))
        dots = "".join(f'<circle class="dot" cx="{x(i):.1f}" cy="{y(b["p50_ms"]):.1f}" r="3.5"/>' for i, b in enumerate(r["create"]))
        last = r["create"][-1]
        out.append(
            f'<g class="{cls}"><polyline class="p95" points="{p95}"/><polyline class="ln" points="{p50}"/>{dots}'
            f'<text class="lbl" x="{x(n - 1) + 12:.1f}" y="{y(last["p50_ms"]) + 4:.1f}">{escape(r["name"])}</text></g>'
        )
    out.append(footer(env, h - 14, w))
    return svg(w, h, "".join(out), "Session create latency vs live sessions",
               "Line chart of create latency p50 and p95 for tymux and tmux as live sessions grow to " + str(env["sessions"]))


def bar_chart(res, title, subtitle, getter, unit, filename_desc, fmt="{:.2f}", whisker=None):
    env, backends = res["env"], res["backends"]
    w, left, right, top, row = 760, 190, 150, 70, 54
    h = top + row * len(backends) + 72
    pw = w - left - right
    top_v = max((whisker(r) if whisker else getter(r)) for r in backends)
    xmax, step = nice_max(top_v)
    x = lambda v: left + pw * v / xmax
    out = [f'<text class="fg title" x="24" y="28">{escape(title)}</text>', f'<text class="mut sub" x="24" y="48">{escape(subtitle)}</text>']
    v = 0.0
    while v <= xmax + 1e-9:
        out.append(f'<line class="grid" x1="{x(v):.1f}" x2="{x(v):.1f}" y1="{top - 8}" y2="{top + row * len(backends) - 12}"/>')
        out.append(f'<text class="mut" x="{x(v):.1f}" y="{top + row * len(backends) + 6}" text-anchor="middle">{v:g}</text>')
        v += step
    out.append(f'<text class="mut" x="{left + pw / 2}" y="{top + row * len(backends) + 26}" text-anchor="middle">{escape(unit)}</text>')
    for i, (r, cls) in enumerate(zip(backends, SERIES_CLASS)):
        cy = top + row * i + row / 2 - 14
        val = getter(r)
        out.append(f'<g class="{cls}"><text class="fg" x="{left - 12}" y="{cy + 5:.1f}" text-anchor="end">{escape(r["name"])}</text>'
                   f'<rect class="bar" x="{left}" y="{cy - 11:.1f}" width="{max(x(val) - left, 1.5):.1f}" height="22" rx="2"/>')
        end = x(val)
        if whisker:
            wv = whisker(r)
            out.append(f'<line class="whisk" x1="{end:.1f}" x2="{x(wv):.1f}" y1="{cy:.1f}" y2="{cy:.1f}"/>'
                       f'<line class="whisk" x1="{x(wv):.1f}" x2="{x(wv):.1f}" y1="{cy - 6:.1f}" y2="{cy + 6:.1f}"/>')
            end = x(wv)
        txt = fmt.format(val) + (f" (p95 {fmt.format(whisker(r))})" if whisker else "")
        out.append(f'<text class="fg" x="{end + 8:.1f}" y="{cy + 5:.1f}">{txt}</text></g>')
    out.append(footer(env, h - 14, w))
    return svg(w, h, "".join(out), title, filename_desc)


def table(res):
    env, backends = res["env"], res["backends"]
    first, last = 0, -1
    head = "| Backend | Create p50 (0–100 live) | Create p50 (500–600 live) | Kill p50 | Server RSS per session |\n|---|---:|---:|---:|---:|\n"
    rows = ""
    for r in backends:
        rows += (f'| {r["name"]} | {r["create"][first]["p50_ms"]:.2f} ms | {r["create"][last]["p50_ms"]:.2f} ms '
                 f'| {r["kill"]["p50_ms"]:.2f} ms | {r["server_rss_kib_per_session"]:.0f} KiB |\n')
    return head + rows


def main():
    src = Path(sys.argv[1] if len(sys.argv) > 1 else "docs/benchmarks/results.json")
    out = Path(sys.argv[2] if len(sys.argv) > 2 else src.parent)
    res = json.loads(src.read_text())
    (out / "create-scaling.svg").write_text(scaling_chart(res))
    (out / "kill-latency.svg").write_text(bar_chart(
        res, "Session kill latency", "p50 with p95 whisker, across all sessions; lower is better",
        lambda r: r["kill"]["p50_ms"], "kill latency (ms)", "Bar chart of kill latency for tymux and tmux",
        whisker=lambda r: r["kill"]["p95_ms"]))
    (out / "memory-per-session.svg").write_text(bar_chart(
        res, "Server memory per session", "resident memory of the server process ÷ sessions, at the maximum session count; lower is better",
        lambda r: r["server_rss_kib_per_session"], "KiB per session", "Bar chart of server memory per session for tymux and tmux",
        fmt="{:.0f}"))
    (out / "results-table.md").write_text(table(res))


if __name__ == "__main__":
    main()
