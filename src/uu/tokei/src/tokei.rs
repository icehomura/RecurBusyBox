// This file is part of RecurBusyBox.
//
// `tokei`: count lines of code by language. A thin CLI over the `tokei`
// library's [`Languages`] API, which walks the tree and classifies each file
// with the languages tokei knows about. The output is a plain, aligned table
// (or a per-file listing with `-f`).

use std::io::{self, Write as _};
use std::path::PathBuf;

use clap::{Arg, ArgAction, Command};

/// The metric a `--sort` request orders rows by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Files,
    Lines,
    Code,
    Comments,
    Blanks,
}

struct Options {
    paths: Vec<PathBuf>,
    excludes: Vec<String>,
    files: bool,
    sort: Option<SortKey>,
}

pub fn uu_app() -> Command {
    Command::new("tokei")
        .version(uucore::crate_version!())
        .about("Count lines of code by language.")
        .arg(Arg::new("paths").value_name("PATH").num_args(0..))
        .arg(
            Arg::new("exclude")
                .short('e')
                .long("exclude")
                .value_name("GLOB")
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("files")
                .short('f')
                .long("files")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("sort")
                .short('s')
                .long("sort")
                .value_name("files|lines|code|comments|blanks"),
        )
}

/// Parse the argv tail (without argv[0]) into [`Options`].
/// `Err(Some(msg))` is a usage error; `Err(None)` means the app already
/// handled the request (`--help` / `--version`).
fn parse(argv: &[String]) -> Result<Options, Option<String>> {
    let mut opts = Options {
        paths: Vec::new(),
        excludes: Vec::new(),
        files: false,
        sort: None,
    };
    let mut positionals: Vec<String> = Vec::new();
    let mut no_more_flags = false;
    let mut iter = argv.iter();
    while let Some(a) = iter.next() {
        if no_more_flags {
            positionals.push(a.clone());
            continue;
        }
        // Support `--flag=value` for long options that take a value.
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        match name {
            "--" => no_more_flags = true,
            "-e" | "--exclude" => {
                let v = inline
                    .or_else(|| iter.next().cloned())
                    .ok_or_else(|| usage("--exclude needs a value"))?;
                opts.excludes.push(v);
            }
            "-s" | "--sort" => {
                let v = inline
                    .or_else(|| iter.next().cloned())
                    .ok_or_else(|| usage("--sort needs a value"))?;
                opts.sort = Some(parse_sort(&v)?);
            }
            "-f" | "--files" => opts.files = true,
            "-h" | "--help" => {
                let _ = uu_app().print_help();
                return Err(None);
            }
            "-V" | "--version" => {
                let _ = writeln!(io::stdout(), "tokei {}", uucore::crate_version!());
                return Err(None);
            }
            // Attached short values: -eGLOB, -sKEY.
            s if s.starts_with("-e") && s.len() > 2 => opts.excludes.push(s[2..].to_string()),
            s if s.starts_with("-s") && s.len() > 2 => opts.sort = Some(parse_sort(&s[2..])?),
            s if s.starts_with('-') && s != "-" => {
                return Err(Some(s.to_string()));
            }
            _ => positionals.push(a.clone()),
        }
    }

    opts.paths = positionals.into_iter().map(PathBuf::from).collect();
    if opts.paths.is_empty() {
        opts.paths.push(PathBuf::from("."));
    }
    Ok(opts)
}

fn parse_sort(v: &str) -> Result<SortKey, String> {
    match v {
        "files" => Ok(SortKey::Files),
        "lines" => Ok(SortKey::Lines),
        "code" => Ok(SortKey::Code),
        "comments" => Ok(SortKey::Comments),
        "blanks" => Ok(SortKey::Blanks),
        other => Err(usage(&format!(
            "invalid sort key '{other}' (expected files, lines, code, comments, or blanks)"
        ))),
    }
}

fn usage(what: &str) -> String {
    format!("{what} (usage: tokei [OPTIONS] [PATH ...])")
}

/// One language's aggregate row in the summary table.
struct Row {
    name: String,
    files: usize,
    lines: usize,
    code: usize,
    comments: usize,
    blanks: usize,
    inaccurate: bool,
}

fn metric(row: &Row, key: SortKey) -> usize {
    match key {
        SortKey::Files => row.files,
        SortKey::Lines => row.lines,
        SortKey::Code => row.code,
        SortKey::Comments => row.comments,
        SortKey::Blanks => row.blanks,
    }
}

