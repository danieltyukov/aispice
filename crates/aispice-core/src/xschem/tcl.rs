//! A small Tcl subset for xschem's `tcleval(...)` strings, with no side
//! effects.
//!
//! xschem hands `tcleval(...)` text to Tcl's `subst`. Symbols use that for
//! arithmetic (IHP's `ptap1`: `R=[ev {1.0 / (...)}]`), for a choice
//! (xschem's `vsource`: `[expr {@savecurrent ? "..." : "..."}]`), for the
//! simulator in use (`[if {[sim_is_ngspice]} {return {...}}]`) and for model
//! paths (`$::MODELS_NGSPICE`). This module evaluates exactly that: backslash
//! sequences, variables from a fixed table, braces and quotes, and the
//! commands `expr`, `ev`, `if`, `return` and `sim_is_<simulator>`. Any other
//! command is an error, never a guess, so a file cannot make aispice do
//! anything but compute a string.

use std::collections::BTreeMap;

/// Nesting of command substitutions and expressions.
const MAX_DEPTH: usize = 32;
/// Characters of output, and of any intermediate string.
const MAX_OUTPUT: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TclError {
    #[error(
        "Tcl command `{0}` is not one aispice evaluates (it evaluates only expr, ev, if, return and sim_is_*)"
    )]
    Unsupported(String),
    #[error("Tcl variable `{0}` is not one aispice knows")]
    UnknownVariable(String),
    #[error("Tcl syntax: {0}")]
    Syntax(String),
    #[error("Tcl expression: {0}")]
    Expr(String),
    #[error("Tcl evaluation is nested or grows too far")]
    Limit,
}

/// What evaluation may read: variables (by name, without `::`) and the
/// simulator `sim_is_<name>` answers 1 for.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub vars: BTreeMap<String, String>,
    pub simulator: String,
}

impl Env {
    pub fn new(simulator: &str) -> Self {
        Self {
            vars: BTreeMap::new(),
            simulator: simulator.to_string(),
        }
    }

    pub fn with_var(mut self, name: &str, value: &str) -> Self {
        self.vars.insert(name.to_string(), value.to_string());
        self
    }
}

/// What xschem's `tclpropeval2` does with a property or netlist line that
/// starts with `tcleval(`: drop the wrapper and `subst` the rest.
pub fn tcleval(text: &str, env: &Env) -> Result<String, TclError> {
    let s = text.replace("\\}", "}");
    let s = s.strip_prefix("tcleval(").unwrap_or(&s);
    // The last `)`, keeping the white space after it.
    let trimmed = s.trim_end_matches([' ', '\n', '\t']);
    let body = match trimmed.strip_suffix(')') {
        Some(b) => format!("{b}{}", &s[trimmed.len()..]),
        None => s.to_string(),
    };
    subst(&body, env)
}

/// Tcl's `subst`: backslash, variable and command substitution; braces are
/// ordinary characters.
pub fn subst(text: &str, env: &Env) -> Result<String, TclError> {
    let mut i = Interp { env, depth: 0 };
    let chars: Vec<char> = text.chars().collect();
    let mut pos = 0;
    let mut out = String::new();
    while pos < chars.len() {
        match chars[pos] {
            '\\' => {
                let (s, n) = backslash(&chars, pos);
                out.push_str(&s);
                pos += n;
            }
            '$' => {
                let (s, n) = i.variable(&chars, pos)?;
                out.push_str(&s);
                pos += n;
            }
            '[' => {
                let (s, n) = i.command_subst(&chars, pos)?;
                out.push_str(&s);
                pos += n;
            }
            c => {
                out.push(c);
                pos += 1;
            }
        }
        if out.len() > MAX_OUTPUT {
            return Err(TclError::Limit);
        }
    }
    Ok(out)
}

