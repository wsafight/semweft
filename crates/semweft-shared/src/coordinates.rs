use serde::{Deserialize, Serialize};

/// Public positions use one-based lines and UTF-8 byte columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Span {
    pub start: Position,
    pub end: Position,
}

impl Span {
    pub fn contains(&self, position: Position) -> bool {
        self.start <= position && position < self.end
    }
}

pub fn byte_offset(text: &str, pos: Position) -> Result<usize, String> {
    if pos.line == 0 || pos.column == 0 {
        return Err("positions start at 1".into());
    }
    let start = if pos.line == 1 {
        0
    } else {
        text.match_indices('\n')
            .nth(pos.line as usize - 2)
            .map(|(index, _)| index + 1)
            .ok_or("line out of bounds")?
    };
    let line_end = text[start..].find('\n').map_or(text.len(), |n| start + n);
    let offset = start
        .checked_add(pos.column as usize - 1)
        .ok_or("column overflow")?;
    if offset > line_end || !text.is_char_boundary(offset) {
        return Err("column out of bounds or inside a UTF-8 character".into());
    }
    Ok(offset)
}

pub fn to_utf16(text: &str, pos: Position) -> Result<(u32, u32), String> {
    let offset = byte_offset(text, pos)?;
    let start = text[..offset].rfind('\n').map_or(0, |n| n + 1);
    Ok((
        pos.line - 1,
        text[start..offset].encode_utf16().count() as u32,
    ))
}

