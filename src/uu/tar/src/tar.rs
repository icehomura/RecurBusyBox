// This file is part of RecurBusyBox.
//
// `tar`: a focused, pure-Rust subset of GNU tar built on the `tar` crate.
// Supports create (`-c`), extract (`-x`) and list (`-t`), the compression
// selection flags (`-z`/`-j`/`-J`/`--zstd`), `--exclude` globs, `-C` and
// verbose listing (`-v` / `-tvf`). Every compression backend is pure Rust
// (no C library is linked): gzip via `flate2` (rust_backend), xz via
// `lzma-rs`, bzip2 via `bzip2-rs` and zstd via `ruzstd`. Because the bzip2
// and zstd backends are decompress-only, creating such an archive returns an
// honest error rather than pretending to succeed.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use bzip2_rs::DecoderReader;
use clap::{Arg, ArgAction, Command};
use flate2::write::GzEncoder;
use flate2::Compression as FlateCompression;
use ruzstd::decoding::StreamingDecoder;
use tar::{Archive, Builder, EntryType, Header};

/// Exit code returned for usage errors and I/O failures (matches the applet
/// contract used by `rg`): `0` success, `2` usage/IO error.
const EXIT_ERROR: i32 = 2;

/// Archive operation, taken from the leading operation letter or long option.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Create,
    Extract,
    List,
}

/// Compression format selection. `None` means an uncompressed `.tar`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

impl Compression {
    fn label(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Gzip => "gzip",
            Compression::Bzip2 => "bzip2",
            Compression::Xz => "xz",
            Compression::Zstd => "zstd",
        }
    }

    /// True when this backend can only *read* (decompress) with the pure-Rust
    /// stack, never *write* (compress).
    fn is_extract_only(self) -> bool {
        matches!(self, Compression::Bzip2 | Compression::Zstd)
    }
}

struct Options {
    operation: Option<Operation>,
    archive: Option<PathBuf>,
    compression: Option<Compression>,
    verbose: bool,
    directory: Option<PathBuf>,
    excludes: Vec<String>,
    members: Vec<String>,
}

pub fn uu_app() -> Command {
    Command::new("tar")
        .version(uucore::crate_version!())
        .about("Create, list, or extract archives (GNU tar subset, pure Rust).")
        .arg(
            Arg::new("create")
                .short('c')
                .long("create")
                .help("create a new archive")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("extract")
                .short('x')
                .long("extract")
                .help("extract files from an archive")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("list")
                .short('t')
                .long("list")
                .help("list the contents of an archive")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("file")
                .short('f')
                .long("file")
                .value_name("ARCHIVE")
                .help("archive file or device"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .help("verbose output")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("gzip")
                .short('z')
                .long("gzip")
                .help("filter the archive through gzip")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("bzip2")
                .short('j')
                .long("bzip2")
                .help("filter the archive through bzip2 (extract only)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("xz")
                .short('J')
                .long("xz")
                .help("filter the archive through xz")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("zstd")
                .long("zstd")
                .help("filter the archive through zstd (extract only)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("directory")
                .short('C')
                .long("directory")
                .value_name("DIR")
                .help("change to DIR before doing anything"),
        )
        .arg(
            Arg::new("exclude")
                .long("exclude")
                .value_name("GLOB")
                .action(ArgAction::Append)
                .help("exclude members matching GLOB (repeatable)"),
        )
        .arg(
            Arg::new("members")
                .value_name("MEMBER")
                .num_args(0..),
        )
}

/// `TarError` carries a human-readable message plus the exit status to use.
struct TarError {
    message: String,
    code: i32,
}

impl TarError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_ERROR,
        }
    }
    fn io(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_ERROR,
        }
    }
}