/// A backslash sequence at `pos`: its value and how many characters it uses.
fn backslash(chars: &[char], pos: usize) -> (String, usize) {
    let Some(&c) = chars.get(pos + 1) else {
        return ("\\".into(), 1);
    };
    let simple = |s: char| (s.to_string(), 2);
    match c {
        'n' => simple('\n'),
        't' => simple('\t'),
        'r' => simple('\r'),
        'a' => simple('\u{7}'),
        'b' => simple('\u{8}'),
        'f' => simple('\u{c}'),
        'v' => simple('\u{b}'),
        '\n' => {
            // Backslash, newline and the white space after it are one space.
            let mut n = 2;
            while chars.get(pos + n).is_some_and(|c| *c == ' ' || *c == '\t') {
                n += 1;
            }
            (" ".into(), n)
        }
        'x' | 'u' => {
            let max = if c == 'x' { 2 } else { 4 };
            let digits: String = chars[pos + 2..]
                .iter()
                .take(max)
                .take_while(|d| d.is_ascii_hexdigit())
                .collect();
            match u32::from_str_radix(&digits, 16)
                .ok()
                .and_then(char::from_u32)
            {
                Some(ch) if !digits.is_empty() => (ch.to_string(), 2 + digits.len()),
                _ => simple(c),
            }
        }
        '0'..='7' => {
            let digits: String = chars[pos + 1..]
                .iter()
                .take(3)
                .take_while(|d| ('0'..='7').contains(d))
                .collect();
            let v = u32::from_str_radix(&digits, 8).unwrap_or(0);
            (
                char::from_u32(v).unwrap_or('?').to_string(),
                1 + digits.len(),
            )
        }
        other => simple(other),
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The end of a brace group that opens at `pos`, or an error.
fn close_brace(chars: &[char], pos: usize) -> Result<usize, TclError> {
    let mut depth = 0usize;
    let mut k = pos;
    while k < chars.len() {
        match chars[k] {
            '\\' => k += 1,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(k);
                }
            }
            _ => {}
        }
        k += 1;
    }
    Err(TclError::Syntax("missing close brace".into()))
}

/// The end of a command substitution that opens at `pos`.
fn close_bracket(chars: &[char], pos: usize) -> Result<usize, TclError> {
    let mut depth = 0usize;
    let mut k = pos;
    while k < chars.len() {
        match chars[k] {
            '\\' => k += 1,
            '{' => {
                // Braces inside a command group their contents, brackets
                // and all.
                k = close_brace(chars, k)?;
            }
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(k);
                }
            }
            _ => {}
        }
        k += 1;
    }
    Err(TclError::Syntax("missing close bracket".into()))
}

/// How a script ended: normally, or by `return`.
enum Flow {
    Normal(String),
    Return(String),
}

impl Flow {
    fn value(self) -> String {
        match self {
            Flow::Normal(s) | Flow::Return(s) => s,
        }
    }
}

struct Interp<'a> {
    env: &'a Env,
    depth: usize,
}

