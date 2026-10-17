// RecurBusyBox - a multi-call binary of high-performance Rust tools.
//
// `xsv`: a focused subset of the `xsv` CSV Swiss-army tool. The CSV dialect is
// handled by the `csv` crate (RFC 4180 quoting, arbitrary single-byte
// delimiters) and regexp matching by the `regex` crate; the subcommands
// (`headers`, `select`, `search`, `freq`, `stats`, `sample`, `tojson`) are thin
// layers over a `Table` that is read eagerly, because every operation either
// needs the whole file (`freq`, `stats`, `sample`) or rewrites the header row
// (`select`, `search`).

use std::collections::BTreeMap;
use std::io::{self, Read as _, Write as _};

use clap::{Arg, ArgAction, Command};
use csv::{ReaderBuilder, Writer, WriterBuilder};
use regex::RegexBuilder;

/// One parsed CSV file: a header row plus every data row. Ragged rows are
/// padded out to the header width, because real-world CSV is messy.
struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn width(&self) -> usize {
        self.headers.len()
    }
}

/// The work a successful parse of argv asked for.
enum Kind {
    Headers {
        file: Option<String>,
    },
    Select {
        columns: String,
        no_headers: bool,
        file: Option<String>,
    },
    Search {
        pattern: String,
        ignore_case: bool,
        file: Option<String>,
    },
    Freq {
        column: String,
        file: Option<String>,
    },
    Stats {
        file: Option<String>,
    },
    Sample {
        n: usize,
        seed: Option<u64>,
        file: Option<String>,
    },
    ToJson {
        file: Option<String>,
    },
}

struct Options {
    delimiter: u8,
    kind: Kind,
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// `-d/--delimiter`, shared by every subcommand. The value is a single
/// character or one of the escapes `\t`, `\s`, `\;`, `\,`, `\n`, `\0`, plus the
/// words `TAB`/`tab`.
fn delimiter_arg() -> Arg {
    Arg::new("delimiter")
        .short('d')
        .long("delimiter")
        .value_name("CHAR")
        .allow_hyphen_values(true)
        .help("Column delimiter: one character, or \\t for TSV (default: ,)")
}

/// The optional input file; stdin when omitted or given as `-`.
fn file_arg() -> Arg {
    Arg::new("file").value_name("FILE").help("Input file (default: stdin)")
}

pub fn uu_app() -> Command {
    Command::new("xsv")
        .version(uucore::crate_version!())
        .about("Slice, search and summarise CSV/TSV data (xsv subset).")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true)
        .subcommand(
            Command::new("headers")
                .about("List every column name with its index, then the column count")
                .arg(delimiter_arg())
                .arg(file_arg().required(true)),
        )
        .subcommand(
            Command::new("select")
                .about("Keep only the named columns, by name or 0-based index")
                .long_about(
                    "Keep only the named columns, in the order given.\n\n\
                     COLUMNS is a comma-separated list of column names or 0-based indices.\n\
                     A '!' prefix drops a column instead: `xsv select '!secret' data.csv` keeps\n\
                     everything except 'secret'; `a,b,!c` keeps a and b but never c.",
                )
                .arg(Arg::new("columns").value_name("COLUMNS").required(true))
                .arg(delimiter_arg())
                .arg(
                    Arg::new("no-headers")
                        .short('H')
                        .long("no-headers")
                        .action(ArgAction::SetTrue)
                        .help("Treat the first row as data: only indices select columns, and no header row is written"),
                )
                .arg(file_arg()),
        )
        .subcommand(
            Command::new("search")
                .about("Print the header row plus every row where some field matches a regexp")
                .arg(Arg::new("regexp").value_name("REGEXP").required(true))
                .arg(delimiter_arg())
                .arg(
                    Arg::new("ignore-case")
                        .short('i')
                        .long("ignore-case")
                        .action(ArgAction::SetTrue)
                        .help("Fold case when matching"),
                )
                .arg(file_arg()),
        )
        .subcommand(
            Command::new("freq")
                .about("Value frequency table for one column, as count,value rows")
                .arg(
                    Arg::new("column")
                        .value_name("COLUMN")
                        .required(true)
                        .help("Column name or 0-based index"),
                )
                .arg(delimiter_arg())
                .arg(file_arg()),
        )
        .subcommand(
            Command::new("stats")
                .about("Per-column statistics: type, min, max, sum, mean, stddev, null count")
                .arg(delimiter_arg())
                .arg(file_arg()),
        )
        .subcommand(
            Command::new("sample")
                .about("Take N rows: evenly spaced by default, reservoir sampled with --seed")
                .arg(
                    Arg::new("size")
                        .value_name("N")
                        .required(true)
                        .help("Number of rows to emit"),
                )
                .arg(delimiter_arg())
                .arg(
                    Arg::new("seed")
                        .long("seed")
                        .value_name("N")
                        .allow_hyphen_values(true)
                        .help("Use a seeded random reservoir sample instead of the deterministic stride"),
                )
                .arg(file_arg()),
        )
        .subcommand(
            Command::new("tojson")
                .about("Convert every row to a JSON object keyed by column name")
                .arg(delimiter_arg())
                .arg(file_arg()),
        )
}

