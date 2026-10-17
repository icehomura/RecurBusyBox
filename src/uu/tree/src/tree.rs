// This file is part of RecurBusyBox.
//
// `tree`: display directories and files as an indented tree. A focused subset
// of the classic `tree` utility. The listing is built as an owned tree of
// [`Node`]s first (via `std::fs::read_dir` recursion, or `ignore::WalkBuilder`
// when `--gitignore` is given), then sorted, optionally pruned, and finally
// rendered with box-drawing characters.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, Command};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

struct Node {
    name: String,
    is_dir: bool,
    /// File size in bytes (0 for directories; symlinks are not followed).
    size: u64,
    children: Vec<Self>,
}

impl Node {
    fn leaf(name: String, is_dir: bool, size: u64) -> Self {
        Self {
            name,
            is_dir,
            size,
            children: Vec::new(),
        }
    }

    /// Render this node (and its subtree) into `out`, updating `counts`.
    fn render(&self, prefix: &str, last: bool, opts: &Options, out: &mut String, counts: &mut Counts) {
        let branch = if last { "└── " } else { "├── " };
        let _ = write!(out, "{prefix}{branch}{}", self.name);
        if self.is_dir {
            counts.dirs += 1;
        } else {
            counts.files += 1;
            if opts.human_readable {
                let _ = write!(out, "  [{}]", human_size(self.size));
                counts.bytes += self.size;
            }
        }
        out.push('\n');

        if !self.is_dir || self.children.is_empty() {
            return;
        }
        let filler = if last { "    " } else { "│   " };
        let child_prefix = format!("{prefix}{filler}");
        let total = self.children.len();
        for (index, child) in self.children.iter().enumerate() {
            child.render(&child_prefix, index + 1 == total, opts, out, counts);
        }
    }
}

struct Options {
    path: PathBuf,
    all: bool,
    dirs_only: bool,
    level: Option<usize>,
    prune: bool,
    human_readable: bool,
    gitignore: bool,
    ignore_patterns: Vec<String>,
}

struct Counts {
    dirs: u64,
    files: u64,
    bytes: u64,
}

pub fn uu_app() -> Command {
    Command::new("tree")
        .version(uucore::crate_version!())
        // `-h` is `--human-readable` here (as in the classic `tree`), so the
        // automatic help flag must be replaced by a long-only one.
        .disable_help_flag(true)
        .arg(Arg::new("help").long("help").action(ArgAction::Help)
                .help("Print help"))
        .about("Display directories and files as an indented tree.")
        .arg(Arg::new("path")
                .value_name("PATH")
                .help("Directory to list (default: .)"),)
        .arg(
            Arg::new("all")
                .short('a')
                .long("all")
                .action(ArgAction::SetTrue)
                .help("Include entries starting with ."),
        )
        .arg(
            Arg::new("dirs-only")
                .short('d')
                .long("dirs-only")
                .action(ArgAction::SetTrue)
                .help("List directories only"),
        )
        .arg(
            Arg::new("level")
                .short('L')
                .long("level")
                .value_name("N")
                .help("Max display depth"),
        )
        .arg(
            Arg::new("ignore")
                .short('I')
                .long("ignore")
                .value_name("PATTERN")
                .action(ArgAction::Append)
                .help("Skip entries matching a shell glob (| separates alternatives)"),
        )
        .arg(
            Arg::new("prune")
                .short('p')
                .long("prune")
                .action(ArgAction::SetTrue)
                .help("Do not print empty directories"),
        )
        .arg(
            Arg::new("human-readable")
                .short('h')
                .long("human-readable")
                .action(ArgAction::SetTrue)
                .help("Show file sizes and a byte total"),
        )
        .arg(
            Arg::new("gitignore")
                .long("gitignore")
                .action(ArgAction::SetTrue)
                .help("Respect .gitignore rules"),
        )
}

