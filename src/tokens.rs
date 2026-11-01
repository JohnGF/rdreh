//! `rdr tokens`: token usage per agent tool, read from each tool's local data.
//!
//! - Claude Code: `~/.claude/projects/**/*.jsonl` (or `$CLAUDE_CONFIG_DIR`).
//!   Usage per assistant message; streamed blocks repeat the same message id
//!   with identical usage, so the first occurrence counts.
//! - OpenCode: `$XDG_DATA_HOME/opencode/opencode.db`, `message.data.tokens`.
//! - agy (Antigravity CLI): `~/.gemini/antigravity-cli/conversations/*.db`.
//!   Unofficial: usage is decoded from the protobuf in `gen_metadata.data` at
//!   field path 1.4 (2 = input, 3 = output, 5 = cached). Records carry no
//!   timestamp, so usage is dated by the conversation file's mtime.
//!
//! Scanning is incremental: Claude files are read from their last offset, agy
//! databases only when their mtime changes. The scanner state is cached on
//! disk (`$XDG_CACHE_HOME/rdr/tokens.json`), so a new `rdr tokens` only reads
//! what changed since the last run instead of the whole history.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub calls: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.calls += o.calls;
    }
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
    /// Everything the model read: uncached input plus cache reads and writes.
    pub fn input_total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
    /// Share of input served from cache, if there was any input.
    pub fn cache_hit(&self) -> Option<f64> {
        let i = self.input_total();
        (i > 0).then(|| self.cache_read as f64 / i as f64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    OpenCode,
    Agy,
}

pub const TOOLS: [Tool; 3] = [Tool::Claude, Tool::OpenCode, Tool::Agy];

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::OpenCode => "opencode",
            Tool::Agy => "agy",
        }
    }
}

pub struct Paths {
    pub claude_projects: PathBuf,
    pub opencode_db: PathBuf,
    pub agy_conversations: PathBuf,
}

impl Paths {
    pub fn detect() -> Paths {
        let home = dirs::home_dir().unwrap_or_default();
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let data = dirs::data_dir().unwrap_or_else(|| home.join(".local/share"));
        Paths {
            claude_projects: claude.join("projects"),
            opencode_db: data.join("opencode/opencode.db"),
            agy_conversations: home.join(".gemini/antigravity-cli/conversations"),
        }
    }
}

// ------------------------------------------------------------------ time

/// Milliseconds since the Unix epoch for an RFC 3339 UTC time such as
/// `2026-11-01T09:09:14.581Z`. Returns None for other shapes.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut ms = 0;
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        ms = format!("{digits:0<3}")[..3].parse().ok()?;
        rest = &frac[digits.len()..];
    }
    let offset_s = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o[1..3].parse::<i64>().ok()? * 3600 + o[4..6].parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec - offset_s) * 1000) + ms)
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Start of the current local day, in epoch milliseconds.
pub fn local_midnight_ms(now_ms: i64) -> i64 {
    let secs = (now_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r only writes into the provided struct.
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return now_ms - now_ms.rem_euclid(86_400_000);
    }
    let since_midnight = i64::from(tm.tm_hour) * 3600 + i64::from(tm.tm_min) * 60 + i64::from(tm.tm_sec);
    (now_ms / 1000 - since_midnight) * 1000
}

fn mtime_ms(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- claude

#[derive(Deserialize)]
struct ClaudeLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    id: Option<String>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ClaudeUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

/// (dedup key, timestamp, usage) from one transcript line, if it is an
/// assistant message with usage.
fn parse_claude_line(line: &str) -> Option<(String, i64, Usage)> {
    if !line.contains("\"usage\"") {
        return None;
    }
    let l: ClaudeLine = serde_json::from_str(line).ok()?;
    if l.kind.as_deref() != Some("assistant") {
        return None;
    }
    let msg = l.message?;
    let u = msg.usage?;
    let key = format!("{}:{}", msg.id.unwrap_or_default(), l.request_id.unwrap_or_default());
    let ts = l.timestamp.as_deref().and_then(parse_rfc3339_ms).unwrap_or(0);
    Some((
        key,
        ts,
        Usage {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_write: u.cache_creation_input_tokens,
            calls: 1,
        },
    ))
}

fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => jsonl_files(&p, out),
            Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") => out.push(p),
            _ => {}
        }
    }
}