/// Turn argv (without argv[0]) into [`Options`].
/// `Err(Some(msg))` is a usage error (exit 2, `msg` may be empty when clap has
/// already explained itself); `Err(None)` means `--help`/`--version` was
/// handled by clap.
fn parse(argv: &[String]) -> Result<Options, Option<String>> {
    let mut cmdline: Vec<String> = Vec::with_capacity(argv.len() + 1);
    cmdline.push("xsv".to_string());
    cmdline.extend_from_slice(argv);

    let matches = match uu_app().try_get_matches_from(&cmdline) {
        Ok(matches) => matches,
        Err(err) => {
            let _ = err.print();
            return if is_clean_exit(&err) {
                Err(None)
            } else {
                Err(Some(String::new()))
            };
        }
    };
    let (name, sub) = matches
        .subcommand()
        .ok_or_else(|| usage("a subcommand is required"))?;

    let delimiter = match sub.get_one::<String>("delimiter") {
        Some(raw) => parse_delimiter(raw)?,
        None => b',',
    };
    let file = sub.get_one::<String>("file").cloned();

    let kind = match name {
        "headers" => Kind::Headers { file },
        "select" => Kind::Select {
            columns: sub
                .get_one::<String>("columns")
                .cloned()
                .ok_or_else(|| usage("select needs a COLUMNS argument"))?,
            no_headers: sub.get_flag("no-headers"),
            file,
        },
        "search" => Kind::Search {
            pattern: sub
                .get_one::<String>("regexp")
                .cloned()
                .ok_or_else(|| usage("search needs a REGEXP argument"))?,
            ignore_case: sub.get_flag("ignore-case"),
            file,
        },
        "freq" => Kind::Freq {
            column: sub
                .get_one::<String>("column")
                .cloned()
                .ok_or_else(|| usage("freq needs a COLUMN argument"))?,
            file,
        },
        "stats" => Kind::Stats { file },
        "sample" => Kind::Sample {
            n: parse_usize(
                sub.get_one::<String>("size")
                    .ok_or_else(|| usage("sample needs a row count"))?,
                "sample size",
            )?,
            seed: sub
                .get_one::<String>("seed")
                .map(|raw| parse_usize64(raw, "seed"))
                .transpose()?,
            file,
        },
        "tojson" => Kind::ToJson { file },
        other => return Err(Some(usage(&format!("unknown subcommand '{other}'")))),
    };

    Ok(Options { delimiter, kind })
}

/// clap handled the request itself (help/version); anything else is a usage
/// error that already went to stderr.
fn is_clean_exit(err: &clap::error::Error) -> bool {
    matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp
            | clap::error::ErrorKind::DisplayVersion
            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    )
}

fn parse_usize(raw: &str, what: &str) -> Result<usize, String> {
    raw.parse::<usize>()
        .map_err(|_| usage(&format!("invalid {what} '{raw}' (expected a non-negative integer)")))
}

fn parse_usize64(raw: &str, what: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .map_err(|_| usage(&format!("invalid {what} '{raw}' (expected a non-negative integer)")))
}

fn usage(what: &str) -> String {
    format!("{what} (usage: xsv <COMMAND> [ARGS])")
}

