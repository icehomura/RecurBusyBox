// This file is part of RecurBusyBox.
//
// `dust`: show the on-disk size of directories, sorted largest first. A
// focused subset of the classic `dust` utility ("du but sorted and pretty").
//
// Every PATH is walked exactly once with `walkdir` (symlinks are never
// followed). Each regular file's size is added to every ancestor directory up
// to that PATH's root, so memory stays proportional to the number of
// directories rather than the number of files. The resulting entries are then
// sorted by size and rendered with a percentage and an ASCII-block bar.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, Command};
use walkdir::WalkDir;

const DEFAULT_NUMBER: usize = 12;
const BAR_MAX: usize = 40;
const BLOCK: char = '\u{2588}'; // █

struct Options {
    paths: Vec<PathBuf>,
    number: usize,
    depth: Option<usize>,
    all: bool,
    summarize: bool,
    bytes: bool,
}

/// One rendered line: the entry's size, its share of its own root, the path.
struct Entry {
    size: u64,
    percent: f64,
    /// Display form; uses `/` separators on every platform.
    path: String,
    is_root: bool,
}

pub fn uu_app() -> Command {
    Command::new("dust")
        .version(uucore::crate_version!())
        .about("Show directory disk usage, sorted largest first.")
        .arg(
            Arg::new("number")
                .short('n')
                .long("number")
                .value_name("K")
                .help("Show at most K entries (default 12)"),
        )
        .arg(
            Arg::new("depth")
                .short('d')
                .long("depth")
                .value_name("N")
                .help("Only consider directories up to depth N"),
        )
        .arg(
            Arg::new("all")
                .short('a')
                .long("all")
                .help("Also list files, not just directories")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("summarize")
                .short('s')
                .long("summarize")
                .help("Print only the total for each PATH")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("human-readable")
                .short('H')
                .long("human-readable")
                .help("Human readable sizes (the default)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("bytes")
                .short('b')
                .long("bytes")
                .help("Print raw byte counts instead of human readable sizes")
                .action(ArgAction::SetTrue),
        )
        .arg(Arg::new("paths").value_name("PATH").num_args(0..))
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() {
        &argv[..]
    } else {
        &argv[1..]
    };

    let mut full = Vec::with_capacity(tail.len() + 1);
    full.push("dust".to_string());
    full.extend(tail.iter().cloned());

    let matches = match uu_app().try_get_matches_from(full) {
        Ok(m) => m,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };

    let mut number = DEFAULT_NUMBER;
    if let Some(raw) = matches.get_one::<String>("number") {
        let Ok(parsed) = raw.parse::<usize>() else {
            let _ = writeln!(io::stderr(), "dust: invalid number of entries: {raw}");
            return 2;
        };
        number = parsed;
    }

    let mut depth = None;
    if let Some(raw) = matches.get_one::<String>("depth") {
        let Ok(parsed) = raw.parse::<usize>() else {
            let _ = writeln!(io::stderr(), "dust: invalid depth: {raw}");
            return 2;
        };
        depth = Some(parsed);
    }

    let paths: Vec<PathBuf> = matches
        .get_many::<String>("paths")
        .map(|vals| vals.map(PathBuf::from).collect())
        .unwrap_or_default();
    let paths = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths
    };

    let opts = Options {
        paths,
        number,
        depth,
        all: matches.get_flag("all"),
        summarize: matches.get_flag("summarize"),
        bytes: matches.get_flag("bytes"),
    };

    run(&opts)
}

fn run(opts: &Options) -> i32 {
    let mut status = 0;
    let mut entries: Vec<Entry> = Vec::new();
    let mut grand_total: u64 = 0;

    for root in &opts.paths {
        if fs::symlink_metadata(root).is_err() {
            let _ = writeln!(io::stderr(), "dust: cannot access '{}'", root.display());
            status = 2;
            continue;
        }
        let (mut found, total) = collect(root, opts);
        grand_total = grand_total.saturating_add(total);
        if opts.summarize {
            found.retain(|e| e.is_root);
        }
        entries.extend(found);
    }

    // Largest first; ties broken by path so output is deterministic.
    entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));

    let mut out = String::new();
    for entry in entries.iter().take(opts.number) {
        let raw_size = render_size(entry.size, opts.bytes);
        let pct = entry.percent;
        let bar = bar_for(pct);
        let path = &entry.path;
        let _ = writeln!(out, "{raw_size}  {pct:>5.1}% {bar:<BAR_MAX$}  {path}");
    }
    let _ = writeln!(out, "TOTAL  {}", render_size(grand_total, opts.bytes));

    let stdout = io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(out.as_bytes());
    let _ = lock.flush();

    status
}

