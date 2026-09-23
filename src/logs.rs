//! Per-replica JSONL storage and shared readers for CLI and TUI output.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(crate) const CHUNK_BYTES: usize = 64 * 1024;
const FILE_BYTES: u64 = 10 * 1024 * 1024;
const FILE_COUNT: u64 = 4;
// JSON escaping can expand a byte into six characters.
const RECORD_BYTES: u64 = (CHUNK_BYTES * 6 + 8192) as u64;

pub(crate) fn directory(daemon_log: &Path) -> PathBuf {
    daemon_log.with_extension("").join("logs")
}

fn create_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(crate::paths::FILE_MODE);
    }
    options.open(path)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Identity {
    service: String,
    replica: u16,
}

impl Identity {
    fn matches(&self, filters: &[String]) -> bool {
        filters.is_empty()
            || filters
                .iter()
                .any(|f| f == &self.service || f == &format!("{}[{}]", self.service, self.replica))
    }

    fn key(&self) -> String {
        let readable: String = self
            .service
            .chars()
            .take(32)
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let hash = crate::paths::hex_encode(&Sha256::digest(self.service.as_bytes()));
        format!("{readable}-{hash}-{}", self.replica)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    pub timestamp: String,
    pub service: String,
    pub replica: u16,
    pub name: String,
    pub stream: String,
    pub message: String,
    /// This chunk ended at the size limit rather than a newline or EOF.
    pub partial: bool,
}

impl Record {
    fn render(&self, strip: bool) -> String {
        if strip {
            self.message.clone()
        } else {
            format!("[{}] {}", self.name, self.message)
        }
    }
}

/// Registry locks are only used when a process starts; output writers lock
/// independently per replica. There is no global output ordering lock.
#[derive(Debug)]
pub(crate) struct Store {
    root: PathBuf,
    writers: Mutex<BTreeMap<String, Arc<Mutex<Writer>>>>,
}

impl Store {
    pub fn new(daemon_log: &Path) -> anyhow::Result<Self> {
        let root = directory(daemon_log);
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        crate::paths::create_dir_secure(root.parent().expect("log parent"))?;
        crate::paths::create_dir_secure(&root)?;
        let session = format!(
            "{}:{}",
            std::process::id(),
            humantime::format_rfc3339_nanos(SystemTime::now())
        );
        create_file(&root.join("session"))?.write_all(session.as_bytes())?;
        Ok(Self {
            root,
            writers: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn writer(&self, service: &str, replica: u16) -> io::Result<Arc<Mutex<Writer>>> {
        let identity = Identity {
            service: service.into(),
            replica,
        };
        let key = identity.key();
        let mut writers = self.writers.lock().unwrap();
        if let Some(writer) = writers.get(&key) {
            return Ok(writer.clone());
        }
        let writer = Writer::new(self.root.clone(), key.clone(), identity, FILE_BYTES)?;
        let writer = Arc::new(Mutex::new(writer));
        writers.insert(key, writer.clone());
        Ok(writer)
    }
}

#[derive(Debug)]
pub(crate) struct Writer {
    root: PathBuf,
    key: String,
    identity: Identity,
    file: File,
    generation: u64,
    size: u64,
    limit: u64,
    last_time: SystemTime,
}

impl Writer {
    fn new(root: PathBuf, key: String, identity: Identity, limit: u64) -> io::Result<Self> {
        let file = create_file(&root.join(format!("{key}.0.jsonl")))?;
        // Publish metadata atomically so readers never observe partial JSON.
        let tmp = root.join(format!("{key}.meta.tmp"));
        create_file(&tmp)?.write_all(&serde_json::to_vec(&identity)?)?;
        fs::rename(tmp, root.join(format!("{key}.meta.json")))?;
        Ok(Self {
            root,
            key,
            identity,
            file,
            generation: 0,
            size: 0,
            limit,
            last_time: SystemTime::UNIX_EPOCH,
        })
    }

    pub fn write(
        &mut self,
        name: &str,
        stream: &str,
        message: &str,
        partial: bool,
    ) -> io::Result<()> {
        let mut remaining = message;
        while remaining.len() > CHUNK_BYTES {
            let mut end = CHUNK_BYTES;
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            self.write_record(name, stream, &remaining[..end], true)?;
            remaining = &remaining[end..];
        }
        self.write_record(name, stream, remaining, partial)
    }

    fn write_record(
        &mut self,
        name: &str,
        stream: &str,
        message: &str,
        partial: bool,
    ) -> io::Result<()> {
        // Clamp clock corrections per replica to preserve its observed order.
        let time = SystemTime::now().max(self.last_time);
        self.last_time = time;
        let record = Record {
            timestamp: humantime::format_rfc3339_nanos(time).to_string(),
            service: self.identity.service.clone(),
            replica: self.identity.replica,
            name: name.into(),
            stream: stream.into(),
            message: message.into(),
            partial,
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log record metadata exceeds size limit",
            ));
        }
        if self.size > 0 && self.size + bytes.len() as u64 > self.limit {
            let generation = self.generation + 1;
            let file = create_file(&self.root.join(format!("{}.{generation}.jsonl", self.key)))?;
            self.file = file;
            self.generation = generation;
            self.size = 0;
            if generation >= FILE_COUNT {
                let old = self
                    .root
                    .join(format!("{}.{}.jsonl", self.key, generation - FILE_COUNT));
                match fs::remove_file(old) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        self.file.write_all(&bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }
}

/// Read bounded chunks, including output without a newline. Keep incomplete
/// UTF-8 at chunk boundaries; genuinely invalid bytes become replacement chars.
pub(crate) struct Chunks<R> {
    reader: R,
    pending: Vec<u8>,
    continuing: bool,
}

impl<R: AsyncBufRead + Unpin> Chunks<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            continuing: false,
        }
    }

    pub async fn next(&mut self) -> io::Result<Option<(String, bool)>> {
        loop {
            let available = self.reader.fill_buf().await?;
            let eof = available.is_empty();
            let count = available.len().min(CHUNK_BYTES - self.pending.len());
            let newline = available[..count].iter().position(|b| *b == b'\n');
            let count = newline.map_or(count, |i| i + 1);
            self.pending.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if newline.is_some() || eof || self.pending.len() == CHUNK_BYTES {
                if self.pending.is_empty() && eof {
                    return Ok(None);
                }
                let partial = newline.is_none() && !eof;
                let mut end = self.pending.len();
                if partial {
                    // Inspect just the trailing code point; invalid bytes
                    // earlier in the chunk must not hide an incomplete suffix.
                    for back in 1..=3.min(self.pending.len()) {
                        let start = self.pending.len() - back;
                        if self.pending[start] >= 0xc0
                            && let Err(err) = std::str::from_utf8(&self.pending[start..])
                            && err.error_len().is_none()
                        {
                            end = start;
                            break;
                        }
                    }
                    if self.pending.get(end.wrapping_sub(1)) == Some(&b'\r') {
                        end -= 1;
                    }
                }
                let remainder = self.pending.split_off(end);
                let mut bytes = std::mem::replace(&mut self.pending, remainder);
                if newline.is_some() {
                    bytes.pop();
                    if bytes.last() == Some(&b'\r') {
                        bytes.pop();
                    }
                }
                if bytes.is_empty() && self.continuing {
                    self.continuing = partial;
                    continue;
                }
                self.continuing = partial;
                return Ok(Some((
                    String::from_utf8_lossy(&bytes).into_owned(),
                    partial,
                )));
            }
        }
    }
}

/// Byte offsets belong to immutable generation filenames, so rotating a file
/// cannot cause duplicates or confuse a follower. Incomplete records are retried.
#[derive(Default)]
pub(crate) struct Reader {
    session: String,
    offsets: BTreeMap<PathBuf, u64>,
    legacy_offset: u64,
}

impl Reader {
    /// Keep disk reads and JSON decoding off the async UI/IPC executor.
    pub async fn poll(
        &mut self,
        path: &Path,
        filters: &[String],
        tail: Option<usize>,
    ) -> io::Result<Vec<String>> {
        let mut reader = std::mem::take(self);
        let path = path.to_owned();
        let filters = filters.to_vec();
        let (reader, result) = tokio::task::spawn_blocking(move || {
            let result = reader.read(&path, &filters, tail);
            (reader, result)
        })
        .await
        .map_err(io::Error::other)?;
        *self = reader;
        result
    }

