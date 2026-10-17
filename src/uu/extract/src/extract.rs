// This file is part of RecurBusyBox.
//
// `extract`: a universal archive extractor. It figures out the archive format
// from the file extension first and falls back to magic-byte sniffing so that a
// misnamed file still works, then unpacks it with pure-Rust decoders only — no
// C/C++ bindings anywhere in the dependency graph.

use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read, Write as _};
use std::path::{Component, Path, PathBuf};

use clap::{Arg, ArgAction, Command};

/// Recognised archive/compression formats.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
    Tar,
    Zip,
    SevenZ,
    Rar,
    Gz,
    Bz2,
    Xz,
    Zst,
    Unknown,
}

/// A resolved command line.
struct Options {
    archive: PathBuf,
    dest: PathBuf,
    list: bool,
    verbose: bool,
    force: bool,
}

pub fn uu_app() -> Command {
    Command::new("extract")
        .version(uucore::crate_version!())
        .about("Universal archive extractor (pure Rust).")
        .long_about(
            "Unpack a wide range of archives by auto-detecting the format\n\
             (first by extension, then by magic bytes).\n\n\
             Formats: tar(.gz/.bz2/.xz/.zst), .zip, .7z, .rar, and bare\n\
             .gz/.bz2/.xz/.zst streams.\n\n\
             DEST defaults to a directory named after the archive minus its\n\
             extension (for example `foo.tar.gz` unpacks into `./foo/`); if the\n\
             extension cannot be stripped it defaults to `.`.\n\n\
             Entry mtimes and file permissions are NOT preserved.\n\
             Path-traversal entries (`../`, absolute paths, drive letters,\n\
             backslashes) are rejected and reported.\n\n\
             .rar is intentionally not supported: the bundled pure-Rust `rar`\n\
             reader cannot decompress archives and offers no listing or safe\n\
             extraction path, so a clear runtime error is printed instead.",
        )
        .arg(Arg::new("archive").value_name("ARCHIVE"))
        .arg(Arg::new("dest").value_name("DEST"))
        .arg(
            Arg::new("list")
                .short('l')
                .long("list")
                .action(ArgAction::SetTrue)
                .help("List archive entries without extracting"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::SetTrue)
                .help("Print each entry as it is written"),
        )
        .arg(
            Arg::new("dir")
                .short('d')
                .long("dir")
                .value_name("DEST")
                .help("Explicit destination directory"),
        )
        .arg(
            Arg::new("force")
                .short('f')
                .long("force")
                .action(ArgAction::SetTrue)
                .help("Allow overwriting existing files"),
        )
}

/// Parse the argv tail (without argv[0]).
/// `Err(Some(msg))` is a usage error; `Err(None)` means help/version was shown.
fn parse(argv: &[String]) -> Result<Options, Option<String>> {
    let mut list = false;
    let mut verbose = false;
    let mut force = false;
    let mut dir: Option<String> = None;
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
            "-l" | "--list" => list = true,
            "-v" | "--verbose" => verbose = true,
            "-f" | "--force" => force = true,
            "-d" | "--dir" => {
                let v = iter.next().ok_or_else(|| usage("--dir needs a value"))?;
                dir = Some(v.clone());
            }
            "-h" | "--help" => {
                let _ = uu_app().print_help();
                return Err(None);
            }
            "--version" | "-V" => {
                println!("extract {}", uucore::crate_version!());
                return Err(None);
            }
            s if s.starts_with("--dir=") => dir = Some(s["--dir=".len()..].to_string()),
            s if s.starts_with("-d") && s.len() > 2 => dir = Some(s[2..].to_string()),
            s if s.starts_with('-') && s != "-" => {
                return Err(Some(format!("unrecognised option '{s}'")));
            }
            _ => positionals.push(a.clone()),
        }
    }

    let mut it = positionals.into_iter();
    let archive = it
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| usage("an ARCHIVE is required"))?;
    let positional_dest = it.next().map(PathBuf::from);

    let dest = match (dir.map(PathBuf::from), positional_dest) {
        (Some(d), _) => d,
        (None, Some(d)) => d,
        (None, None) => default_dest(&archive),
    };

    Ok(Options {
        archive,
        dest,
        list,
        verbose,
        force,
    })
}

fn usage(what: &str) -> String {
    format!("{what} (usage: extract [OPTIONS] <ARCHIVE> [DEST])")
}

