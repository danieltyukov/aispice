//! The xschem file format, for schematics (`.sch`) and symbols (`.sym`).
//!
//! A file is a list of records, each starting with a tag character:
//!
//! | Tag | Record |
//! |---|---|
//! | `v` | `v {xschem version=3.4.4 file_version=1.2}` |
//! | `G` `K` `V` `S` `E` | global property strings (VHDL, symbol, Verilog, SPICE, tEDAx) |
//! | `N` | wire: `N x1 y1 x2 y2 {props}` |
//! | `C` | instance: `C {symbol} x y rot flip {props}`, optionally followed by an embedded symbol in `[ ... ]` |
//! | `T` | text: `T {text} x y rot flip xscale yscale {props}` |
//! | `L` `B` | line and box: `L layer x1 y1 x2 y2 {props}`; a box on layer 5 is a symbol pin |
//! | `P` | polygon: `P layer n x1 y1 ... xn yn {props}` |
//! | `A` | arc: `A layer x y r start sweep {props}` |
//! | `F` `#` | skipped |
//!
//! A `{...}` string ends at the first `}` not escaped by a backslash; the
//! backslash itself is dropped. Coordinates are floats, y grows downward.
//! The reader is bounded: file size, string size, record count and nesting
//! are capped, and every loop advances or stops at the end of the input.

use std::fmt::Write as _;

/// Files larger than this are refused.
pub const MAX_FILE_BYTES: usize = 4 << 20;
/// A single `{...}` string larger than this is refused.
pub const MAX_STRING_BYTES: usize = 1 << 20;
/// More records than this in one file is refused.
pub const MAX_RECORDS: usize = 20_000;
/// Embedded symbols nest at most this deep.
const MAX_EMBED_DEPTH: usize = 4;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct XFile {
    /// The `v` record's text: `xschem version=3.4.4 file_version=1.2`.
    pub version: String,
    /// `G`: VHDL properties, and a symbol's own properties in old files.
    pub vhdl: String,
    /// `K`: a symbol's own properties (`type`, `format`, `template`).
    pub symbol: String,
    /// `V`: Verilog properties.
    pub verilog: String,
    /// `S`: SPICE text xschem appends to the netlist.
    pub spice: String,
    /// `E`: tEDAx properties.
    pub tedax: String,
    pub wires: Vec<XWire>,
    pub instances: Vec<XInstance>,
    pub texts: Vec<XText>,
    pub shapes: Vec<XShape>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XWire {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub props: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XInstance {
    /// The symbol reference as written: `devices/res.sym`, `sg13g2_pr/rppd.sym`.
    pub symbol: String,
    pub x: f64,
    pub y: f64,
    /// Quarter turns clockwise on the sheet, 0 to 3.
    pub rot: u8,
    /// Mirrored left to right, applied before the rotation.
    pub flip: bool,
    pub props: String,
    /// A symbol definition stored in the schematic itself.
    pub embedded: Option<Box<XFile>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XText {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub rot: u8,
    pub flip: bool,
    pub xscale: f64,
    pub yscale: f64,
    pub props: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    Line,
    Box,
    Polygon,
    Arc,
}

impl ShapeKind {
    fn tag(self) -> char {
        match self {
            ShapeKind::Line => 'L',
            ShapeKind::Box => 'B',
            ShapeKind::Polygon => 'P',
            ShapeKind::Arc => 'A',
        }
    }
}

/// Graphics: lines and boxes as `x1 y1 x2 y2`, polygons as their points,
/// arcs as `x y r start sweep`.
#[derive(Debug, Clone, PartialEq)]
pub struct XShape {
    pub kind: ShapeKind,
    pub layer: i32,
    pub coords: Vec<f64>,
    pub props: String,
}

/// The layer xschem keeps symbol pins on.
pub const PIN_LAYER: i32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("the file is {0} bytes; xschem files over {MAX_FILE_BYTES} bytes are refused")]
    TooLarge(usize),
    #[error("line {line}: {message}")]
    Malformed { line: usize, message: String },
}

impl XFile {
    /// A symbol's own properties: the `K` record, or the `G` record in files
    /// written before `K` existed.
    pub fn symbol_props(&self) -> &str {
        if !self.symbol.trim().is_empty() {
            &self.symbol
        } else if super::props::get(&self.vhdl, "type").is_some() {
            &self.vhdl
        } else {
            ""
        }
    }
}

struct Reader<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
    records: usize,
}

