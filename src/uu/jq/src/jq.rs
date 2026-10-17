// RecurBusyBox - a multi-call binary of high-performance Rust tools.
//
// `jq`: a jq-compatible JSON processor. The jq language (lexer, parser,
// compiler and interpreter) is provided by the `jaq` crates; this applet
// only wires up the CLI, input handling and output formatting.

use std::io::{self, Read as _, Write as _};

use clap::{Arg, ArgAction, Command};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Ctx, Vars};
use jaq_json::read::parse_many;
use jaq_json::write::{self, Pp};
use jaq_json::Val;

struct Options {
    filter: String,
    files: Vec<String>,
    raw_output: bool,
    slurp: bool,
    null_input: bool,
}

pub fn uu_app() -> Command {
    Command::new("jq")
        .version(uucore::crate_version!())
        .about("jq-compatible JSON processor.")
        .arg(Arg::new("filter").value_name("FILTER"))
        .arg(Arg::new("files").value_name("FILE").num_args(0..))
        .arg(
            Arg::new("compact-output")
                .short('c')
                .long("compact-output")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("raw-output")
                .short('r')
                .long("raw-output")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("slurp")
                .short('s')
                .long("slurp")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("null-input")
                .short('n')
                .long("null-input")
                .action(ArgAction::SetTrue),
        )
}

fn usage(what: &str) -> String {
    format!("{what} (usage: jq [OPTIONS] FILTER [FILE ...])")
}

/// Parse the argv tail (without argv[0]) into [`Options`].
/// `Err(Some(msg))` is a usage error; `Err(None)` means the request was
/// already handled (`--help` / `--version`).
fn parse(argv: &[String]) -> Result<Options, Option<String>> {
    let mut raw_output = false;
    let mut slurp = false;
    let mut null_input = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut no_more_flags = false;
    let iter = argv.iter();

    for a in iter {
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
                let _ = writeln!(io::stdout(), "jq {}", uucore::crate_version!());
                return Err(None);
            }
            "-c" | "--compact-output" => {}
            "-r" | "--raw-output" => raw_output = true,
            "-s" | "--slurp" => slurp = true,
            "-n" | "--null-input" => null_input = true,
            // Combined short flags, e.g. `-cr`.
            s if s.starts_with('-') && s.len() > 1 && !s.starts_with("--") => {
                for c in s[1..].chars() {
                    match c {
                        'c' => {}
                        'r' => raw_output = true,
                        's' => slurp = true,
                        'n' => null_input = true,
                        _ => return Err(Some(usage(&format!("unknown option -{c}")))),
                    }
                }
            }
            _ => positionals.push(a.clone()),
        }
    }

    let mut it = positionals.into_iter();
    let filter = it.next().ok_or_else(|| usage("a filter is required"))?;
    let files: Vec<String> = it.collect();
    Ok(Options {
        filter,
        files,
        raw_output,
        slurp,
        null_input,
    })
}

fn read_bytes(path: &str) -> Result<Vec<u8>, String> {
    if path == "-" {
        let mut buf = Vec::new();
        io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| format!("stdin: {e}"))?;
        Ok(buf)
    } else {
        std::fs::read(path).map_err(|e| format!("{path}: {e}"))
    }
}

fn run(opts: &Options) -> Result<i32, String> {
    // Compile the filter against jaq's core + std + json libraries.
    let arena = Arena::default();
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let loader = Loader::new(defs);
    let program = File {
        code: opts.filter.as_str(),
        path: (),
    };
    let modules = loader
        .load(&arena, program)
        .map_err(|e| format!("parse error: {e:?}"))?;
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());
    let filter = jaq_core::Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|e| format!("compile error: {e:?}"))?;

    // Gather inputs, honouring `--null-input` and `--slurp`.
    let inputs: Vec<Val> = if opts.null_input {
        vec![Val::default()]
    } else {
        let mut vals = Vec::new();
        if opts.files.is_empty() {
            let bytes = read_bytes("-")?;
            for v in parse_many(&bytes) {
                vals.push(v.map_err(|e| format!("parse error: {e}"))?);
            }
        } else {
            for f in &opts.files {
                let bytes = read_bytes(f)?;
                for v in parse_many(&bytes) {
                    vals.push(v.map_err(|e| format!("{f}: {e}"))?);
                }
            }
        }
        if opts.slurp {
            vec![Val::Arr(jaq_json::Rc::new(vals))]
        } else {
            vals
        }
    };

    let out = io::stdout();
    let mut out = out.lock();
    let mut produced = false;
    let mut had_error = false;

    for x in inputs {
        let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
        for y in filter.id.run((ctx, x)).map(unwrap_valr) {
            match y {
                Ok(v) => {
                    produced = true;
                    match &v {
                        Val::TStr(s) | Val::BStr(s) if opts.raw_output => {
                            out.write_all(s.as_ref())
                                .and_then(|()| writeln!(out))
                                .map_err(|e| format!("write error: {e}"))?;
                            continue;
                        }
                        _ => {}
                    }
                    write::write(&mut out, &Pp::default(), 0, &v)
                        .and_then(|()| writeln!(out))
                        .map_err(|e| format!("write error: {e}"))?;
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "jq: error: {e}");
                    had_error = true;
                }
            }
        }
    }

    if had_error {
        return Ok(5);
    }
    Ok(i32::from(!produced))
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(code) => code,
            Err(e) => {
                let _ = writeln!(io::stderr(), "jq: {e}");
                2
            }
        },
        Err(Some(msg)) => {
            let _ = writeln!(io::stderr(), "jq: {msg}");
            2
        }
        Err(None) => 0,
    }
}
