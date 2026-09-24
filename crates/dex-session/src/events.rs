//! Events journal (`<id>.events.jsonl`): the single owner of the SSE
//! replay cursor. Steady-state polls serve from a per-path cursor (one
//! `stat`, no file open); scans resume from byte-offset checkpoints and
//! publish only drained tips, so a page-limited scan is never mistaken
//! for EOF.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;
use serde_json::value::RawValue;

use super::{file_id, FileId, PathCache};

/// Steady-state events cursor (perf doc §12): per events-journal path, the
/// exact parsed-end identity, highest seq served, and byte-offset
/// checkpoints (first seq per 64 KiB chunk) so a poll seeks past
/// already-served rows. The idle 2 s poll with no new rows is then one
/// `stat` and no file open.
pub struct EventsCursor {
    pub id: FileId,
    pub max_seq: Option<u64>,
    pub checkpoints: Vec<(u64, u64)>,
    /// The scan that published this entry reached EOF. A page-limited scan
    /// (§1) stops early, so the fast path must not treat its `max_seq` as
    /// the file tip — the next poll re-scans from its checkpoint instead.
    drained: bool,
}

/// Byte spacing of events checkpoints: a poll seeks to the newest chunk at
/// or before its cursor and parses only the tail.
const EVENTS_CHECKPOINT_BYTES: u64 = 64 * 1024;
/// Checkpoint count cap per journal (perf-only: ancient cursors scan more).
const EVENTS_CHECKPOINT_CAP: usize = 4096;
pub fn events_cache() -> &'static Mutex<PathCache<EventsCursor>> {
    static CACHE: OnceLock<Mutex<PathCache<EventsCursor>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(PathCache {
            map: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

/// Refresh the events cursor after one of our own appends (perf doc §12):
/// the common case keeps a poll from ever re-scanning. `written` is the byte
/// count this handle just appended; a foreign writer (another handle/task)
/// interleaving a row between our write and this stat would make the recorded
/// tip and checkpoints describe bytes we didn't write — a tip below the
/// foreign row would then starve a poller parked at that row. Detect it by
/// the length delta and drop the cursor rather than publish it.
pub fn events_cache_touched(events_path: &Path, seq: u64, written: u64) {
    let Some(id) = file_id(events_path) else {
        return;
    };
    let mut cache = events_cache().lock().expect("events cache lock");
    let foreign = match cache.get_mut(events_path) {
        Some(entry) if entry.id.len.saturating_add(written) == id.len => {
            let prev_len = entry.id.len;
            entry.id = id;
            entry.max_seq = Some(entry.max_seq.map_or(seq, |m| m.max(seq)));
            let base = entry.checkpoints.last().map(|&(_, off)| off).unwrap_or(0);
            if id.len.saturating_sub(base) >= EVENTS_CHECKPOINT_BYTES {
                entry.checkpoints.push((seq, prev_len));
                if entry.checkpoints.len() > EVENTS_CHECKPOINT_CAP {
                    let excess = entry.checkpoints.len() - EVENTS_CHECKPOINT_CAP;
                    entry.checkpoints.drain(..excess);
                }
            }
            false
        }
        Some(_) => true,
        None => false,
    };
    if foreign {
        cache.evict(events_path);
    }
}

/// One events-journal row: `seq` drives the replay cursor, `payload`
/// borrows the raw JSON — no parse → re-serialize round trip on the hot
/// poll path (perf doc §12).
#[derive(Deserialize)]
struct EventRow<'a> {
    seq: u64,
    #[serde(borrow)]
    payload: &'a RawValue,
}

/// Scan the events journal from the newest checkpoint at or before `since`,
/// returning `(rows with seq >= since, highest seq in the file)`. When
/// `collect` is false payloads are skipped (the `max_event_seq` path).
type EventsScan = (Vec<(u64, String)>, Option<u64>);

/// Newest checkpoint at or before `since`: the kept checkpoint list, the seek
/// offset, and the seq that offset should resume at. The file only grows in
/// production, but a checkpoint is verified against its row before use, so an
/// out-of-band rewrite falls back to a full scan instead of serving garbage.
fn checkpoint_resume(
    events_path: &Path,
    meta_len: u64,
    since: u64,
) -> (Vec<(u64, u64)>, u64, Option<u64>) {
    let cache = events_cache().lock().expect("events cache lock");
    match cache.get(events_path) {
        Some(entry) if meta_len >= entry.id.len => {
            let kept: Vec<(u64, u64)> = entry
                .checkpoints
                .iter()
                .copied()
                .filter(|&(_, off)| off <= meta_len)
                .collect();
            match kept.iter().rev().find(|&&(seq, _)| seq <= since).copied() {
                Some((seq, off)) => (kept, off, Some(seq)),
                None => (Vec::new(), 0, None),
            }
        }
        _ => (Vec::new(), 0, None),
    }
}

/// Highest seq already cached below the resume point, so the fresh max covers
/// the whole file, not just the scanned tail.
fn cached_max_seq(events_path: &Path, seek_to: u64) -> Option<u64> {
    if seek_to == 0 {
        return None;
    }
    events_cache()
        .lock()
        .expect("events cache lock")
        .get(events_path)
        .and_then(|e| e.max_seq)
}

fn scan_events(
    events_path: &Path,
    since: u64,
    collect: bool,
    limit: usize,
) -> io::Result<EventsScan> {
    // A zero page serves nothing: without this the `out.len() >= limit`
    // check below runs only after the first push and returns one row.
    if collect && limit == 0 {
        return Ok((Vec::new(), None));
    }
    // Cursor is the next seq to serve (inclusive): initial 0 serves seq 0,
    // and `next_seq = max + 1` resumes without loss or duplication.
    // Fast path: the journal is byte-identical to a previous scan and the
    // cursor is past everything served — the idle poll. No file open.
    // Only a drained scan publishes a servable tip (a page-limited scan
    // stops early, so its `max_seq` is not the file end — §1).
    if let Some(id) = file_id(events_path) {
        let cache = events_cache().lock().expect("events cache lock");
        if let Some(entry) = cache.get(events_path) {
            if entry.id == id && entry.drained && entry.max_seq.is_none_or(|m| since > m) {
                return Ok((Vec::new(), entry.max_seq));
            }
        }
    }
    let meta_len = fs::metadata(events_path)?.len();
    // Resume from the newest checkpoint at or before the cursor.
    let (mut checkpoints, mut seek_to, seek_seq) = checkpoint_resume(events_path, meta_len, since);
    let mut file = File::open(events_path)?;
    // Checkpoint beyond EOF (a shrink raced the stat): full scan instead.
    if seek_to > meta_len {
        seek_to = 0;
        checkpoints.clear();
    }
    if seek_to > 0 {
        file.seek(SeekFrom::Start(seek_to))?;
        let probe = {
            let mut probe_reader = BufReader::new(&file);
            let mut probe = String::new();
            match probe_reader.read_line(&mut probe) {
                Ok(_) => serde_json::from_str::<EventRow<'_>>(&probe)
                    .ok()
                    .map(|r| r.seq),
                Err(_) => None,
            }
        };
        if probe != seek_seq {
            seek_to = 0;
            checkpoints.clear();
        }
        file.seek(SeekFrom::Start(seek_to))?;
    }
    // Checkpoints from before the resume point would interleave with the
    // fresh ones appended during the scan, leaving the vector unsorted and
    // breaking `iter().rev().find(...)` and `events_cache_touched`'s
    // `last()` base. A checkpoint at or before the resume point is kept.
    checkpoints.retain(|&(_, off)| off <= seek_to);
    let mut reader = BufReader::new(file);
    let mut max: Option<u64> = cached_max_seq(events_path, seek_to);
    let mut next_chunk_at = seek_to.saturating_add(EVENTS_CHECKPOINT_BYTES);
    let mut out = Vec::new();
    let mut off = seek_to;
    let mut line = String::new();
    let mut drained = false;
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(0) => {
                drained = true;
                break;
            }
            Ok(n) => n,
            // Torn read: stop like EOF (matches `for_each_line`), but the
            // end wasn't reached — don't publish a servable tip.
            Err(_) => break,
        };
        let row_start = off;
        off += n as u64;
        let Ok(row) = serde_json::from_str::<EventRow<'_>>(line.trim_end()) else {
            continue;
        };
        max = Some(max.map_or(row.seq, |m| m.max(row.seq)));
        if row_start >= next_chunk_at {
            checkpoints.push((row.seq, row_start));
            next_chunk_at = row_start.saturating_add(EVENTS_CHECKPOINT_BYTES);
        }
        if collect && row.seq >= since {
            out.push((row.seq, row.payload.get().to_owned()));
            // Page-limited serving (§1): stop after `limit` served rows.
            // `max`/checkpoints cover exactly the served prefix, so the
            // next page resumes from its checkpoint; `drained` stays false
            // so the fast path can't mistake this tip for EOF.
            if out.len() >= limit {
                break;
            }
        }
    }
    if let Some(id) = file_id(events_path) {
        // Publish only when the identity still describes what was parsed:
        // an append racing past the scan end would make `max` stale, and
        // the next poll then re-scans from its checkpoint instead.
        if id.len == off {
            if checkpoints.len() > EVENTS_CHECKPOINT_CAP {
                let excess = checkpoints.len() - EVENTS_CHECKPOINT_CAP;
                checkpoints.drain(..excess);
            }
            events_cache().lock().expect("events cache lock").insert(
                events_path,
                EventsCursor {
                    id,
                    max_seq: max,
                    checkpoints,
                    drained,
                },
            );
        }
    }
    Ok((out, max))
}