impl Reader<'_> {
    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError::Malformed {
            line: self.line,
            message: message.into(),
        }
    }

    fn next(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_space(&mut self) {
        while self.chars.peek().is_some_and(|c| c.is_whitespace()) {
            self.next();
        }
    }

    /// Discard the rest of the line, including its newline.
    fn skip_line(&mut self) {
        while let Some(c) = self.next() {
            if c == '\n' {
                break;
            }
        }
    }

    /// Discard the rest of the record's line, reading over `{...}` strings
    /// that span lines, as xschem does for unknown records.
    fn skip_record(&mut self) -> Result<(), ParseError> {
        loop {
            match self.chars.peek() {
                None => return Ok(()),
                Some('\n') => {
                    self.next();
                    return Ok(());
                }
                Some('{') => {
                    self.string()?;
                }
                Some(_) => {
                    self.next();
                }
            }
        }
    }

    /// A `{...}` string, unescaped. Carriage returns are dropped.
    fn string(&mut self) -> Result<String, ParseError> {
        self.skip_space();
        match self.next() {
            Some('{') => {}
            Some(c) => return Err(self.err(format!("expected `{{`, found `{c}`"))),
            None => return Err(self.err("expected `{`, found the end of the file")),
        }
        let mut out = String::new();
        let mut escape = false;
        loop {
            let Some(c) = self.next() else {
                return Err(self.err("a `{` string is not closed before the end of the file"));
            };
            if c == '\r' {
                continue;
            }
            if !escape {
                if c == '}' {
                    return Ok(out);
                }
                if c == '\\' {
                    escape = true;
                    continue;
                }
            }
            escape = false;
            out.push(c);
            if out.len() > MAX_STRING_BYTES {
                return Err(self.err(format!(
                    "a `{{` string is longer than {MAX_STRING_BYTES} bytes"
                )));
            }
        }
    }

    /// A number token. Like xschem's `fscanf`, any white space may come
    /// before it.
    fn number(&mut self) -> Result<f64, ParseError> {
        self.skip_space();
        let mut tok = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() || c == '{' || tok.len() > 64 {
                break;
            }
            tok.push(c);
            self.next();
        }
        tok.parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| self.err(format!("expected a number, found `{tok}`")))
    }

    fn numbers<const N: usize>(&mut self) -> Result<[f64; N], ParseError> {
        let mut out = [0.0; N];
        for v in out.iter_mut() {
            *v = self.number()?;
        }
        Ok(out)
    }

    fn quarter(&self, v: f64) -> Result<u8, ParseError> {
        if v.fract() == 0.0 && (0.0..4.0).contains(&v) {
            Ok(v as u8)
        } else {
            Err(self.err(format!("rotation must be 0 to 3, found {v}")))
        }
    }

    fn flag(&self, v: f64) -> Result<bool, ParseError> {
        match v {
            0.0 => Ok(false),
            1.0 => Ok(true),
            _ => Err(self.err(format!("flip must be 0 or 1, found {v}"))),
        }
    }

    fn layer(&mut self) -> Result<i32, ParseError> {
        let v = self.number()?;
        if v.fract() == 0.0 && (0.0..=1000.0).contains(&v) {
            Ok(v as i32)
        } else {
            Err(self.err(format!("bad layer {v}")))
        }
    }

    /// Records until the end of the input, or until a `]` line when reading
    /// an embedded symbol.
    fn file(&mut self, depth: usize) -> Result<XFile, ParseError> {
        let mut f = XFile::default();
        loop {
            self.skip_space();
            let Some(tag) = self.next() else {
                if depth > 0 {
                    return Err(self.err("an embedded symbol is not closed with `]`"));
                }
                return Ok(f);
            };
            self.records += 1;
            if self.records > MAX_RECORDS {
                return Err(self.err(format!("more than {MAX_RECORDS} records")));
            }
            match tag {
                ']' if depth > 0 => {
                    self.skip_line();
                    return Ok(f);
                }
                'v' => f.version = self.string()?,
                'G' => f.vhdl = self.string()?,
                'K' => f.symbol = self.string()?,
                'V' => f.verilog = self.string()?,
                'S' => f.spice = self.string()?,
                'E' => f.tedax = self.string()?,
                'F' | '#' => {
                    self.skip_line();
                    continue;
                }
                'N' => {
                    let [x1, y1, x2, y2] = self.numbers()?;
                    let props = self.string()?;
                    f.wires.push(XWire {
                        x1,
                        y1,
                        x2,
                        y2,
                        props,
                    });
                }
                'C' => {
                    let symbol = self.string()?;
                    let [x, y, rot, flip] = self.numbers()?;
                    let props = self.string()?;
                    f.instances.push(XInstance {
                        symbol,
                        x,
                        y,
                        rot: self.quarter(rot)?,
                        flip: self.flag(flip)?,
                        props,
                        embedded: None,
                    });
                }
                '[' => {
                    if depth >= MAX_EMBED_DEPTH {
                        return Err(self.err("embedded symbols nest too deep"));
                    }
                    self.skip_line();
                    let inner = self.file(depth + 1)?;
                    match f.instances.last_mut() {
                        Some(inst) if inst.embedded.is_none() => {
                            inst.embedded = Some(Box::new(inner));
                        }
                        _ => return Err(self.err("`[` must follow a `C` record")),
                    }
                    continue;
                }
                'T' => {
                    let text = self.string()?;
                    let [x, y, rot, flip, xscale, yscale] = self.numbers()?;
                    let props = self.string()?;
                    f.texts.push(XText {
                        text,
                        x,
                        y,
                        rot: self.quarter(rot)?,
                        flip: self.flag(flip)?,
                        xscale,
                        yscale,
                        props,
                    });
                }
                'L' | 'B' => {
                    let layer = self.layer()?;
                    let coords = self.numbers::<4>()?.to_vec();
                    let props = self.string()?;
                    f.shapes.push(XShape {
                        kind: if tag == 'L' {
                            ShapeKind::Line
                        } else {
                            ShapeKind::Box
                        },
                        layer,
                        coords,
                        props,
                    });
                }
                'A' => {
                    let layer = self.layer()?;
                    let coords = self.numbers::<5>()?.to_vec();
                    let props = self.string()?;
                    f.shapes.push(XShape {
                        kind: ShapeKind::Arc,
                        layer,
                        coords,
                        props,
                    });
                }
                'P' => {
                    let layer = self.layer()?;
                    let n = self.number()?;
                    if n.fract() != 0.0 || !(0.0..=10_000.0).contains(&n) {
                        return Err(self.err(format!("bad polygon point count {n}")));
                    }
                    let mut coords = Vec::with_capacity(2 * n as usize);
                    for _ in 0..2 * n as usize {
                        coords.push(self.number()?);
                    }
                    let props = self.string()?;
                    f.shapes.push(XShape {
                        kind: ShapeKind::Polygon,
                        layer,
                        coords,
                        props,
                    });
                }
                _ => {
                    // Unknown records and stray strings are skipped, as
                    // xschem does.
                    self.skip_record()?;
                    continue;
                }
            }
            // Anything after the last field on the record's line is ignored.
            self.skip_record()?;
        }
    }
}

