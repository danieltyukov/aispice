//! LEF, far enough to compare a macro's class, size, origin and pins.
//! ChipLeap's `chipleap.macro.views.parse_lef` reads it the same way.

use super::is_ws;
use super::limits::{LimitError, Limits};
use super::text::quote;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum LefError {
    #[error("{0}")]
    Syntax(String),
    #[error(transparent)]
    Limit(#[from] LimitError),
}

fn syntax<T>(msg: impl Into<String>) -> Result<T, LefError> {
    Err(LefError::Syntax(msg.into()))
}

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

fn tokens(text: &str, limits: &Limits) -> Result<Vec<String>, LimitError> {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < n {
        let c = s[i];
        if is_ws(c) {
            i += 1;
            continue;
        }
        if c == '#' {
            while i < n && s[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if toks.len() >= limits.tokens {
            return Err(too_many_tokens("LEF", limits));
        }
        if c == '"' {
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
    Ok(toks)
}

pub(crate) fn too_many_tokens(what: &str, limits: &Limits) -> LimitError {
    LimitError(format!("the {what} has more than {} tokens", limits.tokens))
}

struct Tokens<'a> {
    toks: &'a [String],
    i: usize,
    what: String,
}

impl Tokens<'_> {
    fn next(&mut self) -> Result<&str, LefError> {
        let Some(t) = self.toks.get(self.i) else {
            return syntax(format!("unexpected end of file in {}", self.what));
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
    match s.parse() {
        Ok(v) if is_number(s) => Ok(v),
        _ => syntax(format!("expected a number, found {s:?}")),
    }
}

/// Every MACRO in a LEF file, by name, with the default limits.
pub fn parse_lef(text: &str) -> Result<BTreeMap<String, LefMacro>, LefError> {
    parse_lef_with(text, &Limits::DEFAULT)
}

/// Every MACRO in a LEF file, by name. Fails when it is malformed, has too
/// many tokens or a macro has too many pins.
pub fn parse_lef_with(text: &str, limits: &Limits) -> Result<BTreeMap<String, LefMacro>, LefError> {
    let toks = tokens(text, limits)?;
    let mut macros = BTreeMap::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == "MACRO" && i + 1 < toks.len() {
            let mut t = Tokens {
                toks: &toks,
                i: i + 2,
                what: format!("MACRO {}", toks[i + 1]),
            };
            let m = parse_macro(&mut t, &toks[i + 1], limits)?;
            macros.entry(m.name.clone()).or_insert(m);
            i = t.i;
        } else {
            i += 1;
        }
    }
    Ok(macros)
}

fn parse_macro(t: &mut Tokens, name: &str, limits: &Limits) -> Result<LefMacro, LefError> {
    let mut m = LefMacro {
        name: name.to_string(),
        ..LefMacro::default()
    };
    let mut pins = 0usize;
    loop {
        let kw = t.next()?.to_string();
        match kw.as_str() {
            "END" => {
                if t.next()? != name {
                    return syntax(format!("MACRO {name} ends with the wrong name"));
                }
                return Ok(m);
            }
            "CLASS" => m.class = t.statement()?.join(" "),
            "ORIGIN" => {
                let args = t.statement()?;
                let [x, y, ..] = &args[..] else {
                    return syntax(format!("ORIGIN of {name} needs two numbers"));
                };
                m.origin = (number(x)?, number(y)?);
            }
            "SIZE" => {
                let args = t.statement()?;
                let [w, by, h] = &args[..] else {
                    return syntax(format!("SIZE of {name} is not 'SIZE w BY h'"));
                };
                if by != "BY" {
                    return syntax(format!("SIZE of {name} is not 'SIZE w BY h'"));
                }
                m.size = Some((number(w)?, number(h)?));
            }
            "PIN" => {
                pins += 1;
                if pins > limits.pins {
                    return Err(LimitError(format!(
                        "MACRO {} has more than {} pins",
                        quote(name),
                        limits.pins
                    ))
                    .into());
                }
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
                    return syntax(format!("PIN {name} ends with the wrong name"));
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
        assert!(e.to_string().contains("end of file"), "{e}");
        let e = parse_lef("MACRO x\nEND y\n").unwrap_err();
        assert!(e.to_string().contains("wrong name"), "{e}");
        assert!(parse_lef("MACRO x\nSIZE 1_0 BY 2 ;\nEND x\n").is_err());
    }
}