/// `foo.tar.gz` -> `foo`, `archive.zip` -> `archive`, else `.`.
fn default_dest(archive: &Path) -> PathBuf {
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let stem = match COMPOUND_EXTENSIONS
        .iter()
        .find(|ext| name.ends_with(*ext))
        .map(|ext| &name[..name.len() - ext.len()])
    {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => Path::new(&name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    if stem.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(stem)
    }
}

const COMPOUND_EXTENSIONS: &[&str] = &[
    ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tgz", ".tbz2", ".tbz", ".txz", ".tzst",
];

/// Detect the format from the (lower-cased) file name, falling back to `magic`.
fn detect(name: &str, magic: &[u8]) -> Format {
    // Longest/most specific extensions first.
    let by_ext = [
        (".tar.gz", Format::TarGz),
        (".tgz", Format::TarGz),
        (".tar.bz2", Format::TarBz2),
        (".tbz2", Format::TarBz2),
        (".tbz", Format::TarBz2),
        (".tar.xz", Format::TarXz),
        (".txz", Format::TarXz),
        (".tar.zst", Format::TarZst),
        (".tzst", Format::TarZst),
        (".tar", Format::Tar),
        (".zip", Format::Zip),
        (".7z", Format::SevenZ),
        (".rar", Format::Rar),
        (".gz", Format::Gz),
        (".gzip", Format::Gz),
        (".bz2", Format::Bz2),
        (".xz", Format::Xz),
        (".zst", Format::Zst),
    ];
    for (ext, fmt) in by_ext {
        if name.ends_with(ext) {
            return fmt;
        }
    }

    // Magic-byte fallback for misnamed files.
    let m = magic;
    if m.starts_with(&[0x1f, 0x8b]) {
        Format::Gz
    } else if m.starts_with(b"BZh") {
        Format::Bz2
    } else if m.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        Format::Xz
    } else if m.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Format::Zst
    } else if m.starts_with(&[0x50, 0x4b, 0x03, 0x04])
        || m.starts_with(&[0x50, 0x4b, 0x53, 0x50])
    {
        Format::Zip
    } else if m.starts_with(&[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c]) {
        Format::SevenZ
    } else if m.starts_with(&[0x52, 0x61, 0x72, 0x21, 0x1a, 0x07]) {
        Format::Rar
    } else {
        Format::Unknown
    }
}

/// Read the first 16 bytes of `path` for magic sniffing.
fn read_magic(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 16];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Decide whether an archive entry's relative name is safe to write under
/// `dest`. Returns the sanitised absolute target, or the offending raw name if
/// the entry escapes the destination.
fn safe_join(dest: &Path, raw: &str) -> Result<PathBuf, String> {
    // Reject Windows drive letters (`C:`) and UNC prefixes outright.
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(raw.to_string());
    }
    if raw.contains('\\') {
        return Err(raw.to_string());
    }

    // Reject absolute paths.
    if raw.starts_with('/') {
        return Err(raw.to_string());
    }

    let mut path = dest.to_path_buf();
    for c in Path::new(raw).components() {
        match c {
            Component::Normal(seg) => path.push(seg),
            Component::CurDir => {}
            // Prefix, RootDir or ParentDir => escape.
            _ => return Err(raw.to_string()),
        }
    }

    // Belt-and-braces: compare against the canonicalised destination.
    let canon_dest = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
    let canon_target = path.canonicalize().unwrap_or_else(|_| path.clone());
    if !(canon_target.starts_with(&canon_dest) || path.starts_with(dest)) {
        return Err(raw.to_string());
    }

    Ok(path)
}

/// Prepare an output file honouring `--force`; `Ok(false)` means "skipped".
fn prepare_output(target: &Path, opts: &Options) -> Result<Option<File>, String> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    if target.exists() && !opts.force {
        return Ok(None);
    }
    let f = File::create(target).map_err(|e| format!("create {}: {e}", target.display()))?;
    Ok(Some(f))
}

fn report(opts: &Options, kind: &str, name: &str) {
    if opts.verbose {
        println!("{kind}: {name}");
    }
}

