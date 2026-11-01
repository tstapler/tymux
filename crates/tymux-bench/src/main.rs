//! `tymux-bench render [--check]` regenerates (or verifies) the charts and
//! README table from `docs/benchmarks/results.json`; `tymux-bench gate`
//! fails when a fresh run regressed against it; `tymux-bench export` converts a
//! run to benchdiff JSON for the CI comparison. Run from the repo root.

mod export;
mod gate;
mod render;
mod results;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use results::Results;
use std::path::{Path, PathBuf};

const TABLE_START: &str = "<!-- bench-table:start -->";
const TABLE_END: &str = "<!-- bench-table:end -->";

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write the SVGs, table and README block from the results (or, with
    /// --check, fail if the files on disk differ from what would be written).
    Render {
        #[arg(long, default_value = "docs/benchmarks/results.json")]
        results: PathBuf,
        #[arg(long, default_value = "docs/benchmarks")]
        out_dir: PathBuf,
        #[arg(long, default_value = "README.md")]
        readme: PathBuf,
        #[arg(long)]
        check: bool,
    },
    /// Fail if a run no longer backs the README's claims, or regressed
    /// catastrophically against tmux.
    Gate {
        #[arg(long, default_value = "docs/benchmarks/results.json")]
        results: PathBuf,
    },
    /// Convert a run into benchdiff canonical JSON (stdout unless --out).
    Export {
        #[arg(long, default_value = "docs/benchmarks/results.json")]
        results: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// Replaces the generated block in `readme` with `table`.
fn splice_table(readme: &str, table: &str) -> Result<String> {
    let start = readme
        .find(TABLE_START)
        .context("README is missing the bench-table:start marker")?;
    let end = readme
        .find(TABLE_END)
        .context("README is missing the bench-table:end marker")?;
    if end < start {
        bail!("README bench-table markers are out of order");
    }
    Ok(format!(
        "{}{TABLE_START}\n{table}{}",
        &readme[..start],
        &readme[end..]
    ))
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Render {
            results,
            out_dir,
            readme,
            check,
        } => {
            let r = Results::load(&results)?;
            let files = render::render_all(&r);
            let table = &files
                .iter()
                .find(|(n, _)| *n == "results-table.md")
                .unwrap()
                .1;
            let readme_text = std::fs::read_to_string(&readme)
                .with_context(|| format!("read {}", readme.display()))?;
            let new_readme = splice_table(&readme_text, table)?;

            let mut stale = Vec::new();
            for (name, body) in &files {
                let path = out_dir.join(name);
                if check {
                    if std::fs::read_to_string(&path).ok().as_deref() != Some(body) {
                        stale.push(path.display().to_string());
                    }
                } else {
                    std::fs::write(&path, body)
                        .with_context(|| format!("write {}", path.display()))?;
                }
            }
            if check {
                if new_readme != readme_text {
                    stale.push(readme.display().to_string());
                }
                if !stale.is_empty() {
                    bail!(
                        "stale benchmark output (run `cargo run -p tymux-bench -- render`): {}",
                        stale.join(", ")
                    );
                }
                println!("benchmark charts and README table are up to date");
            } else {
                write_if_changed(&readme, &new_readme)?;
            }
            Ok(())
        }
        Cmd::Export { results, out } => {
            let doc = export::to_benchdiff(&Results::load(&results)?);
            let text = serde_json::to_string_pretty(&doc)? + "\n";
            match out {
                Some(path) => {
                    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))
                }
                None => {
                    print!("{text}");
                    Ok(())
                }
            }
        }
        Cmd::Gate { results } => {
            let rows = gate::evaluate(&Results::load(&results)?);
            if rows.is_empty() {
                bail!("no comparable metrics in {}", results.display());
            }
            let mut failed = 0;
            for row in &rows {
                let mark = if row.ok { "ok  " } else { "FAIL" };
                println!(
                    "{mark} {:<64} {:>6.2}x (max {:.1}x)",
                    row.label, row.ratio, row.max
                );
                failed += usize::from(!row.ok);
            }
            if failed > 0 {
                bail!("{failed} benchmark rule(s) failed");
            }
            Ok(())
        }
    }
}

fn write_if_changed(path: &Path, body: &str) -> Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() != Some(body) {
        std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_should_replace_only_the_marked_block() {
        let readme = format!("before\n{TABLE_START}\nold\n{TABLE_END}\nafter\n");
        let out = splice_table(&readme, "NEW\n").unwrap();
        assert_eq!(
            out,
            format!("before\n{TABLE_START}\nNEW\n{TABLE_END}\nafter\n")
        );
        assert_eq!(splice_table(&out, "NEW\n").unwrap(), out, "idempotent");
    }

    #[test]
    fn splice_should_error_when_markers_missing() {
        assert!(splice_table("no markers", "x").is_err());
    }
}
