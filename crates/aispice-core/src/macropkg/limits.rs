//! Limits that keep a check of a package from someone else small and quick.
//!
//! A package is untrusted input: a check reads every file in it, and a file
//! built to be large, deeply nested or self-referencing must end in a
//! finding, never in a hang, a crash or a run out of memory. Each limit is
//! far above what a real macro needs. The spec (`docs/macro-package.md`,
//! "Limits") lists them; ChipLeap's validator uses the same values
//! (`chipleap.macro.limits`), so both tools report the same findings. Tests
//! pass smaller limits to reach them with small files.

use std::io::Read;
use std::path::Path;

const MIB: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    // File sizes in bytes, checked before a file is read.
    pub manifest_bytes: u64,
    pub specs_bytes: u64,
    pub evidence_bytes: u64,
    pub liberty_bytes: u64,
    /// LEF, Verilog and SPICE.
    pub view_bytes: u64,
    pub gds_bytes: u64,
    /// Tables, arrays and liberty groups nested inside each other.
    pub nesting: usize,
    /// Pin bits in macro.toml, pins in each view.
    pub pins: usize,
    pub corners: usize,
    pub evidence_files: usize,
    /// Package files the evidence inputs name, each hashed once.
    pub hashed_files: usize,
    /// Tokens of a LEF, liberty or Verilog file; lines of a SPICE file.
    pub tokens: usize,
    pub gds_records: usize,
    pub gds_elements: usize,
    pub gds_cells: usize,
    pub gds_depth: usize,
    /// Points measured for references at angles other than quarter turns.
    pub gds_points: usize,
    pub findings: usize,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        manifest_bytes: 4 * MIB,
        specs_bytes: MIB,
        evidence_bytes: 4 * MIB,
        liberty_bytes: 16 * MIB,
        view_bytes: 64 * MIB,
        gds_bytes: 256 * MIB,
        nesting: 64,
        pins: 10_000,
        corners: 64,
        evidence_files: 64,
        hashed_files: 32,
        tokens: 8_000_000,
        gds_records: 32_000_000,
        gds_elements: 8_000_000,
        gds_cells: 100_000,
        gds_depth: 64,
        gds_points: 4_000_000,
        findings: 1_000,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Limits::DEFAULT
    }
}

/// A file goes past one of the limits; the message says which, in spec
/// wording.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LimitError(pub String);

/// Why a package file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// Larger than the limit for its kind; the message is the finding's.
    #[error("{0}")]
    TooLarge(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn too_large(size: u64, limit: u64) -> String {
    format!("file is {size} bytes, larger than the limit of {limit} bytes")
}

/// A file's bytes, after its size says it is within `limit`.
pub fn read_capped(path: &Path, limit: u64) -> Result<Vec<u8>, ReadError> {
    read_file_capped(std::fs::File::open(path)?, limit)
}

/// An open file's bytes, after its size says it is within `limit`.
pub fn read_file_capped(file: std::fs::File, limit: u64) -> Result<Vec<u8>, ReadError> {
    let size = file.metadata()?.len();
    if size > limit {
        return Err(ReadError::TooLarge(too_large(size, limit)));
    }
    let mut data = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut data)?;
    let read = data.len() as u64;
    if read > limit {
        // It grew after the stat.
        return Err(ReadError::TooLarge(too_large(size.max(read), limit)));
    }
    Ok(data)
}

