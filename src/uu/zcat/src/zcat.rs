// RecurBusyBox - a multi-call binary of high-performance Rust tools.
//
// `zcat`: concatenate compressed files to standard output without ever writing
// to disk — the companion filter for piping archives into `rg`, `jq` or `wc`.
//
// The format of every input is sniffed from its magic bytes and never from its
// file name, so a gzip stream called `.txt` still decompresses and a plain file
// is copied through unchanged (the behaviour of GNU `zcat`/`bzcat`). Every
// decoder is pure Rust — `flate2` (rust backend), `bzip2-rs`, `lzma-rs` and
// `ruzstd` — so no C or C++ toolchain and no bindings are involved.

use std::fs::File;
use std::io::{self, BufRead, BufWriter, Read, Write};

use clap::error::ErrorKind;
use clap::{Arg, ArgAction, Command};

/// Length of the longest magic number we look at (xz is six bytes).
const MAGIC_LEN: usize = 6;

/// Read-ahead buffer of [`Sniffer`], reused for the standard output buffer.
const BUF_SIZE: usize = 128 * 1024;

/// Compression formats recognised by the magic-byte sniffing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Gzip,
    Bzip2,
    Zstd,
    Xz,
    /// No known magic: the bytes are copied through unchanged.
    Plain,
}

impl Format {
    fn label(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Bzip2 => "bzip2",
            Self::Zstd => "zstd",
            Self::Xz => "xz",
            Self::Plain => "uncompressed",
        }
    }
}

/// Identify a stream by its magic bytes.
fn detect(magic: &[u8]) -> Format {
    if magic.starts_with(&[0x1f, 0x8b]) {
        Format::Gzip
    } else if magic.starts_with(b"BZh") {
        Format::Bzip2
    } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) || is_zstd_skippable(magic) {
        Format::Zstd
    } else if magic.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        Format::Xz
    } else {
        Format::Plain
    }
}

/// Zstandard skippable frames are `50 2a 4d 18` ..= `5f 2a 4d 18`, the
/// little-endian rendering of `0x184D2A50..=0x184D2A5F`. They carry metadata,
/// not data, and `ruzstd` refuses them, so the sniff claims them and
/// [`skip_zstd_frame`] steps over them before the next frame is decoded.
fn is_zstd_skippable(magic: &[u8]) -> bool {
    matches!(magic, [0x50..=0x5f, 0x2a, 0x4d, 0x18, ..])
}

/// `Read::read`, quietly retrying on `Interrupted`.
fn read_retry<R: Read + ?Sized>(inner: &mut R, out: &mut [u8]) -> io::Result<usize> {
    loop {
        match inner.read(out) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

/// A buffered reader that can look at the bytes ahead of it without consuming
/// them, so the magic number stays available to whichever decoder we pick.
struct Sniffer<R> {
    inner: R,
    buf: Vec<u8>,
    start: usize,
    end: usize,
    /// Bytes handed to consumers so far. The member loop uses it to notice a
    /// decoder that stops without making progress, instead of spinning forever.
    delivered: u64,
}

impl<R: Read> Sniffer<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: vec![0; BUF_SIZE],
            start: 0,
            end: 0,
            delivered: 0,
        }
    }

    /// Up to `n` upcoming bytes, without consuming them. An empty vector means
    /// the input has been read to the end.
    fn peek(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let want = n.min(self.buf.len());
        while self.end - self.start < want {
            if self.end == self.buf.len() {
                // Unread bytes are stranded at the end of a full buffer.
                self.buf.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            match read_retry(&mut self.inner, &mut self.buf[self.end..])? {
                0 => break,
                read => self.end += read,
            }
        }
        let stop = self.start + want.min(self.end - self.start);
        Ok(self.buf[self.start..stop].to_vec())
    }
}

impl<R: Read> Read for Sniffer<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.start == self.end {
            // Nothing buffered: read straight through.
            let read = read_retry(&mut self.inner, out)?;
            self.delivered += u64::try_from(read).unwrap_or(u64::MAX);
            return Ok(read);
        }
        let n = out.len().min(self.end - self.start);
        out[..n].copy_from_slice(&self.buf[self.start..self.start + n]);
        self.start += n;
        self.delivered += u64::try_from(n).unwrap_or(u64::MAX);
        Ok(n)
    }
}