/// Parse the argv tail (without argv[0]) into [`Options`].
/// `Err(Some(msg))` is a usage error; `Err(None)` means the app already
/// handled the request (`--help` / `--version`).
fn parse(argv: &[String]) -> Result<Options, Option<String>> {
    let mut opts = Options {
        path: PathBuf::from("."),
        all: false,
        dirs_only: false,
        level: None,
        prune: false,
        human_readable: false,
        gitignore: false,
        ignore_patterns: Vec::new(),
    };
    let mut positionals: Vec<String> = Vec::new();
    let mut no_more_flags = false;
    let mut iter = argv.iter();
    while let Some(a) = iter.next() {
        if no_more_flags {
            positionals.push(a.clone());
            continue;
        }
        // Support `--flag=value` for the long options that take a value.
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        match name {
            "--" => no_more_flags = true,
            "-a" | "--all" => opts.all = true,
            "-d" | "--dirs-only" => opts.dirs_only = true,
            "-p" | "--prune" => opts.prune = true,
            "-h" | "--human-readable" => opts.human_readable = true,
            "--gitignore" => opts.gitignore = true,
            "-L" | "--level" => {
                let v = inline
                    .or_else(|| iter.next().cloned())
                    .ok_or_else(|| usage("--level needs a value"))?;
                opts.level = Some(parse_level(&v)?);
            }
            "-I" | "--ignore" => {
                let v = inline
                    .or_else(|| iter.next().cloned())
                    .ok_or_else(|| usage("--ignore needs a value"))?;
                opts.ignore_patterns.push(v);
            }
            "--help" => {
                let _ = uu_app().print_help();
                return Err(None);
            }
            "-V" | "--version" => {
                let _ = writeln!(io::stdout(), "tree {}", uucore::crate_version!());
                return Err(None);
            }
            // Attached value forms: -L1, -IGLOB.
            s if s.starts_with("-L") && s.len() > 2 => {
                opts.level = Some(parse_level(&s[2..])?);
            }
            s if s.starts_with("-I") && s.len() > 2 => opts.ignore_patterns.push(s[2..].to_string()),
            // Combined boolean shorts such as -dh or -ap.
            s if s.starts_with('-') && s.len() > 1 && !s.starts_with("--") => {
                for c in s[1..].chars() {
                    match c {
                        'a' => opts.all = true,
                        'd' => opts.dirs_only = true,
                        'p' => opts.prune = true,
                        'h' => opts.human_readable = true,
                        _ => return Err(Some(format!("unknown option '-{c}'"))),
                    }
                }
            }
            s if s.starts_with('-') && s != "-" => {
                return Err(Some(s.to_string()));
            }
            _ => positionals.push(a.clone()),
        }
    }

    if let Some(p) = positionals.into_iter().next() {
        opts.path = PathBuf::from(p);
    }
    Ok(opts)
}

fn usage(what: &str) -> String {
    format!("{what} (usage: tree [OPTIONS] [PATH])")
}

fn parse_level(v: &str) -> Result<usize, String> {
    let n = v
        .parse::<usize>()
        .map_err(|_| format!("invalid level '{v}'"))?;
    // `-L 0` would print nothing at all; clamp to the top-level entries.
    Ok(n.max(1))
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        // Classic `tree -I` accepts an alternation: `foo|bar`. Split on `|` and
        // treat each piece as its own shell glob.
        let mut pieces: Vec<&str> = pattern.split('|').filter(|p| !p.is_empty()).collect();
        if pieces.is_empty() {
            pieces.push("");
        }
        for piece in pieces {
            let glob = Glob::new(piece).map_err(|e| format!("invalid pattern '{pattern}': {e}"))?;
            builder.add(glob);
        }
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| format!("invalid pattern: {e}"))
}

/// `512B`, `4.5K`, `1.0M`, `2.0G` — the `tree -h` size style.
fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes < KIB {
        format!("{bytes}B")
    } else if bytes < MIB {
        format!("{:.1}K", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1}M", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    }
}

/// Should this entry be left out of the listing?
fn skip_entry(name: &str, is_dir: bool, opts: &Options, globs: Option<&GlobSet>) -> bool {
    if !opts.all && name.starts_with('.') {
        return true;
    }
    if opts.dirs_only && !is_dir {
        return true;
    }
    globs.is_some_and(|set| set.is_match(name))
}

fn depth_ok(depth: usize, opts: &Options) -> bool {
    // `depth` is 1 for the target's own children.
    opts.level.is_none_or(|max| depth <= max)
}

fn sort_children(nodes: &mut Vec<Node>) {
    nodes.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for node in nodes {
        sort_children(&mut node.children);
    }
}

/// Drop directories that became empty (`-p`), bottom-up.
fn prune_children(nodes: &mut Vec<Node>) {
    for node in nodes.iter_mut() {
        if node.is_dir {
            prune_children(&mut node.children);
        }
    }
    nodes.retain(|node| !node.is_dir || !node.children.is_empty());
}

/// Build the children of `path` at `depth` by reading the directory directly.
/// Unreadable directories are silently skipped (and stay childless).
fn build_children(path: &Path, depth: usize, opts: &Options, globs: Option<&GlobSet>) -> Vec<Node> {
    if !depth_ok(depth, opts) {
        return Vec::new();
    }
    let Ok(read) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut nodes: Vec<Node> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let is_dir = meta.file_type().is_dir();
        if skip_entry(&name, is_dir, opts, globs) {
            continue;
        }
        let size = if is_dir { 0 } else { meta.len() };
        let mut node = Node::leaf(name, is_dir, size);
        if is_dir {
            node.children = build_children(&entry.path(), depth + 1, opts, globs);
        }
        nodes.push(node);
    }
    sort_children(&mut nodes);
    if opts.prune {
        prune_children(&mut nodes);
    }
    nodes
}

/// Path key for the walker index (lossy strings are good enough: they only
/// need to match each other).
fn key_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A node in the flat, index-linked tree built by the `--gitignore` walker.
struct FlatNode {
    name: String,
    is_dir: bool,
    size: u64,
    children: Vec<usize>,
}