pub fn from_utf16(text: &str, line: u32, character: u32) -> Result<Position, String> {
    let line = line.checked_add(1).ok_or("line overflow")?;
    let start = byte_offset(text, Position { line, column: 1 })?;
    let line_text = text[start..].split('\n').next().unwrap_or("");
    let mut units = 0;
    for (byte, ch) in line_text.char_indices() {
        if units == character {
            return Ok(Position {
                line,
                column: u32::try_from(byte)
                    .ok()
                    .and_then(|n| n.checked_add(1))
                    .ok_or("column overflow")?,
            });
        }
        units = units
            .checked_add(ch.len_utf16() as u32)
            .ok_or("UTF-16 column overflow")?;
        if units > character {
            return Err("position splits a UTF-16 surrogate pair".into());
        }
    }
    if units == character {
        Ok(Position {
            line,
            column: u32::try_from(line_text.len())
                .ok()
                .and_then(|n| n.checked_add(1))
                .ok_or("column overflow")?,
        })
    } else {
        Err("UTF-16 column out of bounds".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_round_trip_and_invalid_boundaries() {
        let text = "// 中文😀\nfn café() {}\n";
        let pos = Position {
            line: 1,
            column: 14,
        };
        assert_eq!(to_utf16(text, pos).unwrap(), (0, 7));
        assert_eq!(from_utf16(text, 0, 7).unwrap(), pos);
        assert!(byte_offset(text, Position { line: 1, column: 5 }).is_err());
        assert!(from_utf16(text, 0, 6).is_err());
        assert!(byte_offset(text, Position { line: 0, column: 1 }).is_err());
        assert!(byte_offset(text, Position { line: 4, column: 1 }).is_err());
        assert_eq!(
            from_utf16(text, 2, 0).unwrap(),
            Position { line: 3, column: 1 }
        );
    }

    #[test]
    fn line_and_column_boundaries_are_enforced() {
        let text = "ab\ncd\n";
        // Columns one past the last byte of a line are valid (insertion point).
        assert_eq!(
            byte_offset(text, Position { line: 1, column: 3 }).unwrap(),
            2
        );
        assert_eq!(
            byte_offset(text, Position { line: 2, column: 3 }).unwrap(),
            5
        );
        assert!(byte_offset(text, Position { line: 1, column: 4 }).is_err());
        assert!(byte_offset(text, Position { line: 3, column: 1 }).is_ok());
        assert!(byte_offset(text, Position { line: 3, column: 2 }).is_err());
        assert!(byte_offset("", Position { line: 1, column: 1 }).is_ok());
        assert!(byte_offset("", Position { line: 1, column: 2 }).is_err());
        assert!(byte_offset("", Position { line: 2, column: 1 }).is_err());
    }

    #[test]
    fn crlf_and_missing_final_newline_use_byte_columns() {
        let text = "a\r\nbc";
        assert_eq!(
            byte_offset(text, Position { line: 2, column: 1 }).unwrap(),
            3
        );
        assert_eq!(
            byte_offset(text, Position { line: 2, column: 2 }).unwrap(),
            4
        );
        assert_eq!(
            to_utf16(text, Position { line: 2, column: 2 }).unwrap(),
            (1, 1)
        );
        assert_eq!(
            from_utf16(text, 1, 2).unwrap(),
            Position { line: 2, column: 3 }
        );
        assert!(from_utf16(text, 1, 3).is_err());
        // The trailing `\r` belongs to the first line.
        assert_eq!(
            to_utf16(text, Position { line: 1, column: 2 }).unwrap(),
            (0, 1)
        );
    }

    #[test]
    fn spans_include_the_start_and_exclude_the_end() {
        let span = Span {
            start: Position { line: 1, column: 2 },
            end: Position { line: 1, column: 5 },
        };
        assert!(!span.contains(Position { line: 1, column: 1 }));
        assert!(span.contains(Position { line: 1, column: 2 }));
        assert!(span.contains(Position { line: 1, column: 4 }));
        assert!(!span.contains(Position { line: 1, column: 5 }));
        assert!(!span.contains(Position { line: 2, column: 2 }));
    }

    #[test]
    fn multi_line_utf16_round_trips() {
        let text = "fn café() {}\nfn 😀x() {}\n";
        for (index, line_text) in text.split('\n').enumerate() {
            let line = index as u32;
            let len = line_text.encode_utf16().count() as u32;
            for character in 0..=len {
                match from_utf16(text, line, character) {
                    Ok(position) => {
                        assert_eq!(to_utf16(text, position).unwrap(), (line, character))
                    }
                    Err(error) => assert!(error.contains("surrogate"), "{error}"),
                }
            }
        }
    }

    #[test]
    fn untrusted_utf16_coordinates_return_errors_without_overflowing() {
        assert!(from_utf16("", u32::MAX, 0).is_err());
        assert!(from_utf16("😀", 0, u32::MAX).is_err());
        assert!(
            byte_offset(
                "",
                Position {
                    line: u32::MAX,
                    column: u32::MAX
                }
            )
            .is_err()
        );
    }
}

/// A line table tied to its source, so offsets cannot be reused with other text.
pub struct LineIndex<'a> {
    text: &'a str,
    starts: Vec<usize>,
}
impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            starts: std::iter::once(0)
                .chain(text.match_indices('\n').map(|(i, _)| i + 1))
                .collect(),
        }
    }
    pub fn line(&self, line: u32) -> Result<&'a str, String> {
        let index = line.checked_sub(1).ok_or("positions start at 1")? as usize;
        let start = *self.starts.get(index).ok_or("line out of bounds")?;
        let end = self
            .starts
            .get(index + 1)
            .map_or(self.text.len(), |next| next - 1);
        Ok(&self.text[start..end])
    }
    pub fn byte_offset(&self, pos: Position) -> Result<usize, String> {
        if pos.column == 0 {
            return Err("positions start at 1".into());
        }
        let line = self.line(pos.line)?;
        let column = pos.column as usize - 1;
        if column > line.len() || !line.is_char_boundary(column) {
            return Err("column out of bounds or inside a UTF-8 character".into());
        }
        self.starts[pos.line as usize - 1]
            .checked_add(column)
            .ok_or_else(|| "column overflow".into())
    }
    pub fn to_utf16(&self, pos: Position) -> Result<(u32, u32), String> {
        self.byte_offset(pos)?;
        Ok((
            pos.line - 1,
            self.line(pos.line)?[..pos.column as usize - 1]
                .encode_utf16()
                .count() as u32,
        ))
    }
    pub fn from_utf16(&self, line: u32, character: u32) -> Result<Position, String> {
        let line = line.checked_add(1).ok_or("line overflow")?;
        let text = self.line(line)?;
        let mut units = 0u32;
        for (byte, ch) in text.char_indices() {
            if units == character {
                return Ok(Position {
                    line,
                    column: u32::try_from(byte)
                        .ok()
                        .and_then(|b| b.checked_add(1))
                        .ok_or("column overflow")?,
                });
            }
            units = units
                .checked_add(ch.len_utf16() as u32)
                .ok_or("UTF-16 column overflow")?;
            if units > character {
                return Err("position splits a UTF-16 surrogate pair".into());
            }
        }
        if units != character {
            return Err("UTF-16 column out of bounds".into());
        }
        Ok(Position {
            line,
            column: u32::try_from(text.len())
                .ok()
                .and_then(|b| b.checked_add(1))
                .ok_or("column overflow")?,
        })
    }
}

#[cfg(test)]
mod line_index_tests {
    use super::*;
    #[test]
    fn line_table_matches_scan_reference_at_every_valid_and_invalid_boundary() {
        for text in ["", "\n", "a\r\n中文😀\n", "last", "😀\r\n\nend\n"] {
            let index = LineIndex::new(text);
            for line in 0..=8 {
                for column in 0..=20 {
                    let pos = Position { line, column };
                    assert_eq!(index.byte_offset(pos), byte_offset(text, pos));
                    assert_eq!(index.to_utf16(pos), to_utf16(text, pos));
                    assert_eq!(
                        index.from_utf16(line, column),
                        from_utf16(text, line, column)
                    );
                }
            }
            assert!(
                index
                    .byte_offset(Position {
                        line: u32::MAX,
                        column: u32::MAX
                    })
                    .is_err()
            );
            assert!(index.from_utf16(u32::MAX, 0).is_err());
        }
    }
}
