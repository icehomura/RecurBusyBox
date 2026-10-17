// This file is part of RecurBusyBox.
//
// `ps`: report a snapshot of the current processes. A non-interactive subset of
// POSIX `ps`, built only on `sysinfo` (pure Rust, no C bindings).

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, Write as _};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::error::ErrorKind;
use clap::{Arg, ArgAction, Command};
use sysinfo::{Pid, Process, ProcessStatus, System, Uid, Users};

/// Every column we can render, in the classic `ps` order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Col {
    Pid,
    Ppid,
    User,
    Cpu,
    Mem,
    Vsz,
    Rss,
    Stat,
    Start,
    Time,
    Command,
}

const ALL_COLS: [Col; 11] = [
    Col::Pid,
    Col::Ppid,
    Col::User,
    Col::Cpu,
    Col::Mem,
    Col::Vsz,
    Col::Rss,
    Col::Stat,
    Col::Start,
    Col::Time,
    Col::Command,
];

impl Col {
    fn header(self) -> &'static str {
        match self {
            Col::Pid => "PID",
            Col::Ppid => "PPID",
            Col::User => "USER",
            Col::Cpu => "%CPU",
            Col::Mem => "%MEM",
            Col::Vsz => "VSZ",
            Col::Rss => "RSS",
            Col::Stat => "STAT",
            Col::Start => "START",
            Col::Time => "TIME",
            Col::Command => "COMMAND",
        }
    }

    /// Numeric columns are right-aligned, text columns left-aligned.
    fn right_aligned(self) -> bool {
        !matches!(self, Col::User | Col::Stat | Col::Command)
    }

    /// Accept both our header names and the ps-style spellings.
    fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "pid" => Some(Col::Pid),
            "ppid" => Some(Col::Ppid),
            "user" | "uid" => Some(Col::User),
            "cpu" | "%cpu" => Some(Col::Cpu),
            "mem" | "%mem" => Some(Col::Mem),
            "vsz" | "vsize" => Some(Col::Vsz),
            "rss" => Some(Col::Rss),
            "stat" | "state" | "s" => Some(Col::Stat),
            "start" | "stime" | "lstart" => Some(Col::Start),
            "time" => Some(Col::Time),
            "comm" | "command" | "cmd" | "args" => Some(Col::Command),
            _ => None,
        }
    }
}

struct Opts {
    cols: Vec<Col>,
    pids: Option<HashSet<u32>>,
    user: Option<String>,
    no_header: bool,
    sort: Col,
}

struct Row {
    pid: u32,
    ppid: u32,
    user: String,
    cpu: f32,
    mem: f32,
    vsz: u64,
    rss: u64,
    stat: char,
    start_time: u64,
    cpu_ms: u64,
    comm: String,
}

impl Row {
    fn cell(&self, col: Col, now: u64) -> String {
        match col {
            Col::Pid => self.pid.to_string(),
            Col::Ppid => self.ppid.to_string(),
            Col::User => self.user.clone(),
            Col::Cpu => format!("{:.1}", self.cpu),
            Col::Mem => format!("{:.1}", self.mem),
            Col::Vsz => self.vsz.to_string(),
            Col::Rss => self.rss.to_string(),
            Col::Stat => self.stat.to_string(),
            Col::Start => {
                if self.start_time == 0 {
                    String::new()
                } else {
                    format_age(now.saturating_sub(self.start_time))
                }
            }
            Col::Time => format_cpu_time(self.cpu_ms),
            Col::Command => self.comm.clone(),
        }
    }
}