impl fmt::Display for TarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<io::Error> for TarError {
    fn from(e: io::Error) -> Self {
        TarError::io(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn parse(argv: &[String]) -> Result<Options, TarError> {
    // Manual GNU-style parsing (the same approach `rg` uses in this repo).
    // Handles bundled operation letters such as `-cf`, `-czf`, `-tvf`, where a
    // value-taking letter (`f` or `C`) consumes the rest of its bundle, or the
    // following argv token when the bundle ends there.
    let mut opts = Options {
        operation: None,
        archive: None,
        compression: None,
        verbose: false,
        directory: None,
        excludes: Vec::new(),
        members: Vec::new(),
    };

    let mut i = 0usize;

    // Traditional leading cluster without a dash (e.g. `tar cf a.tar dir`) is
    // accepted for the very first token only when it is made up entirely of
    // option letters; real member paths contain `/`/`.` and never match.
    if i < argv.len() {
        let first = argv[i].as_str();
        let letters: Vec<char> = first.chars().collect();
        let cluster = !first.is_empty()
            && !first.starts_with('-')
            && letters
                .iter()
                .all(|c| matches!(c, 'c' | 'x' | 't' | 'v' | 'z' | 'j' | 'J' | 'f' | 'C'))
            && letters.iter().any(|c| matches!(c, 'c' | 'x' | 't'));
        if cluster {
            eat_short_with_next(&mut opts, &letters, argv, &mut i)?;
            i += 1;
        }
    }

    let mut no_more_flags = false;
    while i < argv.len() {
        let tok = argv[i].clone();
        i += 1;

        if no_more_flags {
            opts.members.push(tok);
            continue;
        }
        if tok == "--" {
            no_more_flags = true;
            continue;
        }

        if let Some(long) = tok.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let mut value = || -> Result<String, TarError> {
                if let Some(v) = &inline {
                    return Ok(v.clone());
                }
                let v = argv
                    .get(i)
                    .cloned()
                    .ok_or_else(|| TarError::usage(format!("--{name} needs a value")))?;
                i += 1;
                Ok(v)
            };
            match name {
                "create" => set_op(&mut opts, Operation::Create)?,
                "extract" => set_op(&mut opts, Operation::Extract)?,
                "list" => set_op(&mut opts, Operation::List)?,
                "verbose" => opts.verbose = true,
                "gzip" => set_compression(&mut opts, Compression::Gzip)?,
                "bzip2" => set_compression(&mut opts, Compression::Bzip2)?,
                "xz" => set_compression(&mut opts, Compression::Xz)?,
                "zstd" => set_compression(&mut opts, Compression::Zstd)?,
                "file" => opts.archive = Some(PathBuf::from(value()?)),
                "directory" => opts.directory = Some(PathBuf::from(value()?)),
                "exclude" => opts.excludes.push(value()?),
                "help" => {
                    let _ = uu_app().print_help();
                    return Err(TarError {
                        message: String::new(),
                        code: 0,
                    });
                }
                "version" => {
                    let _ = writeln!(io::stdout(), "tar {}", uucore::crate_version!());
                    return Err(TarError {
                        message: String::new(),
                        code: 0,
                    });
                }
                other => return Err(TarError::usage(format!("unrecognized option '--{other}'"))),
            }
            continue;
        }

        if tok.len() > 1 && tok.starts_with('-') {
            let letters: Vec<char> = tok[1..].chars().collect();
            eat_short_with_next(&mut opts, &letters, argv, &mut i)?;
            continue;
        }

        opts.members.push(tok);
    }

    Ok(opts)
}

/// Apply a run of short option letters (leading `-` already stripped), pulling
/// the next argv token as a value when a value-taking letter (`f`/`C`) is the
/// last one in the bundle.
fn eat_short_with_next(
    opts: &mut Options,
    letters: &[char],
    argv: &[String],
    i: &mut usize,
) -> Result<(), TarError> {
    match take_value_option(letters) {
        Some(c) => {
            let pos = letters.iter().position(|x| *x == c).unwrap();
            for &x in &letters[..pos] {
                apply_short_flag(opts, x)?;
            }
            let rest: String = letters[pos + 1..].iter().collect();
            let v = if !rest.is_empty() {
                rest
            } else if *i < argv.len() {
                let s = argv[*i].clone();
                *i += 1;
                s
            } else {
                return Err(TarError::usage(format!("-{c} needs a value")));
            };
            if c == 'f' {
                opts.archive = Some(PathBuf::from(v));
            } else {
                opts.directory = Some(PathBuf::from(v));
            }
        }
        None => {
            for &x in letters {
                apply_short_flag(opts, x)?;
            }
        }
    }
    Ok(())
}

/// If the bundle contains a value-taking option (`f` or `C`), return it.
fn take_value_option(letters: &[char]) -> Option<char> {
    letters.iter().copied().find(|c| *c == 'f' || *c == 'C')
}

/// Apply a single non-value short option letter to `opts`.
fn apply_short_flag(opts: &mut Options, c: char) -> Result<(), TarError> {
    match c {
        'c' => set_op(opts, Operation::Create),
        'x' => set_op(opts, Operation::Extract),
        't' => set_op(opts, Operation::List),
        'v' => {
            opts.verbose = true;
            Ok(())
        }
        'z' => set_compression(opts, Compression::Gzip),
        'j' => set_compression(opts, Compression::Bzip2),
        'J' => set_compression(opts, Compression::Xz),
        'h' => {
            let _ = uu_app().print_help();
            Err(TarError {
                message: String::new(),
                code: 0,
            })
        }
        other => Err(TarError::usage(format!("unrecognized option -{other}"))),
    }
}

fn set_op(opts: &mut Options, op: Operation) -> Result<(), TarError> {
    match opts.operation {
        None => {
            opts.operation = Some(op);
            Ok(())
        }
        Some(existing) if existing == op => Ok(()),
        Some(_) => Err(TarError::usage(
            "you may not specify more than one operation (choose -c, -x or -t)",
        )),
    }
}

fn set_compression(opts: &mut Options, c: Compression) -> Result<(), TarError> {
    match opts.compression {
        None => {
            opts.compression = Some(c);
            Ok(())
        }
        Some(existing) if existing == c => Ok(()),
        Some(_) => Err(TarError::usage(
            "you may not select more than one compression method",
        )),
    }
}

fn run(opts: &Options) -> Result<(), TarError> {
    let op = opts
        .operation
        .ok_or_else(|| TarError::usage("an operation must be specified: -c, -x or -t"))?;

    match op {
        Operation::Create => create(opts),
        Operation::Extract => extract_or_list(opts, true),
        Operation::List => extract_or_list(opts, false),
    }
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

fn create(opts: &Options) -> Result<(), TarError> {
    let archive = opts
        .archive
        .as_ref()
        .ok_or_else(|| TarError::usage("-c requires an archive file via -f"))?;

    let compression = opts.compression.unwrap_or(Compression::None);
    if compression.is_extract_only() {
        return Err(TarError::usage(format!(
            "compression to {} is not available with the pure-Rust backend; extraction works",
            compression.label()
        )));
    }
    if archive_in_members(archive, &opts.members) {
        return Err(TarError::io(
            "cannot archive an archive into itself".to_string(),
        ));
    }

    let file = File::create(archive)
        .map_err(|e| TarError::io(format!("cannot create {}: {e}", archive.display())))?;

    // Build the raw (uncompressed) tar stream in memory, then compress on
    // write. xz via lzma-rs is a whole-buffer operation, so a single buffer
    // keeps one code path for every backend we support for creation.
    let mut builder = Builder::new(Vec::new());
    for member in &opts.members {
        let path = Path::new(member);
        let meta = fs::symlink_metadata(path)
            .map_err(|e| TarError::io(format!("{}: {e}", path.display())))?;
        let arc = archive_name(member);
        if meta.is_dir() {
            add_tree(&mut builder, path, &arc, &opts.excludes, opts.verbose)?;
        } else if !excluded_member(&arc, &opts.excludes) {
            append_one(&mut builder, path, &arc, &meta, opts.verbose)?;
        }
    }
    let raw = builder
        .into_inner()
        .map_err(|e| TarError::io(format!("failed to finish tar stream: {e}")))?;

    match compression {
        Compression::None => {
            let mut w = BufWriter::new(file);
            w.write_all(&raw)?;
            w.flush()?;
        }
        Compression::Gzip => {
            let mut enc = GzEncoder::new(file, FlateCompression::default());
            enc.write_all(&raw)?;
            enc.finish().map_err(io_err("gzip finalize failed"))?;
        }
        Compression::Xz => {
            let mut w = BufWriter::new(file);
            let mut reader = Cursor::new(raw);
            lzma_rs::xz_compress(&mut reader, &mut w).map_err(io_err("xz compression failed"))?;
            w.flush()?;
        }
        _ => unreachable!("extract-only backends rejected above"),
    }
    Ok(())
}

/// Recursively add a directory tree, honouring excludes and verbose output.
fn add_tree<W: Write>(
    builder: &mut Builder<W>,
    dir: &Path,
    arc: &str,
    excludes: &[String],
    verbose: bool,
) -> Result<(), TarError> {
    if excluded_member(arc, excludes) {
        return Ok(());
    }
    let meta =
        fs::symlink_metadata(dir).map_err(|e| TarError::io(format!("{}: {e}", dir.display())))?;
    append_one(builder, dir, arc, &meta, verbose)?;

    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| TarError::io(format!("{}: {e}", dir.display())))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for child in entries {
        let child_arc = format!("{arc}/{}", file_name(&child));
        let child_meta = fs::symlink_metadata(&child)
            .map_err(|e| TarError::io(format!("{}: {e}", child.display())))?;
        if child_meta.is_dir() {
            add_tree(builder, &child, &child_arc, excludes, verbose)?;
        } else if !excluded_member(&child_arc, excludes) {
            append_one(builder, &child, &child_arc, &child_meta, verbose)?;
        }
    }
    Ok(())
}

/// Append a single filesystem entry (file, directory or symlink) and, when
/// verbose, echo its archive name.
fn append_one<W: Write>(
    builder: &mut Builder<W>,
    path: &Path,
    arc: &str,
    meta: &fs::Metadata,
    verbose: bool,
) -> Result<(), TarError> {
    let arc_path = Path::new(arc);
    if meta.is_symlink() {
        let link =
            fs::read_link(path).map_err(|e| TarError::io(format!("{}: {e}", path.display())))?;
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header
            .set_path(arc_path)
            .map_err(io_err("invalid archive path"))?;
        header
            .set_link_name(&link)
            .map_err(io_err("invalid link target"))?;
        header.set_mode(0o777);
        header.set_size(0);
        header.set_mtime(mtime_secs(meta));
        header.set_cksum();
        builder
            .append(&header, io::empty())
            .map_err(io_err("failed to add symlink"))?;
    } else if meta.is_dir() {
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Directory);
        header
            .set_path(arc_path)
            .map_err(io_err("invalid archive path"))?;
        header.set_mode(dir_mode(meta));
        header.set_mtime(mtime_secs(meta));
        header.set_size(0);
        header.set_cksum();
        builder
            .append(&header, io::empty())
            .map_err(io_err("failed to add directory"))?;
    } else {
        let mut file =
            File::open(path).map_err(|e| TarError::io(format!("{}: {e}", path.display())))?;
        builder
            .append_file(arc_path, &mut file)
            .map_err(io_err("failed to add file"))?;
    }

    if verbose {
        let out = io::stdout();
        let mut out = out.lock();
        let _ = writeln!(out, "{arc}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Extract / list
// ---------------------------------------------------------------------------

fn extract_or_list(opts: &Options, do_extract: bool) -> Result<(), TarError> {
    let archive = opts
        .archive
        .as_ref()
        .ok_or_else(|| TarError::usage("need an archive file via -f"))?;

    let format = detect_format(archive, opts.compression);
    let file = File::open(archive)
        .map_err(|e| TarError::io(format!("cannot open {}: {e}", archive.display())))?;

    let dest = opts.directory.clone().unwrap_or_else(|| PathBuf::from("."));

    if do_extract {
        fs::create_dir_all(&dest)
            .map_err(|e| TarError::io(format!("{}: {e}", dest.display())))?;
    }

    match format {
        Compression::None => {
            let mut ar = Archive::new(BufReader::new(file));
            walk_entries(&mut ar, opts, &dest, do_extract)?;
        }
        Compression::Gzip => {
            let dec = flate2::read::GzDecoder::new(BufReader::new(file));
            let mut ar = Archive::new(dec);
            walk_entries(&mut ar, opts, &dest, do_extract)?;
        }
        Compression::Bzip2 => {
            let dec = DecoderReader::new(BufReader::new(file));
            let mut ar = Archive::new(dec);
            walk_entries(&mut ar, opts, &dest, do_extract)?;
        }
        Compression::Zstd => {
            let dec = StreamingDecoder::new(BufReader::new(file))
                .map_err(|e| TarError::io(format!("zstd stream start failed: {e}")))?;
            let mut ar = Archive::new(dec);
            walk_entries(&mut ar, opts, &dest, do_extract)?;
        }
        Compression::Xz => {
            // lzma-rs decompresses as a whole-buffer push; materialise first.
            let mut raw = Vec::new();
            let mut reader = BufReader::new(file);
            lzma_rs::xz_decompress(&mut reader, &mut raw)
                .map_err(|e| TarError::io(format!("xz decompression failed: {e}")))?;
            let mut ar = Archive::new(Cursor::new(raw));
            walk_entries(&mut ar, opts, &dest, do_extract)?;
        }
    }
    Ok(())
}

/// Iterate entries, applying selection + exclude filters, path-traversal
/// defence (on extract), and verbose output.
fn walk_entries<R: Read>(
    ar: &mut Archive<R>,
    opts: &Options,
    dest: &Path,
    do_extract: bool,
) -> Result<(), TarError> {
    let entries = ar
        .entries()
        .map_err(|e| TarError::io(format!("failed to read archive entries: {e}")))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| TarError::io(format!("archive read error: {e}")))?;

        // Snapshot everything we need from the header up front so the immutable
        // header borrow ends before we (mutably) read the entry payload.
        let (name, kind, mode, size, mtime) = {
            let header = entry.header();
            let name =
                String::from_utf8_lossy(&header.path_bytes()).replace('\\', "/");
            (
                name,
                header.entry_type(),
                header.mode().unwrap_or(0),
                header.size().unwrap_or(0),
                header.mtime().unwrap_or(0),
            )
        };

        if excluded_member(&name, &opts.excludes) || !selected(&name, &opts.members) {
            continue;
        }

        // Output rules mirror GNU tar: `tar -t` always lists names (with `-v`
        // using the detailed `-tvf` format); `tar -x` stays silent unless `-v`.
        if !do_extract {
            let out = io::stdout();
            let mut out = out.lock();
            if opts.verbose {
                let _ = writeln!(
                    out,
                    "{}{} 0/0 {:>12} {} {name}",
                    type_char(kind),
                    permission_string(mode),
                    size,
                    format_mtime(mtime)
                );
            } else {
                let _ = writeln!(out, "{name}");
            }
        } else if opts.verbose {
            let out = io::stdout();
            let mut out = out.lock();
            let _ = writeln!(out, "{name}");
        }

        if !do_extract {
            continue; // the entries iterator skips the payload on the next() call
        }

        // ---- path traversal defence ----
        if let Some(reason) = unsafe_reason(&name) {
            let err = TarError::io(format!(
                "refusing to extract '{name}': {reason} path escapes the destination"
            ));
            let _ = writeln!(io::stderr(), "tar: {err}");
            return Err(err);
        }

        let full = dest.join(name.as_str());
        match kind {
            EntryType::Directory => {
                fs::create_dir_all(&full)
                    .map_err(|e| TarError::io(format!("{}: {e}", full.display())))?;
            }
            EntryType::Symlink | EntryType::Link => {
                let target = entry
                    .link_name()
                    .map_err(io_err("failed to read link target"))?
                    .ok_or_else(|| TarError::io("link entry has no target"))?;
                if let Some(parent) = full.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if kind == EntryType::Symlink {
                    make_symlink(target.as_ref(), &full)?;
                } else {
                    let link = dest.join(target.to_string_lossy().replace('\\', "/"));
                    fs::hard_link(&link, &full).map_err(|e| {
                        TarError::io(format!("failed to hard link {}: {e}", full.display()))
                    })?;
                }
            }
            _ => {
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| TarError::io(format!("{}: {e}", parent.display())))?;
                }
                let mut out =
                    File::create(&full).map_err(|e| TarError::io(format!("{}: {e}", full.display())))?;
                io::copy(&mut entry, &mut out)?;
                set_permissions(&full, mode);
            }
        }
    }
    Ok(())
}

