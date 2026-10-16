// This file is part of RecurBusyBox.
//
// `sd`: find and replace text. A focused subset of the `sd` tool built on the
// `regex` crate. Reads all of stdin when no files are given, otherwise edits
// files in place (or streams transformed contents with -s / -p).

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::PathBuf;

use clap::{Arg, ArgAction, Command};

pub fn uu_app() -> Command {
    Command::new("sd")
        .version(uucore::crate_version!())
        .about("Find and replace text (sd subset).")
        .arg(
            Arg::new("preview")
                .short('p')
                .long("preview")
                .help("Print the result to stdout and do not modify files")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("fixed-strings")
                .short('F')
                .long("fixed-strings")
                .help("Treat FIND as a literal string, not a regex")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("stdout")
                .short('s')
                .long("stdout")
                .help("Send each file's transformed contents to stdout instead of editing in place")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("find")
                .value_name("FIND")
                .required(true)
                .allow_hyphen_values(true),
        )
        .arg(
            Arg::new("replace")
                .value_name("REPLACE_WITH")
                .required(true)
                .allow_hyphen_values(true),
        )
        .arg(
            Arg::new("files")
                .value_name("FILES")
                .num_args(0..)
                .allow_hyphen_values(true),
        )
}

/// Build the effective regex pattern from FIND.
fn build_pattern(find: &str, fixed_strings: bool) -> Result<regex::Regex, String> {
    let pattern = if fixed_strings {
        regex::escape(find)
    } else {
        find.to_string()
    };
    regex::Regex::new(&pattern).map_err(|e| format!("invalid pattern: {e}"))
}

/// Read a file as UTF-8, rejecting non-UTF-8 bytes with a clear error.
fn read_utf8(path: &std::path::Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| format!("{}: file is not valid UTF-8", path.display()))
}

fn run(re: &regex::Regex, replacement: &str, opts: &Opts) -> Result<(), String> {
    if opts.files.is_empty() {
        let mut buf = Vec::new();
        io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| format!("stdin: {e}"))?;
        let text =
            String::from_utf8(buf).map_err(|_| "stdin: input is not valid UTF-8".to_string())?;
        let result = re.replace_all(&text, replacement);
        let out = io::stdout();
        let mut out = out.lock();
        out.write_all(result.as_bytes())
            .map_err(|e| format!("stdout: {e}"))?;
        out.flush().map_err(|e| format!("stdout: {e}"))?;
        return Ok(());
    }

    if opts.preview || opts.stdout {
        let out = io::stdout();
        let mut out = out.lock();
        for path in &opts.files {
            let text = read_utf8(path)?;
            let result = re.replace_all(&text, replacement);
            out.write_all(result.as_bytes())
                .map_err(|e| format!("stdout: {e}"))?;
        }
        out.flush().map_err(|e| format!("stdout: {e}"))?;
        return Ok(());
    }

    for path in &opts.files {
        let text = read_utf8(path)?;
        let result = re.replace_all(&text, replacement);
        fs::write(path, result.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

struct Opts {
    preview: bool,
    fixed_strings: bool,
    stdout: bool,
    files: Vec<PathBuf>,
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let matches = match uu_app().try_get_matches_from(&argv) {
        Ok(m) => m,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };

    let find = matches
        .get_one::<String>("find")
        .expect("required by clap");
    let replacement = matches
        .get_one::<String>("replace")
        .expect("required by clap");
    let opts = Opts {
        preview: matches.get_flag("preview"),
        fixed_strings: matches.get_flag("fixed-strings"),
        stdout: matches.get_flag("stdout"),
        files: matches
            .get_many::<String>("files")
            .map(|v| v.map(PathBuf::from).collect())
            .unwrap_or_default(),
    };

    let re = match build_pattern(find, opts.fixed_strings) {
        Ok(re) => re,
        Err(e) => {
            let _ = writeln!(io::stderr(), "sd: {e}");
            return 2;
        }
    };

    match run(&re, replacement, &opts) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(io::stderr(), "sd: {e}");
            2
        }
    }
}
