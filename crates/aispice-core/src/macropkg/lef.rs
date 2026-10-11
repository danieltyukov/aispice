//! LEF, far enough to compare a macro's class, size, origin and pins.
//! ChipLeap's `chipleap.macro.views.parse_lef` reads it the same way.

use super::is_ws;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LefError(pub String);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LefPin {
    pub name: String,
    /// INPUT, OUTPUT, INOUT, FEEDTHRU; empty when not given.
    pub direction: String,
    /// SIGNAL, CLOCK, ANALOG, POWER, GROUND, ...; empty when not given.
    pub use_: String,
    pub layers: Vec<String>,
    pub shapes: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LefMacro {
    pub name: String,
    pub class: String,
    pub origin: (f64, f64),
    pub size: Option<(f64, f64)>,
    pub pins: BTreeMap<String, LefPin>,
}

fn tokens(text: &str) -> Vec<String> {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < n {
        let c = s[i];
        if is_ws(c) {
            i += 1;
        } else if c == '#' {
            while i < n && s[i] != '\n' {
                i += 1;
            }
        } else if c == '"' {
            let j = (i + 1..n).find(|&j| s[j] == '"').unwrap_or(n);
            toks.push(s[i + 1..j].iter().collect());
            i = j + 1;
        } else if c == ';' {
            toks.push(";".into());
            i += 1;
        } else {
            let mut j = i;
            while j < n && !is_ws(s[j]) && s[j] != ';' {
                j += 1;
            }
            toks.push(s[i..j].iter().collect());
            i = j;
        }
    }
    toks
}

struct Tokens<'a> {
    toks: &'a [String],
    i: usize,
    what: String,
}

impl Tokens<'_> {
    fn next(&mut self) -> Result<&str, LefError> {
        let Some(t) = self.toks.get(self.i) else {
            return Err(LefError(format!("unexpected end of file in {}", self.what)));
        };
        self.i += 1;
        Ok(t)
    }

    /// The rest of a statement up to and including its semicolon.
    fn statement(&mut self) -> Result<Vec<String>, LefError> {
        let mut out = Vec::new();
        loop {
            let t = self.next()?;
            if t == ";" {
                return Ok(out);
            }
            out.push(t.to_string());
        }
    }
}

/// A plain decimal number, as ChipLeap's reader accepts it.
fn is_number(s: &str) -> bool {
    let b = s.strip_prefix(['+', '-']).unwrap_or(s).as_bytes();
    let mut i = 0;
    let int = b.iter().take_while(|c| c.is_ascii_digit()).count();
    i += int;
    let mut frac = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        frac = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        i += frac;
    }
    if int + frac == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if exp == 0 {
            return false;
        }
        i += exp;
    }
    i == b.len()
}

fn number(s: &str) -> Result<f64, LefError> {
    if !is_number(s) {
        return Err(LefError(format!("expected a number, found {s:?}")));
    }
    s.parse()
        .map_err(|_| LefError(format!("expected a number, found {s:?}")))
}

/// Every MACRO in a LEF file, by name.
pub fn parse_lef(text: &str) -> Result<BTreeMap<String, LefMacro>, LefError> {
    let toks = tokens(text);
    let mut macros = BTreeMap::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == "MACRO" && i + 1 < toks.len() {
            let mut t = Tokens {
                toks: &toks,
                i: i + 2,
                what: format!("MACRO {}", toks[i + 1]),
            };
            let m = parse_macro(&mut t, &toks[i + 1])?;
            macros.entry(m.name.clone()).or_insert(m);
            i = t.i;
        } else {
            i += 1;
        }
    }
    Ok(macros)
}