fn type_char(kind: EntryType) -> char {
    match kind {
        EntryType::Directory => 'd',
        EntryType::Symlink => 'l',
        EntryType::Link => 'h',
        EntryType::Char => 'c',
        EntryType::Block => 'b',
        EntryType::Fifo => 'p',
        _ => '-',
    }
}

// ---------------------------------------------------------------------------
// Verbose listing helpers (`tar -tvf`)
// ---------------------------------------------------------------------------

fn permission_string(mode: u32) -> String {
    const BITS: [char; 9] = ['r', 'w', 'x', 'r', 'w', 'x', 'r', 'w', 'x'];
    (0..9)
        .map(|i| {
            if mode & (1u32 << (8 - i)) != 0 {
                BITS[i]
            } else {
                '-'
            }
        })
        .collect()
}

/// Render seconds-since-epoch as `"YYYY-MM-DD HH:MM"` in UTC using a
/// proleptic-Gregorian civil-from-days conversion (no date crate).
fn format_mtime(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Format detection
// ---------------------------------------------------------------------------

/// Decide the archive format for extract/list. An explicit `-z/-j/-J/--zstd`
/// wins; otherwise prefer the file extension, then fall back to magic bytes.
fn detect_format(archive: &Path, explicit: Option<Compression>) -> Compression {
    if let Some(c) = explicit {
        return c;
    }
    if let Some(c) = format_from_extension(archive) {
        return c;
    }
    detect_from_magic(archive).unwrap_or(Compression::None)
}

fn format_from_extension(archive: &Path) -> Option<Compression> {
    let name = archive.to_str()?.to_ascii_lowercase();
    // Longer / compound extensions first.
    const MAP: &[(&str, Compression)] = &[
        (".tar.gz", Compression::Gzip),
        (".tgz", Compression::Gzip),
        (".tar.bz2", Compression::Bzip2),
        (".tbz2", Compression::Bzip2),
        (".tbz", Compression::Bzip2),
        (".tar.xz", Compression::Xz),
        (".txz", Compression::Xz),
        (".tar.zst", Compression::Zstd),
        (".tar.zstd", Compression::Zstd),
        (".tzst", Compression::Zstd),
        (".tar", Compression::None),
    ];
    MAP.iter()
        .find(|(ext, _)| name.ends_with(ext))
        .map(|(_, comp)| *comp)
}

fn detect_from_magic(archive: &Path) -> Option<Compression> {
    let mut f = File::open(archive).ok()?;
    let mut buf = [0u8; 8];
    // A short read is fine: magic_for only inspects the prefix that was filled.
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    magic_for(&buf[..filled])
}

fn magic_for(buf: &[u8]) -> Option<Compression> {
    if buf.starts_with(&[0x1f, 0x8b]) {
        Some(Compression::Gzip)
    } else if buf.starts_with(&[0x42, 0x5a, 0x68]) {
        Some(Compression::Bzip2)
    } else if buf.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        Some(Compression::Xz)
    } else if buf.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Some(Compression::Zstd)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Path-traversal defence
// ---------------------------------------------------------------------------

/// Return a reason if the member name could escape the extraction directory.
/// Flags `..`, absolute paths, drive letters and backslash forms.
fn unsafe_reason(name: &str) -> Option<&'static str> {
    if name.contains('\\') {
        return Some("backslash");
    }
    let bytes = name.as_bytes();
    if bytes.first() == Some(&b'/') {
        return Some("absolute");
    }
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some("drive letter");
    }
    if name.split('/').any(|part| part == "..") {
        return Some("'..'");
    }
    None
}