// ------------------------------------------------------------------- agy

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut r = 0u64;
    for shift in (0..64).step_by(7) {
        let c = *b.get(*i)?;
        *i += 1;
        r |= u64::from(c & 0x7f) << shift;
        if c < 0x80 {
            return Some(r);
        }
    }
    None
}

enum Field<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

/// Decode one protobuf message level; None if the bytes are not a message.
fn pb_fields(b: &[u8]) -> Option<Vec<(u64, Field<'_>)>> {
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let key = varint(b, &mut i)?;
        let (field, wire) = (key >> 3, key & 7);
        if field == 0 {
            return None;
        }
        match wire {
            0 => out.push((field, Field::Int(varint(b, &mut i)?))),
            1 => i += 8,
            5 => i += 4,
            2 => {
                let n = varint(b, &mut i)? as usize;
                let bytes = b.get(i..i.checked_add(n)?)?;
                i += n;
                out.push((field, Field::Bytes(bytes)));
            }
            _ => return None,
        }
    }
    (i == b.len()).then_some(out)
}

/// Usage record at field path 1.4 of an agy `gen_metadata` blob.
pub fn agy_usage(blob: &[u8]) -> Option<Usage> {
    let mut cur = blob;
    for want in [1u64, 4] {
        cur = pb_fields(cur)?.into_iter().find_map(|(f, v)| match v {
            Field::Bytes(b) if f == want => Some(b),
            _ => None,
        })?;
    }
    let ints: HashMap<u64, u64> = pb_fields(cur)?
        .into_iter()
        .filter_map(|(f, v)| match v {
            Field::Int(n) => Some((f, n)),
            Field::Bytes(_) => None,
        })
        .collect();
    let input = ints.get(&2).copied().unwrap_or(0);
    let output = ints.get(&3).copied().unwrap_or(0);
    if input == 0 && output == 0 {
        return None;
    }
    Some(Usage { input, output, cache_read: ints.get(&5).copied().unwrap_or(0), cache_write: 0, calls: 1 })
}

fn open_ro(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let c = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    c.busy_timeout(std::time::Duration::from_millis(500))?;
    Ok(c)
}

// --------------------------------------------------------------- scanner

#[derive(Default, Serialize, Deserialize)]
struct ClaudeFile {
    /// Inode when last read: a replaced file is read again from the start.
    ino: u64,
    offset: u64,
    /// (dedup key, timestamp ms, usage)
    events: Vec<(String, i64, Usage)>,
}

/// Bump when the cached layout or the counting rules change.
const CACHE_VERSION: u32 = 1;

/// The scanner state persisted between runs.
#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    sources: [PathBuf; 3],
    claude_files: HashMap<PathBuf, ClaudeFile>,
    agy_files: HashMap<PathBuf, (i64, Usage)>,
    opencode: HashMap<String, (i64, Usage)>,
    opencode_updated: i64,
    opencode_stamp: Option<[(u64, i64); 2]>,
}

pub fn default_cache_path() -> Option<PathBuf> {
    match std::env::var_os("RDR_TOKENS_CACHE") {
        Some(v) if v == "off" || v.is_empty() => None,
        Some(v) => Some(PathBuf::from(v)),
        None => dirs::cache_dir().map(|d| d.join("rdr/tokens.json")),
    }
}

pub struct Scanner {
    paths: Paths,
    cache: Option<PathBuf>,
    /// State changed since the last save.
    dirty: bool,
    claude_files: HashMap<PathBuf, ClaudeFile>,
    claude_seen: HashSet<String>,
    agy_files: HashMap<PathBuf, (i64, Usage)>,
    /// message id -> (created ms, usage); refreshed by time_updated.
    opencode: HashMap<String, (i64, Usage)>,
    opencode_updated: i64,
    /// (len, mtime) of opencode.db and its WAL at the last scan.
    opencode_stamp: Option<[(u64, i64); 2]>,
    pub errors: Vec<String>,
}

