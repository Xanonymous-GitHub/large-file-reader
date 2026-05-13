use std::{
    env,
    fs::File,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process,
    sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use memchr::memmem::Finder;

const READ_CHUNK_SIZE: usize = 8 * 1024;
const BACK_SCAN_LIMIT: u64 = 1024 * 1024;
pub const SEARCH_CHUNK_SIZE: usize = 64 * 1024;
const PROGRESS_INTERVAL: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchUpdate {
    Found(SearchMatch),
    Progress { bytes_scanned: u64 },
    Finished { matches: usize, bytes_scanned: u64 },
    Failed(String),
}

pub struct SearchHandle {
    receiver: Receiver<SearchUpdate>,
}

impl SearchHandle {
    pub fn try_recv(&self) -> Result<SearchUpdate, TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<SearchUpdate, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatUpdate {
    Progress { bytes_read: u64 },
    Finished { path: PathBuf, bytes_read: u64 },
    Failed(String),
}

pub struct FormatHandle {
    receiver: Receiver<FormatUpdate>,
    output_path: PathBuf,
}

impl FormatHandle {
    pub fn try_recv(&self) -> Result<FormatUpdate, TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<FormatUpdate, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    #[must_use]
    pub fn output_path(&self) -> &Path {
        &self.output_path
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    String,
    Number,
    Boolean,
    Null,
    Bracket,
    Colon,
    Comma,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonToken {
    pub text: String,
    pub kind: TokenKind,
}

#[must_use]
pub fn highlight_json_line(line: &str) -> Vec<JsonToken> {
    let mut tokens = Vec::new();
    let mut chars = line.char_indices().peekable();

    while let Some((start, ch)) = chars.next() {
        let (end, kind) = match ch {
            c if c.is_whitespace() => {
                let end = consume_while(&mut chars, start + c.len_utf8(), |c| c.is_whitespace());
                (end, TokenKind::Whitespace)
            }
            '"' => {
                let end = consume_string(&mut chars, start + ch.len_utf8());
                (end, TokenKind::String)
            }
            '-' | '0'..='9' => {
                let end = consume_while(&mut chars, start + ch.len_utf8(), |c| {
                    c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')
                });
                (end, TokenKind::Number)
            }
            'a'..='z' | 'A'..='Z' => {
                let end = consume_while(&mut chars, start + ch.len_utf8(), |c| {
                    c.is_ascii_alphabetic()
                });
                let kind = match &line[start..end] {
                    "true" | "false" => TokenKind::Boolean,
                    "null" => TokenKind::Null,
                    _ => TokenKind::Other,
                };
                (end, kind)
            }
            '{' | '}' | '[' | ']' => (start + ch.len_utf8(), TokenKind::Bracket),
            ':' => (start + ch.len_utf8(), TokenKind::Colon),
            ',' => (start + ch.len_utf8(), TokenKind::Comma),
            _ => (start + ch.len_utf8(), TokenKind::Other),
        };

        tokens.push(JsonToken {
            text: line[start..end].to_owned(),
            kind,
        });
    }

    tokens
}

fn consume_while(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    mut end: usize,
    mut predicate: impl FnMut(char) -> bool,
) -> usize {
    while let Some(&(index, ch)) = chars.peek() {
        if !predicate(ch) {
            break;
        }
        chars.next();
        end = index + ch.len_utf8();
    }
    end
}

fn consume_string(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    mut end: usize,
) -> usize {
    let mut escaped = false;

    for (index, ch) in chars.by_ref() {
        end = index + ch.len_utf8();
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            break;
        }
    }

    end
}

pub fn start_search(path: PathBuf, query: String, start_offset: u64) -> io::Result<SearchHandle> {
    let len = File::open(&path)?.metadata()?.len();
    let (sender, receiver) = mpsc::channel();

    thread::spawn(move || {
        if let Err(error) = search_file(
            path,
            query.into_bytes(),
            start_offset.min(len),
            len,
            &sender,
        ) {
            let _ = sender.send(SearchUpdate::Failed(error.to_string()));
        }
    });

    Ok(SearchHandle { receiver })
}

fn search_file(
    path: PathBuf,
    query: Vec<u8>,
    start_offset: u64,
    len: u64,
    sender: &mpsc::Sender<SearchUpdate>,
) -> io::Result<()> {
    if query.is_empty() {
        let _ = sender.send(SearchUpdate::Finished {
            matches: 0,
            bytes_scanned: 0,
        });
        return Ok(());
    }

    let mut file = File::open(path)?;
    let mut matches = 0;
    let mut bytes_scanned = 0;

    matches += scan_range(
        &mut file,
        &query,
        start_offset,
        len,
        sender,
        &mut bytes_scanned,
    )?;
    if start_offset > 0 {
        matches += scan_range(
            &mut file,
            &query,
            0,
            start_offset,
            sender,
            &mut bytes_scanned,
        )?;
    }

    let _ = sender.send(SearchUpdate::Finished {
        matches,
        bytes_scanned,
    });
    Ok(())
}

fn scan_range(
    file: &mut File,
    query: &[u8],
    start: u64,
    end: u64,
    sender: &mpsc::Sender<SearchUpdate>,
    bytes_scanned: &mut u64,
) -> io::Result<usize> {
    if start >= end {
        return Ok(0);
    }

    let finder = Finder::new(query);
    let mut buf = vec![0_u8; SEARCH_CHUNK_SIZE];
    let mut carry = Vec::with_capacity(query.len().saturating_sub(1));
    let mut position = start;
    let mut matches = 0;
    let mut next_progress = *bytes_scanned + PROGRESS_INTERVAL;

    file.seek(SeekFrom::Start(start))?;

    while position < end {
        let remaining = usize::try_from((end - position).min(SEARCH_CHUNK_SIZE as u64))
            .expect("search chunk size fits usize");
        let read = file.read(&mut buf[..remaining])?;
        if read == 0 {
            break;
        }

        let chunk_start = position;
        position += read as u64;
        *bytes_scanned += read as u64;

        let mut searchable = Vec::with_capacity(carry.len() + read);
        searchable.extend_from_slice(&carry);
        searchable.extend_from_slice(&buf[..read]);
        let searchable_start = chunk_start.saturating_sub(carry.len() as u64);

        for index in finder.find_iter(&searchable) {
            let offset = searchable_start + index as u64;
            let match_end = offset + query.len() as u64;
            if offset < start || match_end > end || match_end <= chunk_start {
                continue;
            }
            matches += 1;
            if sender
                .send(SearchUpdate::Found(SearchMatch { offset }))
                .is_err()
            {
                return Ok(matches);
            }
        }

        let keep = query.len().saturating_sub(1).min(searchable.len());
        carry.clear();
        carry.extend_from_slice(&searchable[searchable.len() - keep..]);

        if *bytes_scanned >= next_progress {
            if sender
                .send(SearchUpdate::Progress {
                    bytes_scanned: *bytes_scanned,
                })
                .is_err()
            {
                return Ok(matches);
            }
            next_progress = *bytes_scanned + PROGRESS_INTERVAL;
        }
    }

    Ok(matches)
}

pub fn start_format(path: PathBuf) -> io::Result<FormatHandle> {
    let input_len = File::open(&path)?.metadata()?.len();
    let output_path = temp_format_path();
    let (sender, receiver) = mpsc::channel();
    let worker_output_path = output_path.clone();

    thread::spawn(move || {
        let progress_sender = sender.clone();
        let mut bytes_read = 0;
        let result = format_json_file(&path, &worker_output_path, |read| {
            bytes_read = read;
            let _ = progress_sender.send(FormatUpdate::Progress { bytes_read: read });
        });

        match result {
            Ok(()) => {
                let _ = sender.send(FormatUpdate::Finished {
                    path: worker_output_path,
                    bytes_read: bytes_read.max(input_len),
                });
            }
            Err(error) => {
                let _ = std::fs::remove_file(&worker_output_path);
                let _ = sender.send(FormatUpdate::Failed(error.to_string()));
            }
        }
    });

    Ok(FormatHandle {
        receiver,
        output_path,
    })
}

pub fn format_json_file(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    progress: impl FnMut(u64),
) -> io::Result<()> {
    let input = File::open(input)?;
    let output = File::create(output)?;
    let reader = ProgressReader::new(BufReader::with_capacity(1024 * 1024, input), progress);
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let writer = BufWriter::with_capacity(1024 * 1024, output);
    let mut serializer = serde_json::Serializer::pretty(writer);

    serde_transcode::transcode(&mut deserializer, &mut serializer).map_err(io::Error::other)?;
    serializer.into_inner().flush()
}

struct ProgressReader<R, F> {
    inner: R,
    progress: F,
    bytes_read: u64,
    next_report: u64,
}

impl<R, F> ProgressReader<R, F> {
    fn new(inner: R, progress: F) -> Self {
        Self {
            inner,
            progress,
            bytes_read: 0,
            next_report: PROGRESS_INTERVAL,
        }
    }
}

impl<R: Read, F: FnMut(u64)> Read for ProgressReader<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read == 0 {
            return Ok(0);
        }

        self.bytes_read += read as u64;
        if self.bytes_read >= self.next_report {
            (self.progress)(self.bytes_read);
            self.next_report = self.bytes_read + PROGRESS_INTERVAL;
        }
        Ok(read)
    }
}

fn temp_format_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    env::temp_dir().join(format!(
        "large-json-reader-{}-{nanos}.pretty.json",
        process::id()
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualLine {
    pub start_offset: u64,
    pub next_offset: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub start_offset: u64,
    pub file_len: u64,
    pub lines: Vec<VisualLine>,
}

pub struct LargeFile {
    file: File,
    len: u64,
}

impl LargeFile {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }

    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn read_window(&mut self, start: u64, width: usize, height: usize) -> io::Result<Window> {
        let start = start.min(self.len);
        if height == 0 {
            return Ok(Window {
                start_offset: start,
                file_len: self.len,
                lines: Vec::new(),
            });
        }

        let width = width.max(1);
        let mut lines = Vec::with_capacity(height);
        let mut bytes = Vec::with_capacity(width.min(4096));
        let mut buf = [0_u8; READ_CHUNK_SIZE];
        let mut offset = start;
        let mut line_start = start;
        let mut skip_empty_newline_at = None;

        self.file.seek(SeekFrom::Start(start))?;

        while lines.len() < height {
            let read = self.file.read(&mut buf)?;
            if read == 0 {
                if !bytes.is_empty() {
                    lines.push(visual_line(line_start, offset, &bytes));
                }
                break;
            }

            for byte in &buf[..read] {
                offset += 1;

                if *byte == b'\n' {
                    if bytes.is_empty() && skip_empty_newline_at == Some(line_start) {
                        skip_empty_newline_at = None;
                    } else {
                        if bytes.last() == Some(&b'\r') {
                            bytes.pop();
                        }
                        lines.push(visual_line(line_start, offset, &bytes));
                    }
                    bytes.clear();
                    line_start = offset;
                } else {
                    skip_empty_newline_at = None;
                    bytes.push(*byte);
                    if bytes.len() >= width {
                        lines.push(visual_line(line_start, offset, &bytes));
                        bytes.clear();
                        line_start = offset;
                        skip_empty_newline_at = Some(line_start);
                    }
                }

                if lines.len() == height {
                    break;
                }
            }
        }

        Ok(Window {
            start_offset: start,
            file_len: self.len,
            lines,
        })
    }

    pub fn previous_visual_offset(&mut self, offset: u64, width: usize) -> io::Result<u64> {
        let width = width.max(1);
        let offset = offset.min(self.len);
        if offset == 0 {
            return Ok(0);
        }

        let scan_start = offset.saturating_sub(BACK_SCAN_LIMIT);
        let scan_len = usize::try_from(offset - scan_start)
            .expect("back scan length is bounded by BACK_SCAN_LIMIT");
        let mut buf = vec![0_u8; scan_len];
        self.file.seek(SeekFrom::Start(scan_start))?;
        self.file.read_exact(&mut buf)?;

        let trusted_index = if scan_start == 0 {
            0
        } else if let Some(index) = buf.iter().position(|byte| *byte == b'\n') {
            index + 1
        } else {
            return Ok(offset.saturating_sub(width as u64));
        };

        let trusted_offset = scan_start + trusted_index as u64;
        if trusted_offset >= offset {
            return Ok(offset.saturating_sub(width as u64));
        }

        let mut starts = vec![trusted_offset];
        let mut column = 0_usize;
        for (relative_index, byte) in buf[trusted_index..].iter().enumerate() {
            let absolute_offset = trusted_offset + relative_index as u64;
            if absolute_offset >= offset {
                break;
            }

            if *byte == b'\n' {
                let next = absolute_offset + 1;
                if next < offset {
                    starts.push(next);
                }
                column = 0;
            } else {
                column += 1;
                if column >= width {
                    let next = absolute_offset + 1;
                    let next_is_newline = buf
                        .get(trusted_index + relative_index + 1)
                        .is_some_and(|byte| *byte == b'\n');
                    if next < offset && !next_is_newline {
                        starts.push(next);
                    }
                    column = 0;
                }
            }
        }

        Ok(starts
            .into_iter()
            .rev()
            .find(|start| *start < offset)
            .unwrap_or(0))
    }

    pub fn near_end_offset(&mut self, width: usize, height: usize) -> io::Result<u64> {
        let mut offset = self.len;
        for _ in 0..height {
            let previous = self.previous_visual_offset(offset, width)?;
            offset = previous;
            if offset == 0 {
                break;
            }
        }
        Ok(offset)
    }
}

fn visual_line(start_offset: u64, next_offset: u64, bytes: &[u8]) -> VisualLine {
    VisualLine {
        start_offset,
        next_offset,
        text: String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FormatUpdate, LargeFile, SEARCH_CHUNK_SIZE, SearchUpdate, TokenKind, format_json_file,
        highlight_json_line, start_format, start_search,
    };
    use std::{fs, io::Write, time::Duration};
    use tempfile::NamedTempFile;

    fn temp_json(contents: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("temp file");
        file.write_all(contents).expect("write temp json");
        file.flush().expect("flush temp json");
        file
    }

    #[test]
    fn highlights_json_primitives() {
        let tokens = highlight_json_line(r#"{"name": "Ada", "age": 42, "ok": true, "none": null}"#);

        assert!(
            tokens
                .iter()
                .any(|token| token.text == r#""name""# && token.kind == TokenKind::String)
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.text == "42" && token.kind == TokenKind::Number)
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.text == "true" && token.kind == TokenKind::Boolean)
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.text == "null" && token.kind == TokenKind::Null)
        );
    }

    #[test]
    fn reads_bounded_visual_window_from_long_line() {
        let file = temp_json(
            br#"{"numbers":[0,1,2,3,4,5,6,7,8,9]}
{"done":true}
"#,
        );
        let mut reader = LargeFile::open(file.path()).expect("open temp json");

        let window = reader.read_window(0, 10, 3).expect("read window");

        assert_eq!(window.start_offset, 0);
        assert_eq!(window.file_len, reader.len());
        assert_eq!(window.lines.len(), 3);
        assert_eq!(window.lines[0].start_offset, 0);
        assert_eq!(window.lines[0].next_offset, 10);
        assert_eq!(window.lines[0].text, "{\"numbers\"");
        assert_eq!(window.lines[1].start_offset, 10);
        assert_eq!(window.lines[1].next_offset, 20);
        assert_eq!(window.lines[1].text, ":[0,1,2,3,");
        assert_eq!(window.lines[2].start_offset, 20);
    }

    #[test]
    fn previous_visual_offset_handles_wrapped_lines() {
        let file = temp_json(b"abcdefghijklmnopqrst\nxyz\n");
        let mut reader = LargeFile::open(file.path()).expect("open temp json");

        assert_eq!(reader.previous_visual_offset(15, 5).expect("prev"), 10);
        assert_eq!(reader.previous_visual_offset(10, 5).expect("prev"), 5);
        assert_eq!(reader.previous_visual_offset(5, 5).expect("prev"), 0);
        assert_eq!(reader.previous_visual_offset(21, 5).expect("prev"), 15);
    }

    #[test]
    fn near_end_offset_returns_start_for_last_screen() {
        let file = temp_json(b"abcdefghijklmnopqrst");
        let mut reader = LargeFile::open(file.path()).expect("open temp json");

        assert_eq!(reader.near_end_offset(5, 2).expect("near end"), 10);
    }

    #[test]
    fn given_query_across_chunk_boundary_when_background_search_runs_then_first_match_offset_arrives()
     {
        let mut contents = vec![b'x'; SEARCH_CHUNK_SIZE - 3];
        contents.extend_from_slice(b"needle");
        contents.extend_from_slice(b" trailing");
        let file = temp_json(&contents);

        let search =
            start_search(file.path().to_path_buf(), "needle".to_owned(), 0).expect("start search");
        let mut found_offset = None;

        loop {
            match search
                .recv_timeout(Duration::from_secs(2))
                .expect("search update")
            {
                SearchUpdate::Found(found) => found_offset = Some(found.offset),
                SearchUpdate::Finished { .. } => break,
                SearchUpdate::Progress { .. } => {}
                SearchUpdate::Failed(message) => panic!("search failed: {message}"),
            }
        }

        assert_eq!(found_offset, Some((SEARCH_CHUNK_SIZE - 3) as u64));
    }

    #[test]
    fn given_compact_json_when_formatter_streams_file_then_pretty_json_is_written() {
        let input = temp_json(br#"{"name":"Ada","items":[1,true,null]}"#);
        let output = NamedTempFile::new().expect("temp output");

        format_json_file(input.path(), output.path(), |_| {}).expect("format json");

        assert_eq!(
            fs::read_to_string(output.path()).expect("read formatted json"),
            "{\n  \"name\": \"Ada\",\n  \"items\": [\n    1,\n    true,\n    null\n  ]\n}"
        );
    }

    #[test]
    fn given_compact_json_when_background_format_runs_then_finished_file_can_be_viewed() {
        let input = temp_json(br#"{"ok":true}"#);
        let format = start_format(input.path().to_path_buf()).expect("start format");
        let formatted_path = loop {
            match format
                .recv_timeout(Duration::from_secs(2))
                .expect("format update")
            {
                FormatUpdate::Finished { path, .. } => break path,
                FormatUpdate::Progress { .. } => {}
                FormatUpdate::Failed(message) => panic!("format failed: {message}"),
            }
        };

        assert_eq!(
            fs::read_to_string(formatted_path).expect("read formatted json"),
            "{\n  \"ok\": true\n}"
        );
    }
}