/// Interpret a `-d` value as a single ASCII delimiter byte.
fn parse_delimiter(raw: &str) -> Result<u8, String> {
    let word = match raw {
        "TAB" | "tab" => "\\t",
        other => other,
    };
    let mut chars = word.chars();
    let byte = match (chars.next(), chars.next()) {
        (Some(c), None) => c,
        (Some('\\'), Some(c)) => match c {
            't' => '\t',
            's' => ' ',
            'n' => '\n',
            'r' => '\r',
            '0' => '\0',
            other => other,
        },
        _ => return Err(usage(&format!("invalid delimiter '{raw}'"))),
    };
    if !byte.is_ascii() {
        return Err(usage(&format!(
            "delimiter '{raw}' must be a single ASCII character"
        )));
    }
    Ok(u8::try_from(byte).expect("ASCII character fits in a byte"))
}

// ---------------------------------------------------------------------------
// Reading and writing CSV
// ---------------------------------------------------------------------------

/// Read `file` (or stdin when `None` or `-`). With `header == false` the first
/// line is kept as data and synthetic `column0..` names stand in for headers.
fn read_table(file: Option<&str>, delimiter: u8, header: bool) -> Result<Table, String> {
    let source = input(file)?;
    let mut reader = ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(header)
        .flexible(true)
        .from_reader(source);

    // With `has_headers(false)` the `csv` crate synthesises `column0..` names
    // from the first record and still yields that record as data.
    let headers: Vec<String> = match reader.headers() {
        Ok(h) => h.iter().map(str::to_string).collect(),
        Err(e) => return Err(format!("failed to read header row: {e}")),
    };
    if headers.is_empty() {
        return Err("the input has no columns".to_string());
    }
    let width = headers.len();

    let mut rows: Vec<Vec<String>> = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| format!("failed to read row {}: {e}", rows.len() + 1))?;
        let mut row: Vec<String> = record.iter().map(str::to_string).collect();
        // Ragged rows are padded and truncated to the header width.
        row.resize(width, String::new());
        row.truncate(width);
        rows.push(row);
    }

    Ok(Table { headers, rows })
}

fn input(file: Option<&str>) -> Result<Box<dyn io::Read>, String> {
    match file {
        None | Some("-") => {
            let mut buf = Vec::new();
            io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| format!("failed to read stdin: {e}"))?;
            Ok(Box::new(io::Cursor::new(buf)))
        }
        Some(path) => std::fs::File::open(path)
            .map(|f| Box::new(f) as Box<dyn io::Read>)
            .map_err(|e| format!("failed to open '{path}': {e}")),
    }
}

fn writer(delimiter: u8) -> Writer<Box<dyn io::Write>> {
    WriterBuilder::new()
        .delimiter(delimiter)
        .from_writer(Box::new(io::stdout()) as Box<dyn io::Write>)
}

fn write_row(out: &mut Writer<Box<dyn io::Write>>, cells: &[&str]) -> Result<(), String> {
    out.write_record(cells)
        .map_err(|e| format!("failed to write row: {e}"))
}

fn refs(row: &[String]) -> Vec<&str> {
    row.iter().map(String::as_str).collect()
}

// ---------------------------------------------------------------------------
// Column selection
// ---------------------------------------------------------------------------

/// A column reference: a 0-based index (always allowed) or a header name
/// (`allow_names` is false under `-H/--no-headers`, where names are ambiguous).
fn resolve_column(headers: &[String], name: &str, allow_names: bool) -> Result<usize, String> {
    if let Ok(idx) = name.parse::<usize>() {
        return if idx < headers.len() {
            Ok(idx)
        } else {
            Err(usage(&format!(
                "column index {idx} is out of range ({} columns)",
                headers.len()
            )))
        };
    }
    if !allow_names {
        return Err(usage(&format!(
            "column '{name}' is a name but -H/--no-headers only accepts indices"
        )));
    }
    headers
        .iter()
        .position(|h| h == name)
        .ok_or_else(|| usage(&format!("no column named '{name}' (available: {})", headers.join(", "))))
}