/// An upper bound on how deeply a TOML document nests tables and arrays,
/// from its brackets, braces, table headers and dotted keys, without parsing
/// it.
///
/// The toml crate stops at 80 levels, and Python's TOML reader recurses on
/// nested arrays and slows down quadratically on long dotted keys, so both
/// tools check this bound against [`Limits::nesting`] before parsing.
/// ChipLeap computes it the same way. A value sits below the last table
/// header, below each open array or inline table, and below every part of the
/// dotted keys that lead to it, so the bound adds those up; a dot in a number
/// adds one more, which only overestimates. Only ASCII bytes matter, so it
/// walks bytes.
pub fn toml_depth(text: &str) -> usize {
    let s = text.as_bytes();
    let n = s.len();
    let (mut deepest, mut base, mut dots, mut pending) = (0usize, 0usize, 0usize, 0usize);
    // For each open array or inline table, the dots of the key it is the value of.
    let mut keys: Vec<usize> = Vec::new();
    let mut key_dots = 0usize;
    let mut line_start = true;
    let mut i = 0;
    while i < n {
        let c = s[i];
        match c {
            b'#' => {
                i = s[i..].iter().position(|&b| b == b'\n').map_or(n, |k| i + k);
                continue;
            }
            b'"' | b'\'' => {
                i = toml_string_end(s, i);
                line_start = false;
                continue;
            }
            b'\n' => {
                line_start = true;
                dots = 0;
                if keys.is_empty() {
                    pending = 0;
                }
                i += 1;
                continue;
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
                continue;
            }
            b'[' if keys.is_empty() && line_start => {
                // A table header, [a.b] or [[a.b]]: the keys below it nest under it.
                let array = s.get(i + 1) == Some(&b'[');
                i += if array { 2 } else { 1 };
                let mut header_dots = 0;
                while i < n && s[i] != b']' && s[i] != b'\n' {
                    if s[i] == b'"' || s[i] == b'\'' {
                        i = toml_string_end(s, i);
                        continue;
                    }
                    header_dots += usize::from(s[i] == b'.');
                    i += 1;
                }
                for _ in 0..if array { 2 } else { 1 } {
                    if i < n && s[i] == b']' {
                        i += 1;
                    }
                }
                base = header_dots + if array { 2 } else { 1 };
                deepest = deepest.max(base);
                line_start = false;
                dots = 0;
                pending = 0;
                continue;
            }
            _ => {}
        }
        line_start = false;
        match c {
            b'[' | b'{' => {
                keys.push(pending);
                key_dots += pending;
                dots = 0;
                pending = 0;
            }
            b']' | b'}' => {
                if let Some(k) = keys.pop() {
                    key_dots -= k;
                }
                dots = 0;
                pending = 0;
            }
            b'=' => {
                pending = dots;
                dots = 0;
            }
            b',' => {
                dots = 0;
                pending = 0;
            }
            b'.' => dots += 1,
            _ => {}
        }
        deepest = deepest.max(base + keys.len() + key_dots + pending + dots + 1);
        i += 1;
    }
    deepest
}

/// The index just after the string that starts at `s[i]`. An unterminated
/// one-line string ends at the line end, an unterminated multi-line one at
/// the end.
fn toml_string_end(s: &[u8], i: usize) -> usize {
    let n = s.len();
    let q = s[i];
    let triple_at = |k: usize| k + 2 < n && s[k] == q && s[k + 1] == q && s[k + 2] == q;
    let triple = triple_at(i);
    let mut j = i + if triple { 3 } else { 1 };
    while j < n {
        let c = s[j];
        if c == b'\\' && q == b'"' {
            j += 2;
            continue;
        }
        if c == b'\n' && !triple {
            return j;
        }
        if c == q {
            if !triple {
                return j + 1;
            }
            if triple_at(j) {
                // One or two more quotes just inside the closing delimiter are content.
                let mut k = j + 3;
                while k < n && k < j + 5 && s[k] == q {
                    k += 1;
                }
                return k;
            }
        }
        j += 1;
    }
    n
}

/// How deeply a JSON document nests objects and arrays, from its brackets
/// outside strings. serde_json stops at 128 levels; Python's JSON reader
/// recurses.
pub fn json_depth(text: &str) -> usize {
    let s = text.as_bytes();
    let (mut depth, mut deepest) = (0usize, 0usize);
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'"' => {
                i += 1;
                while i < s.len() && s[i] != b'"' {
                    i += if s[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'[' | b'{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    deepest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_depth_bounds_every_way_to_nest() {
        // The same table as ChipLeap's test_toml_depth.
        let cases: &[(&str, usize)] = &[
            ("", 0),
            ("a = 1\n", 1),
            ("a = 1.5\n", 2),
            ("a.b.c = 1\n", 3),
            ("a = [[1], [2]]\n", 3),
            ("a = { b = { c = 1 } }\n", 3),
            ("[a.b]\nc = 1\n", 3),
            ("[[a]]\nb = 1\n", 3),
            ("[[a.b]]\nc.d = 1\n", 5),
            ("a = \"[[[[\"\n", 1),
            ("a = '''\n[[[[\n'''\n", 1),
            ("a = \"\"\"x\"\"\"\" , [[1]]\n", 3),
            ("a = \"\\\"[[\" # [[[[\n", 1),
            ("  [a]\n", 1),
            ("a = [\n[1]]\n", 3),
            ("a.b.c = [[1, {x = [2]}]]\n", 7),
            ("[a.b.c]\nd.e = { f = [[1]] }\n", 8),
        ];
        for (text, want) in cases {
            assert_eq!(toml_depth(text), *want, "{text:?}");
        }
    }

    #[test]
    fn json_depth_skips_strings() {
        assert_eq!(json_depth(r#"{"a": [1, {"b": "[[[["}]}"#), 3);
        assert_eq!(json_depth(r#"["\"]]]", [[]]]"#), 3);
        assert_eq!(json_depth("]]][["), 2);
        assert_eq!(json_depth("\"abc\\"), 0);
    }
}