impl<R: Read> BufRead for Sniffer<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.start == self.end {
            self.start = 0;
            self.end = read_retry(&mut self.inner, &mut self.buf)?;
        }
        Ok(&self.buf[self.start..self.end])
    }

    fn consume(&mut self, amt: usize) {
        let new_start = (self.start + amt).min(self.end);
        self.delivered += u64::try_from(new_start - self.start).unwrap_or(u64::MAX);
        self.start = new_start;
    }
}

/// Step over a zstandard skippable frame: 4 magic bytes, a little-endian frame
/// size, then that many bytes of payload. `ruzstd` refuses such a frame, so we
/// discard it ourselves and let the member loop look at what follows.
fn skip_zstd_frame<R: Read>(source: &mut Sniffer<R>) -> Result<(), String> {
    let mut header = [0u8; 8];
    source
        .read_exact(&mut header)
        .map_err(|e| format!("zstd: truncated skippable frame: {e}"))?;
    let payload = u32::from_le_bytes(
        header[4..]
            .try_into()
            .expect("four bytes sliced from a fixed array"),
    );
    let mut sink = io::sink();
    let mut discard = (&mut *source).take(u64::from(payload));
    io::copy(&mut discard, &mut sink).map_err(|e| format!("zstd: {e}"))?;
    Ok(())
}

#[derive(Debug)]
struct Options {
    files: Vec<String>,
    pass_through: bool,
}

pub fn uu_app() -> Command {
    Command::new("zcat")
        .version(uucore::crate_version!())
        .about("Concatenate compressed files to stdout (gzip, bzip2, xz, zstd).")
        .after_help(concat!(
            "Formats are detected from magic bytes, never from the file name, and\n",
            "input with no known magic is copied through unchanged unless\n",
            "--no-pass-through is given. `-` (or no operand at all) reads standard\n",
            "input.\n",
            "\n",
            "Members are decoded one at a time, so a file may concatenate gzip,\n",
            "bzip2, xz and zstd members in any order, and zstandard skippable frames\n",
            "are stepped over. Trailing bytes that match no magic are passed through\n",
            "verbatim.\n",
            "\n",
            "Limitations: each member is decoded by a pure-Rust decoder that only\n",
            "supports decompression, so dictionary-aided zstd and LZMA-alone (.lzma)\n",
            "streams are not handled. A concatenated gzip file and a multi-frame zstd\n",
            "file decode fully; for bzip2 only the first stream of a concatenated file\n",
            "comes out (its decoder buffers past the member end) and a second xz\n",
            "stream in the same file is rejected as trailing data.\n",
        ))
        .arg(
            Arg::new("pass-through")
                .short('n')
                .long("no-pass-through")
                .action(ArgAction::SetTrue)
                .help("Fail instead of copying input that has no known magic"),
        )
        .arg(
            Arg::new("files")
                .value_name("FILE")
                .action(ArgAction::Append)
                .num_args(0..),
        )
}

/// Parse the argv tail (without argv[0]). `Err(Some(msg))` is a usage error;
/// `Err(None)` means the app already answered `--help` / `--version`.
fn parse(argv: &[String]) -> Result<Options, Option<String>> {
    // clap wants the command name as the first item of the iterator, and the
    // applet contract hands us the tail without it, so re-supply it.
    let iter = std::iter::once("zcat").chain(argv.iter().map(String::as_str));
    let matches = match uu_app().try_get_matches_from(iter) {
        Ok(m) => m,
        Err(e) => {
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                let _ = e.print();
                return Err(None);
            }
            return Err(Some(e.to_string()));
        }
    };

    Ok(Options {
        pass_through: !matches.get_flag("pass-through"),
        files: matches
            .get_many("files")
            .map(|vals| vals.cloned().collect())
            .unwrap_or_default(),
    })
}

/// Open one operand. `-` — like the absence of any operand — reads standard
/// input; boxing the reader keeps the sniffing generic over files and pipes.
fn open(name: &str) -> io::Result<Box<dyn Read>> {
    if name == "-" {
        Ok(Box::new(io::stdin()))
    } else {
        Ok(Box::new(File::open(name)?))
    }
}