/// The column indices `xsv select` should emit, in order. Positive specs pick
/// (and order) columns; `!`-prefixed specs remove them again. With only `!`
/// specs the starting point is every column.
fn select_spec(spec: &str, headers: &[String], allow_names: bool) -> Result<Vec<usize>, String> {
    let mut keeps: Vec<String> = Vec::new();
    let mut drops: Vec<String> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(usage(&format!("empty column in select list '{spec}'")));
        }
        match part.strip_prefix('!') {
            Some(neg) if !neg.is_empty() => drops.push(neg.to_string()),
            None => keeps.push(part.to_string()),
            Some(_) => return Err(usage(&format!("'!' with no column in '{spec}'"))),
        }
    }
    if keeps.is_empty() {
        keeps = headers.to_vec();
    }

    let mut indices: Vec<usize> = Vec::with_capacity(keeps.len());
    for keep in &keeps {
        indices.push(resolve_column(headers, keep, allow_names)?);
    }
    for drop in &drops {
        let dropped = resolve_column(headers, drop, allow_names)?;
        indices.retain(|&i| i != dropped);
    }
    if indices.is_empty() {
        return Err(usage(&format!("select '{spec}' matched no columns")));
    }
    Ok(indices)
}

// ---------------------------------------------------------------------------
// Numeric handling for `stats`
// ---------------------------------------------------------------------------

/// Accepts the number shapes spreadsheets and `xsv` produce: plain floats,
/// signs, scientific notation, and optional `_`/`,` thousands separators.
fn parse_number(cell: &str) -> Option<f64> {
    let trimmed = cell.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = trimmed.parse::<f64>() {
        return Some(v);
    }
    let stripped: String = trimmed
        .chars()
        .filter(|c| *c != ',' && *c != '_')
        .collect();
    stripped.parse::<f64>().ok()
}

/// Render a float the way `xsv stats` does: integral values without a decimal
/// point, otherwise Rust's shortest round-trip form.
fn format_number(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// One column's aggregate.
#[derive(Default)]
struct ColumnStats {
    /// Every parseable value, in file order.
    values: Vec<f64>,
    /// Non-empty cells that did not parse as numbers.
    non_numeric: usize,
    /// Empty cells.
    nulls: usize,
    /// Cells seen in total.
    count: usize,
}

impl ColumnStats {
    fn numeric(&self) -> bool {
        !self.values.is_empty() && self.non_numeric == 0
    }

    fn min(&self) -> Option<f64> {
        self.values.iter().copied().reduce(f64::min)
    }

    fn max(&self) -> Option<f64> {
        self.values.iter().copied().reduce(f64::max)
    }

    fn sum(&self) -> f64 {
        self.values.iter().sum()
    }

    fn mean(&self) -> Option<f64> {
        if self.values.is_empty() {
            None
        } else {
            Some(self.sum() / self.values.len() as f64)
        }
    }

    /// Sample standard deviation (n-1 denominator); 0.0 for a single value.
    fn stddev(&self) -> Option<f64> {
        let mean = self.mean()?;
        if self.values.len() < 2 {
            return Some(0.0);
        }
        let variance = self
            .values
            .iter()
            .map(|v| (v - mean) * (v - mean))
            .sum::<f64>()
            / (self.values.len() - 1) as f64;
        Some(variance.sqrt())
    }
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

fn cmd_headers(table: &Table) {
    let pad = table.width().to_string().len();
    for (i, name) in table.headers.iter().enumerate() {
        println!("{i:>pad$}. {name}");
    }
    println!("{} columns", table.width());
}

fn cmd_select(table: &Table, spec: &str, no_headers: bool, delimiter: u8) -> Result<(), String> {
    let indices = select_spec(spec, &table.headers, !no_headers)?;
    let mut out = writer(delimiter);
    if !no_headers {
        let cells: Vec<&str> = indices.iter().map(|&i| table.headers[i].as_str()).collect();
        write_row(&mut out, &cells)?;
    }
    for row in &table.rows {
        let cells: Vec<&str> = indices.iter().map(|&i| row[i].as_str()).collect();
        write_row(&mut out, &cells)?;
    }
    out.flush().map_err(|e| e.to_string())
}

fn cmd_search(table: &Table, pattern: &str, ignore_case: bool, delimiter: u8) -> Result<(), String> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| format!("invalid regexp '{pattern}': {e}"))?;

    let mut out = writer(delimiter);
    write_row(&mut out, &refs(&table.headers))?;
    for row in &table.rows {
        if row.iter().any(|field| re.is_match(field)) {
            write_row(&mut out, &refs(row))?;
        }
    }
    out.flush().map_err(|e| e.to_string())
}