fn parse_macro(t: &mut Tokens, name: &str) -> Result<LefMacro, LefError> {
    let mut m = LefMacro {
        name: name.to_string(),
        ..LefMacro::default()
    };
    loop {
        let kw = t.next()?.to_string();
        match kw.as_str() {
            "END" => {
                if t.next()? != name {
                    return Err(LefError(format!("MACRO {name} ends with the wrong name")));
                }
                return Ok(m);
            }
            "CLASS" => m.class = t.statement()?.join(" "),
            "ORIGIN" => {
                let args = t.statement()?;
                if args.len() < 2 {
                    return Err(LefError(format!("ORIGIN of {name} needs two numbers")));
                }
                m.origin = (number(&args[0])?, number(&args[1])?);
            }
            "SIZE" => {
                let args = t.statement()?;
                if args.len() != 3 || args[1] != "BY" {
                    return Err(LefError(format!("SIZE of {name} is not 'SIZE w BY h'")));
                }
                m.size = Some((number(&args[0])?, number(&args[2])?));
            }
            "PIN" => {
                let pin_name = t.next()?.to_string();
                let pin = parse_pin(t, &pin_name)?;
                m.pins.entry(pin.name.clone()).or_insert(pin);
            }
            "OBS" | "DENSITY" => {
                while t.next()? != "END" {
                    t.statement()?;
                }
            }
            _ => {
                t.statement()?;
            }
        }
    }
}

fn parse_pin(t: &mut Tokens, name: &str) -> Result<LefPin, LefError> {
    let mut p = LefPin {
        name: name.to_string(),
        ..LefPin::default()
    };
    loop {
        let kw = t.next()?.to_string();
        match kw.as_str() {
            "END" => {
                if t.next()? != name {
                    return Err(LefError(format!("PIN {name} ends with the wrong name")));
                }
                return Ok(p);
            }
            // OUTPUT TRISTATE is an output.
            "DIRECTION" => p.direction = t.statement()?.into_iter().next().unwrap_or_default(),
            "USE" => p.use_ = t.statement()?.into_iter().next().unwrap_or_default(),
            "PORT" => loop {
                let kw = t.next()?.to_string();
                if kw == "END" {
                    break;
                }
                let args = t.statement()?;
                if kw == "LAYER" {
                    if let Some(layer) = args.into_iter().next() {
                        p.layers.push(layer);
                    }
                } else if matches!(kw.as_str(), "RECT" | "POLYGON" | "VIA") {
                    p.shapes += 1;
                }
            },
            _ => {
                t.statement()?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_chipleap_reads() {
        // The same input as ChipLeap's test_lef_reader.
        let text = r#"
    VERSION 5.8 ;
    # a comment ; with a semicolon
    MACRO other
      SIZE 1 BY 1 ;
    END other
    MACRO blk
      CLASS BLOCK ;
      ORIGIN 1.5 -2 ;
      SIZE 10.25 BY 4 ;
      PROPERTY foo "a ; b" ;
      PIN d[0]
        DIRECTION OUTPUT TRISTATE ;
        PORT
          LAYER Metal3 ;
            RECT 0 0 1 1 ;
          LAYER Metal4 ;
            POLYGON 0 0 1 0 1 1 ;
        END
      END d[0]
      PIN VPWR
        USE POWER ;
      END VPWR
      OBS
        LAYER Metal1 ;
          RECT 0 0 10 4 ;
      END
      DENSITY
        LAYER Metal1 ;
          RECT 0 0 1 1 50 ;
      END
    END blk
    END LIBRARY
    "#;
        let macros = parse_lef(text).unwrap();
        let blk = &macros["blk"];
        assert_eq!(
            (blk.class.as_str(), blk.origin, blk.size),
            ("BLOCK", (1.5, -2.0), Some((10.25, 4.0)))
        );
        let d0 = &blk.pins["d[0]"];
        assert_eq!(
            (d0.direction.as_str(), d0.use_.as_str(), d0.shapes),
            ("OUTPUT", "", 2)
        );
        assert_eq!(d0.layers, ["Metal3", "Metal4"]);
        assert_eq!(blk.pins["VPWR"].use_, "POWER");
        assert_eq!(blk.pins["VPWR"].shapes, 0);
        let e = parse_lef("MACRO x\n PIN a\n DIRECTION INPUT ;\n").unwrap_err();
        assert!(e.0.contains("end of file"), "{e}");
        let e = parse_lef("MACRO x\nEND y\n").unwrap_err();
        assert!(e.0.contains("wrong name"), "{e}");
        assert!(parse_lef("MACRO x\nSIZE 1_0 BY 2 ;\nEND x\n").is_err());
    }
}
