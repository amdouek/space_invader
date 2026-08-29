mod dupes;
#[cfg(windows)]
mod mft;
mod report;
mod scan;
mod units;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use colored::Colorize;

use units::{commas, human};

#[derive(Parser)]
#[command(
    name = "space_invader",
    version,
    about = "\u{1F47E} space_invader \u{2014} fast, read-only disk space scanner.\nFinds what's eating your drive and flags duplicate files. Never modifies anything."
)]
struct Args {
    /// Path to scan (drive or folder), e.g. C:\ or C:\Users\me
    path: PathBuf,

    /// Rows per table in the terminal summary
    #[arg(long, default_value_t = 15)]
    top: usize,

    /// Write the HTML report here (default: space_invader_report.html in the current directory)
    #[arg(long)]
    report: Option<PathBuf>,

    /// Skip generating the HTML report
    #[arg(long)]
    no_report: bool,

    /// Skip duplicate detection
    #[arg(long)]
    no_dupes: bool,

    /// Minimum file size considered for hash-based duplicate detection (e.g. 500K, 1M)
    #[arg(long, default_value = "1M", value_parser = units::parse_size)]
    dupe_min_size: u64,

    /// Files larger than this are sample-hashed only and flagged "likely", not "absolute" duplicates
    #[arg(long, default_value = "4G", value_parser = units::parse_size)]
    max_full_hash: u64,

    /// Open the HTML report in the default browser when done
    #[arg(long)]
    open: bool,

    /// Worker threads (default: automatic)
    #[arg(long)]
    threads: Option<usize>,

    /// EXPERIMENTAL, not yet validated: read the NTFS Master File Table directly
    /// (requires an elevated terminal and a local NTFS volume; falls back to the
    /// standard scan otherwise)
    #[cfg(windows)]
    #[arg(long)]
    mft: bool,
}