impl Scanner {
    pub fn new(paths: Paths) -> Scanner {
        Scanner {
            paths,
            cache: None,
            dirty: false,
            claude_files: HashMap::new(),
            claude_seen: HashSet::new(),
            agy_files: HashMap::new(),
            opencode: HashMap::new(),
            opencode_updated: i64::MIN,
            opencode_stamp: None,
            errors: Vec::new(),
        }
    }

    /// A scanner that starts from, and saves back to, the cache at `cache`.
    /// A missing, unreadable or stale cache just means a full scan.
    pub fn with_cache(paths: Paths, cache: Option<PathBuf>) -> Scanner {
        let mut sc = Scanner::new(paths);
        if let Some(path) = &cache
            && let Ok(bytes) = std::fs::read(path)
            && let Ok(snap) = serde_json::from_slice::<Snapshot>(&bytes)
            && snap.version == CACHE_VERSION
            && snap.sources == sc.sources()
        {
            sc.claude_seen = snap.claude_files.values().flat_map(|f| f.events.iter().map(|e| e.0.clone())).collect();
            sc.claude_files = snap.claude_files;
            sc.agy_files = snap.agy_files;
            sc.opencode = snap.opencode;
            sc.opencode_updated = snap.opencode_updated;
            sc.opencode_stamp = snap.opencode_stamp;
        }
        sc.cache = cache;
        sc
    }

    fn sources(&self) -> [PathBuf; 3] {
        let p = &self.paths;
        [p.claude_projects.clone(), p.opencode_db.clone(), p.agy_conversations.clone()]
    }

    /// Write the state to the cache (atomically) if it changed.
    pub fn save(&mut self) {
        let Some(path) = self.cache.clone() else { return };
        if !self.dirty {
            return;
        }
        let snap = Snapshot {
            version: CACHE_VERSION,
            sources: self.sources(),
            claude_files: std::mem::take(&mut self.claude_files),
            agy_files: std::mem::take(&mut self.agy_files),
            opencode: std::mem::take(&mut self.opencode),
            opencode_updated: self.opencode_updated,
            opencode_stamp: self.opencode_stamp,
        };
        let res = (|| -> std::io::Result<()> {
            let dir = path.parent().unwrap_or(Path::new("."));
            std::fs::create_dir_all(dir)?;
            let tmp = dir.join(format!(".tokens.{}.tmp", std::process::id()));
            std::fs::write(&tmp, serde_json::to_vec(&snap)?)?;
            std::fs::rename(&tmp, &path)
        })();
        self.claude_files = snap.claude_files;
        self.agy_files = snap.agy_files;
        self.opencode = snap.opencode;
        match res {
            Ok(()) => self.dirty = false,
            Err(e) => self.errors.push(format!("cache {}: {e}", path.display())),
        }
    }

    pub fn refresh(&mut self) {
        let timing = std::env::var_os("RDR_TOKENS_TIMING").is_some();
        let t = std::time::Instant::now();
        self.errors.clear();
        self.scan_claude();
        let t_claude = t.elapsed();
        if let Err(e) = self.scan_opencode() {
            self.errors.push(format!("opencode: {e}"));
        }
        let t_oc = t.elapsed();
        self.scan_agy();
        let t_agy = t.elapsed();
        self.save();
        if timing {
            eprintln!(
                "refresh: claude {:?} opencode {:?} agy {:?} save {:?}",
                t_claude,
                t_oc - t_claude,
                t_agy - t_oc,
                t.elapsed() - t_agy
            );
        }
    }

