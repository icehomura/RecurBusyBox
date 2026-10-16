// RecurBusyBox - a multi-call binary of high-performance Rust tools.
//
// `rg`: recursively search the current tree for a regexp. A focused subset of
// ripgrep, built directly on ripgrep's own crates (`grep-regex`,
// `grep-searcher`) and `ignore` (the recursive walker that honours
// `.gitignore`), so the engine is the same one ripgrep ships — only the CLI
// surface is smaller.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, Command};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::UTF8};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

struct Options {
    pattern: String,
    paths: Vec<PathBuf>,
    ignore_case: bool,
    line_number: bool,
    files_with_matches: bool,
    count: bool,
    fixed_strings: bool,
    hidden: bool,
    no_ignore: bool,
    globs: Vec<String>,
}

pub fn uu_app() -> Command {
    Command::new("rg")
        .version(uucore::crate_version!())
        .about("Recursively search for a regexp (ripgrep subset).")
        .arg(Arg::new("pattern").value_name("PATTERN"))
        .arg(Arg::new("paths").value_name("PATH").num_args(0..))
        .arg(
            Arg::new("regexp")
                .short('e')
                .long("regexp")
                .value_name("PATTERN")
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("ignore-case")
                .short('i')
                .long("ignore-case")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("line-number")
                .short('n')
                .long("line-number")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("no-line-number")
                .short('N')
                .long("no-line-number")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("files-with-matches")
                .short('l')
                .long("files-with-matches")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("count")
                .short('c')
                .long("count")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("fixed-strings")
                .short('F')
                .long("fixed-strings")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("hidden")
                .long("hidden")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("no-ignore")
                .long("no-ignore")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("glob")
                .short('g')
                .long("glob")
                .value_name("GLOB")
                .action(ArgAction::Append),
        )
}

/// Parse the argv tail (without argv[0]) into [`Options`].
/// `Err(Some(msg))` is a usage error; `Err(None)` means the app already
/// handled the request (`--help` / `--version`).
fn parse(argv: &[String]) -> Result<Options, Option<String>> {
    let mut opts = Options {
        pattern: String::new(),
        paths: Vec::new(),
        ignore_case: false,
        line_number: true,
        files_with_matches: false,
        count: false,
        fixed_strings: false,
        hidden: false,
        no_ignore: false,
        globs: Vec::new(),
    };
    let mut explicit_pattern: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut no_more_flags = false;
    let mut iter = argv.iter();
    while let Some(a) = iter.next() {
        if no_more_flags {
            positionals.push(a.clone());
            continue;
        }
        match a.as_str() {
            "--" => no_more_flags = true,
            "-i" | "--ignore-case" => opts.ignore_case = true,
            "-n" | "--line-number" => opts.line_number = true,
            "-N" | "--no-line-number" => opts.line_number = false,
            "-l" | "--files-with-matches" => opts.files_with_matches = true,
            "-c" | "--count" => opts.count = true,
            "-F" | "--fixed-strings" => opts.fixed_strings = true,
            "--hidden" => opts.hidden = true,
            "--no-ignore" => opts.no_ignore = true,
            "-e" | "--regexp" => {
                let v = iter.next().ok_or_else(|| usage("--regexp needs a value"))?;
                explicit_pattern = Some(v.clone());
            }
            "-g" | "--glob" => {
                let v = iter.next().ok_or_else(|| usage("--glob needs a value"))?;
                opts.globs.push(v.clone());
            }
            "-h" | "--help" => {
                let _ = uu_app().print_help();
                return Err(None);
            }
            "-V" | "--version" => {
                let _ = writeln!(io::stdout(), "rg {}", uucore::crate_version!());
                return Err(None);
            }
            s if s.starts_with("-g") && s.len() > 2 => opts.globs.push(s[2..].to_string()),
            s if s.starts_with("-e") && s.len() > 2 => explicit_pattern = Some(s[2..].to_string()),
            s if s.starts_with('-') && s != "-" => {
                return Err(Some(s.to_string()));
            }
            _ => positionals.push(a.clone()),
        }
    }

    match explicit_pattern {
        Some(p) => {
            opts.pattern = p;
            opts.paths = positionals.into_iter().map(PathBuf::from).collect();
        }
        None => {
            let mut it = positionals.into_iter();
            opts.pattern = it
                .next()
                .ok_or_else(|| usage("a search pattern is required"))?;
            opts.paths = it.map(PathBuf::from).collect();
        }
    }
    Ok(opts)
}

