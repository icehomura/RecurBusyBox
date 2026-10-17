// RecurBusyBox - a multi-call binary of high-performance Rust tools.
//
// `zip`: create ZIP archives. A focused subset of Info-ZIP's `zip` (see also
// the `extract` applet, which unpacks them). Built on the pure-Rust `zip`
// crate — no C toolchain.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, Command};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Parsed command line.
struct Options {
    archive: PathBuf,
    items: Vec<PathBuf>,
    recursive: bool,
    quiet: bool,
    force: bool,
    store: bool,
    excludes: Vec<String>,
}

pub fn uu_app() -> Command {
    Command::new("zip")
        .version(uucore::crate_version!())
        .about("Create ZIP archives.")
        .arg(Arg::new("archive").value_name("ARCHIVE.zip"))
        .arg(Arg::new("items").value_name("ITEM").num_args(0..))
        .arg(
            Arg::new("recursive")
                .short('r')
                .long("recursive")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("force")
                .short('f')
                .long("force")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("store")
                .long("store")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("exclude")
                .short('x')
                .long("exclude")
                .value_name("GLOB")
                .action(ArgAction::Append),
        )
}

/// Parse the argv tail (without argv[0]).
fn parse(argv: &[String]) -> Result<Options, String> {
    let mut opts = Options {
        archive: PathBuf::new(),
        items: Vec::new(),
        recursive: false,
        quiet: false,
        force: false,
        store: false,
        excludes: Vec::new(),
    };
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
            "-r" | "--recursive" => opts.recursive = true,
            "-q" | "--quiet" => opts.quiet = true,
            "-f" | "--force" => opts.force = true,
            "--store" => opts.store = true,
            "-x" | "--exclude" => {
                let v = iter.next().ok_or("option '-x' needs a value")?;
                opts.excludes.push(v.clone());
            }
            s if s.starts_with("-x") && s.len() > 2 => opts.excludes.push(s[2..].to_string()),
            "-h" | "--help" => {
                let _ = uu_app().print_help();
                return Err(String::new());
            }
            "-V" | "--version" => {
                let _ = writeln!(io::stdout(), "zip {}", uucore::crate_version!());
                return Err(String::new());
            }
            s if s.starts_with('-') && s != "-" => return Err(format!("unrecognized option '{s}'")),
            _ => positionals.push(a.clone()),
        }
    }
    let mut it = positionals.into_iter();
    opts.archive = PathBuf::from(it.next().ok_or("an archive name is required")?);
    opts.items = it.map(PathBuf::from).collect();
    Ok(opts)
}

/// A simple shell-glob matcher supporting `*`, `?` and `[...]`-free literals.
/// (Kept dependency-free; `-x` patterns are almost always bare `*.ext`.)
fn glob_matches(pattern: &str, text: &str) -> bool {
    // Split on '*' and match the literal chunks in order.
    let mut rest = text;
    let mut first = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            first = false;
            continue;
        }
        if first {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
        } else if let Some(idx) = rest.find(part) {
            rest = &rest[idx + part.len()..];
        } else {
            return false;
        }
        first = false;
    }
    // A trailing '*' matches anything; otherwise the whole text must be consumed.
    pattern.ends_with('*') || rest.is_empty()
}

fn excluded(opts: &Options, name: &str) -> bool {
    opts.excludes.iter().any(|p| glob_matches(p, name))
}

/// Convert a path to the forward-slash form ZIP stores.
fn zip_name(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Recursively add `path` under `prefix` (the archive-relative parent).
fn add_path(
    writer: &mut ZipWriter<File>,
    opts: &Options,
    path: &Path,
    prefix: &str,
    options: SimpleFileOptions,
) -> Result<u64, String> {
    let name = if prefix.is_empty() {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("cannot derive a name for {}", path.display()))?
    } else {
        format!("{prefix}/{}", path.file_name().unwrap().to_string_lossy())
    };
    let name = zip_name(Path::new(&name));

    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;

    if meta.is_dir() {
        if excluded(opts, &format!("{name}/")) || excluded(opts, &name) {
            return Ok(0);
        }
        if !opts.quiet {
            println!("   adding: {name}/");
        }
        writer
            .add_directory(format!("{name}/"), options)
            .map_err(|e| e.to_string())?;
        let mut count = 0;
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();
        for entry in entries {
            count += add_path(writer, opts, &entry, &name, options)?;
        }
        Ok(count)
    } else {
        if excluded(opts, &name) {
            return Ok(0);
        }
        let mut data = Vec::new();
        File::open(path)
            .and_then(|mut f| f.read_to_end(&mut data))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if !opts.quiet {
            println!("  adding: {name} ({} bytes)", data.len());
        }
        writer
            .start_file(name, options)
            .map_err(|e| e.to_string())?;
        writer.write_all(&data).map_err(|e| e.to_string())?;
        Ok(1)
    }
}

fn run(opts: &Options) -> Result<i32, String> {
    if opts.archive.exists() && !opts.force {
        return Err(format!(
            "{} already exists (use -f to overwrite)",
            opts.archive.display()
        ));
    }

    let method = if opts.store {
        CompressionMethod::Stored
    } else {
        CompressionMethod::Deflated
    };
    let file = File::create(&opts.archive).map_err(|e| format!("{}: {e}", opts.archive.display()))?;
    let mut writer = ZipWriter::new(file);

    let mut count = 0u64;
    for item in &opts.items {
        if item.is_dir() && !opts.recursive {
            return Err(format!(
                "{} is a directory (use -r to recurse)",
                item.display()
            ));
        }
        // Store paths relative to the item's parent, so `zip out.zip src/x`
        // yields entries starting at `src/x/...`.
        let prefix = String::new();
        let options = SimpleFileOptions::default()
            .compression_method(method)
            .unix_permissions(0o644);
        count += add_path(&mut writer, opts, item, &prefix, options)?;
    }

    writer.finish().map_err(|e| e.to_string())?;
    if !opts.quiet {
        println!("  added {count} file(s) to {}", opts.archive.display());
    }
    Ok(0)
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(code) => code,
            Err(e) => {
                let _ = writeln!(io::stderr(), "zip: {e}");
                2
            }
        },
        Err(e) => {
            if !e.is_empty() {
                let _ = writeln!(io::stderr(), "zip: {e}");
            }
            2
        }
    }
}