    fn scan_claude(&mut self) {
        use std::os::unix::fs::MetadataExt;
        let mut files = Vec::new();
        jsonl_files(&self.paths.claude_projects, &mut files);
        let metas: Vec<_> = files.into_iter().filter_map(|p| std::fs::metadata(&p).ok().map(|m| (p, m))).collect();
        // A message is counted once, in the first file read that holds it. If
        // a file vanished or was replaced, its messages may live on in another
        // file, so start Claude over rather than lose or double count them.
        let live: HashMap<&PathBuf, &std::fs::Metadata> = metas.iter().map(|(p, m)| (p, m)).collect();
        let stale = self.claude_files.iter().any(|(p, f)| {
            live.get(p).is_none_or(|m| m.ino() != f.ino || m.len() < f.offset)
        });
        if stale {
            self.claude_files.clear();
            self.claude_seen.clear();
            self.dirty = true;
        }
        for (path, meta) in metas {
            let entry = self.claude_files.entry(path.clone()).or_default();
            entry.ino = meta.ino();
            if meta.len() == entry.offset {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(&path) else { continue };
            if f.seek(SeekFrom::Start(entry.offset)).is_err() {
                continue;
            }
            let mut buf = Vec::with_capacity((meta.len() - entry.offset) as usize);
            if f.read_to_end(&mut buf).is_err() {
                continue;
            }
            // Only consume complete lines; a partial last line is re-read later.
            let Some(end) = buf.iter().rposition(|&c| c == b'\n') else { continue };
            for line in buf[..end].split(|&c| c == b'\n') {
                let Ok(line) = std::str::from_utf8(line) else { continue };
                if let Some((key, ts, u)) = parse_claude_line(line)
                    && self.claude_seen.insert(key.clone())
                {
                    entry.events.push((key, ts, u));
                }
            }
            entry.offset += end as u64 + 1;
            self.dirty = true;
        }
    }

    fn scan_opencode(&mut self) -> rusqlite::Result<()> {
        let db = &self.paths.opencode_db;
        let stamp_of = |p: &Path| std::fs::metadata(p).map(|m| (m.len(), mtime_ms(&m))).unwrap_or((0, 0));
        let mut wal = db.clone().into_os_string();
        wal.push("-wal");
        let stamp = [stamp_of(db), stamp_of(Path::new(&wal))];
        if stamp[0].0 == 0 {
            if !self.opencode.is_empty() || self.opencode_stamp.is_some() {
                self.opencode.clear();
                self.opencode_updated = i64::MIN;
                self.opencode_stamp = None;
                self.dirty = true;
            }
            return Ok(());
        }
        if self.opencode_stamp == Some(stamp) {
            return Ok(()); // nothing written since the last scan
        }
        let c = open_ro(db)?;
        let mut stmt = c.prepare(
            "SELECT id, time_created, time_updated,
                    coalesce(json_extract(data, '$.tokens.input'), 0),
                    coalesce(json_extract(data, '$.tokens.output'), 0)
                      + coalesce(json_extract(data, '$.tokens.reasoning'), 0),
                    coalesce(json_extract(data, '$.tokens.cache.read'), 0),
                    coalesce(json_extract(data, '$.tokens.cache.write'), 0)
             FROM message
             WHERE time_updated >= ?1 AND json_extract(data, '$.role') = 'assistant'",
        )?;
        let rows = stmt.query_map([self.opencode_updated], |r| {
            let n = |i: usize| r.get::<_, i64>(i).map(|v| v.max(0) as u64);
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                Usage { input: n(3)?, output: n(4)?, cache_read: n(5)?, cache_write: n(6)?, calls: 1 },
            ))
        })?;
        for row in rows {
            let (id, created, updated, u) = row?;
            self.opencode_updated = self.opencode_updated.max(updated);
            if u.total() > 0 {
                self.opencode.insert(id, (created, u));
            }
        }
        self.opencode_stamp = Some(stamp);
        self.dirty = true;
        Ok(())
    }

    fn scan_agy(&mut self) {
        let Ok(rd) = std::fs::read_dir(&self.paths.agy_conversations) else { return };
        let mut live = HashSet::new();
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "db") {
                continue;
            }
            live.insert(path.clone());
            let Ok(meta) = e.metadata() else { continue };
            let mtime = mtime_ms(&meta);
            if self.agy_files.get(&path).is_some_and(|(t, _)| *t == mtime) {
                continue;
            }
            match Self::read_agy_db(&path) {
                Ok(u) => {
                    self.agy_files.insert(path, (mtime, u));
                    self.dirty = true;
                }
                Err(e) => self.errors.push(format!("agy {}: {e}", path.display())),
            }
        }
        let before = self.agy_files.len();
        self.agy_files.retain(|p, _| live.contains(p));
        self.dirty |= self.agy_files.len() != before;
    }

    fn read_agy_db(path: &Path) -> rusqlite::Result<Usage> {
        let c = open_ro(path)?;
        let mut stmt = c.prepare("SELECT data FROM gen_metadata")?;
        let mut total = Usage::default();
        let rows = stmt.query_map([], |r| r.get::<_, Option<Vec<u8>>>(0))?;
        for blob in rows.flatten().flatten() {
            if let Some(u) = agy_usage(&blob) {
                total.add(&u);
            }
        }
        Ok(total)
    }

    /// Usage per tool since `since_ms` (all time when None), largest first.
    pub fn summary(&self, since_ms: Option<i64>) -> Vec<(Tool, Usage)> {
        let keep = |ts: i64| since_ms.is_none_or(|s| ts >= s);
        let mut per: HashMap<Tool, Usage> = TOOLS.iter().map(|t| (*t, Usage::default())).collect();
        let mut add = |tool: Tool, u: &Usage| per.get_mut(&tool).unwrap().add(u);
        for f in self.claude_files.values() {
            for (_, ts, u) in &f.events {
                if keep(*ts) {
                    add(Tool::Claude, u);
                }
            }
        }
        for (ts, u) in self.opencode.values() {
            if keep(*ts) {
                add(Tool::OpenCode, u);
            }
        }
        for (ts, u) in self.agy_files.values() {
            if keep(*ts) {
                add(Tool::Agy, u);
            }
        }
        let mut v: Vec<_> = per.into_iter().collect();
        v.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then(a.0.cmp(&b.0)));
        v
    }
}