pub fn uu_app() -> Command {
    Command::new("ps")
        .version(uucore::crate_version!())
        .about("Report a snapshot of the current processes.")
        .after_help(
            "Columns (in default order): PID PPID USER %CPU %MEM VSZ RSS STAT \
             START TIME COMMAND.\n\
             START is the process age (elapsed time since it started) formatted \
             HH:MM.\n\
             TIME is accumulated CPU time formatted MM:SS.\n\
             USER is empty when the platform cannot map the process owner.\n\
             -f/--full is accepted for compatibility but the layout is unchanged.",
        )
        .arg(
            Arg::new("every")
                .short('e')
                .long("every")
                .action(ArgAction::SetTrue)
                .help("Show every process (always on; accepted for compatibility)"),
        )
        .arg(
            Arg::new("full")
                .short('f')
                .long("full")
                .action(ArgAction::SetTrue)
                .help("Full listing (accepted; layout is unchanged)"),
        )
        .arg(
            Arg::new("format")
                .short('o')
                .long("format")
                .value_name("COLS")
                .value_delimiter(',')
                .action(ArgAction::Append)
                .help("Comma-separated subset/order of columns"),
        )
        .arg(
            Arg::new("pid")
                .short('p')
                .long("pid")
                .value_name("PID[,PID...]")
                .value_delimiter(',')
                .action(ArgAction::Append)
                .help("Restrict output to these process IDs"),
        )
        .arg(
            Arg::new("user")
                .short('u')
                .long("user")
                .value_name("NAME")
                .action(ArgAction::Set)
                .help("Restrict output to processes owned by this user"),
        )
        .arg(
            Arg::new("no-header")
                .long("no-header")
                .action(ArgAction::SetTrue)
                .help("Do not print the header row"),
        )
        .arg(
            Arg::new("sort")
                .long("sort")
                .value_name("KEY")
                .default_value("pid")
                .help("Sort by pid|ppid|cpu|mem|rss|start|time|comm (cpu/mem descending)"),
        )
}

fn parse(args: impl uucore::Args) -> Result<Opts, i32> {
    let argv: Vec<OsString> = args.collect();
    let argv = if argv.is_empty() {
        vec![OsString::from("ps")]
    } else {
        argv
    };

    let matches = match uu_app().try_get_matches_from(argv) {
        Ok(m) => m,
        Err(e) => {
            let _ = e.print();
            return Err(match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                _ => 2,
            });
        }
    };

    let cols = match matches.get_many::<String>("format") {
        Some(values) => {
            let mut cols = Vec::new();
            for token in values {
                match Col::parse(token) {
                    Some(c) if !cols.contains(&c) => cols.push(c),
                    Some(_) => {}
                    None => {
                        let _ = writeln!(io::stderr(), "ps: unknown column: {token}");
                        return Err(2);
                    }
                }
            }
            cols
        }
        None => ALL_COLS.to_vec(),
    };

    let pids = match matches.get_many::<String>("pid") {
        Some(values) => {
            let mut set = HashSet::new();
            for token in values {
                match token.trim().parse::<u32>() {
                    Ok(p) => {
                        set.insert(p);
                    }
                    Err(_) => {
                        let _ = writeln!(io::stderr(), "ps: invalid pid: {token}");
                        return Err(2);
                    }
                }
            }
            Some(set)
        }
        None => None,
    };

    let sort = match matches.get_one::<String>("sort") {
        Some(key) => match Col::parse(key) {
            Some(c @ (Col::Pid | Col::Ppid | Col::Cpu | Col::Mem | Col::Rss | Col::Start
            | Col::Time | Col::Command)) => c,
            _ => {
                let _ = writeln!(io::stderr(), "ps: invalid sort key: {key}");
                return Err(2);
            }
        },
        None => Col::Pid,
    };

    Ok(Opts {
        cols,
        pids,
        user: matches.get_one::<String>("user").cloned(),
        no_header: matches.get_flag("no-header"),
        sort,
    })
}

fn run(opts: &Opts) -> i32 {
    let mut sys = System::new_all();
    // CPU usage is a delta between two refreshes, so take a second sample.
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_all();

    let users = Users::new_with_refreshed_list();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Resolve the requested user name to the set of matching uids.
    let wanted_uids: Option<Vec<Uid>> = opts.user.as_ref().map(|name| {
        users
            .list()
            .iter()
            .filter(|u| u.name().eq_ignore_ascii_case(name))
            .map(|u| u.id().clone())
            .collect()
    });

    let total_mem = sys.total_memory().max(1);

    let mut rows: Vec<Row> = sys
        .processes()
        .values()
        .filter(|p| {
            opts.pids
                .as_ref()
                .is_none_or(|set| set.contains(&p.pid().as_u32()))
        })
        .filter(|p| match &wanted_uids {
            Some(uids) => p.user_id().is_some_and(|uid| uids.contains(uid)),
            None => true,
        })
        .map(|p| build_row(p, &users, total_mem, now))
        .collect();

    rows.sort_by(|a, b| cmp_rows(a, b, opts.sort));

    print_table(opts, &rows, now);
    0
}