fn run(opts: &Options) -> Result<u32, String> {
    let name = opts
        .archive
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let magic = read_magic(&opts.archive).map_err(|e| format!("{}: {e}", opts.archive.display()))?;
    let fmt = detect(&name, &magic);

    match fmt {
        Format::Tar | Format::TarGz | Format::TarBz2 | Format::TarXz | Format::TarZst => {
            extract_tar(opts, fmt)
        }
        Format::Zip => extract_zip(opts),
        Format::SevenZ => extract_7z(opts),
        Format::Rar => Err(
            ".rar extraction is not supported: the pure-Rust `rar` reader cannot \
             decompress archives and has no safe list/extract API"
                .to_string(),
        ),
        Format::Gz | Format::Bz2 | Format::Xz | Format::Zst => {
            if opts.list {
                // A bare stream has a single member; report the stripped name.
                println!("{}", strip_single_ext(&name));
                return Ok(0);
            }
            extract_single(opts, fmt)
        }
        Format::Unknown => Err(format!(
            "unrecognised archive format for '{}'",
            opts.archive.display()
        )),
    }
}

fn strip_single_ext(name: &str) -> String {
    let exts = [".gz", ".gzip", ".bz2", ".xz", ".zst"];
    for e in exts {
        if name.ends_with(e) {
            return name[..name.len() - e.len()].to_string();
        }
    }
    format!("{name}.out")
}

// ---------------------------------------------------------------------------
// tar family (optionally streamed through a decompressor)
// ---------------------------------------------------------------------------

fn extract_tar(opts: &Options, fmt: Format) -> Result<u32, String> {
    let file = File::open(&opts.archive).map_err(|e| format!("open: {e}"))?;
    let mut reader: Box<dyn Read> = match fmt {
        Format::TarGz => Box::new(flate2::read::GzDecoder::new(file)),
        Format::TarBz2 => Box::new(bzip2_rs::DecoderReader::new(file)),
        Format::TarZst => Box::new(
            ruzstd::decoding::StreamingDecoder::new(file).map_err(|e| format!("zstd: {e}"))?,
        ),
        Format::TarXz => {
            // lzma-rs exposes a pull-to-sink API, so buffer the whole member.
            let mut buf = Vec::new();
            let mut br = BufReader::new(file);
            lzma_rs::xz_decompress(&mut br, &mut buf).map_err(|e| format!("xz: {e}"))?;
            Box::new(Cursor::new(buf))
        }
        _ => Box::new(file),
    };

    let mut ar = tar::Archive::new(reader.as_mut());
    let entries = ar.entries().map_err(|e| format!("tar entries: {e}"))?;

    fs::create_dir_all(&opts.dest).map_err(|e| format!("create dest: {e}"))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        let raw = entry
            .path()
            .map_err(|e| format!("tar path: {e}"))?
            .to_string_lossy()
            .into_owned();
        if raw.is_empty() {
            continue;
        }

        if opts.list {
            println!("{raw}");
            continue;
        }

        let header = entry.header();
        let is_dir = header.entry_type().is_dir();

        let target = match safe_join(&opts.dest, &raw) {
            Ok(p) => p,
            Err(bad) => {
                eprintln!("extract: rejected '{bad}' (path traversal)");
                continue;
            }
        };

        if is_dir {
            fs::create_dir_all(&target).map_err(|e| format!("mkdir: {e}"))?;
            report(opts, "dir", &raw);
            continue;
        }

        if !header.entry_type().is_file() {
            report(opts, "skip", &raw);
            continue;
        }

        let mut out = match prepare_output(&target, opts)? {
            Some(f) => f,
            None => {
                eprintln!("extract: skipping existing '{}' (use -f to overwrite)", target.display());
                continue;
            }
        };
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("write: {e}"))?;
        out.flush().ok();
        report(opts, "file", &raw);
    }

    Ok(0)
}

// ---------------------------------------------------------------------------
// zip
// ---------------------------------------------------------------------------

fn extract_zip(opts: &Options) -> Result<u32, String> {
    let file = File::open(&opts.archive).map_err(|e| format!("open: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("zip: {e}"))?;

    if !opts.list {
        fs::create_dir_all(&opts.dest).map_err(|e| format!("create dest: {e}"))?;
    }

    for i in 0..zip.len() {
        let mut zf = zip.by_index(i).map_err(|e| format!("zip entry: {e}"))?;
        let raw = match zf.name() {
            Ok(n) => n.into_owned(),
            Err(e) => {
                eprintln!("extract: bad entry name ({e})");
                continue;
            }
        };

        if opts.list {
            println!("{raw}");
            continue;
        }

        let target = match safe_join(&opts.dest, &raw) {
            Ok(p) => p,
            Err(bad) => {
                eprintln!("extract: rejected '{bad}' (path traversal)");
                continue;
            }
        };

        if zf.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("mkdir: {e}"))?;
            report(opts, "dir", &raw);
            continue;
        }

        let mut out = match prepare_output(&target, opts)? {
            Some(f) => f,
            None => {
                eprintln!("extract: skipping existing '{}' (use -f to overwrite)", target.display());
                continue;
            }
        };
        std::io::copy(&mut zf, &mut out).map_err(|e| format!("write: {e}"))?;
        out.flush().ok();
        report(opts, "file", &raw);
    }

    Ok(0)
}