/// Decompress every operand in order. Returns the exit status: 0 when all of
/// them succeeded, 2 if any of them failed — the rest are still tried.
fn run(opts: &Options) -> i32 {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(BUF_SIZE, stdout.lock());
    let mut status = 0;

    let operands = if opts.files.is_empty() {
        vec![String::from("-")]
    } else {
        opts.files.clone()
    };

    for name in &operands {
        let mut source = match open(name) {
            Ok(reader) => Sniffer::new(reader),
            Err(e) => {
                let _ = writeln!(io::stderr(), "zcat: {name}: {e}");
                status = 2;
                continue;
            }
        };

        if let Err(e) = decode(&mut source, &mut out, opts.pass_through) {
            let _ = writeln!(io::stderr(), "zcat: {name}: {e}");
            status = 2;
        }
    }

    if out.flush().is_err() {
        status = 2;
    }
    status
}

/// Stream one input into `out`, member by member.
fn decode<R: Read>(
    source: &mut Sniffer<R>,
    out: &mut impl Write,
    pass_through: bool,
) -> Result<(), String> {
    loop {
        let magic = source.peek(MAGIC_LEN).map_err(|e| e.to_string())?;
        if magic.is_empty() {
            return Ok(()); // clean end of input
        }
        let format = detect(&magic);
        let before = source.delivered;

        match format {
            Format::Gzip => {
                // One member per pass: the `bufread` decoder consumes exactly its
                // own bytes, so the loop can walk multi-member .gz files and even
                // streams that change format between members.
                let mut gz = flate2::bufread::GzDecoder::new(&mut *source);
                io::copy(&mut gz, out).map_err(|e| format!("gzip: {e}"))?;
            }
            Format::Bzip2 => {
                let mut bz = bzip2_rs::DecoderReader::new(&mut *source);
                io::copy(&mut bz, out).map_err(|e| format!("bzip2: {e}"))?;
            }
            Format::Zstd => {
                if is_zstd_skippable(&magic) {
                    skip_zstd_frame(source)?;
                    continue;
                }
                let mut zs = ruzstd::decoding::StreamingDecoder::new(&mut *source)
                    .map_err(|e| format!("zstd: {e}"))?;
                io::copy(&mut zs, out).map_err(|e| format!("zstd: {e}"))?;
            }
            Format::Xz => {
                // The xz decoder is push-based: it writes straight into `out`.
                lzma_rs::xz_decompress(&mut *source, out).map_err(|e| format!("xz: {e}"))?;
            }
            Format::Plain => {
                if !pass_through {
                    return Err("not in a recognised compressed format".to_string());
                }
                // A plain file passes through untouched — real zcat behaviour.
                io::copy(source, out).map_err(|e| e.to_string())?;
                return Ok(());
            }
        }

        if source.delivered == before {
            // No progress and no end of input: a re-detected magic (a lone
            // trailing skippable zstd frame does this) would loop forever.
            return Err(format!("{}: stream did not advance", format.label()));
        }
    }
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() {
        &argv[..]
    } else {
        &argv[1..]
    };
    match parse(tail) {
        Ok(opts) => run(&opts),
        Err(Some(msg)) => {
            let _ = writeln!(io::stderr(), "zcat: {msg}");
            2
        }
        Err(None) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    /// Decode an in-memory stream the way `zcat` decodes a file.
    fn decode_bytes(input: &[u8], pass_through: bool) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let mut source = Sniffer::new(input);
        decode(&mut source, &mut out, pass_through)?;
        Ok(out)
    }

    fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(payload).unwrap();
        enc.finish().unwrap()
    }

    /// One zstandard frame holding an uncompressed ("raw") block: magic, a frame
    /// descriptor with no content size and no checksum, a 128 KiB window, then a
    /// last-block header followed by the payload itself.
    fn zstd_frame(payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(payload.len()).expect("test payload below 4 GiB");
        let header = (1u32 | (size << 3)).to_le_bytes();
        let mut v = Vec::new();
        v.extend_from_slice(&0xFD2F_B528_u32.to_le_bytes());
        v.extend_from_slice(&[0, 7 << 3]);
        v.extend_from_slice(&header[..3]);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn magic_bytes_select_the_format() {
        // The sniff must never look at file names, only at these bytes.
        assert_eq!(detect(&[0x1f, 0x8b, 0x08]), Format::Gzip);
        assert_eq!(detect(b"BZh9"), Format::Bzip2);
        assert_eq!(detect(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]), Format::Xz);
        assert_eq!(detect(&[0x28, 0xb5, 0x2f, 0xfd]), Format::Zstd);
        // Every skippable magic number, and nothing outside the range.
        for byte in 0x50..=0x5f {
            assert_eq!(detect(&[byte, 0x2a, 0x4d, 0x18]), Format::Zstd);
        }
        assert_eq!(detect(&[0x60, 0x2a, 0x4d, 0x18]), Format::Plain);
        assert_eq!(detect(b"plain text"), Format::Plain);
        assert_eq!(detect(&[]), Format::Plain);
    }

    #[test]
    fn gzip_member_decodes_and_plain_input_passes_through() {
        assert_eq!(
            decode_bytes(&gzip(b"hello zcat\n"), true).unwrap(),
            b"hello zcat\n"
        );
        assert_eq!(
            decode_bytes(b"just bytes\n", true).unwrap(),
            b"just bytes\n"
        );
    }

    #[test]
    fn no_pass_through_rejects_uncompressed_input() {
        let err = decode_bytes(b"just bytes\n", false).unwrap_err();
        assert!(err.contains("recognised compressed format"), "{err}");
    }

    #[test]
    fn concatenated_gzip_members_are_all_decoded() {
        let mut stream = gzip(b"one\n");
        stream.extend_from_slice(&gzip(b"two\n"));
        assert_eq!(decode_bytes(&stream, true).unwrap(), b"one\ntwo\n");
    }

    #[test]
    fn members_of_different_formats_are_decoded_in_order() {
        let mut stream = gzip(b"A\n");
        stream.extend_from_slice(&zstd_frame(b"B\n"));
        assert_eq!(decode_bytes(&stream, false).unwrap(), b"A\nB\n");
    }

    #[test]
    fn zstd_skippable_frames_are_stepped_over() {
        let mut stream = Vec::new();
        let body = b"metadata";
        stream.extend_from_slice(&0x184D_2A5F_u32.to_le_bytes());
        stream.extend_from_slice(&(body.len() as u32).to_le_bytes());
        stream.extend_from_slice(body);
        stream.extend_from_slice(&zstd_frame(b"payload\n"));
        assert_eq!(decode_bytes(&stream, true).unwrap(), b"payload\n");
    }

    #[test]
    fn empty_input_yields_no_output() {
        assert!(decode_bytes(&[], true).unwrap().is_empty());
        assert!(decode_bytes(&gzip(b""), true).unwrap().is_empty());
    }

    #[test]
    fn truncated_gzip_is_an_error_not_a_hang() {
        let mut stream = gzip(b"hello zcat world\n");
        stream.truncate(stream.len() - 10);
        assert!(decode_bytes(&stream, true).is_err());
    }

    #[test]
    fn operands_and_flags_are_parsed() {
        let opts = parse(&["a.gz".into(), "-".into(), "b".into()]).unwrap();
        assert_eq!(opts.files, ["a.gz", "-", "b"]);
        assert!(opts.pass_through);

        let opts = parse(["-n".to_string(), "-".to_string()].as_slice()).unwrap();
        assert!(!opts.pass_through);
        assert_eq!(opts.files, ["-"]);

        // Everything after `--` is an operand, even if it looks like a flag.
        let opts = parse(["--".to_string(), "-n".to_string()].as_slice()).unwrap();
        assert_eq!(opts.files, ["-n"]);

        // `--help` is answered by clap itself and is not a usage error.
        assert!(
            parse(["--help".to_string()].as_slice())
                .unwrap_err()
                .is_none()
        );
        assert!(
            parse(["--bogus".to_string()].as_slice())
                .unwrap_err()
                .is_some()
        );
    }

    #[test]
    fn peek_does_not_consume_and_reports_eof() {
        let mut source = Sniffer::new(&b"abcdefghij"[..]);
        assert_eq!(source.peek(6).unwrap(), b"abcdef".to_vec());
        assert_eq!(source.peek(6).unwrap(), b"abcdef".to_vec());
        let mut small = [0u8; 3];
        source.read_exact(&mut small).unwrap();
        assert_eq!(&small, b"abc");
        assert_eq!(source.peek(6).unwrap(), b"defghi".to_vec());
    }
}