impl Interp<'_> {
    fn enter(&mut self) -> Result<(), TclError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(TclError::Limit)
        } else {
            Ok(())
        }
    }

    /// `$name`, `$::name`, `${name}` or `$name(index)` at `pos`.
    fn variable(&mut self, chars: &[char], pos: usize) -> Result<(String, usize), TclError> {
        let mut k = pos + 1;
        let name: String;
        if chars.get(k) == Some(&'{') {
            let end = chars[k..]
                .iter()
                .position(|c| *c == '}')
                .ok_or_else(|| TclError::Syntax("missing close brace in ${...}".into()))?;
            name = chars[k + 1..k + end].iter().collect();
            k += end + 1;
        } else {
            let start = k;
            loop {
                if chars.get(k).copied().is_some_and(is_name_char) {
                    k += 1;
                } else if chars.get(k) == Some(&':') && chars.get(k + 1) == Some(&':') {
                    k += 2;
                } else {
                    break;
                }
            }
            if k == start {
                return Ok(("$".into(), 1));
            }
            name = chars[start..k].iter().collect();
            if chars.get(k) == Some(&'(') {
                return Err(TclError::UnknownVariable(format!("{name}(...)")));
            }
        }
        let key = name.trim_start_matches("::");
        match self.env.vars.get(key) {
            Some(v) => Ok((v.clone(), k - pos)),
            None => Err(TclError::UnknownVariable(name)),
        }
    }

    /// `[script]` at `pos`: its result and length.
    fn command_subst(&mut self, chars: &[char], pos: usize) -> Result<(String, usize), TclError> {
        let end = close_bracket(chars, pos)?;
        let script: String = chars[pos + 1..end].iter().collect();
        self.enter()?;
        let out = self.script(&script)?.value();
        self.depth -= 1;
        Ok((out, end + 1 - pos))
    }

    /// Run a script: commands separated by newlines or `;`.
    fn script(&mut self, script: &str) -> Result<Flow, TclError> {
        let chars: Vec<char> = script.chars().collect();
        let mut pos = 0;
        let mut last = String::new();
        loop {
            let (words, next) = self.words(&chars, pos)?;
            if !words.is_empty() {
                match self.command(&words)? {
                    Flow::Return(v) => return Ok(Flow::Return(v)),
                    Flow::Normal(v) => last = v,
                }
            }
            if next >= chars.len() {
                return Ok(Flow::Normal(last));
            }
            pos = next;
        }
    }

    /// The words of the command at `pos`, substituted, and where the next
    /// command starts.
    fn words(&mut self, chars: &[char], mut pos: usize) -> Result<(Vec<String>, usize), TclError> {
        let mut words = Vec::new();
        loop {
            while chars
                .get(pos)
                .is_some_and(|c| *c == ' ' || *c == '\t' || *c == '\r')
                || (chars.get(pos) == Some(&'\\') && chars.get(pos + 1) == Some(&'\n'))
            {
                pos += if chars[pos] == '\\' { 2 } else { 1 };
            }
            match chars.get(pos) {
                None => return Ok((words, chars.len())),
                Some('\n' | ';') => return Ok((words, pos + 1)),
                Some('#') if words.is_empty() => {
                    while chars.get(pos).is_some_and(|c| *c != '\n') {
                        pos += 1;
                    }
                    continue;
                }
                Some('{') => {
                    let end = close_brace(chars, pos)?;
                    words.push(chars[pos + 1..end].iter().collect());
                    pos = end + 1;
                }
                Some('"') => {
                    let mut k = pos + 1;
                    let mut word = String::new();
                    loop {
                        match chars.get(k) {
                            None => return Err(TclError::Syntax("missing close quote".into())),
                            Some('"') => break,
                            Some('\\') => {
                                let (s, n) = backslash(chars, k);
                                word.push_str(&s);
                                k += n;
                            }
                            Some('$') => {
                                let (s, n) = self.variable(chars, k)?;
                                word.push_str(&s);
                                k += n;
                            }
                            Some('[') => {
                                let (s, n) = self.command_subst(chars, k)?;
                                word.push_str(&s);
                                k += n;
                            }
                            Some(c) => {
                                word.push(*c);
                                k += 1;
                            }
                        }
                    }
                    words.push(word);
                    pos = k + 1;
                }
                Some(_) => {
                    let mut word = String::new();
                    while let Some(&c) = chars.get(pos) {
                        match c {
                            ' ' | '\t' | '\r' | '\n' | ';' => break,
                            '\\' => {
                                let (s, n) = backslash(chars, pos);
                                word.push_str(&s);
                                pos += n;
                            }
                            '$' => {
                                let (s, n) = self.variable(chars, pos)?;
                                word.push_str(&s);
                                pos += n;
                            }
                            '[' => {
                                let (s, n) = self.command_subst(chars, pos)?;
                                word.push_str(&s);
                                pos += n;
                            }
                            c => {
                                word.push(c);
                                pos += 1;
                            }
                        }
                    }
                    words.push(word);
                }
            }
            if words.iter().map(String::len).sum::<usize>() > MAX_OUTPUT {
                return Err(TclError::Limit);
            }
        }
    }

    fn command(&mut self, words: &[String]) -> Result<Flow, TclError> {
        let name = words[0].as_str();
        let args = &words[1..];
        if let Some(sim) = name.strip_prefix("sim_is_") {
            if !args.is_empty() {
                return Err(TclError::Syntax(format!("{name} takes no arguments")));
            }
            let yes = sim.eq_ignore_ascii_case(&self.env.simulator);
            return Ok(Flow::Normal(if yes { "1" } else { "0" }.into()));
        }
        match name {
            "expr" => {
                let text = args.join(" ");
                Ok(Flow::Normal(self.expr(&text)?.to_tcl()))
            }
            "ev" => {
                let [arg] = args else {
                    return Err(TclError::Syntax("ev takes one argument".into()));
                };
                // xschem's `ev`: the expression's value as %.4g, or the
                // text unchanged when it does not evaluate.
                Ok(Flow::Normal(match self.expr(arg) {
                    Ok(v) => format_g(v.as_f64()?, 4),
                    Err(TclError::Limit) => return Err(TclError::Limit),
                    Err(_) => arg.clone(),
                }))
            }
            "return" => match args {
                [] => Ok(Flow::Return(String::new())),
                [v] => Ok(Flow::Return(v.clone())),
                _ => Err(TclError::Unsupported("return with options".into())),
            },
            "if" => self.if_command(args),
            other => Err(TclError::Unsupported(other.to_string())),
        }
    }

    /// `if cond ?then? body ?elseif cond ?then? body ...? ?else? ?body?`
    fn if_command(&mut self, args: &[String]) -> Result<Flow, TclError> {
        let mut k = 0;
        loop {
            let cond = args
                .get(k)
                .ok_or_else(|| TclError::Syntax("if without a condition".into()))?;
            k += 1;
            if args.get(k).is_some_and(|w| w == "then") {
                k += 1;
            }
            let body = args
                .get(k)
                .ok_or_else(|| TclError::Syntax("if without a body".into()))?;
            k += 1;
            if self.expr(cond)?.truth()? {
                self.enter()?;
                let r = self.script(body);
                self.depth -= 1;
                return r;
            }
            match args.get(k).map(String::as_str) {
                None => return Ok(Flow::Normal(String::new())),
                Some("elseif") => k += 1,
                Some("else") => {
                    let body = args
                        .get(k + 1)
                        .ok_or_else(|| TclError::Syntax("else without a body".into()))?;
                    self.enter()?;
                    let r = self.script(body);
                    self.depth -= 1;
                    return r;
                }
                Some(body) => {
                    // The implicit else: `if c a b`.
                    let body = body.to_string();
                    self.enter()?;
                    let r = self.script(&body);
                    self.depth -= 1;
                    return r;
                }
            }
        }
    }

    /// Evaluate an expression. The nesting depth is restored whatever the
    /// outcome, since `ev` carries on after an expression that fails.
    fn expr(&mut self, text: &str) -> Result<Value, TclError> {
        let saved = self.depth;
        self.enter()?;
        let chars: Vec<char> = text.chars().collect();
        let mut p = ExprParser {
            chars: &chars,
            pos: 0,
            interp: self,
        };
        let result = p.ternary().and_then(|v| {
            p.skip_space();
            if p.pos < chars.len() {
                Err(TclError::Expr(format!(
                    "unexpected `{}` in `{text}`",
                    chars[p.pos..].iter().take(12).collect::<String>()
                )))
            } else {
                Ok(v)
            }
        });
        self.depth = saved;
        result
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Int(i64),
    Float(f64),
    Str(String),
}