/// Build the children of `root` with `ignore::WalkBuilder` so `.gitignore`
/// rules are honoured, then re-assemble them into an owned tree.
fn build_gitignore_children(root: &Path, opts: &Options, globs: Option<&GlobSet>) -> Vec<Node> {
    let root_key = key_of(root);
    let mut builder = WalkBuilder::new(root);
    builder
        // With `-a` we want dot-entries; otherwise the walker skips them and
        // does not descend into them.
        .hidden(!opts.all)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(false)
        .parents(true)
        .follow_links(false)
        // The walker counts the root as depth 0, so `-L N` maps to N.
        .max_depth(opts.level);

    let mut flat: Vec<FlatNode> = Vec::new();
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut skipped_dirs: HashSet<String> = HashSet::new();
    let mut top: Vec<usize> = Vec::new();

    for result in builder.build() {
        let Ok(entry) = result else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let Some(ft) = entry.file_type() else { continue };
        let is_dir = ft.is_dir();
        let path = entry.path();
        let parent_key = path.parent().map(key_of).unwrap_or_default();

        // Entries living inside a directory we dropped must go with it.
        if skipped_dirs.contains(&parent_key) {
            if is_dir {
                skipped_dirs.insert(key_of(path));
            }
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        if skip_entry(&name, is_dir, opts, globs) {
            if is_dir {
                skipped_dirs.insert(key_of(path));
            }
            continue;
        }

        let size = if is_dir {
            0
        } else {
            fs::symlink_metadata(path).map_or(0, |m| m.len())
        };
        let index = flat.len();
        flat.push(FlatNode {
            name,
            is_dir,
            size,
            children: Vec::new(),
        });
        if is_dir {
            by_path.insert(key_of(path), index);
        }

        if parent_key == root_key {
            top.push(index);
        } else if let Some(&parent) = by_path.get(&parent_key) {
            flat[parent].children.push(index);
        } else {
            // Only reachable when the parent was filtered away by .gitignore
            // while the walker still reported this entry; keep it at top level
            // rather than losing it.
            top.push(index);
        }
    }

    let mut children: Vec<Node> = top.iter().map(|index| materialize(&flat, *index)).collect();
    sort_children(&mut children);
    if opts.prune {
        prune_children(&mut children);
    }
    children
}

/// Convert the index-linked flat tree rooted at `index` into owned [`Node`]s.
fn materialize(flat: &[FlatNode], index: usize) -> Node {
    let node = &flat[index];
    let mut owned = Node::leaf(node.name.clone(), node.is_dir, node.size);
    for child in &node.children {
        owned.children.push(materialize(flat, *child));
    }
    owned
}

/// Render every node below the root, then the summary lines.
fn render_tree(children: &[Node], opts: &Options, out: &mut String, counts: &mut Counts) {
    let total = children.len();
    for (index, child) in children.iter().enumerate() {
        // Blank prefix at the root level.
        child.render("", index + 1 == total, opts, out, counts);
    }
}

fn run(opts: &Options) -> Result<u32, String> {
    let globs = build_globset(&opts.ignore_patterns)?;
    let root = if opts.path.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        opts.path.clone()
    };
    let meta = fs::symlink_metadata(&root).map_err(|e| io_message(&root, e))?;

    let mut out = String::new();
    let mut counts = Counts {
        dirs: 0,
        files: 0,
        bytes: 0,
    };

    if meta.is_dir() {
        counts.dirs += 1;
        let _ = writeln!(out, "{}", root.display());

        let children = if opts.gitignore {
            build_gitignore_children(&root, opts, globs.as_ref())
        } else {
            build_children(&root, 1, opts, globs.as_ref())
        };
        render_tree(&children, opts, &mut out, &mut counts);
    } else {
        // A plain-file target prints as its own header line.
        let size = meta.len();
        let _ = write!(out, "{}", root.display());
        if opts.human_readable {
            let _ = write!(out, "  [{}]", human_size(size));
            counts.bytes += size;
        }
        out.push('\n');
        counts.files += 1;
    }

    let _ = writeln!(
        out,
        "\n{} directories, {} files",
        counts.dirs, counts.files
    );
    if opts.human_readable {
        let _ = writeln!(out, "total {}", counts.bytes);
    }

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(out.as_bytes()).map_err(|e| e.to_string())?;
    handle.flush().map_err(|e| e.to_string())?;
    Ok(0)
}

fn io_message(path: &Path, e: io::Error) -> String {
    if e.kind() == io::ErrorKind::NotFound {
        format!("{}.  No such file or directory", path.display())
    } else {
        format!("{}: {e}", path.display())
    }
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(code) => code as i32,
            Err(e) => {
                let _ = writeln!(io::stderr(), "tree: {e}");
                2
            }
        },
        Err(Some(msg)) => {
            let _ = writeln!(io::stderr(), "tree: {msg}");
            2
        }
        Err(None) => 0,
    }
}