fn cmd_freq(table: &Table, column: &str, delimiter: u8) -> Result<(), String> {
    let idx = resolve_column(&table.headers, column, true)?;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in &table.rows {
        *counts.entry(row[idx].as_str()).or_insert(0) += 1;
    }

    let mut pairs: Vec<(&str, usize)> = counts.iter().map(|(v, c)| (*v, *c)).collect();
    // Most frequent first; ties fall back to the value so the order is stable.
    pairs.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let mut out = writer(delimiter);
    write_row(&mut out, &["count", "value"])?;
    for (value, count) in pairs {
        write_row(&mut out, &[&count.to_string(), value])?;
    }
    out.flush().map_err(|e| e.to_string())
}

/// Tally every column in one pass: numeric values, non-numeric cells, empty
/// cells, and the total number of cells seen.
fn column_stats(table: &Table) -> Vec<ColumnStats> {
    let width = table.width();
    let mut stats: Vec<ColumnStats> = (0..width).map(|_| ColumnStats::default()).collect();
    for row in &table.rows {
        for (col, cell) in stats.iter_mut().zip(row.iter()) {
            col.count += 1;
            if cell.is_empty() {
                col.nulls += 1;
            } else if let Some(v) = parse_number(cell) {
                col.values.push(v);
            } else {
                col.non_numeric += 1;
            }
        }
    }
    stats
}

fn cmd_stats(table: &Table, delimiter: u8) -> Result<(), String> {
    let stats = column_stats(table);

    let mut out = writer(delimiter);
    write_row(
        &mut out,
        &[
            "field", "type", "min", "max", "sum", "mean", "stddev", "count", "null_count",
        ],
    )?;
    for (i, col) in stats.iter().enumerate() {
        let numeric = col.numeric();
        let blank = String::new();
        let numbers = if numeric {
            [
                col.min().map(format_number).unwrap_or_default(),
                col.max().map(format_number).unwrap_or_default(),
                format_number(col.sum()),
                col.mean().map(format_number).unwrap_or_default(),
                col.stddev().map(format_number).unwrap_or_default(),
            ]
        } else {
            [blank.clone(), blank.clone(), blank.clone(), blank.clone(), String::new()]
        };
        let [min, max, sum, mean, stddev] = numbers;
        write_row(
            &mut out,
            &[
                table.headers[i].as_str(),
                if numeric { "Number" } else { "String" },
                &min,
                &max,
                &sum,
                &mean,
                &stddev,
                &col.count.to_string(),
                &col.nulls.to_string(),
            ],
        )?;
    }
    out.flush().map_err(|e| e.to_string())
}

fn cmd_sample(table: &Table, n: usize, seed: Option<u64>, delimiter: u8) -> Result<(), String> {
    if n == 0 {
        return Err(usage("sample size must be at least 1"));
    }
    let total = table.rows.len();
    let indices: Vec<usize> = match seed {
        None => stride_indices(n, total),
        Some(s) => reservoir_indices(n, total, s),
    };

    let mut out = writer(delimiter);
    write_row(&mut out, &refs(&table.headers))?;
    for i in indices {
        write_row(&mut out, &refs(&table.rows[i]))?;
    }
    out.flush().map_err(|e| e.to_string())
}

/// The default sample: `floor(i * total / n)` for each `i` in `0..n`, which
/// always yields exactly `min(n, total)` distinct rows spread end to end across
/// the file without any randomness.
fn stride_indices(n: usize, total: usize) -> Vec<usize> {
    if total <= n {
        (0..total).collect()
    } else {
        (0..n).map(|i| i * total / n).collect()
    }
}