impl Value {
    /// A string operand read as a number when it is one, as Tcl does.
    fn from_text(s: &str) -> Value {
        let t = s.trim();
        if let Ok(i) = t.parse::<i64>() {
            return Value::Int(i);
        }
        if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))
            && let Ok(i) = i64::from_str_radix(h, 16)
        {
            return Value::Int(i);
        }
        if let Ok(f) = t.parse::<f64>()
            && !t.eq_ignore_ascii_case("inf")
            && !t.eq_ignore_ascii_case("nan")
            && !t.eq_ignore_ascii_case("infinity")
        {
            return Value::Float(f);
        }
        Value::Str(s.to_string())
    }

    fn as_f64(&self) -> Result<f64, TclError> {
        match self {
            Value::Int(i) => Ok(*i as f64),
            Value::Float(f) => Ok(*f),
            Value::Str(s) => match Value::from_text(s) {
                Value::Str(_) => Err(TclError::Expr(format!("`{s}` is not a number"))),
                v => v.as_f64(),
            },
        }
    }

    fn numeric(self) -> Result<Value, TclError> {
        match self {
            Value::Str(s) => match Value::from_text(&s) {
                Value::Str(_) => Err(TclError::Expr(format!("`{s}` is not a number"))),
                v => Ok(v),
            },
            v => Ok(v),
        }
    }

    fn truth(&self) -> Result<bool, TclError> {
        match self {
            Value::Int(i) => Ok(*i != 0),
            Value::Float(f) => Ok(*f != 0.0),
            Value::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" => Ok(true),
                "false" | "no" | "off" => Ok(false),
                _ => match Value::from_text(s) {
                    Value::Str(_) => Err(TclError::Expr(format!("`{s}` is not a boolean"))),
                    v => v.truth(),
                },
            },
        }
    }

    fn to_tcl(&self) -> String {
        match self {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => tcl_double(*f),
            Value::Str(s) => s.clone(),
        }
    }
}

/// A double as Tcl prints it: the shortest text that reads back the same,
/// with `.0` on whole numbers and an exponent for very large or small ones.
fn tcl_double(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Inf" } else { "-Inf" }.into();
    }
    let a = f.abs();
    if a != 0.0 && !(1e-4..1e17).contains(&a) {
        let s = format!("{f:e}");
        return match s.split_once('e') {
            Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
            _ => s,
        };
    }
    let s = format!("{f}");
    if s.contains('.') { s } else { format!("{s}.0") }
}

/// C's `%.<precision>g`.
pub fn format_g(v: f64, precision: usize) -> String {
    let p = precision.max(1);
    if v == 0.0 {
        return "0".into();
    }
    if !v.is_finite() {
        return tcl_double(v);
    }
    let sci = format!("{:.*e}", p - 1, v);
    let (mantissa, exp) = sci.split_once('e').expect("exponent form");
    let exp: i32 = exp.parse().expect("exponent digits");
    let trim = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if exp < -4 || exp >= p as i32 {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa), exp.abs())
    } else {
        let decimals = (p as i32 - 1 - exp).max(0) as usize;
        trim(&format!("{v:.decimals$}"))
    }
}

struct ExprParser<'a, 'b, 'c> {
    chars: &'a [char],
    pos: usize,
    interp: &'b mut Interp<'c>,
}

