// This file is part of RecurBusyBox.
//
// `fd`: recursively find filesystem entries whose file name matches a regexp.
// A focused subset of the `fd` tool, built on the `ignore` crate's
// [`WalkBuilder`] (the same recursive walker ripgrep ships) so `.gitignore`
// files are honoured by default.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, Command};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::RegexBuilder;

struct Options {
    pattern: String,
    paths: Vec<PathBuf>,
    hidden: bool,
    no_ignore: bool,
    extensions: Vec<String>,
    types: Vec<char>,
    max_depth: Option<usize>,
    globs: Vec<String>,
    ignore_case: bool,
    absolute_path: bool,
}

pub fn uu_app() -> Command {
    Command::new("fd")
        .version(uucore::crate_version!())
        .about("Recursively find entries by name (fd subset).")
        .arg(Arg::new("pattern").value_name("PATTERN"))
        .arg(Arg::new("paths").value_name("PATH").num_args(0..))
        .arg(
            Arg::new("hidden")
                .short('H')
                .long("hidden")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("no-ignore")
                .short('I')
                .long("no-ignore")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("extension")
                .short('e')
                .long("extension")
                .value_name("EXT")
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("type")
                .short('t')
                .long("type")
                .value_name("f|d")
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("max-depth")
                .short('d')
                .long("max-depth")
                .value_name("N"),
        )
        .arg(
            Arg::new("glob")
                .short('g')
                .long("glob")
                .value_name("GLOB")
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("ignore-case")
                .short('i')
                .long("ignore-case")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("absolute-path")
                .short('a')
                .long("absolute-path")
                .action(ArgAction::SetTrue),
        )
}

/// Parse the argv tail (without argv[0]) into [`Options`].
/// `Err(Some(msg))` is a usage error; `Err(None)` means the app already
/// handled the request (`--help` / `--version`).
fn parse(argv: &[String]) -> Result<Options, Option<String>> {
    let mut opts = Options {
        pattern: String::new(),
        paths: Vec::new(),
        hidden: false,
        no_ignore: false,
        extensions: Vec::new(),
        types: Vec::new(),
        max_depth: None,
        globs: Vec::new(),
        ignore_case: false,
        absolute_path: false,
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
            "-H" | "--hidden" => opts.hidden = true,
            "-I" | "--no-ignore" => opts.no_ignore = true,
            "-i" | "--ignore-case" => opts.ignore_case = true,
            "-a" | "--absolute-path" => opts.absolute_path = true,
            "-e" | "--extension" => {
                let v = inline
                    .or_else(|| iter.next().cloned())
                    .ok_or_else(|| usage("--extension needs a value"))?;
                opts.extensions.push(v);
            }
            "-t" | "--type" => {
                let v = inline
                    .or_else(|| iter.next().cloned())
                    .ok_or_else(|| usage("--type needs a value"))?;
                for c in v.chars() {
                    if c != 'f' && c != 'd' {
                        return Err(Some(format!("invalid type '{v}' (expected f or d)")));
                    }
                    opts.types.push(c);
                }
            }
            "-d" | "--max-depth" => {
                let v = inline
                    .or_else(|| iter.next().cloned())
                    .ok_or_else(|| usage("--max-depth needs a value"))?;
                let n = v
                    .parse::<usize>()
                    .map_err(|_| format!("invalid max-depth '{v}'"))?;
                opts.max_depth = Some(n);
            }
            "-g" | "--glob" => {
                let v = inline
                    .or_else(|| iter.next().cloned())
                    .ok_or_else(|| usage("--glob needs a value"))?;
                opts.globs.push(v);
            }
            "-h" | "--help" => {
                let _ = uu_app().print_help();
                return Err(None);
            }
            "-V" | "--version" => {
                let _ = writeln!(io::stdout(), "fd {}", uucore::crate_version!());
                return Err(None);
            }
            // Attached short values: -eEXT, -tf, -dN, -gGLOB.
            s if s.starts_with("-e") && s.len() > 2 => opts.extensions.push(s[2..].to_string()),
            s if s.starts_with("-t") && s.len() > 2 => {
                for c in s[2..].chars() {
                    if c != 'f' && c != 'd' {
                        return Err(Some(format!(
                            "invalid type '{}' (expected f or d)",
                            &s[2..]
                        )));
                    }
                    opts.types.push(c);
                }
            }
            s if s.starts_with("-d") && s.len() > 2 => {
                let v = &s[2..];
                let n = v
                    .parse::<usize>()
                    .map_err(|_| format!("invalid max-depth '{v}'"))?;
                opts.max_depth = Some(n);
            }
            s if s.starts_with("-g") && s.len() > 2 => opts.globs.push(s[2..].to_string()),
            s if s.starts_with('-') && s != "-" => {
                return Err(Some(s.to_string()));
            }
            _ => positionals.push(a.clone()),
        }
    }

    let mut it = positionals.into_iter();
    if let Some(p) = it.next() {
        opts.pattern = p;
    }
    // Remaining positionals are search paths; default to `.` when absent.
    opts.paths = it.map(PathBuf::from).collect();
    if opts.paths.is_empty() {
        opts.paths.push(PathBuf::from("."));
    }
    Ok(opts)
}