fn run(opts: &Options) -> Result<u32, String> {
    let mut languages = tokei::Languages::new();
    let config = tokei::Config::default();
    let excludes: Vec<&str> = opts.excludes.iter().map(String::as_str).collect();
    languages.get_statistics(&opts.paths[..], &excludes, &config);

    let out = io::stdout();
    let mut out = out.lock();
    if opts.files {
        print_files(&languages, opts, &mut out);
    } else {
        print_summary(&languages, opts, &mut out);
    }
    out.flush().map_err(|e| e.to_string())?;
    Ok(0)
}

fn print_summary(languages: &tokei::Languages, opts: &Options, out: &mut impl io::Write) {
    let mut rows: Vec<Row> = languages
        .iter()
        .map(|(ty, lang)| {
            let stats = lang.summarise();
            Row {
                name: ty.name().to_string(),
                files: lang.reports.len(),
                lines: stats.lines(),
                code: stats.code,
                comments: stats.comments,
                blanks: stats.blanks,
                inaccurate: lang.inaccurate,
            }
        })
        .collect();

    if let Some(key) = opts.sort {
        rows.sort_by(|a, b| metric(b, key).cmp(&metric(a, key)));
    }

    let name_w = rows
        .iter()
        .map(|r| r.name.len() + usize::from(r.inaccurate))
        .chain(["Language".len(), "Total".len()])
        .max()
        .unwrap_or(8);
    let num_w = |f: fn(&Row) -> usize| {
        rows.iter()
            .map(|r| f(r).to_string().len())
            .chain([1])
            .max()
            .unwrap_or(1)
            .max(6)
    };
    let files_w = num_w(|r| r.files);
    let lines_w = num_w(|r| r.lines);
    let code_w = num_w(|r| r.code);
    let comments_w = num_w(|r| r.comments);
    let blanks_w = num_w(|r| r.blanks);

    let _ = writeln!(
        out,
        "{:<name_w$}  {:>files_w$}  {:>lines_w$}  {:>code_w$}  {:>comments_w$}  {:>blanks_w$}",
        "Language", "Files", "Lines", "Code", "Comments", "Blanks",
    );
    let sep = "-".repeat(name_w + files_w + lines_w + code_w + comments_w + blanks_w + 10);
    let _ = writeln!(out, "{sep}");

    let mut total = Row {
        name: "Total".to_string(),
        files: 0,
        lines: 0,
        code: 0,
        comments: 0,
        blanks: 0,
        inaccurate: false,
    };
    for row in &rows {
        let mark = if row.inaccurate { "!" } else { "" };
        let _ = writeln!(
            out,
            "{:<name_w$}  {:>files_w$}  {:>lines_w$}  {:>code_w$}  {:>comments_w$}  {:>blanks_w$}",
            format!("{}{mark}", row.name),
            row.files,
            row.lines,
            row.code,
            row.comments,
            row.blanks,
        );
        total.files += row.files;
        total.lines += row.lines;
        total.code += row.code;
        total.comments += row.comments;
        total.blanks += row.blanks;
    }

    let _ = writeln!(
        out,
        "{:<name_w$}  {:>files_w$}  {:>lines_w$}  {:>code_w$}  {:>comments_w$}  {:>blanks_w$}",
        total.name, total.files, total.lines, total.code, total.comments, total.blanks,
    );
}

fn print_files(languages: &tokei::Languages, opts: &Options, out: &mut impl io::Write) {
    let mut rows: Vec<(usize, String)> = Vec::new();
    for (_, lang) in languages.iter() {
        for report in &lang.reports {
            rows.push((report.stats.lines(), report.name.display().to_string()));
        }
    }

    match opts.sort {
        Some(SortKey::Files) => rows.sort_by(|a, b| a.1.cmp(&b.1)),
        Some(_) => rows.sort_by(|a, b| b.0.cmp(&a.0)),
        None => {}
    }

    let lines_w = rows
        .iter()
        .map(|(n, _)| n.to_string().len())
        .chain(["Lines".len()])
        .max()
        .unwrap_or(5);
    let _ = writeln!(out, "{:>lines_w$}  File", "Lines");
    for (lines, path) in &rows {
        let _ = writeln!(out, "{lines:>lines_w$}  {path}");
    }
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(code) => code as i32,
            Err(e) => {
                let _ = writeln!(io::stderr(), "tokei: {e}");
                2
            }
        },
        Err(Some(msg)) => {
            let _ = writeln!(io::stderr(), "tokei: {msg}");
            2
        }
        Err(None) => 0,
    }
}
