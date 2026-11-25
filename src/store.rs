//! Output. One thread owns every output file: WARC files of the fetched pages (each record its
//! own gzip member, so a crash or a cut-off file loses at most the last record) and
//! `pages.jsonl`, one line per fetch. Exact and near-duplicate pages are marked here, since this
//! is the one place that sees every page.

use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use flate2::{Compression, write::GzEncoder};
use serde::Serialize;
use tokio::sync::mpsc::{Receiver, error::TryRecvError};
use xxhash_rust::xxh3::xxh3_128;

use crate::dedup::NearDupIndex;

/// One row of `pages.jsonl`. Every fetch gets one, errors and non-HTML pages included.
#[derive(Serialize, Default, Debug)]
pub struct PageRecord {
    pub url: String,
    pub host: String,
    pub depth: u16,
    pub parent_url: Option<String>,
    pub priority: i32,
    /// Unix milliseconds.
    pub fetched_at: u64,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub content_type: Option<String>,
    pub bytes: u64,
    pub truncated: bool,
    pub title: String,
    pub text: String,
    pub canonical: Option<String>,
    pub noindex: bool,
    pub links: Vec<LinkOut>,
    /// The last three are set by the store, from `StoreMsg`: hashes as 16-digit hex, and the URL
    /// of an earlier page within SimHash distance `NEAR_DUP_BITS`.
    pub simhash: Option<String>,
    pub body_hash: Option<String>,
    pub near_dup: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct LinkOut {
    pub url: String,
    pub anchor: String,
    pub nofollow: bool,
}

/// A page as archived: the header block from `fetch` and the stored (decoded, capped) body.
pub struct WarcPayload {
    pub http_head: Vec<u8>,
    pub body: Vec<u8>,
    pub truncated: bool,
}

/// A `response` record compressed as one gzip member, ready to append to a WARC file.
pub struct WarcRecord(Vec<u8>);

pub struct StoreMsg {
    pub record: PageRecord,
    pub simhash: Option<u64>,
    pub body_hash: Option<u64>,
    pub warc: Option<WarcRecord>,
}

impl StoreMsg {
    /// A row whose page is archived: the record for the row's URL, dated from its `fetched_at`.
    /// It is compressed here, on the caller's thread (the parse pool), because one store thread
    /// compressing every page was the crawl's bottleneck; members are independent, so any
    /// thread can make one.
    pub fn archived(
        record: PageRecord,
        simhash: Option<u64>,
        body_hash: Option<u64>,
        page: &WarcPayload,
    ) -> Self {
        let warc = Some(response(&record.url, record.fetched_at / 1000, page));
        Self {
            record,
            simhash,
            body_hash,
            warc,
        }
    }
}

#[derive(Serialize, Default, Debug, Clone)]
pub struct StoreSummary {
    pub records: u64,
    /// `response` records; each file's `warcinfo` is not counted.
    pub warc_records: u64,
    pub warc_files: u32,
    pub exact_dups: u64,
    pub near_dups: u64,
}

/// `YYYY-MM-DDThh:mm:ssZ` (UTC) for `WARC-Date`. Howard Hinnant's `civil_from_days`, so no
/// date crate; unsigned throughout, since the input can't be before 1970.
pub fn iso8601(unix_secs: u64) -> String {
    let (days, secs) = (unix_secs / 86_400, unix_secs % 86_400);
    let z = days + 719_468; // days since 0000-03-01
    let era = z / 146_097;
    let doe = z % 146_097; // day of the 400-year era
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of the year, counted from March
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + u64::from(month <= 2);
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// One WARC/1.1 record: the header lines in a fixed order, the payload, then `\r\n\r\n`.
/// `target` and the `extra` values must be single-line; a serialized `Url` always is.
pub fn warc_record(
    kind: &str,
    target: Option<&str>,
    date: &str,
    content_type: &str,
    extra: &[(&str, String)],
    payload: &[u8],
    id: &str,
) -> Vec<u8> {
    let mut rec = warc_header(kind, target, date, content_type, extra, payload.len(), id);
    rec.extend_from_slice(payload);
    rec.extend_from_slice(b"\r\n\r\n");
    rec
}

fn warc_header(
    kind: &str,
    target: Option<&str>,
    date: &str,
    content_type: &str,
    extra: &[(&str, String)],
    len: usize,
    id: &str,
) -> Vec<u8> {
    let mut h =
        format!("WARC/1.1\r\nWARC-Type: {kind}\r\nWARC-Record-ID: {id}\r\nWARC-Date: {date}\r\n");
    if let Some(t) = target {
        h += &format!("WARC-Target-URI: {t}\r\n");
    }
    for (name, value) in extra {
        h += &format!("{name}: {value}\r\n");
    }
    h += &format!("Content-Type: {content_type}\r\nContent-Length: {len}\r\n\r\n");
    h.into_bytes()
}

/// `<urn:uuid:…>`, unique per record: a hash of the clock, the process id and a process-wide
/// counter, laid out as an RFC 9562 version 8 (vendor-defined) UUID.
fn record_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut seed = [0u8; 28];
    seed[..16].copy_from_slice(&nanos.to_le_bytes());
    seed[16..20].copy_from_slice(&std::process::id().to_le_bytes());
    seed[20..].copy_from_slice(&n.to_le_bytes());
    let h =
        (xxh3_128(&seed) & !(0xFu128 << 76) & !(0x3u128 << 62)) | (0x8u128 << 76) | (0x2u128 << 62);
    format!(
        "<urn:uuid:{:08x}-{:04x}-{:04x}-{:04x}-{:012x}>",
        h >> 96,
        (h >> 80) & 0xFFFF,
        (h >> 64) & 0xFFFF,
        (h >> 48) & 0xFFFF,
        h & 0xFFFF_FFFF_FFFF
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `parts`, compressed as one gzip member.
// ponytail: level 6 with flate2's default backend (miniz_oxide); the zlib-rs backend is faster
// if the parse pool's CPU becomes the limit.
fn gzip(parts: &[&[u8]]) -> Vec<u8> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    for part in parts {
        gz.write_all(part).expect("writing to memory can't fail");
    }
    gz.finish().expect("writing to memory can't fail")
}

/// The `response` record of `url`, compressed.
fn response(url: &str, unix_secs: u64, p: &WarcPayload) -> WarcRecord {
    let truncated = [("WARC-Truncated", "length".to_owned())];
    let extra: &[(&str, String)] = if p.truncated { &truncated } else { &[] };
    let header = warc_header(
        "response",
        Some(url),
        &iso8601(unix_secs),
        "application/http;msgtype=response",
        extra,
        p.http_head.len() + p.body.len(),
        &record_id(),
    );
    WarcRecord(gzip(&[&header, &p.http_head, &p.body, b"\r\n\r\n"]))
}

/// Creates `crawl-{index:05}.warc.gz`, never overwriting one, and writes its `warcinfo` record.
fn new_file(dir: &Path, index: u32) -> io::Result<(BufWriter<File>, u64)> {
    let name = format!("crawl-{index:05}.warc.gz");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(&name))?;
    let mut file = BufWriter::with_capacity(1 << 16, file);
    let fields = b"software: McCrawl/0.1\r\nformat: WARC File Format 1.1\r\n\
        conformsTo: http://iipc.github.io/warc-specifications/specifications/warc-format/warc-1.1/\r\n";
    let info = warc_record(
        "warcinfo",
        None,
        &iso8601(unix_now()),
        "application/warc-fields",
        &[("WARC-Filename", name)],
        fields,
        &record_id(),
    );
    let gz = gzip(&[&info]);
    file.write_all(&gz)?;
    Ok((file, gz.len() as u64))
}

/// Appends `response` records to `crawl-NNNNN.warc.gz` files in one directory, starting a new
/// file (with its own `warcinfo`) once the current one reaches `rotate_at` bytes.
pub struct WarcWriter {
    dir: PathBuf,
    rotate_at: u64,
    file: BufWriter<File>,
    file_bytes: u64,
    files: u32,
    records: u64,
}

impl WarcWriter {
    /// Starts `dir/crawl-00000.warc.gz`. Fails if it already exists.
    pub fn create(dir: &Path, rotate_at: u64) -> io::Result<Self> {
        let (file, file_bytes) = new_file(dir, 0)?;
        Ok(Self {
            dir: dir.to_owned(),
            rotate_at,
            file,
            file_bytes,
            files: 1,
            records: 0,
        })
    }