/// Walk `root` once and return its renderable entries plus its own total.
fn collect(root: &Path, opts: &Options) -> (Vec<Entry>, u64) {
    let mut dir_sizes: HashMap<PathBuf, u64> = HashMap::new();
    let mut file_sizes: HashMap<PathBuf, u64> = HashMap::new();
    // Remember the depth of each directory so `-d` can filter cheaply.
    let mut dir_depths: HashMap<PathBuf, usize> = HashMap::new();
    let mut file_depths: HashMap<PathBuf, usize> = HashMap::new();

    let root_is_dir = fs::symlink_metadata(root).is_ok_and(|m| m.is_dir());

    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let depth = relative_depth(root, path);
        let Ok(meta) = entry.metadata() else {
            continue;
        };

        if meta.is_dir() {
            dir_sizes.entry(path.to_path_buf()).or_insert(0);
            dir_depths.entry(path.to_path_buf()).or_insert(depth);
            continue;
        }
        if !meta.is_file() {
            // Sockets, fifos, devices and dangling symlinks contribute nothing.
            continue;
        }

        let size = allocated_size(&meta);
        if opts.all {
            file_sizes.entry(path.to_path_buf()).or_insert(size);
            file_depths.entry(path.to_path_buf()).or_insert(depth);
        }

        // Credit every ancestor directory, stopping at the walk root.
        let mut cursor = path.parent();
        while let Some(dir) = cursor {
            if let Some(slot) = dir_sizes.get_mut(dir) {
                *slot = slot.saturating_add(size);
            } else {
                // Should not happen inside a walked tree, but never lose bytes.
                dir_sizes.insert(dir.to_path_buf(), size);
            }
            if dir == root {
                break;
            }
            if !dir.starts_with(root) {
                break;
            }
            cursor = dir.parent();
        }
    }

    let total = if root_is_dir {
        dir_sizes.get(root).copied().unwrap_or(0)
    } else {
        file_sizes.get(root).copied().unwrap_or(0)
    };

    let mut out = Vec::with_capacity(dir_sizes.len() + file_sizes.len());
    let within_depth = |depth: usize| opts.depth.is_none_or(|max| depth <= max);

    for (path, size) in &dir_sizes {
        let depth = dir_depths.get(path).copied().unwrap_or(0);
        if !within_depth(depth) {
            continue;
        }
        // Empty directories would otherwise always render as `0B  0.0%`.
        if *size == 0 && path != root {
            continue;
        }
        out.push(Entry {
            size: *size,
            percent: percent_of(*size, total),
            path: display_path(root, path),
            is_root: path == root,
        });
    }

    for (path, size) in &file_sizes {
        let depth = file_depths.get(path).copied().unwrap_or(0);
        if !within_depth(depth) {
            continue;
        }
        out.push(Entry {
            size: *size,
            percent: percent_of(*size, total),
            path: display_path(root, path),
            is_root: path == root,
        });
    }

    (out, total)
}

fn relative_depth(root: &Path, path: &Path) -> usize {
    path.strip_prefix(root).map_or_else(
        |_| {
            path.components()
                .count()
                .saturating_sub(root.components().count())
        },
        |rest| rest.components().count(),
    )
}

fn percent_of(size: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (size as f64 / total as f64) * 100.0
    }
}

/// Cosmetic cleanup: keep `src/uu` rather than `./src/uu`, and make the `.`
/// root render as `.` instead of the empty path.
fn display_path(root: &Path, path: &Path) -> String {
    let trimmed: &Path = if path == root {
        root
    } else if let Ok(rest) = path.strip_prefix("./") {
        rest
    } else if root == Path::new(".") {
        match path.strip_prefix(".") {
            Ok(rest) if rest.as_os_str().is_empty() => Path::new("."),
            Ok(rest) => rest,
            Err(_) => path,
        }
    } else {
        path
    };
    trimmed.to_string_lossy().replace('\\', "/")
}

fn allocated_size(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // `blocks()` counts 512-byte blocks actually allocated on disk.
        let bytes = meta.blocks().saturating_mul(512);
        if bytes > 0 { bytes } else { meta.len() }
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

fn render_size(bytes: u64, raw: bool) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;
    if raw {
        return format!("{bytes:>8}");
    }
    let text = if bytes < KIB {
        format!("{bytes}B")
    } else if bytes < MIB {
        format!("{:.1}K", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1}M", bytes as f64 / MIB as f64)
    } else if bytes < TIB {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1}T", bytes as f64 / TIB as f64)
    };
    format!("{text:>8}")
}

fn bar_for(percent: f64) -> String {
    if percent <= 0.0 {
        return String::new();
    }
    let scaled = (percent / 100.0 * BAR_MAX as f64).round() as usize;
    let len = scaled.clamp(1, BAR_MAX);
    std::iter::repeat_n(BLOCK, len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes_use_base_1024() {
        assert_eq!(render_size(0, false).trim(), "0B");
        assert_eq!(render_size(1024, false).trim(), "1.0K");
        assert_eq!(render_size(1024 * 1024, false).trim(), "1.0M");
        assert_eq!(render_size(3 * 1024 * 1024 * 1024, false).trim(), "3.0G");
        assert_eq!(render_size(5 * 1024_u64.pow(4), false).trim(), "5.0T");
    }

    #[test]
    fn bytes_mode_prints_counts() {
        assert_eq!(render_size(1234, true).trim(), "1234");
    }

    #[test]
    fn bar_scales_and_caps() {
        assert_eq!(bar_for(0.0), "");
        assert_eq!(bar_for(50.0).chars().count(), 20);
        assert_eq!(bar_for(100.0).chars().count(), BAR_MAX);
        assert_eq!(bar_for(1000.0).chars().count(), BAR_MAX);
        assert_eq!(bar_for(0.1).chars().count(), 1);
    }

    #[test]
    fn percent_of_handles_empty_total() {
        assert_eq!(percent_of(0, 0), 0.0);
        assert_eq!(percent_of(1, 4), 25.0);
    }
}