fn usage(what: &str) -> String {
    format!("{what} (usage: fd [OPTIONS] [PATTERN] [PATH ...])")
}

/// True when `path` has one of the requested extensions.
fn extension_ok(path: &Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => extensions.iter().any(|w| w == ext),
        None => false,
    }
}

/// True when the entry's kind is allowed by the requested `-t` types.
fn type_ok(is_dir: bool, types: &[char]) -> bool {
    if types.is_empty() {
        return true;
    }
    types.iter().any(|t| (*t == 'd') == is_dir)
}

fn absolute(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
        }
    })
}

fn run(opts: &Options) -> Result<u32, String> {
    let matcher = RegexBuilder::new(&opts.pattern)
        .case_insensitive(opts.ignore_case)
        .build()
        .map_err(|e| format!("invalid pattern: {e}"))?;

    let out = io::stdout();
    let mut out = out.lock();
    let mut matched_any = false;

    for target in &opts.paths {
        // A plain file target is tested directly (the walker would also yield
        // it at depth 0, so this avoids any special-casing below).
        if target.is_file() {
            let is_dir = false;
            if type_ok(is_dir, &opts.types)
                && extension_ok(target, &opts.extensions)
                && name_matches(&matcher, target)
            {
                matched_any = true;
                print_path(target, opts, &mut out);
            }
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
        if let Some(d) = opts.max_depth {
            builder.max_depth(Some(d));
        }
        if !opts.globs.is_empty() {
            let mut ov = OverrideBuilder::new(root);
            for g in &opts.globs {
                ov.add(g).map_err(|e| format!("invalid glob '{g}': {e}"))?;
            }
            let ov = ov.build().map_err(|e| e.to_string())?;
            builder.overrides(ov);
        }

        for entry in builder.build() {
            let Ok(entry) = entry else {
                continue;
            };
            let Some(ft) = entry.file_type() else {
                continue;
            };
            // Never print the root directory itself.
            if entry.depth() == 0 && ft.is_dir() {
                continue;
            }
            let is_dir = ft.is_dir();
            if !type_ok(is_dir, &opts.types) {
                continue;
            }
            if !extension_ok(entry.path(), &opts.extensions) {
                continue;
            }
            if !name_matches(&matcher, entry.path()) {
                continue;
            }
            matched_any = true;
            print_path(entry.path(), opts, &mut out);
        }
    }

    out.flush().map_err(|e| e.to_string())?;
    Ok(u32::from(!matched_any))
}

/// Match the regexp against just the final component of `path`.
fn name_matches(matcher: &regex::Regex, path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    matcher.is_match(&name)
}

fn print_path(path: &Path, opts: &Options, out: &mut impl io::Write) {
    if opts.absolute_path {
        let _ = writeln!(out, "{}", absolute(path).display());
    } else {
        let _ = writeln!(out, "{}", path.display());
    }
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(code) => code as i32,
            Err(e) => {
                let _ = writeln!(io::stderr(), "fd: {e}");
                2
            }
        },
        Err(Some(msg)) => {
            let _ = writeln!(io::stderr(), "fd: {msg}");
            2
        }
        Err(None) => 0,
    }
}