// ---------------------------------------------------------------------------
// Session-facing queries (thin wrappers live on `Session` in `mod.rs` so
// `Session::load_events` / `Session::max_event_seq` paths keep working).
// ---------------------------------------------------------------------------

/// Replay stream events with `seq >= since`, in order (`since` is the
/// next seq to serve, inclusive — `next_seq` chains without loss or
/// duplication). `path` is the
/// SESSION file; the journal lives at `<session>.events.jsonl`.
/// Served from the steady-state cursor when the journal hasn't grown
/// (one `stat`, no file open — perf doc §12), otherwise scanned from
/// the newest checkpoint at or before `since`.
pub fn load_events(path: &Path, since: u64, limit: usize) -> io::Result<Vec<(u64, String)>> {
    let events_path = path.with_extension("events.jsonl");
    Ok(scan_events(&events_path, since, true, limit)?.0)
}

/// Highest event seq recorded for a session (`None` when no seq is
/// journaled yet — distinct from a journal holding exactly seq 0).
/// Served from the cursor without opening the file when the journal
/// hasn't grown (perf doc §12).
pub fn max_event_seq(path: &Path) -> Option<u64> {
    let events_path = path.with_extension("events.jsonl");
    // The tip query must see the whole file (a limit here would corrupt
    // seq seeding) — only serving scans page (§1).
    scan_events(&events_path, u64::MAX, false, usize::MAX)
        .ok()
        .and_then(|(_, max)| max)
}