fn main() -> ExitCode {
    #[cfg(windows)]
    let _ = colored::control::set_virtual_terminal(true);

    let args = Args::parse();

    let root = match dunce::canonicalize(&args.path) {
        Ok(p) if p.is_dir() => p,
        Ok(p) => {
            eprintln!("{} {} is not a directory", "error:".red().bold(), p.display());
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!(
                "{} cannot access {}: {e}",
                "error:".red().bold(),
                args.path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    if let Some(n) = args.threads {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(n).build_global();
    }

    println!(
        "\n{} {}",
        "\u{1F47E} space_invader".bold().green(),
        format!("v{}", env!("CARGO_PKG_VERSION")).dimmed()
    );
    println!("   scanning {} (read-only)\n", root.display().to_string().bold());

    #[cfg(windows)]
    let scan_result = if args.mft {
        match mft::scan(&root) {
            Ok(r) => {
                println!("   {}", "(MFT fast path)".dimmed());
                r
            }
            Err(e) => {
                eprintln!(
                    "   {} {e}\n   falling back to the standard scan",
                    "mft:".yellow().bold()
                );
                scan::scan(&root, args.threads)
            }
        }
    } else {
        scan::scan(&root, args.threads)
    };
    #[cfg(not(windows))]
    let scan_result = scan::scan(&root, args.threads);

    let secs = scan_result.elapsed.as_secs_f64();
    println!(
        "   {} in {} \u{2014} {} files, {} folders, {} errors ({}/s)",
        human(scan_result.total_size).bold().yellow(),
        format!("{secs:.1}s").bold(),
        commas(scan_result.files.len() as u64),
        commas(scan_result.dir_count),
        commas(scan_result.error_count),
        commas((scan_result.files.len() as f64 / secs.max(0.001)) as u64)
    );
    if scan_result.error_count > 0 {
        #[cfg(windows)]
        let hint = "Note: Some items were inaccessible; run from an elevated terminal for full coverage";
        #[cfg(target_os = "macos")]
        let hint = "Note: Some items were inaccessible; grant Full Disk Access to your terminal (System Settings \u{2192} Privacy & Security) for full coverage";
        #[cfg(all(unix, not(target_os = "macos")))]
        let hint = "Note: Some items were inaccessible; run with sudo for full coverage";
        println!("   {}", hint.dimmed());
    }

    print_ranked(
        "largest folders",
        scan_result
            .dir_sizes
            .iter()
            .filter(|(p, _)| *p != &scan_result.root)
            .map(|(p, s)| (p.display().to_string(), *s))
            .collect(),
        scan_result.total_size,
        args.top,
    );

    print_ranked(
        "largest files",
        scan_result
            .files
            .iter()
            .map(|f| (f.path.display().to_string(), f.size))
            .collect(),
        scan_result.total_size,
        args.top,
    );

    let dupe_report = if args.no_dupes {
        None
    } else {
        println!("\n{}", "duplicate detection".bold().underline());
        let d = dupes::find_dupes(&scan_result.files, args.dupe_min_size, args.max_full_hash);
        println!(
            "   hashed {} candidate files (\u{2265} {})",
            commas(d.candidates_hashed),
            human(args.dupe_min_size)
        );
        println!(
            "   {} absolute duplicate groups \u{2014} {} reclaimable",
            commas(d.absolute.len() as u64),
            human(d.wasted_absolute()).bold().red()
        );
        println!(
            "   {} likely duplicate groups \u{2014} up to {} reclaimable",
            commas((d.likely_sample.len() + d.likely_name.len()) as u64),
            human(d.wasted_likely()).bold().yellow()
        );

        for g in d.absolute.iter().take(args.top.min(10)) {
            println!(
                "   {} \u{00D7} {}  wasted {}",
                g.files.len().to_string().bold(),
                human(g.size),
                human(g.wasted).red()
            );
            for f in g.files.iter().take(4) {
                println!("      {}", f.dimmed());
            }
            if g.files.len() > 4 {
                println!("      {}", format!("... and {} more", g.files.len() - 4).dimmed());
            }
        }
        Some(d)
    };

    if !args.no_report {
        let out = args
            .report
            .clone()
            .unwrap_or_else(|| PathBuf::from("space_invader_report.html"));
        match report::generate(
            &scan_result,
            dupe_report.as_ref(),
            args.dupe_min_size,
            args.max_full_hash,
            &out,
        ) {
            Ok(()) => {
                let shown = dunce::canonicalize(&out).unwrap_or(out);
                println!(
                    "\n{} {}",
                    "report:".bold().green(),
                    shown.display().to_string().underline()
                );
                if args.open {
                    open_in_browser(&shown);
                }
            }
            Err(e) => {
                eprintln!("{} failed to write report: {e}", "error:".red().bold());
                return ExitCode::FAILURE;
            }
        }
    }

    println!();
    ExitCode::SUCCESS
}

/// Open the report in the platform's default browser. Best-effort: a failure
/// only means the user opens the printed path themselves.
fn open_in_browser(path: &std::path::Path) {
    #[cfg(windows)]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(path)
        .spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = result {
        eprintln!("could not open browser: {e}");
    }
}

fn print_ranked(title: &str, mut rows: Vec<(String, u64)>, total: u64, top: usize) {
    println!("\n{}", title.bold().underline());
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    rows.truncate(top);
    let max = rows.first().map(|r| r.1).unwrap_or(1).max(1);
    for (path, size) in &rows {
        let frac_of_max = *size as f64 / max as f64;
        let filled = (frac_of_max * 24.0).round() as usize;
        let bar = format!(
            "{}{}",
            "\u{2588}".repeat(filled).cyan(),
            "\u{2591}".repeat(24 - filled).dimmed()
        );
        let pct = if total > 0 {
            *size as f64 * 100.0 / total as f64
        } else {
            0.0
        };
        println!(
            "  {bar} {:>10} {:>5.1}%  {}",
            human(*size).bold(),
            pct,
            path
        );
    }
    if rows.is_empty() {
        println!("  {}", "(nothing found)".dimmed());
    }
}
