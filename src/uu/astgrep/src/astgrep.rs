// RecurBusyBox - a multi-call binary of high-performance Rust tools.
//
// `astgrep`: structural (AST) code search over files/directories, a focused
// subset of ast-grep's `run` command. It is built directly on ast-grep's own
// engine crates (`ast-grep-core`, `ast-grep-language`), so patterns and
// rewrites behave like ast-grep's, while the recursive walker uses `ignore`
// (the same crate ripgrep ships) so `.gitignore` is honoured.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use ast_grep_core::ops::Op;
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::Pattern;
use ast_grep_language::{Language, LanguageExt, SupportLang};
use clap::{Arg, Command};
use ignore::WalkBuilder;

struct Options {
    pattern: String,
    paths: Vec<PathBuf>,
    lang: Option<SupportLang>,
    rewrite: Option<String>,
}

pub fn uu_app() -> Command {
    Command::new("astgrep")
        .version(uucore::crate_version!())
        .about("Structural (AST) code search over files or directories.")
        .arg(
            Arg::new("pattern")
                .short('p')
                .long("pattern")
                .value_name("PATTERN")
                .help("ast-grep structural pattern, e.g. 'console.log($A)'"),
        )
        .arg(
            Arg::new("lang")
                .short('l')
                .long("lang")
                .value_name("LANG")
                .help("force a language for all files (rust, typescript, python, ...)"),
        )
        .arg(
            Arg::new("rewrite")
                .short('r')
                .long("rewrite")
                .value_name("TEMPLATE")
                .help("rewrite matches using a meta-variable template"),
        )
        .arg(
            Arg::new("paths")
                .value_name("PATH")
                .num_args(0..)
                .help("files or directories to search (default: .)"),
        )

}

fn usage(what: &str) -> String {
    format!("{what} (usage: astgrep -p PATTERN [OPTIONS] [PATH ...])")
}

/// Parse the argv tail (without argv[0]) into [`Options`].
/// `Err(Some(msg))` is a usage error; `Err(None)` means the request was
/// already handled (`--help` / `--version`).
fn parse(argv: &[String]) -> Result<Options, Option<String>> {
    let mut explicit_pattern: Option<String> = None;
    let mut lang: Option<SupportLang> = None;
    let mut rewrite: Option<String> = None;
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
            "-h" | "--help" => {
                let _ = uu_app().print_help();
                return Err(None);
            }
            "-V" | "--version" => {
                let _ = writeln!(io::stdout(), "astgrep {}", uucore::crate_version!());
                return Err(None);
            }
            "-p" | "--pattern" => {
                let v = iter.next().ok_or_else(|| usage("--pattern needs a value"))?;
                explicit_pattern = Some(v.clone());
            }
            "-l" | "--lang" => {
                let v = iter.next().ok_or_else(|| usage("--lang needs a value"))?;
                lang = Some(
                    SupportLang::from_str(v)
                        .map_err(|_| format!("unsupported language: {v}"))?,
                );
            }
            "-r" | "--rewrite" => {
                let v = iter.next().ok_or_else(|| usage("--rewrite needs a value"))?;
                rewrite = Some(v.clone());
            }
            s if s.starts_with("-p") && s.len() > 2 => {
                explicit_pattern = Some(s[2..].to_string());
            }
            s if s.starts_with("-l") && s.len() > 2 => {
                lang = Some(
                    SupportLang::from_str(&s[2..])
                        .map_err(|_| format!("unsupported language: {}", &s[2..]))?,
                );
            }
            s if s.starts_with("-r") && s.len() > 2 => {
                rewrite = Some(s[2..].to_string());
            }
            s if s.starts_with('-') && s != "-" => {
                return Err(Some(s.to_string()));
            }
            _ => positionals.push(a.clone()),
        }
    }

    let pattern = if let Some(p) = explicit_pattern {
        p
    } else {
        if positionals.is_empty() {
            return Err(Some(usage("a search pattern is required")));
        }
        positionals.remove(0)
    };
    if pattern.is_empty() {
        return Err(Some(usage("a search pattern is required")));
    }

    Ok(Options {
        pattern,
        paths: positionals.into_iter().map(PathBuf::from).collect(),
        lang,
        rewrite,
    })
}

/// Compile the user pattern into the set of matching patterns to try for a
/// language. Normally this is a single [`Pattern`]; for a bare Rust `fn`
/// signature (parsed by tree-sitter as `function_signature_item`) we also add
/// function-definition variants, so that e.g. `fn uumain($$$)` matches real
/// definitions carrying a visibility modifier, return type and body.
fn compile_patterns(
    src: &str,
    lang: SupportLang,
) -> Result<Vec<Pattern>, ast_grep_core::PatternError> {
    let mut patterns = vec![Pattern::try_new(src, lang)?];
    for variant in signature_variants(src, lang) {
        if let Ok(p) = Pattern::try_new(&variant, lang) {
            patterns.push(p);
        }
    }
    Ok(patterns)
}