fn build_row(p: &Process, users: &Users, total_mem: u64, _now: u64) -> Row {
    let user = p
        .user_id()
        .and_then(|uid| users.get_user_by_id(uid))
        .map(|u| u.name().to_string())
        .unwrap_or_default();

    let mem = (p.memory() as f64 / total_mem as f64 * 100.0) as f32;

    let cmd = p.cmd();
    let comm = if cmd.is_empty() {
        p.name().to_string_lossy().into_owned()
    } else {
        cmd.iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let comm = sanitize(&comm);

    Row {
        pid: p.pid().as_u32(),
        ppid: p.parent().map_or(0, Pid::as_u32),
        user,
        cpu: p.cpu_usage(),
        mem,
        vsz: p.virtual_memory(),
        rss: p.memory(),
        stat: stat_letter(p.status()),
        start_time: p.start_time(),
        cpu_ms: p.accumulated_cpu_time(),
        comm,
    }
}

fn cmp_rows(a: &Row, b: &Row, key: Col) -> Ordering {
    let primary = match key {
        Col::Pid => a.pid.cmp(&b.pid),
        Col::Ppid => a.ppid.cmp(&b.ppid),
        // cpu/mem are descending (biggest consumers first).
        Col::Cpu => b.cpu.partial_cmp(&a.cpu).unwrap_or(Ordering::Equal),
        Col::Mem => b.mem.partial_cmp(&a.mem).unwrap_or(Ordering::Equal),
        Col::Rss => a.rss.cmp(&b.rss),
        Col::Start => a.start_time.cmp(&b.start_time),
        Col::Time => a.cpu_ms.cmp(&b.cpu_ms),
        Col::Command => a.comm.cmp(&b.comm),
        // Not reachable via --sort, but keep the match total.
        Col::User => a.user.cmp(&b.user),
        Col::Vsz => a.vsz.cmp(&b.vsz),
        Col::Stat => a.stat.cmp(&b.stat),
    };
    primary.then_with(|| a.pid.cmp(&b.pid))
}

fn print_table(opts: &Opts, rows: &[Row], now: u64) {
    let headers: Vec<&str> = opts.cols.iter().map(|c| c.header()).collect();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| opts.cols.iter().map(|c| r.cell(*c, now)).collect())
        .collect();

    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let out = io::stdout();
    let mut out = out.lock();

    if !opts.no_header {
        let _ = writeln!(out, "{}", format_row(&headers, &widths, &opts.cols));
    }
    for row in &cells {
        let refs: Vec<&str> = row.iter().map(String::as_str).collect();
        let _ = writeln!(out, "{}", format_row(&refs, &widths, &opts.cols));
    }
}

fn format_row(cells: &[&str], widths: &[usize], cols: &[Col]) -> String {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let width = widths[i];
        if cols[i].right_aligned() {
            line.push_str(&format!("{cell:>width$}"));
        } else {
            line.push_str(&format!("{cell:<width$}"));
        }
    }
    line
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn stat_letter(status: ProcessStatus) -> char {
    match status {
        ProcessStatus::Run => 'R',
        ProcessStatus::Sleep => 'S',
        ProcessStatus::Stop | ProcessStatus::Suspended | ProcessStatus::LockBlocked => 'T',
        ProcessStatus::Zombie => 'Z',
        ProcessStatus::Idle => 'I',
        _ => '?',
    }
}

/// Elapsed age in `HH:MM` (hours not reduced modulo 24).
fn format_age(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    format!("{hours:02}:{minutes:02}")
}

/// Accumulated CPU time in `MM:SS` (minutes not reduced modulo 60).
fn format_cpu_time(ms: u64) -> String {
    let total = ms / 1000;
    format!("{:02}:{:02}", total / 60, total % 60)
}

pub fn uumain(args: impl uucore::Args) -> i32 {
    match parse(args) {
        Ok(opts) => run(&opts),
        Err(code) => code,
    }
}