// ---------------------------------------------------------------------------
// Exclude + member selection
// ---------------------------------------------------------------------------

fn excluded_member(name: &str, excludes: &[String]) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    excludes
        .iter()
        .any(|g| glob_match(g, name) || glob_match(g, base))
}

fn selected(name: &str, members: &[String]) -> bool {
    if members.is_empty() {
        return true;
    }
    members.iter().any(|m| {
        let m = archive_name(m);
        name == m || name.starts_with(&format!("{m}/"))
    })
}

/// Minimal glob matcher supporting `*` (matches across `/`), `**` and `?`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_from(&p, 0, &t, 0)
}

fn glob_from(p: &[char], mut pi: usize, t: &[char], mut ti: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            '*' => {
                if pi + 1 < p.len() && p[pi + 1] == '*' {
                    pi += 1;
                }
                for k in ti..=t.len() {
                    if glob_from(p, pi + 1, t, k) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if ti >= t.len() {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            c => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn io_err<X: fmt::Display>(msg: &'static str) -> impl Fn(X) -> TarError {
    move |e| TarError::io(format!("{msg}: {e}"))
}

/// Normalise a CLI member path into a tar-style archive name (forward slashes,
/// no trailing separator).
fn archive_name(member: &str) -> String {
    let mut s = member.to_string();
    while s.len() > 1 && (s.ends_with('/') || s.ends_with('\\')) {
        s.pop();
    }
    s.replace('\\', "/")
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

fn archive_in_members(archive: &Path, members: &[String]) -> bool {
    match archive.canonicalize() {
        Ok(a) => members
            .iter()
            .filter_map(|m| fs::canonicalize(m).ok())
            .any(|m| m == a),
        Err(_) => false,
    }
}

fn mtime_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
fn dir_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn dir_mode(_meta: &fs::Metadata) -> u32 {
    0o755
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777));
}

#[cfg(not(unix))]
fn set_permissions(_path: &Path, _mode: u32) {}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> Result<(), TarError> {
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| TarError::io(format!("failed to create symlink {}: {e}", link.display())))
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path) -> Result<(), TarError> {
    std::os::windows::fs::symlink_file(target, link).map_err(|e| {
        TarError::io(format!(
            "failed to create symlink {}: {e} (Windows may require developer mode or \
             elevated privileges)",
            link.display()
        ))
    })
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_target: &Path, _link: &Path) -> Result<(), TarError> {
    Err(TarError::io(
        "symlink extraction is not supported on this platform",
    ))
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(()) => 0,
            Err(e) => {
                if !e.message.is_empty() {
                    let _ = writeln!(io::stderr(), "tar: {}", e.message);
                }
                e.code
            }
        },
        Err(e) => {
            if !e.message.is_empty() {
                let _ = writeln!(io::stderr(), "tar: {}", e.message);
            }
            e.code
        }
    }
}