    pub fn read(
        &mut self,
        daemon_log: &Path,
        filters: &[String],
        tail: Option<usize>,
    ) -> io::Result<Vec<String>> {
        let root = directory(daemon_log);
        let session = match fs::read_to_string(root.join("session")) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return self.legacy(daemon_log, filters, tail);
            }
            Err(e) => return Err(e),
        };
        // If an older daemon replaced a newer one, ignore stale structured logs.
        if let Ok(pid) = fs::read_to_string(daemon_log.with_extension("pid"))
            && session.split(':').next() != Some(pid.trim())
        {
            return self.legacy(daemon_log, filters, tail);
        }
        if self.session != session {
            self.offsets.clear();
            self.session = session;
        }
        let entries: Vec<PathBuf> = fs::read_dir(&root)?
            .map(|e| e.map(|e| e.path()))
            .collect::<io::Result<_>>()?;
        let mut keys = BTreeSet::new();
        for path in &entries {
            if let Some(key) = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_suffix(".meta.json"))
            {
                let identity: Identity = serde_json::from_slice(&fs::read(path)?)?;
                if identity.matches(filters) {
                    keys.insert(key.to_owned());
                }
            }
        }
        let mut files = Vec::new();
        for path in entries {
            if let Some(stem) = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_suffix(".jsonl"))
                && let Some((key, generation)) = stem.rsplit_once('.')
                && keys.contains(key)
                && let Ok(generation) = generation.parse::<u64>()
            {
                files.push((key.to_owned(), generation, path));
            }
        }
        files.sort();
        let active: BTreeSet<_> = files.iter().map(|(_, _, p)| p.clone()).collect();
        self.offsets.retain(|p, _| active.contains(p));
        // Keep only the newest requested records, without retaining whole files.
        let mut records = BTreeMap::new();
        let mut tie = 0usize;
        for (_, _, path) in files {
            let mut file = match File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let len = file.metadata()?.len();
            let offset = self.offsets.entry(path).or_default();
            if tail == Some(0) {
                // Skip backlog cheaply, stopping at the last complete record.
                let start = len.saturating_sub(RECORD_BYTES).max(*offset);
                file.seek(SeekFrom::Start(start))?;
                let mut bytes = Vec::new();
                file.take(len.saturating_sub(start))
                    .read_to_end(&mut bytes)?;
                if let Some(end) = bytes.iter().rposition(|b| *b == b'\n') {
                    *offset = start + end as u64 + 1;
                }
                continue;
            }
            file.seek(SeekFrom::Start(*offset))?;
            let mut reader = BufReader::new(file.take(len.saturating_sub(*offset)));
            loop {
                let mut bytes = Vec::new();
                let count = reader
                    .by_ref()
                    .take(RECORD_BYTES)
                    .read_until(b'\n', &mut bytes)?;
                if count as u64 == RECORD_BYTES && bytes.last() != Some(&b'\n') {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "log record exceeds size limit",
                    ));
                }
                if count == 0 || bytes.last() != Some(&b'\n') {
                    break;
                }
                *offset += count as u64;
                let record: Record = serde_json::from_slice(&bytes)?;
                tie += 1;
                records.insert(
                    (record.timestamp.clone(), tie),
                    record.render(filters.len() == 1),
                );
                if let Some(limit) = tail
                    && records.len() > limit
                {
                    records.pop_first();
                }
            }
        }
        Ok(records.into_values().collect())
    }

    fn legacy(
        &mut self,
        path: &Path,
        filters: &[String],
        tail: Option<usize>,
    ) -> io::Result<Vec<String>> {
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        if len < self.legacy_offset {
            self.legacy_offset = 0;
        }
        file.seek(SeekFrom::Start(self.legacy_offset))?;
        let mut reader = BufReader::new(file.take(len - self.legacy_offset));
        let mut output = std::collections::VecDeque::new();
        loop {
            let mut bytes = Vec::new();
            let count = reader
                .by_ref()
                .take(RECORD_BYTES)
                .read_until(b'\n', &mut bytes)?;
            if count == 0 {
                break;
            }
            if bytes.last() != Some(&b'\n') && (count as u64) < RECORD_BYTES {
                break;
            }
            self.legacy_offset += count as u64;
            let text = String::from_utf8_lossy(&bytes);
            for line in crate::filter_log_lines(&[text.trim_end_matches(['\r', '\n'])], filters) {
                output.push_back(line.to_owned());
                if let Some(limit) = tail
                    && output.len() > limit
                {
                    output.pop_front();
                }
            }
        }
        Ok(output.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("instance.log");
        let store = Store::new(&path).unwrap();
        (tmp, path, store)
    }

    #[test]
    fn isolated_replicas_keep_identity_when_display_name_changes() {
        let (_tmp, path, store) = setup();
        let first = store.writer("api", 1).unwrap();
        first
            .lock()
            .unwrap()
            .write("api", "stdout", "one", false)
            .unwrap();
        store
            .writer("api", 2)
            .unwrap()
            .lock()
            .unwrap()
            .write("api[2]", "stderr", "two", false)
            .unwrap();
        first
            .lock()
            .unwrap()
            .write("api[1]", "stdout", "three", false)
            .unwrap();
        assert!(Arc::ptr_eq(&first, &store.writer("api", 1).unwrap()));
        let mut reader = Reader::default();
        assert_eq!(
            reader.read(&path, &["api[1]".into()], None).unwrap(),
            ["one", "three"]
        );
        assert!(
            reader
                .read(&path, &["api[1]".into()], None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            Reader::default()
                .read(&path, &["api".into()], Some(2))
                .unwrap(),
            ["two", "three"]
        );
        assert_eq!(
            Reader::default().read(&path, &[], None).unwrap(),
            ["[api] one", "[api[2]] two", "[api[1]] three"]
        );
    }

    #[test]
    fn names_are_safe_and_collision_resistant() {
        let (_tmp, path, store) = setup();
        for service in ["../api", ".._api", "a/b", "a_b", &"x".repeat(1000)] {
            let writer = store.writer(service, 1).unwrap();
            let mut writer = writer.lock().unwrap();
            assert!(!writer.key.contains('/'));
            assert!(writer.key.len() < 120);
            writer.write(service, "stdout", "ok", false).unwrap();
        }
        assert_eq!(Reader::default().read(&path, &[], None).unwrap().len(), 5);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(directory(&path)).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for entry in fs::read_dir(directory(&path)).unwrap() {
                assert_eq!(
                    entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn rotation_bounds_storage_and_follow_has_no_duplicates() {
        let (_tmp, path, store) = setup();
        let writer = store.writer("api", 1).unwrap();
        writer.lock().unwrap().limit = 250;
        let mut reader = Reader::default();
        for i in 0..12 {
            writer
                .lock()
                .unwrap()
                .write("api", "stdout", &format!("message {i}"), false)
                .unwrap();
            assert_eq!(
                reader.read(&path, &["api".into()], None).unwrap(),
                [format!("message {i}")]
            );
        }
        let files: Vec<_> = fs::read_dir(directory(&path))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        assert_eq!(files.len(), FILE_COUNT as usize);
        assert!(files.iter().all(|p| fs::metadata(p).unwrap().len() <= 250));
        assert_eq!(
            Reader::default()
                .read(&path, &["api".into()], None)
                .unwrap(),
            ["message 8", "message 9", "message 10", "message 11"]
        );
    }

    #[test]
    fn incomplete_records_are_retried_and_unrelated_files_are_not_read() {
        let (_tmp, path, store) = setup();
        let writer = store.writer("api", 1).unwrap();
        writer
            .lock()
            .unwrap()
            .write("api", "stdout", "hello é\nworld", false)
            .unwrap();
        let file = directory(&path).join(format!("{}.0.jsonl", writer.lock().unwrap().key));
        let bytes = fs::read(&file).unwrap();
        let cut = bytes.iter().position(|b| *b == 0xc3).unwrap() + 1;
        fs::write(&file, &bytes[..cut]).unwrap();
        let mut reader = Reader::default();
        assert!(reader.read(&path, &[], None).unwrap().is_empty());
        OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(&bytes[cut..])
            .unwrap();
        assert_eq!(
            reader.read(&path, &["api".into()], None).unwrap(),
            ["hello é\nworld"]
        );
        let other = store.writer("other", 1).unwrap();
        let other_file = directory(&path).join(format!("{}.0.jsonl", other.lock().unwrap().key));
        fs::write(other_file, "invalid json\n").unwrap();
        assert!(
            reader
                .read(&path, &["api".into()], None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn restart_resets_reader_and_legacy_logs_still_work() {
        let (_tmp, path, store) = setup();
        store
            .writer("api", 1)
            .unwrap()
            .lock()
            .unwrap()
            .write("api", "stdout", "old", false)
            .unwrap();
        let mut reader = Reader::default();
        reader.read(&path, &[], Some(0)).unwrap();
        drop(store);
        let store = Store::new(&path).unwrap();
        store
            .writer("api", 1)
            .unwrap()
            .lock()
            .unwrap()
            .write("api", "stdout", "new", false)
            .unwrap();
        assert_eq!(reader.read(&path, &["api".into()], None).unwrap(), ["new"]);
        fs::remove_dir_all(directory(&path)).unwrap();
        fs::write(&path, "[api] legacy\n[other] nope\n[api] par").unwrap();
        assert_eq!(
            reader.read(&path, &["api".into()], None).unwrap(),
            ["legacy"]
        );
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"tial\n")
            .unwrap();
        assert_eq!(
            reader.read(&path, &["api".into()], None).unwrap(),
            ["partial"]
        );
    }

    #[test]
    fn tail_zero_keeps_incomplete_record_and_timestamp_merge_is_stable() {
        let (_tmp, path, store) = setup();
        let a = store.writer("a", 1).unwrap();
        let b = store.writer("b", 1).unwrap();
        b.lock()
            .unwrap()
            .write("b", "stdout", "earlier", false)
            .unwrap();
        a.lock()
            .unwrap()
            .write("a", "stdout", "later", false)
            .unwrap();
        assert_eq!(
            Reader::default().read(&path, &[], None).unwrap(),
            ["[b] earlier", "[a] later"]
        );
        let file = directory(&path).join(format!("{}.0.jsonl", a.lock().unwrap().key));
        let original = fs::read(&file).unwrap();
        let mut next: Record = serde_json::from_slice(&original).unwrap();
        next.message = "next".into();
        let mut bytes = serde_json::to_vec(&next).unwrap();
        bytes.push(b'\n');
        let cut = bytes.len() / 2;
        OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(&bytes[..cut])
            .unwrap();
        let mut reader = Reader::default();
        assert!(reader.read(&path, &[], Some(0)).unwrap().is_empty());
        OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(&bytes[cut..])
            .unwrap();
        assert_eq!(reader.read(&path, &[], None).unwrap(), ["[a] next"]);
    }

    #[tokio::test]
    async fn chunk_boundary_after_invalid_utf8_and_before_crlf() {
        let mut input = vec![b'x'; CHUNK_BYTES - 1];
        input[0] = 0xff;
        input.extend_from_slice("é\r\n".as_bytes());
        let mut reader = Chunks::new(tokio::io::BufReader::new(input.as_slice()));
        let first = reader.next().await.unwrap().unwrap();
        assert!(first.1);
        assert_eq!(first.0, format!("�{}", "x".repeat(CHUNK_BYTES - 2)));
        assert_eq!(reader.next().await.unwrap(), Some(("é".into(), false)));
        let input = format!("{}\r\n", "x".repeat(CHUNK_BYTES - 1));
        let mut reader = Chunks::new(tokio::io::BufReader::new(input.as_bytes()));
        assert_eq!(
            reader.next().await.unwrap(),
            Some(("x".repeat(CHUNK_BYTES - 1), true))
        );
        assert_eq!(reader.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn bounded_chunks_preserve_unicode_and_flush_unterminated_output() {
        let input = format!(
            "{}é{}\nlast",
            "a".repeat(CHUNK_BYTES - 1),
            "b".repeat(CHUNK_BYTES)
        );
        let mut reader = Chunks::new(tokio::io::BufReader::new(input.as_bytes()));
        let mut parts = Vec::new();
        while let Some((text, partial)) = reader.next().await.unwrap() {
            assert!(text.len() <= CHUNK_BYTES);
            parts.push((text, partial));
        }
        assert!(parts[0].1);
        assert!(!parts.last().unwrap().1);
        assert_eq!(
            parts.into_iter().map(|(s, _)| s).collect::<String>(),
            input.replace('\n', "")
        );
    }

    #[tokio::test]
    async fn exact_chunk_boundaries_empty_lines_and_invalid_utf8() {
        let mut input = vec![b'x'; CHUNK_BYTES];
        input.extend_from_slice(b"\n\n\xff\r\nlast");
        let mut reader = Chunks::new(tokio::io::BufReader::new(input.as_slice()));
        assert_eq!(
            reader.next().await.unwrap(),
            Some(("x".repeat(CHUNK_BYTES), true))
        );
        assert_eq!(reader.next().await.unwrap(), Some((String::new(), false)));
        assert_eq!(reader.next().await.unwrap(), Some(("�".into(), false)));
        assert_eq!(reader.next().await.unwrap(), Some(("last".into(), false)));
        assert_eq!(reader.next().await.unwrap(), None);
    }
}