    pub fn write_response(&mut self, url: &str, unix_secs: u64, p: &WarcPayload) -> io::Result<()> {
        self.append(&response(url, unix_secs, p))
    }

    /// Rotation happens here, before the record, so every file but the first gets a response
    /// right after its `warcinfo`; the first rotates only once it holds one. So no file is just a
    /// `warcinfo`, except the only file of a crawl that archived nothing.
    fn append(&mut self, record: &WarcRecord) -> io::Result<()> {
        if self.records > 0 && self.file_bytes >= self.rotate_at {
            self.file.flush()?;
            (self.file, self.file_bytes) = new_file(&self.dir, self.files)?;
            self.files += 1;
        }
        self.file.write_all(&record.0)?;
        self.file_bytes += record.0.len() as u64;
        self.records += 1;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Everything a crawl writes, plus the duplicate indexes.
pub struct Store {
    jsonl: BufWriter<File>,
    warc: WarcWriter,
    bodies: HashSet<u64>,
    near: NearDupIndex,
    /// URL of each fingerprint in `near`, by id.
    // ponytail: every indexed URL stays in RAM (~100 B/page, u32 ids); fine for one crawl,
    // SP2 moves the index to disk or shards it.
    urls: Vec<String>,
    summary: StoreSummary,
}

impl Store {
    /// Creates `dir` if needed, then `pages.jsonl` and the first WARC file. Fails rather than
    /// overwrite an earlier crawl's output.
    pub fn open(dir: &Path, rotate_at: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let jsonl = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join("pages.jsonl"))?;
        Ok(Self {
            jsonl: BufWriter::with_capacity(1 << 16, jsonl),
            warc: WarcWriter::create(dir, rotate_at)?,
            bodies: HashSet::new(),
            near: NearDupIndex::default(),
            urls: Vec::new(),
            summary: StoreSummary::default(),
        })
    }

    /// Marks duplicates, archives the payload if there is one, and appends the row.
    pub fn write(&mut self, m: StoreMsg) -> io::Result<()> {
        let StoreMsg {
            mut record,
            simhash,
            body_hash,
            warc,
        } = m;
        if let Some(h) = body_hash
            && !self.bodies.insert(h)
        {
            self.summary.exact_dups += 1;
        }
        record.near_dup = None;
        // A SimHash of 0 means no text; empty pages aren't near-duplicates of each other.
        if let Some(fp) = simhash.filter(|&fp| fp != 0) {
            if let Some(id) = self.near.find(fp) {
                record.near_dup = Some(self.urls[id as usize].clone());
                self.summary.near_dups += 1;
            }
            self.near.insert(fp, self.urls.len() as u32);
            self.urls.push(record.url.clone());
        }
        record.simhash = simhash.map(hex);
        record.body_hash = body_hash.map(hex);
        if let Some(r) = &warc {
            self.warc.append(r)?;
        }
        serde_json::to_writer(&mut self.jsonl, &record)?;
        self.jsonl.write_all(b"\n")?;
        self.summary.records += 1;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.jsonl.flush()?;
        self.warc.flush()
    }

    pub fn finish(mut self) -> io::Result<StoreSummary> {
        self.flush()?;
        Ok(StoreSummary {
            warc_records: self.warc.records,
            warc_files: self.warc.files,
            ..self.summary
        })
    }
}

fn hex(v: u64) -> String {
    format!("{v:016x}")
}

/// Runs `store` on its own thread until every sender is dropped, then finishes it. It flushes
/// whenever the channel runs empty, so the output on disk is current whenever the crawl pauses.
/// On a write error the thread stops and returns it; the senders then see a closed channel.
pub fn spawn(mut store: Store, mut rx: Receiver<StoreMsg>) -> JoinHandle<io::Result<StoreSummary>> {
    thread::Builder::new()
        .name("store".into())
        .spawn(move || {
            loop {
                let m = match rx.try_recv() {
                    Ok(m) => m,
                    Err(TryRecvError::Empty) => {
                        store.flush()?;
                        match rx.blocking_recv() {
                            Some(m) => m,
                            None => break,
                        }
                    }
                    Err(TryRecvError::Disconnected) => break,
                };
                store.write(m)?;
            }
            store.finish()
        })
        .expect("failed to start the store thread")
}