// ---------------------------------------------------------------------------
// 7z
// ---------------------------------------------------------------------------

fn extract_7z(opts: &Options) -> Result<u32, String> {
    let mut reader = sevenz_rust::SevenZReader::open(&opts.archive, sevenz_rust::Password::empty())
        .map_err(|e| format!("7z: {e}"))?;

    if !opts.list {
        fs::create_dir_all(&opts.dest).map_err(|e| format!("create dest: {e}"))?;
    }

    let dest = opts.dest.canonicalize().unwrap_or_else(|_| opts.dest.clone());
    let mut err: Option<String> = None;

    reader
        .for_each_entries(&mut |entry: &sevenz_rust::SevenZArchiveEntry, data: &mut dyn Read| {
            let raw = entry.name();
            if opts.list {
                println!("{raw}");
                return Ok(true);
            }

            let target = match safe_join(&dest, raw) {
                Ok(p) => p,
                Err(bad) => {
                    eprintln!("extract: rejected '{bad}' (path traversal)");
                    return Ok(true);
                }
            };

            if entry.is_directory() {
                if let Err(e) = fs::create_dir_all(&target) {
                    err = Some(format!("mkdir: {e}"));
                    return Ok(false);
                }
                if opts.verbose {
                    println!("dir: {raw}");
                }
                return Ok(true);
            }

            match prepare_output(&target, opts) {
                Ok(Some(mut f)) => {
                    if let Err(e) = std::io::copy(data, &mut f) {
                        err = Some(format!("write: {e}"));
                        return Ok(false);
                    }
                    f.flush().ok();
                    if opts.verbose {
                        println!("file: {raw}");
                    }
                }
                Ok(None) => {
                    eprintln!("extract: skipping existing '{}' (use -f to overwrite)", target.display());
                }
                Err(e) => {
                    err = Some(e);
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .map_err(|e| format!("7z: {e}"))?;

    if let Some(e) = err {
        return Err(e);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// bare single-file compressed streams
// ---------------------------------------------------------------------------

fn extract_single(opts: &Options, fmt: Format) -> Result<u32, String> {
    let name = opts
        .archive
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let out_name = strip_single_ext(&name);

    // Destination is a directory: write the stripped name inside it.
    let target = if opts.dest.is_dir() || opts.dest == Path::new(".") {
        opts.dest.join(&out_name)
    } else if opts.dest.extension().is_some() || out_name.contains('.') {
        // Looks like an explicit file target.
        opts.dest.clone()
    } else {
        opts.dest.join(&out_name)
    };

    if target.exists() && !opts.force {
        return Err(format!(
            "'{}' exists (use -f to overwrite)",
            target.display()
        ));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create: {e}"))?;
    }

    let mut out = File::create(&target).map_err(|e| format!("create: {e}"))?;
    let mut reader = BufReader::new(
        File::open(&opts.archive).map_err(|e| format!("open: {e}"))?,
    );

    let written = match fmt {
        Format::Gz => {
            let mut d = flate2::read::GzDecoder::new(reader);
            std::io::copy(&mut d, &mut out).map_err(|e| format!("write: {e}"))?
        }
        Format::Bz2 => {
            let mut d = bzip2_rs::DecoderReader::new(reader);
            std::io::copy(&mut d, &mut out).map_err(|e| format!("write: {e}"))?
        }
        Format::Zst => {
            let mut d =
                ruzstd::decoding::StreamingDecoder::new(reader).map_err(|e| format!("zstd: {e}"))?;
            std::io::copy(&mut d, &mut out).map_err(|e| format!("write: {e}"))?
        }
        Format::Xz => {
            lzma_rs::xz_decompress(&mut reader, &mut out).map_err(|e| format!("xz: {e}"))?;
            0
        }
        _ => unreachable!(),
    };
    let _ = written;
    out.flush().ok();
    report(opts, "file", &out_name);
    Ok(0)
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(code) => code as i32,
            Err(e) => {
                eprintln!("extract: {e}");
                2
            }
        },
        Err(Some(msg)) => {
            eprintln!("extract: {msg}");
            2
        }
        Err(None) => 0,
    }
}