fn usage(what: &str) -> String {
    format!("{what} (usage: rg [OPTIONS] PATTERN [PATH ...])")
}

fn run(opts: &Options) -> Result<u32, String> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.ignore_case)
        .fixed_strings(opts.fixed_strings)
        .build(&opts.pattern)
        .map_err(|e| format!("invalid pattern: {e}"))?;

    let targets = if opts.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        opts.paths.clone()
    };

    let out = io::stdout();
    let mut out = out.lock();
    let mut matched_any = false;

    for target in &targets {
        if target.is_file() {
            search_file(&matcher, target, opts, targets.len() == 1, &mut out, &mut matched_any)?;
            continue;
        }

        let root = if target.as_os_str().is_empty() {
            Path::new(".")
        } else {
            target.as_path()
        };
        let mut builder = WalkBuilder::new(root);
        builder.hidden(!opts.hidden);
        builder.git_ignore(!opts.no_ignore);
        builder.git_global(!opts.no_ignore);
        builder.ignore(!opts.no_ignore);
        builder.parents(!opts.no_ignore);
        if !opts.globs.is_empty() {
            let mut ov = OverrideBuilder::new(root);
            for g in &opts.globs {
                ov.add(g).map_err(|e| format!("invalid glob '{g}': {e}"))?;
            }
            let ov = ov.build().map_err(|e| e.to_string())?;
            builder.overrides(ov);
        }

        for entry in builder.build() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                continue;
            }
            search_file(&matcher, entry.path(), opts, false, &mut out, &mut matched_any)?;
        }
    }

    out.flush().map_err(|e| e.to_string())?;
    Ok(if matched_any { 0 } else { 1 })
}

fn search_file(
    matcher: &grep_regex::RegexMatcher,
    path: &Path,
    opts: &Options,
    single_file: bool,
    out: &mut impl io::Write,
    matched_any: &mut bool,
) -> Result<(), String> {
    let mut searcher = SearcherBuilder::new()
        .line_number(opts.line_number)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();

    if opts.files_with_matches {
        let mut hit = false;
        searcher
            .search_path(
                matcher,
                path,
                UTF8(|_, _| {
                    hit = true;
                    Ok(false)
                }),
            )
            .map_err(|e| e.to_string())?;
        if hit {
            *matched_any = true;
            let _ = writeln!(out, "{}", path.display());
        }
        return Ok(());
    }

    if opts.count {
        let mut n: u64 = 0;
        searcher
            .search_path(
                matcher,
                path,
                UTF8(|_, _| {
                    n += 1;
                    Ok(true)
                }),
            )
            .map_err(|e| e.to_string())?;
        if n > 0 {
            *matched_any = true;
            let _ = writeln!(out, "{}:{}", path.display(), n);
        }
        return Ok(());
    }

    let show_path = !single_file;
    searcher
        .search_path(
            matcher,
            path,
            UTF8(|lnum, line| {
                *matched_any = true;
                let text = line.strip_suffix('\n').unwrap_or(line);
                let text = text.strip_suffix('\r').unwrap_or(text);
                if show_path {
                    let _ = writeln!(out, "{}:{}:{}", path.display(), lnum, text);
                } else if opts.line_number {
                    let _ = writeln!(out, "{lnum}:{text}");
                } else {
                    let _ = writeln!(out, "{text}");
                }
                Ok(true)
            }),
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(code) => code as i32,
            Err(e) => {
                let _ = writeln!(io::stderr(), "rg: {e}");
                2
            }
        },
        Err(Some(msg)) => {
            let _ = writeln!(io::stderr(), "rg: {msg}");
            2
        }
        Err(None) => 0,
    }
}
