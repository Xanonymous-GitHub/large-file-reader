use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

const READ_CHUNK_SIZE: usize = 8 * 1024;
const BACK_SCAN_LIMIT: u64 = 1024 * 1024;

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
    use super::{LargeFile, TokenKind, highlight_json_line};
    use std::io::Write;
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
}