/// Reservoir sampling (Algorithm R) over `0..total`, keeping `n` indices in
/// original order, driven by a deterministic generator so a given seed always
/// replays the same sample.
fn reservoir_indices(n: usize, total: usize, seed: u64) -> Vec<usize> {
    let n = n.min(total);
    let mut rng = XorShift64Star::new(seed);
    let mut reservoir: Vec<usize> = (0..n).collect();
    for i in n..total {
        let slot = (rng.next() % (i as u64 + 1)) as usize;
        if slot < n {
            reservoir[slot] = i;
        }
    }
    reservoir.sort_unstable();
    reservoir
}

/// A deterministic 64-bit xorshift generator (the `xorshift64*` variant), used
/// for `--seed` so results are reproducible across platforms.
struct XorShift64Star {
    state: u64,
}

impl XorShift64Star {
    fn new(seed: u64) -> Self {
        Self {
            // A zero state would stick at zero.
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn cmd_tojson(table: &Table) -> Result<(), String> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if out.write_all(b"[\n").is_err() {
        return Ok(());
    }
    for (k, row) in table.rows.iter().enumerate() {
        let mut obj = String::from("  {");
        for (i, name) in table.headers.iter().enumerate() {
            if i > 0 {
                obj.push_str(", ");
            }
            obj.push_str(&json_string(name));
            obj.push_str(": ");
            obj.push_str(&json_string(&row[i]));
        }
        obj.push('}');
        if k + 1 < table.rows.len() {
            obj.push(',');
        }
        if writeln!(out, "{obj}").is_err() {
            return Ok(());
        }
    }
    out.write_all(b"]\n")
        .map_err(|e| format!("failed to write JSON: {e}"))
}

/// Quote a string as a JSON string literal: the escapes JSON requires, with
/// control characters as `\u00xx`.
fn json_string(s: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

fn run(opts: &Options) -> Result<(), String> {
    let delimiter = opts.delimiter;
    match &opts.kind {
        Kind::Headers { file } => {
            cmd_headers(&read_table(file.as_deref(), delimiter, true)?);
            Ok(())
        }
        Kind::Select {
            columns,
            no_headers,
            file,
        } => cmd_select(
            &read_table(file.as_deref(), delimiter, !*no_headers)?,
            columns,
            *no_headers,
            delimiter,
        ),
        Kind::Search {
            pattern,
            ignore_case,
            file,
        } => cmd_search(
            &read_table(file.as_deref(), delimiter, true)?,
            pattern,
            *ignore_case,
            delimiter,
        ),
        Kind::Freq { column, file } => cmd_freq(
            &read_table(file.as_deref(), delimiter, true)?,
            column,
            delimiter,
        ),
        Kind::Stats { file } => cmd_stats(&read_table(file.as_deref(), delimiter, true)?, delimiter),
        Kind::Sample { n, seed, file } => cmd_sample(
            &read_table(file.as_deref(), delimiter, true)?,
            *n,
            *seed,
            delimiter,
        ),
        Kind::ToJson { file } => cmd_tojson(&read_table(file.as_deref(), delimiter, true)?),
    }
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    let argv: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let tail = if argv.is_empty() { &argv[..] } else { &argv[1..] };
    match parse(tail) {
        Ok(opts) => match run(&opts) {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(io::stderr(), "xsv: {e}");
                2
            }
        },
        Err(Some(msg)) => {
            if !msg.is_empty() {
                let _ = writeln!(io::stderr(), "xsv: {msg}");
            }
            2
        }
        Err(None) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        Table {
            headers: ["name", "age", "city"].map(str::to_string).to_vec(),
            rows: vec![
                ["alice", "30", "london"].map(str::to_string).to_vec(),
                ["bob", "25", "paris"].map(str::to_string).to_vec(),
                ["carol", "35", "london"].map(str::to_string).to_vec(),
            ],
        }
    }

    #[test]
    fn delimiter_accepts_escapes() {
        assert_eq!(parse_delimiter(",").unwrap(), b',');
        assert_eq!(parse_delimiter("\\t").unwrap(), b'\t');
        assert_eq!(parse_delimiter("\t").unwrap(), b'\t');
        assert_eq!(parse_delimiter("TAB").unwrap(), b'\t');
        assert_eq!(parse_delimiter(";").unwrap(), b';');
        assert_eq!(parse_delimiter("\\|").unwrap(), b'|');
        assert!(parse_delimiter("").is_err());
        assert!(parse_delimiter("ab").is_err());
        assert!(parse_delimiter("，").is_err());
    }

    #[test]
    fn select_by_name_and_index() {
        let t = table();
        assert_eq!(select_spec("name,city", &t.headers, true).unwrap(), vec![0, 2]);
        assert_eq!(select_spec("city,name", &t.headers, true).unwrap(), vec![2, 0]);
        assert_eq!(select_spec("1,0", &t.headers, true).unwrap(), vec![1, 0]);
    }

    #[test]
    fn select_negation() {
        let t = table();
        assert_eq!(select_spec("!age", &t.headers, true).unwrap(), vec![0, 2]);
        assert_eq!(select_spec("name,age,city,!age", &t.headers, true).unwrap(), vec![0, 2]);
        assert!(select_spec("!missing", &t.headers, true).is_err());
        assert!(select_spec("!name,!age,!city", &t.headers, true).is_err());
    }

    #[test]
    fn select_rejects_bad_refs() {
        let t = table();
        assert!(select_spec("nope", &t.headers, true).is_err());
        assert!(select_spec("9", &t.headers, true).is_err());
        assert!(select_spec("age,", &t.headers, true).is_err());
        // Under -H, names are not resolvable.
        assert_eq!(select_spec("2,0", &t.headers, false).unwrap(), vec![2, 0]);
        assert!(select_spec("age", &t.headers, false).is_err());
    }

    #[test]
    fn number_parsing() {
        assert_eq!(parse_number("30"), Some(30.0));
        assert_eq!(parse_number("-1.5"), Some(-1.5));
        assert_eq!(parse_number(" 2e3 "), Some(2000.0));
        assert_eq!(parse_number("1,234.5"), Some(1234.5));
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("london"), None);
    }

    #[test]
    fn number_formatting_drops_redundant_zeros() {
        assert_eq!(format_number(30.0), "30");
        assert_eq!(format_number(-1.5), "-1.5");
        assert_eq!(format_number(1.0e20), "100000000000000000000");
    }

    #[test]
    fn stats_aggregates() {
        let t = table();
        let cols = column_stats(&t);
        assert!(!cols[0].numeric());
        assert!(cols[1].numeric());
        assert_eq!(cols[1].min(), Some(25.0));
        assert_eq!(cols[1].max(), Some(35.0));
        assert_eq!(cols[1].sum(), 90.0);
        assert_eq!(cols[1].mean(), Some(30.0));
        assert_eq!(cols[1].stddev().map(|v| (v * 100.0).round() / 100.0), Some(5.0));
        assert_eq!(cols[0].nulls, 0);
        assert_eq!(cols[1].count, 3);
    }

    #[test]
    fn mixed_column_is_not_numeric() {
        let mut t = table();
        t.rows[1][1] = String::from("twenty-five");
        let cols = column_stats(&t);
        assert!(!cols[1].numeric());
        t.rows[1][1] = String::new();
        let cols = column_stats(&t);
        assert!(cols[1].numeric());
        assert_eq!(cols[1].nulls, 1);
        assert_eq!(cols[1].min(), Some(30.0));
    }

    #[test]
    fn stride_sample_is_evenly_spaced_and_clamped() {
        assert_eq!(stride_indices(2, 5), vec![0, 2]);
        assert_eq!(stride_indices(3, 6), vec![0, 2, 4]);
        assert_eq!(stride_indices(10, 3), vec![0, 1, 2]);
    }

    #[test]
    fn json_escapes() {
        assert_eq!(json_string("ok"), "\"ok\"");
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn reservoir_sample_is_deterministic_and_ordered() {
        let a = reservoir_indices(2, 100, 7);
        let b = reservoir_indices(2, 100, 7);
        assert_eq!(a, b);
        assert_eq!(a.len(), 2);
        assert!(a.windows(2).all(|w| w[0] < w[1]));
        assert!(a.iter().all(|&i| i < 100));
        assert_eq!(reservoir_indices(5, 3, 1), vec![0, 1, 2]);
    }
}