// --------------------------------------------------------------- display

pub fn human(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

pub struct Window {
    pub label: &'static str,
    pub since_ms: Option<i64>,
}

pub fn windows(now: i64) -> [Window; 3] {
    [
        Window { label: "TODAY", since_ms: Some(local_midnight_ms(now)) },
        Window { label: "LAST 7 DAYS", since_ms: Some(local_midnight_ms(now) - 6 * 86_400_000) },
        Window { label: "ALL TIME", since_ms: None },
    ]
}

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const CYAN: &str = "\x1b[38;2;16;185;129m"; // emerald, as in the icon

fn tool_color(t: Tool) -> &'static str {
    match t {
        Tool::Claude => "\x1b[38;2;245;158;11m",   // amber
        Tool::OpenCode => "\x1b[38;2;16;185;129m", // emerald
        Tool::Agy => "\x1b[38;2;56;189;248m",      // sky
    }
}

/// Render the dashboard as ANSI text sized to `width` columns.
pub fn render(scanner: &Scanner, now: i64, width: usize, swarm: Option<&str>) -> String {
    let mut out = String::new();
    let clock = {
        let secs = (now / 1000) as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&secs, &mut tm) };
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    };
    let left = format!(" {BOLD}{CYAN}rdreh · tokens{RESET}");
    let right = match swarm {
        Some(s) => format!("{s} · {clock} "),
        None => format!("{clock} "),
    };
    let pad = width.saturating_sub(14 + right.chars().count());
    out.push_str(&format!("{left}{}{DIM}{right}{RESET}\n", " ".repeat(pad)));

    // name(9) bar | total(8) share(5) in(8) out(8) hit(6)
    let fixed = 1 + 9 + 1 + 8 + 5 + 8 + 8 + 6 + 2;
    let pct = |r: Option<f64>| r.map(|r| format!("{:.0}%", r * 100.0)).unwrap_or_else(|| "-".into());
    let bar_w = width.saturating_sub(fixed).clamp(4, 40);
    let label_w = 10 + bar_w;
    out.push_str(&format!(
        "\n {DIM}{:<label_w$}{:>8}{:>5}{:>8}{:>8}{:>6}{RESET}\n",
        "", "total", "", "in", "out", "hit"
    ));
    for (i, w) in windows(now).iter().enumerate() {
        let rows = scanner.summary(w.since_ms);
        let mut sum = Usage::default();
        for (_, u) in &rows {
            sum.add(u);
        }
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            " {BOLD}{:<label_w$}{:>8}{RESET}{DIM}{:>5}{:>8}{:>8}{:>6}{RESET}\n",
            w.label,
            human(sum.total()),
            "",
            human(sum.input_total()),
            human(sum.output),
            pct(sum.cache_hit()),
        ));
        let max = rows.first().map(|(_, u)| u.total()).unwrap_or(0).max(1);
        for (tool, u) in &rows {
            let t = u.total();
            let filled = (((t as f64 / max as f64) * bar_w as f64).round() as usize).min(bar_w);
            let share = if sum.total() > 0 {
                format!("{:.0}%", t as f64 * 100.0 / sum.total() as f64)
            } else {
                "-".into()
            };
            let color = if t == 0 { DIM } else { tool_color(*tool) };
            out.push_str(&format!(
                " {color}{:<9}{RESET} {color}{}{RESET}{}{:>8}{DIM}{:>5}{RESET}{:>8}{:>8}{DIM}{:>6}{RESET}\n",
                tool.name(),
                "█".repeat(filled),
                " ".repeat(bar_w - filled),
                human(t),
                share,
                human(u.input_total()),
                human(u.output),
                pct(u.cache_hit()),
            ));
        }
    }
    out.push_str(&format!(
        "\n {DIM}in = all input (incl. cache) · out incl. thinking · hit = in served from cache · agy dated per conversation{RESET}\n"
    ));
    for e in scanner.errors.iter().take(2) {
        out.push_str(&format!(" {DIM}! {e}{RESET}\n"));
    }
    out
}