impl ExprParser<'_, '_, '_> {
    fn skip_space(&mut self) {
        while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn peek_str(&mut self, s: &str) -> bool {
        self.skip_space();
        let n = s.chars().count();
        self.chars.len() >= self.pos + n
            && self.chars[self.pos..self.pos + n]
                .iter()
                .copied()
                .eq(s.chars())
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.peek_str(s) {
            self.pos += s.chars().count();
            true
        } else {
            false
        }
    }

    /// An operator word (`eq`, `ne`, `in`, `ni`), not the start of a name.
    fn eat_word(&mut self, s: &str) -> bool {
        if self.peek_str(s) {
            let after = self.chars.get(self.pos + s.len()).copied();
            if !after.is_some_and(is_name_char) {
                self.pos += s.len();
                return true;
            }
        }
        false
    }

    fn ternary(&mut self) -> Result<Value, TclError> {
        let cond = self.or()?;
        if self.eat("?") {
            let a = self.ternary()?;
            if !self.eat(":") {
                return Err(TclError::Expr("`?` without `:`".into()));
            }
            let b = self.ternary()?;
            return Ok(if cond.truth()? { a } else { b });
        }
        Ok(cond)
    }

    fn or(&mut self) -> Result<Value, TclError> {
        let mut v = self.and()?;
        while self.eat("||") {
            let r = self.and()?;
            v = Value::Int((v.truth()? || r.truth()?) as i64);
        }
        Ok(v)
    }

    fn and(&mut self) -> Result<Value, TclError> {
        let mut v = self.equality()?;
        while self.eat("&&") {
            let r = self.equality()?;
            v = Value::Int((v.truth()? && r.truth()?) as i64);
        }
        Ok(v)
    }

    fn equality(&mut self) -> Result<Value, TclError> {
        let mut v = self.relational()?;
        loop {
            if self.eat("==") {
                let r = self.relational()?;
                v = Value::Int((compare(&v, &r)? == std::cmp::Ordering::Equal) as i64);
            } else if self.eat("!=") {
                let r = self.relational()?;
                v = Value::Int((compare(&v, &r)? != std::cmp::Ordering::Equal) as i64);
            } else if self.eat_word("eq") {
                let r = self.relational()?;
                v = Value::Int((v.to_tcl() == r.to_tcl()) as i64);
            } else if self.eat_word("ne") {
                let r = self.relational()?;
                v = Value::Int((v.to_tcl() != r.to_tcl()) as i64);
            } else {
                return Ok(v);
            }
        }
    }

    fn relational(&mut self) -> Result<Value, TclError> {
        let mut v = self.additive()?;
        loop {
            use std::cmp::Ordering::*;
            let test: fn(std::cmp::Ordering) -> bool = if self.eat("<=") {
                |o| o != Greater
            } else if self.eat(">=") {
                |o| o != Less
            } else if self.eat("<") {
                |o| o == Less
            } else if self.eat(">") {
                |o| o == Greater
            } else {
                return Ok(v);
            };
            let r = self.additive()?;
            v = Value::Int(test(compare(&v, &r)?) as i64);
        }
    }

    fn additive(&mut self) -> Result<Value, TclError> {
        let mut v = self.multiplicative()?;
        loop {
            if self.eat("+") {
                let r = self.multiplicative()?;
                v = arith(v, r, '+')?;
            } else if self.peek_str("-") {
                self.pos += 1;
                let r = self.multiplicative()?;
                v = arith(v, r, '-')?;
            } else {
                return Ok(v);
            }
        }
    }

    fn multiplicative(&mut self) -> Result<Value, TclError> {
        let mut v = self.power()?;
        loop {
            if self.peek_str("**") {
                return Ok(v);
            }
            if self.eat("*") {
                let r = self.power()?;
                v = arith(v, r, '*')?;
            } else if self.eat("/") {
                let r = self.power()?;
                v = arith(v, r, '/')?;
            } else if self.eat("%") {
                let r = self.power()?;
                v = arith(v, r, '%')?;
            } else {
                return Ok(v);
            }
        }
    }

    fn power(&mut self) -> Result<Value, TclError> {
        let base = self.unary()?;
        if self.eat("**") {
            // Right associative.
            let exp = self.power()?;
            return arith(base, exp, '^');
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<Value, TclError> {
        if self.eat("!") {
            let v = self.unary()?;
            return Ok(Value::Int((!v.truth()?) as i64));
        }
        if self.peek_str("-") {
            self.pos += 1;
            return match self.unary()?.numeric()? {
                Value::Int(i) => Ok(Value::Int(-i)),
                Value::Float(f) => Ok(Value::Float(-f)),
                Value::Str(_) => unreachable!("numeric"),
            };
        }
        if self.eat("+") {
            return self.unary()?.numeric();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Value, TclError> {
        self.skip_space();
        let Some(&c) = self.chars.get(self.pos) else {
            return Err(TclError::Expr("missing operand".into()));
        };
        match c {
            '(' => {
                self.pos += 1;
                let v = self.ternary()?;
                if !self.eat(")") {
                    return Err(TclError::Expr("missing `)`".into()));
                }
                Ok(v)
            }
            '$' => {
                let (s, n) = self.interp.variable(self.chars, self.pos)?;
                self.pos += n;
                Ok(Value::from_text(&s))
            }
            '[' => {
                let (s, n) = self.interp.command_subst(self.chars, self.pos)?;
                self.pos += n;
                Ok(Value::from_text(&s))
            }
            '"' => {
                let mut k = self.pos + 1;
                let mut word = String::new();
                loop {
                    match self.chars.get(k) {
                        None => return Err(TclError::Expr("missing close quote".into())),
                        Some('"') => break,
                        Some('\\') => {
                            let (s, n) = backslash(self.chars, k);
                            word.push_str(&s);
                            k += n;
                        }
                        Some('$') => {
                            let (s, n) = self.interp.variable(self.chars, k)?;
                            word.push_str(&s);
                            k += n;
                        }
                        Some('[') => {
                            let (s, n) = self.interp.command_subst(self.chars, k)?;
                            word.push_str(&s);
                            k += n;
                        }
                        Some(ch) => {
                            word.push(*ch);
                            k += 1;
                        }
                    }
                    if word.len() > MAX_OUTPUT {
                        return Err(TclError::Limit);
                    }
                }
                self.pos = k + 1;
                Ok(Value::Str(word))
            }
            '{' => {
                let end = close_brace(self.chars, self.pos)?;
                let s: String = self.chars[self.pos + 1..end].iter().collect();
                self.pos = end + 1;
                Ok(Value::Str(s))
            }
            c if c.is_ascii_digit() || c == '.' => {
                let start = self.pos;
                let mut k = self.pos;
                while self
                    .chars
                    .get(k)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '.')
                {
                    if matches!(self.chars[k], 'e' | 'E')
                        && matches!(self.chars.get(k + 1), Some('+' | '-'))
                        && !self.chars[start..k].iter().any(|c| matches!(c, 'x' | 'X'))
                    {
                        k += 1;
                    }
                    k += 1;
                }
                let text: String = self.chars[start..k].iter().collect();
                self.pos = k;
                match Value::from_text(&text) {
                    Value::Str(s) => Err(TclError::Expr(format!("`{s}` is not a number"))),
                    v => Ok(v),
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = self.pos;
                let mut k = self.pos;
                while self.chars.get(k).copied().is_some_and(is_name_char) {
                    k += 1;
                }
                let name: String = self.chars[start..k].iter().collect();
                self.pos = k;
                if self.eat("(") {
                    let mut args = Vec::new();
                    if !self.eat(")") {
                        loop {
                            args.push(self.ternary()?);
                            if self.eat(")") {
                                break;
                            }
                            if !self.eat(",") {
                                return Err(TclError::Expr("expected `,` or `)`".into()));
                            }
                        }
                    }
                    return function(&name, &args);
                }
                match name.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "on" | "false" | "no" | "off" => Ok(Value::Str(name)),
                    "inf" => Ok(Value::Float(f64::INFINITY)),
                    _ => Err(TclError::Expr(format!("bare word `{name}`"))),
                }
            }
            other => Err(TclError::Expr(format!("unexpected `{other}`"))),
        }
    }
}

fn compare(a: &Value, b: &Value) -> Result<std::cmp::Ordering, TclError> {
    match (a.clone().numeric(), b.clone().numeric()) {
        (Ok(Value::Int(x)), Ok(Value::Int(y))) => Ok(x.cmp(&y)),
        (Ok(x), Ok(y)) => x
            .as_f64()?
            .partial_cmp(&y.as_f64()?)
            .ok_or_else(|| TclError::Expr("comparison with NaN".into())),
        _ => Ok(a.to_tcl().cmp(&b.to_tcl())),
    }
}

fn arith(a: Value, b: Value, op: char) -> Result<Value, TclError> {
    let (a, b) = (a.numeric()?, b.numeric()?);
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        let (x, y) = (*x, *y);
        let overflow = || TclError::Expr("integer overflow".into());
        return Ok(Value::Int(match op {
            '+' => x.checked_add(y).ok_or_else(overflow)?,
            '-' => x.checked_sub(y).ok_or_else(overflow)?,
            '*' => x.checked_mul(y).ok_or_else(overflow)?,
            '/' | '%' => {
                if y == 0 {
                    return Err(TclError::Expr("divide by zero".into()));
                }
                // Tcl rounds integer division toward minus infinity.
                if op == '/' {
                    floor_div(x, y)
                } else {
                    x - floor_div(x, y) * y
                }
            }
            '^' => {
                if y < 0 {
                    return Ok(Value::Float((x as f64).powf(y as f64)));
                }
                let e = u32::try_from(y).map_err(|_| overflow())?;
                x.checked_pow(e).ok_or_else(overflow)?
            }
            _ => unreachable!("operator"),
        }));
    }
    let (x, y) = (a.as_f64()?, b.as_f64()?);
    Ok(Value::Float(match op {
        '+' => x + y,
        '-' => x - y,
        '*' => x * y,
        '/' => {
            if y == 0.0 {
                return Err(TclError::Expr("divide by zero".into()));
            }
            x / y
        }
        '%' => return Err(TclError::Expr("% needs integers".into())),
        '^' => x.powf(y),
        _ => unreachable!("operator"),
    }))
}

fn floor_div(x: i64, y: i64) -> i64 {
    let q = x / y;
    if (x % y != 0) && ((x < 0) != (y < 0)) {
        q - 1
    } else {
        q
    }
}

fn function(name: &str, args: &[Value]) -> Result<Value, TclError> {
    let f = |i: usize| -> Result<f64, TclError> {
        args.get(i)
            .ok_or_else(|| TclError::Expr(format!("{name}() needs more arguments")))?
            .as_f64()
    };
    let float = |v: f64| Ok(Value::Float(v));
    match name {
        "abs" => match args.first().cloned().map(Value::numeric).transpose()? {
            Some(Value::Int(i)) => Ok(Value::Int(i.abs())),
            _ => float(f(0)?.abs()),
        },
        "sqrt" => float(f(0)?.sqrt()),
        "exp" => float(f(0)?.exp()),
        "log" => float(f(0)?.ln()),
        "log10" => float(f(0)?.log10()),
        "pow" => float(f(0)?.powf(f(1)?)),
        "double" => float(f(0)?),
        "int" | "round" | "floor" | "ceil" => {
            let v = f(0)?;
            let r = match name {
                "int" => v.trunc(),
                "round" => v.round(),
                "floor" => return float(v.floor()),
                _ => return float(v.ceil()),
            };
            Ok(Value::Int(r as i64))
        }
        "min" | "max" => {
            if args.is_empty() {
                return Err(TclError::Expr(format!("{name}() needs arguments")));
            }
            let mut best = args[0].clone().numeric()?;
            for a in &args[1..] {
                let a = a.clone().numeric()?;
                let o = compare(&a, &best)?;
                if (name == "min" && o.is_lt()) || (name == "max" && o.is_gt()) {
                    best = a;
                }
            }
            Ok(best)
        }
        other => Err(TclError::Unsupported(format!("{other}()"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env::new("ngspice").with_var("MODELS_NGSPICE", "/pdk/models")
    }

    #[test]
    fn ihp_model_block() {
        let out = tcleval(
            "tcleval( .lib $::MODELS_NGSPICE/cornerHBT.lib hbt_typ\n )\n",
            &env(),
        )
        .unwrap();
        assert_eq!(out, " .lib /pdk/models/cornerHBT.lib hbt_typ\n \n");
    }

    #[test]
    fn ihp_ptap1_and_pnp_arithmetic() {
        let line = "tcleval(**** spice_prefix X\nR5 ?1 net1 ?1 sub!  ptap1 R=[ev {  1.0 / ( 1.0 / ( 9.8e-10 / ( 10.0e-6 * 1.0e-6 ) ) + 1.0 / ( 9.8e-4 / ( 2.0 * ( 10.0e-6 + 1.0e-6 ) ) ) )}] w=10.0e-6 l=1.0e-6 )\n";
        let out = tcleval(line, &env()).unwrap();
        assert!(out.contains(" R=30.62 w=10.0e-6"), "{out}");
        let pnp = "tcleval(Q1 c b e pnpMPA a=[ev { 1.0e-6 * 2.0e-6 }] p=[ev { ( 1.0e-6 + 2.0e-6 ) * 2 }] m=[ev { 1 }] )";
        assert_eq!(
            tcleval(pnp, &env()).unwrap(),
            "Q1 c b e pnpMPA a=2e-12 p=6e-06 m=1 "
        );
    }

    #[test]
    fn xschem_vsource_choice() {
        let fmt = |save: &str| {
            format!(
                "tcleval([expr {{{save} ? \"V1 ?1 a ?1 b dc 1\n.save I( ?1 V1 )\" : \"V1 ?1 a ?1 b dc 1 \"}}])\n"
            )
        };
        assert_eq!(
            tcleval(&fmt("false"), &env()).unwrap(),
            "V1 ?1 a ?1 b dc 1 \n"
        );
        assert_eq!(
            tcleval(&fmt("true"), &env()).unwrap(),
            "V1 ?1 a ?1 b dc 1\n.save I( ?1 V1 )\n"
        );
    }

    #[test]
    fn simulator_commands_block() {
        let text = "tcleval(\n[if {[ sim_is_ngspice ]} { return {\n.op\n.save all\n}}]\n)\n";
        assert_eq!(tcleval(text, &env()).unwrap(), "\n\n.op\n.save all\n\n\n");
        let text = "tcleval(\n[if {[ sim_is_xyce ]} { return {\n.op\n}}]\n)\n";
        assert_eq!(tcleval(text, &env()).unwrap(), "\n\n\n");
    }

    #[test]
    fn expressions() {
        let e = |s: &str| {
            let mut i = Interp {
                env: &env(),
                depth: 0,
            };
            i.expr(s).map(|v| v.to_tcl())
        };
        assert_eq!(e("1 + 2 * 3").unwrap(), "7");
        assert_eq!(e("7 / 2").unwrap(), "3");
        assert_eq!(e("-7 / 2").unwrap(), "-4");
        assert_eq!(e("-7 % 2").unwrap(), "1");
        assert_eq!(e("7.0 / 2").unwrap(), "3.5");
        assert_eq!(e("2 ** 3 ** 2").unwrap(), "512");
        assert_eq!(e("(1 + 2) * 3").unwrap(), "9");
        assert_eq!(e("1 < 2 && 2 >= 2").unwrap(), "1");
        assert_eq!(e("!0 || 0").unwrap(), "1");
        assert_eq!(e("1 ? \"a\" : \"b\"").unwrap(), "a");
        assert_eq!(e("max(1, 2.5, 2)").unwrap(), "2.5");
        assert_eq!(e("sqrt(16)").unwrap(), "4.0");
        assert_eq!(e("abs(-3)").unwrap(), "3");
        assert_eq!(e("int(2.7)").unwrap(), "2");
        assert_eq!(e("1e-12").unwrap(), "1e-12");
        assert_eq!(e("\"x\" eq \"x\"").unwrap(), "1");
        assert!(e("1 / 0").is_err());
        assert!(e("foo").is_err());
        assert!(e("1 +").is_err());
    }

    #[test]
    fn percent_g() {
        assert_eq!(format_g(30.6226, 4), "30.62");
        assert_eq!(format_g(225.83, 4), "225.8");
        assert_eq!(format_g(2e-12, 4), "2e-12");
        assert_eq!(format_g(6e-6, 4), "6e-06");
        assert_eq!(format_g(1.0, 4), "1");
        assert_eq!(format_g(12345.0, 4), "1.234e+04");
        assert_eq!(format_g(0.0001234, 4), "0.0001234");
        assert_eq!(format_g(-0.5, 4), "-0.5");
    }

    #[test]
    fn ev_returns_text_it_cannot_evaluate() {
        assert_eq!(subst("[ev {1u * 2}]", &env()).unwrap(), "1u * 2");
        // Many failures in one line do not add up to the nesting limit.
        let many = "[ev {[expr {1u}]}] ".repeat(64);
        assert!(subst(&many, &env()).unwrap().starts_with("[expr {1u}]"));
    }

    #[test]
    fn anything_else_is_refused() {
        for bad in [
            "[exec rm -rf /tmp/x]",
            "[open /etc/passwd]",
            "[set x 1]",
            "[source evil.tcl]",
            "[expr {[exec ls]}]",
            "[if {1} {exec ls}]",
            "$env(HOME)",
            "$::unknown",
            "[expr {system(1)}]",
        ] {
            assert!(subst(bad, &env()).is_err(), "{bad}");
        }
        // Escaped brackets and lone dollars are text.
        assert_eq!(subst("a\\[b\\] $ c", &env()).unwrap(), "a[b] $ c");
        assert_eq!(subst("{x} \\\"q\\\"", &env()).unwrap(), "{x} \"q\"");
    }

    #[test]
    fn limits_stop_runaway_input() {
        let deep = format!("{}1{}", "[expr {".repeat(100), "}]".repeat(100));
        assert_eq!(subst(&deep, &env()), Err(TclError::Limit));
        assert!(subst("[expr {(1", &env()).is_err());
        assert!(subst("[if {1}", &env()).is_err());
        assert!(subst("\"unterminated", &env()).is_ok());
    }
}