fn signature_variants(src: &str, lang: SupportLang) -> Vec<String> {
    if lang != SupportLang::Rust {
        return Vec::new();
    }
    let expanded = lang.pre_process_pattern(src);
    let Ok(doc) = StrDoc::try_new(expanded.as_ref(), lang) else {
        return Vec::new();
    };
    let root = doc.tree.root_node();
    let Some(first) = root.named_child(0) else {
        return Vec::new();
    };
    if first.kind() != "function_signature_item" {
        return Vec::new();
    }

    let base = src.trim().trim_end_matches(';').trim_end();
    let Some(close) = base.rfind(')') else {
        return Vec::new();
    };
    let head = &base[..=close];
    let tail = &base[close + 1..];
    let bodies = if base.contains("->") {
        vec![format!("{head}{tail} {{ $$$ }}")]
    } else {
        vec![
            format!("{head}{tail} {{ $$$ }}"),
            format!("{head} -> $__RET{tail} {{ $$$ }}"),
        ]
    };

    let mut variants = Vec::new();
    for body in bodies {
        variants.push(body.clone());
        if !base.trim_start().starts_with("pub") {
            variants.push(format!("pub {body}"));
        }
    }
    variants
}

/// A compiled pattern cache, keyed by the language it was built for.
struct Patterns {
    pattern_src: String,
    cache: HashMap<SupportLang, Option<Vec<Pattern>>>,
    last_error: Option<String>,
}

impl Patterns {
    fn new(src: &str) -> Self {
        Self {
            pattern_src: src.to_string(),
            cache: HashMap::new(),
            last_error: None,
        }
    }

    /// Returns the compiled patterns for `lang`, or `None` when the pattern is
    /// not valid syntax for that language (which is expected while walking a
    /// mixed-language tree).
    fn get(&mut self, lang: SupportLang) -> Option<&[Pattern]> {
        if !self.cache.contains_key(&lang) {
            let compiled = if let Ok(p) = compile_patterns(&self.pattern_src, lang) {
                Some(p)
            } else {
                let src = self.pattern_src.clone();
                self.last_error = Some(format!("pattern is not valid {lang} syntax: {src}"));
                None
            };
            self.cache.insert(lang, compiled);
        }
        self.cache.get(&lang).and_then(|p| p.as_deref())
    }
}

fn run(opts: &Options) -> Result<u32, String> {
    let targets = if opts.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        opts.paths.clone()
    };

    let mut patterns = Patterns::new(&opts.pattern);
    let out = io::stdout();
    let mut out = out.lock();
    let mut matched_any = false;
    let mut compiled_any = false;

    for target in &targets {
        if target.is_file() {
            if process_file(target, opts, &mut patterns, &mut out, &mut matched_any)? {
                compiled_any = true;
            }
            continue;
        }

        let root = if target.as_os_str().is_empty() {
            Path::new(".")
        } else {
            target.as_path()
        };
        let mut builder = WalkBuilder::new(root);
        builder.hidden(true);
        for entry in builder.build() {
            let Ok(entry) = entry else { continue };
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                continue;
            }
            if process_file(entry.path(), opts, &mut patterns, &mut out, &mut matched_any)? {
                compiled_any = true;
            }
        }
    }

    out.flush().map_err(|e| e.to_string())?;

    if !compiled_any && let Some(err) = patterns.last_error {
        return Err(err);
    }
    Ok(u32::from(!matched_any))
}

/// Search a single file. Returns whether the pattern compiled for its language.
fn process_file(
    path: &Path,
    opts: &Options,
    patterns: &mut Patterns,
    out: &mut impl io::Write,
    matched_any: &mut bool,
) -> Result<bool, String> {
    let lang = match opts.lang {
        Some(l) => l,
        None => match SupportLang::from_path(path) {
            Some(l) => l,
            None => return Ok(false),
        },
    };

    let Some(pattern_set) = patterns.get(lang) else {
        return Ok(false);
    };
    let matcher = Op::any(pattern_set.iter().cloned());

    let Ok(src) = fs::read_to_string(path) else {
        return Ok(false);
    };

    let grep = lang.ast_grep(&src);
    let root = grep.root();

    if let Some(template) = &opts.rewrite {
        let mut edits: Vec<(usize, usize, Vec<u8>)> = Vec::new();
        let mut last_end = 0usize;
        for nm in root.find_all(matcher) {
            let edit = nm.replace_by(template.as_str());
            if edit.position < last_end {
                // Skip overlapping/nested edits from an outer match.
                continue;
            }
            last_end = edit.position + edit.deleted_length;
            edits.push((edit.position, edit.deleted_length, edit.inserted_text));
        }
        if edits.is_empty() {
            return Ok(true);
        }

        let mut rewritten = src.clone();
        for (pos, del, ins) in edits.iter().rev() {
            let replacement = String::from_utf8_lossy(ins);
            rewritten.replace_range(*pos..*pos + *del, &replacement);
        }

        let old_lines: Vec<&str> = src.lines().collect();
        let new_lines: Vec<&str> = rewritten.lines().collect();
        for (i, new_line) in new_lines.iter().enumerate() {
            if old_lines.get(i) != Some(new_line) {
                *matched_any = true;
                writeln!(out, "{}:{}: {new_line}", path.display(), i + 1)
                    .map_err(|e| e.to_string())?;
            }
        }
        return Ok(true);
    }

    for nm in root.find_all(matcher) {
        let node = nm.get_node();
        let pos = node.start_pos();
        let line = pos.line() + 1;
        let col = pos.column(node) + 1;
        let text = node.text();
        let snippet = text.lines().next().unwrap_or("").trim_end();
        *matched_any = true;
        writeln!(out, "{}:{line}:{col}: {snippet}", path.display())
            .map_err(|e| e.to_string())?;
    }

    Ok(true)
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(code) => code as i32,
            Err(e) => {
                let _ = writeln!(io::stderr(), "astgrep: {e}");
                2
            }
        },
        Err(Some(msg)) => {
            let _ = writeln!(io::stderr(), "astgrep: {msg}");
            2
        }
        Err(None) => 0,
    }
}