pub fn summary_json(scanner: &Scanner, now: i64) -> serde_json::Value {
    let windows: Vec<_> = windows(now)
        .iter()
        .map(|w| {
            let tools: Vec<_> = scanner
                .summary(w.since_ms)
                .into_iter()
                .map(|(t, u)| serde_json::json!({
                    "tool": t,
                    "total": u.total(),
                    "input": u.input_total(),
                    "output": u.output,
                    "cache_hit": u.cache_hit(),
                    "usage": u,
                }))
                .collect();
            serde_json::json!({ "window": w.label.to_lowercase().replace(' ', "_"), "since_ms": w.since_ms, "tools": tools })
        })
        .collect();
    serde_json::json!({ "windows": windows, "errors": scanner.errors })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("2026-11-01T09:09:14.581Z"), Some(1_793_524_154_581));
        assert_eq!(parse_rfc3339_ms("2026-11-01T10:09:14.5+01:00"), Some(1_793_524_154_500));
        assert_eq!(parse_rfc3339_ms("2000-02-29T12:00:00Z"), Some(951_825_600_000));
        assert_eq!(parse_rfc3339_ms("garbage"), None);
    }

    #[test]
    fn claude_line() {
        let l = r#"{"type":"assistant","timestamp":"1970-01-01T00:00:01Z","requestId":"r1","message":{"id":"m1","model":"x","usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"output_tokens":5}}}"#;
        let (key, ts, u) = parse_claude_line(l).unwrap();
        assert_eq!((key.as_str(), ts), ("m1:r1", 1000));
        assert_eq!((u.input, u.output, u.cache_read, u.cache_write, u.total()), (2, 5, 100, 10, 117));
        assert!(parse_claude_line(r#"{"type":"user","message":{"usage":{}}}"#).is_none());
        assert!(parse_claude_line("not json \"usage\"").is_none());
    }

    pub fn pb_varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn pb_int(f: u64, v: u64, out: &mut Vec<u8>) {
        pb_varint(f << 3, out);
        pb_varint(v, out);
    }

    fn pb_msg(f: u64, body: &[u8], out: &mut Vec<u8>) {
        pb_varint((f << 3) | 2, out);
        pb_varint(body.len() as u64, out);
        out.extend_from_slice(body);
    }

    #[test]
    fn agy_blob() {
        let mut usage = Vec::new();
        for (f, v) in [(1, 1319), (2, 2282), (3, 75), (5, 15898), (6, 24), (9, 11), (10, 64)] {
            pb_int(f, v, &mut usage);
        }
        let mut inner = Vec::new();
        pb_int(3, 7, &mut inner);
        pb_msg(4, &usage, &mut inner);
        let mut blob = Vec::new();
        pb_msg(2, b"\xa6\xa7", &mut blob);
        pb_msg(1, &inner, &mut blob);
        let u = agy_usage(&blob).unwrap();
        assert_eq!((u.input, u.output, u.cache_read), (2282, 75, 15898));
        assert!(agy_usage(b"\xff\xff\xff").is_none());
        assert!(agy_usage(&[]).is_none());
    }

    fn claude_json(id: &str, ts: &str, input: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","requestId":"r-{id}","message":{{"id":"{id}","usage":{{"input_tokens":{input},"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
        )
    }

    #[test]
    fn claude_scan_is_incremental_and_waits_for_complete_lines() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("projects/p/sub/subagents");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join("s.jsonl");
        let mut f = std::fs::File::create(&file).unwrap();
        writeln!(f, "{}", claude_json("a", "2026-01-01T00:00:00Z", 10)).unwrap();
        writeln!(f, "{}", claude_json("a", "2026-01-01T00:00:00Z", 10)).unwrap(); // streamed duplicate
        let paths = Paths {
            claude_projects: dir.path().join("projects"),
            opencode_db: dir.path().join("none.db"),
            agy_conversations: dir.path().join("none"),
        };
        let mut sc = Scanner::new(paths);
        sc.refresh();
        let claude = |sc: &Scanner| sc.summary(None).into_iter().find(|(t, _)| *t == Tool::Claude).unwrap().1;
        assert_eq!((claude(&sc).input, claude(&sc).calls), (10, 1));

        // A half-written line is not counted until its newline arrives.
        let full = claude_json("b", "2026-01-02T00:00:00Z", 5);
        let (head, tail) = full.split_at(30);
        write!(f, "{head}").unwrap();
        f.flush().unwrap();
        sc.refresh();
        assert_eq!(claude(&sc).calls, 1);
        writeln!(f, "{tail}").unwrap();
        f.flush().unwrap();
        sc.refresh();
        assert_eq!((claude(&sc).input, claude(&sc).calls), (15, 2));

        // Windows filter by message timestamp.
        let since = parse_rfc3339_ms("2026-01-02T00:00:00Z");
        assert_eq!(sc.summary(since).into_iter().find(|(t, _)| *t == Tool::Claude).unwrap().1.input, 5);
    }

    #[test]
    fn cache_resumes_and_matches_a_full_scan() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("projects/p");
        std::fs::create_dir_all(&proj).unwrap();
        let cache = dir.path().join("cache/tokens.json");
        let paths = || Paths {
            claude_projects: dir.path().join("projects"),
            opencode_db: dir.path().join("none.db"),
            agy_conversations: dir.path().join("none"),
        };
        let claude = |sc: &Scanner| sc.summary(None).into_iter().find(|(t, _)| *t == Tool::Claude).unwrap().1;
        let fresh = || {
            let mut sc = Scanner::new(paths());
            sc.refresh();
            claude(&sc)
        };
        let cached = || {
            let mut sc = Scanner::with_cache(paths(), Some(cache.clone()));
            sc.refresh();
            claude(&sc)
        };

        let a = proj.join("a.jsonl");
        let b = proj.join("b.jsonl");
        let mut fa = std::fs::File::create(&a).unwrap();
        writeln!(fa, "{}", claude_json("m1", "2026-01-01T00:00:00Z", 10)).unwrap();
        // A resumed session repeats m1 in a second file: counted once.
        std::fs::write(&b, format!("{}\n{}\n", claude_json("m1", "2026-01-01T00:00:00Z", 10), claude_json("m2", "2026-01-01T00:00:00Z", 7))).unwrap();
        assert_eq!(cached().input, 17);
        assert!(cache.exists());

        // The next run starts from the cache and only reads appended lines.
        writeln!(fa, "{}", claude_json("m3", "2026-01-02T00:00:00Z", 100)).unwrap();
        fa.flush().unwrap();
        let mut sc = Scanner::with_cache(paths(), Some(cache.clone()));
        assert_eq!(claude(&sc).input, 17, "state not restored from cache");
        sc.refresh();
        assert_eq!((claude(&sc).input, claude(&sc).calls), (117, 3));

        // A replaced file (new inode, same or larger size) is read again.
        let tmp = proj.join("a.new");
        std::fs::write(&tmp, format!("{}\n{}\n", claude_json("m1", "2026-01-01T00:00:00Z", 10), claude_json("m4", "2026-01-03T00:00:00Z", 1000))).unwrap();
        std::fs::rename(&tmp, &a).unwrap();
        assert_eq!(cached(), fresh());
        assert_eq!(cached().input, 1017);

        // A deleted file: m1 still lives in b and must stay counted once.
        std::fs::remove_file(&a).unwrap();
        assert_eq!(cached(), fresh());
        assert_eq!(cached().input, 17);

        // A cache made for other source paths is ignored.
        let mut other = Scanner::with_cache(
            Paths { claude_projects: dir.path().join("elsewhere"), ..paths() },
            Some(cache.clone()),
        );
        assert_eq!(claude(&other).calls, 0);
        other.refresh();
        assert_eq!(claude(&other).calls, 0);

        // A corrupt cache is a full scan, not an error.
        std::fs::write(&cache, b"{not json").unwrap();
        assert_eq!(cached(), fresh());
    }

    #[test]
    fn opencode_updates_replace_instead_of_double_counting() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE message (id text primary key, session_id text, time_created integer, time_updated integer, data text);
             INSERT INTO message VALUES ('a','s',1000,1000,'{\"role\":\"assistant\",\"tokens\":{\"input\":10,\"output\":0}}');",
        )
        .unwrap();
        let paths = Paths {
            claude_projects: dir.path().join("none"),
            opencode_db: db.clone(),
            agy_conversations: dir.path().join("none"),
        };
        let mut sc = Scanner::new(paths);
        let oc = |sc: &Scanner| sc.summary(None).into_iter().find(|(t, _)| *t == Tool::OpenCode).unwrap().1;
        sc.refresh();
        assert_eq!((oc(&sc).total(), oc(&sc).calls), (10, 1));
        // The streaming message completes (same id, later time_updated) and a new one lands.
        c.execute_batch(
            "UPDATE message SET time_updated=2000, data='{\"role\":\"assistant\",\"tokens\":{\"input\":10,\"output\":5}}' WHERE id='a';
             INSERT INTO message VALUES ('b','s',3000,3000,'{\"role\":\"assistant\",\"tokens\":{\"input\":1,\"output\":1}}');",
        )
        .unwrap();
        sc.refresh();
        assert_eq!((oc(&sc).total(), oc(&sc).calls), (17, 2));
        sc.refresh(); // unchanged files: no rescan, same result
        assert_eq!(oc(&sc).total(), 17);
    }

    #[test]
    fn human_numbers() {
        assert_eq!(human(999), "999");
        assert_eq!(human(1_500), "1.5k");
        assert_eq!(human(82_100_000), "82.1M");
        assert_eq!(human(1_230_000_000), "1.23B");
    }
}