/// Read a schematic or symbol.
pub fn parse(text: &str) -> Result<XFile, ParseError> {
    if text.len() > MAX_FILE_BYTES {
        return Err(ParseError::TooLarge(text.len()));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut r = Reader {
        chars: text.chars().peekable(),
        line: 1,
        records: 0,
    };
    r.file(0)
}

/// A `{...}` string with `\`, `{` and `}` escaped.
pub fn braced(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('{');
    for c in s.chars() {
        if matches!(c, '\\' | '{' | '}') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('}');
    out
}

/// A coordinate as xschem writes it: whole numbers without a fraction.
pub fn num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Write a file. Records come out grouped the way xschem saves them.
pub fn write(f: &XFile) -> String {
    let mut out = String::new();
    write_into(f, &mut out);
    out
}

fn write_into(f: &XFile, out: &mut String) {
    let _ = writeln!(out, "v {}", braced(&f.version));
    for (tag, s) in [
        ('G', &f.vhdl),
        ('K', &f.symbol),
        ('V', &f.verilog),
        ('S', &f.spice),
        ('E', &f.tedax),
    ] {
        let _ = writeln!(out, "{tag} {}", braced(s));
    }
    for s in &f.shapes {
        let _ = write!(out, "{} {}", s.kind.tag(), s.layer);
        if s.kind == ShapeKind::Polygon {
            let _ = write!(out, " {}", s.coords.len() / 2);
        }
        for c in &s.coords {
            let _ = write!(out, " {}", num(*c));
        }
        let _ = writeln!(out, " {}", braced(&s.props));
    }
    for t in &f.texts {
        let _ = writeln!(
            out,
            "T {} {} {} {} {} {} {} {}",
            braced(&t.text),
            num(t.x),
            num(t.y),
            t.rot,
            t.flip as u8,
            num(t.xscale),
            num(t.yscale),
            braced(&t.props)
        );
    }
    for w in &f.wires {
        let _ = writeln!(
            out,
            "N {} {} {} {} {}",
            num(w.x1),
            num(w.y1),
            num(w.x2),
            num(w.y2),
            braced(&w.props)
        );
    }
    for i in &f.instances {
        let _ = writeln!(
            out,
            "C {} {} {} {} {} {}",
            braced(&i.symbol),
            num(i.x),
            num(i.y),
            i.rot,
            i.flip as u8,
            braced(&i.props)
        );
        if let Some(e) = &i.embedded {
            out.push_str("[\n");
            write_into(e, out);
            out.push_str("]\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"v {xschem version=3.4.7 file_version=1.2}
G {}
K {}
V {}
S {}
E {}
B 2 510 -650 1070 -350 {flags=graph
y1=-4.6e-12
node=i(vd)}
T {Ctrl-Click to execute launcher} 650 -330 0 0 0.3 0.3 {layer=11}
N 250 -160 250 -140 {
lab=GND}
N 380 -340 410 -340 {
lab=#net1}
C {devices/code_shown.sym} 0 -100 0 0 {name=MODEL only_toplevel=true
format="tcleval( @value )"
value=".lib cornerMOSlv.lib mos_tt
"}
C {devices/vsource.sym} 250 -190 0 0 {name=Vgs value=1.2}
C {sg13g2_pr/sg13_lv_nmos.sym} 360 -250 0 1 {name=M1
l=0.45u
w=1.0u
spiceprefix=X
}
C {launcher.sym} 710 -290 0 0 {name=h3
tclcommand="
set sim(spice,1,cmd) \{ngspice  \\"$N\\" -a\}
"}
"#;

    #[test]
    fn reads_a_test_bench() {
        let f = parse(SAMPLE).unwrap();
        assert_eq!(f.version, "xschem version=3.4.7 file_version=1.2");
        assert_eq!(f.wires.len(), 2);
        assert_eq!(f.wires[0].props, "\nlab=GND");
        assert_eq!(f.instances.len(), 4);
        let m = &f.instances[2];
        assert_eq!(m.symbol, "sg13g2_pr/sg13_lv_nmos.sym");
        assert_eq!((m.x, m.y, m.rot, m.flip), (360.0, -250.0, 0, true));
        let code = &f.instances[0];
        assert_eq!(
            crate::xschem::props::get(&code.props, "value").as_deref(),
            Some(".lib cornerMOSlv.lib mos_tt\n")
        );
        // The file layer removes one level of escapes.
        let launcher = &f.instances[3];
        assert!(
            launcher
                .props
                .contains(r#"set sim(spice,1,cmd) {ngspice  \"$N\" -a}"#),
            "{}",
            launcher.props
        );
        assert_eq!(f.texts.len(), 1);
        assert_eq!(f.shapes.len(), 1);
        assert_eq!(f.shapes[0].kind, ShapeKind::Box);
    }

    #[test]
    fn writes_what_it_reads() {
        let f = parse(SAMPLE).unwrap();
        let again = parse(&write(&f)).unwrap();
        assert_eq!(f, again);
    }

    #[test]
    fn reads_symbols_and_embedded_symbols() {
        let sym = "v {xschem version=3.4.4 file_version=1.2}\nG {type=netlist_commands\nformat=\"\n@value\n\"}\nV {}\nS {}\nE {}\nB 5 -2.5 -32.5 2.5 -27.5 {name=P dir=inout}\nP 4 3 0 0 10 10 0 10 {fill=true}\nA 4 0 0 5 0 360 {}\nL 4 0 -30 0 30 {}\n";
        let f = parse(sym).unwrap();
        assert!(f.symbol_props().starts_with("type=netlist_commands"));
        assert_eq!(f.shapes.len(), 4);
        assert_eq!(f.shapes[1].coords.len(), 6);
        let sch = format!(
            "v {{xschem version=3.4.4 file_version=1.2}}\nC {{mine.sym}} 0 0 0 0 {{name=x1 embed=true}}\n[\n{sym}]\nC {{devices/gnd.sym}} 0 30 0 0 {{name=l1 lab=GND}}\n"
        );
        let f = parse(&sch).unwrap();
        assert_eq!(f.instances.len(), 2);
        let e = f.instances[0].embedded.as_ref().unwrap();
        assert_eq!(e.shapes.len(), 4);
        assert!(f.instances[1].embedded.is_none());
        assert_eq!(parse(&write(&f)).unwrap(), f);
    }

    #[test]
    fn malformed_input_is_an_error_not_a_hang() {
        for bad in [
            "C {res.sym 0 0 0 0 {}",
            "N 0 0 x 1 {}",
            "C {res.sym} 0 0 7 0 {}",
            "C {res.sym} 0 0 0 2 {}",
            "P 4 99999999 0 0 {}",
            "[\nK {}\n",
            "v {x}\nC {a.sym} 0 0 0 0 {}\n[\nC {b.sym} 0 0 0 0 {}\n[\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(matches!(
            parse(&"#".repeat(MAX_FILE_BYTES + 1)),
            Err(ParseError::TooLarge(_))
        ));
        let many = "F x\n".repeat(MAX_RECORDS + 1);
        assert!(parse(&many).is_err());
        // Unknown records are skipped, as xschem does.
        let f = parse("Z {a\nb} 1 2\nN 0 0 10 0 {}\n").unwrap();
        assert_eq!(f.wires.len(), 1);
    }

    #[test]
    fn braced_strings_escape() {
        assert_eq!(braced("a{b}c\\d"), "{a\\{b\\}c\\\\d}");
        let f = XFile {
            spice: "x {y} \\z".into(),
            ..XFile::default()
        };
        assert_eq!(parse(&write(&f)).unwrap().spice, "x {y} \\z");
        assert_eq!(num(-30.0), "-30");
        assert_eq!(num(19.921875), "19.921875");
    }

    #[test]
    fn hostile_nesting_is_bounded() {
        // `{` strings do not nest: a hundred thousand opening braces are
        // text, and a missing close is an error at the end of the file.
        let deep = format!("S {{{}}}\n", "\\{".repeat(100_000));
        assert_eq!(parse(&deep).unwrap().spice.len(), 100_000);
        assert!(parse(&format!("S {}", "{".repeat(100_000))).is_err());
        // Embedded symbols are capped at a few levels.
        let embeds = "C {a.sym} 0 0 0 0 {}\n[\n".repeat(10_000);
        assert!(parse(&embeds).is_err());
        let string = format!("S {{{}}}", "x".repeat(MAX_STRING_BYTES + 1));
        assert!(parse(&string).is_err());
    }

    /// Random bytes, and real files cut short or with characters changed,
    /// give a file or an error, never a panic; whatever reads also writes
    /// and reads back.
    #[test]
    fn random_and_truncated_files_never_panic() {
        let sym = "v {xschem version=3.4.4 file_version=1.2}\nK {type=nmos\nformat=\"@name @pinlist\"}\nB 5 -2.5 -32.5 2.5 -27.5 {name=D dir=inout}\nP 4 3 0 0 10 10 0 10 {fill=true}\nA 4 0 0 5 0 360 {}\nT {@name} 5 5 0 0 0.2 0.2 {}\n";
        let embedded = format!("C {{x.sym}} 0 0 1 1 {{name=x1}}\n[\n{sym}]\n");
        let corpus = [SAMPLE, sym, embedded.as_str()];
        let alphabet: Vec<char> = "{}[]\\\" \n\r\t0123456789.-eCNTLBPAKSGVEFv#x=\u{e9}\u{1f600}"
            .chars()
            .collect();
        let mut rng = crate::xschem::test_rng(0x5c4e_f11e);
        for case in 0..3000 {
            let text: String = match case % 3 {
                0 => {
                    let base = corpus[case % corpus.len()];
                    let cut = rng.below(base.len() + 1);
                    let cut = (0..=cut)
                        .rev()
                        .find(|i| base.is_char_boundary(*i))
                        .unwrap_or(0);
                    base[..cut].to_string()
                }
                1 => {
                    let mut chars: Vec<char> = corpus[case % corpus.len()].chars().collect();
                    for _ in 0..1 + rng.below(6) {
                        let at = rng.below(chars.len());
                        chars[at] = alphabet[rng.below(alphabet.len())];
                    }
                    chars.into_iter().collect()
                }
                _ => (0..rng.below(300))
                    .map(|_| alphabet[rng.below(alphabet.len())])
                    .collect(),
            };
            if let Ok(f) = parse(&text) {
                assert_eq!(parse(&write(&f)).as_ref(), Ok(&f), "{text:?}");
            }
        }
    }
}
