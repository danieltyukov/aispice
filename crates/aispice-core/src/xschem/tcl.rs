//! A small Tcl subset for xschem's `tcleval(...)` strings, with no side
//! effects.
//!
//! xschem hands `tcleval(...)` text to Tcl's `subst`. Symbols use that for
//! arithmetic (IHP's `ptap1`: `R=[ev {1.0 / (...)}]`), for a choice
//! (xschem's `vsource`: `[expr {@savecurrent ? "..." : "..."}]`), for the
//! simulator in use (`[if {[sim_is_ngspice]} {return {...}}]`) and for model
//! paths (`$::MODELS_NGSPICE`). From 3.4.5 on, xschem's `simulator_commands`
//! symbols ask with `catch` whether the simulator has a `sim_is_` command:
//! `[if {[catch {sim_is_ngspice} retval]} {...} elseif {$retval} {...}]`.
//! This module evaluates exactly that: backslash sequences, variables from a
//! fixed table, braces and quotes, and the commands `expr`, `ev`, `if`,
//! `return`, `catch` and `sim_is_<simulator>`. `catch` runs only a body this
//! subset evaluates without an error, so it never hides a command aispice
//! does not know, and what it sets lasts for that evaluation only. Any other
//! command is an error, never a guess, so a file cannot make aispice do
//! anything but compute a string.
//!
//! The text comes from files aispice did not write, so evaluation is bounded
//! as well as side-effect free: the input and every intermediate string are
//! capped, command substitutions, script bodies, parentheses and operator
//! chains share one nesting limit (the parser recurses, so the limit is what
//! keeps a hostile file from exhausting the stack), integer arithmetic is
//! checked, and every scan stops at the end of the text. Any input gives a
//! string or an error, never a panic.

use std::collections::BTreeMap;

/// Nesting of command substitutions, script bodies, expressions,
/// parentheses and unary or `**` chains, together.
const MAX_DEPTH: usize = 32;
/// Bytes of input, of output, and of any intermediate string; also of all
/// the variables `catch` sets in one evaluation together.
pub const MAX_OUTPUT: usize = 256 * 1024;
/// Variables `catch` may set in one evaluation.
const MAX_LOCALS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TclError {
    #[error(
        "Tcl command `{0}` is not one aispice evaluates (it evaluates only expr, ev, if, return, catch and sim_is_*)"
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
    if text.len() > MAX_OUTPUT {
        return Err(TclError::Limit);
    }
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
    if text.len() > MAX_OUTPUT {
        return Err(TclError::Limit);
    }
    let mut i = Interp::new(env);
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

/// The end of a brace group that opens at `pos`, or an error. Iterative, so
/// any depth of nested braces costs one pass and no stack.
fn close_brace(chars: &[char], pos: usize) -> Result<usize, TclError> {
    let mut depth = 0usize;
    let mut k = pos;
    while k < chars.len() {
        match chars[k] {
            '\\' => k += 1,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
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
                depth = depth.saturating_sub(1);
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
    /// Variables `catch` set during this evaluation, read before `env`.
    locals: BTreeMap<String, String>,
}

impl<'a> Interp<'a> {
    fn new(env: &'a Env) -> Self {
        Self {
            env,
            depth: 0,
            locals: BTreeMap::new(),
        }
    }

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
        // xschem evaluates at the global level, where `$retval` and
        // `$::retval` are the same variable.
        let key = name.trim_start_matches("::");
        let value = match self.locals.get(key) {
            Some(v) => Some(v),
            None => self.env.vars.get(key),
        };
        match value {
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
        let mut words: Vec<String> = Vec::new();
        // Bytes in `words`, kept as a running total: summing the list after
        // every word would be quadratic in the number of words.
        let mut total = 0usize;
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
                        if total + word.len() > MAX_OUTPUT {
                            return Err(TclError::Limit);
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
                        if total + word.len() > MAX_OUTPUT {
                            return Err(TclError::Limit);
                        }
                    }
                    words.push(word);
                }
            }
            total += words.last().map_or(0, String::len);
            if total > MAX_OUTPUT {
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
            "catch" => self.catch_command(args),
            other => Err(TclError::Unsupported(other.to_string())),
        }
    }

    /// `catch body ?var?`: 0 when the body ends normally, 2 when it ends in
    /// `return`, with its result in `var`. A body that fails here is an
    /// error, not a 1: the failure may be a command this subset does not
    /// know, whose effect in Tcl it cannot tell. The variable is a plain
    /// name that `env` does not hold, since xschem would keep it set for
    /// everything it evaluates afterwards.
    fn catch_command(&mut self, args: &[String]) -> Result<Flow, TclError> {
        let (body, var) = match args {
            [body] => (body, None),
            [body, var] => (body, Some(var)),
            [] => return Err(TclError::Syntax("catch without a body".into())),
            _ => {
                return Err(TclError::Unsupported(
                    "catch with an options variable".into(),
                ));
            }
        };
        if let Some(var) = var {
            let plain = var
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && var.chars().all(is_name_char);
            if !plain {
                return Err(TclError::Unsupported(format!("catch into `{var}`")));
            }
            if self.env.vars.contains_key(var.as_str()) {
                return Err(TclError::Unsupported(format!(
                    "catch into `{var}`, which xschem would keep changed"
                )));
            }
        }
        self.enter()?;
        let flow = self.script(body);
        self.depth -= 1;
        let (code, result) = match flow? {
            Flow::Normal(v) => ("0", v),
            Flow::Return(v) => ("2", v),
        };
        if let Some(var) = var {
            self.locals.insert(var.clone(), result);
            let held: usize = self.locals.iter().map(|(k, v)| k.len() + v.len()).sum();
            if self.locals.len() > MAX_LOCALS || held > MAX_OUTPUT {
                return Err(TclError::Limit);
            }
        }
        Ok(Flow::Normal(code.into()))
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
    // Rust always writes `<mantissa>e<exponent>` for a finite value; the
    // fallback only keeps this total.
    let Some((mantissa, exp)) = sci.split_once('e') else {
        return sci;
    };
    let Ok(exp) = exp.parse::<i32>() else {
        return sci;
    };
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
    /// Run `f` one nesting level deeper. The parser recurses for
    /// parentheses, function arguments, `?:`, unary operators and `**`, so
    /// each of those counts against the interpreter's depth limit.
    fn nested<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, TclError>,
    ) -> Result<T, TclError> {
        self.interp.enter()?;
        let r = f(self);
        self.interp.depth = self.interp.depth.saturating_sub(1);
        r
    }

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
        self.nested(|p| {
            let cond = p.or()?;
            if p.eat("?") {
                let a = p.ternary()?;
                if !p.eat(":") {
                    return Err(TclError::Expr("`?` without `:`".into()));
                }
                let b = p.ternary()?;
                return Ok(if cond.truth()? { a } else { b });
            }
            Ok(cond)
        })
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
            let exp = self.nested(Self::power)?;
            return arith(base, exp, '^');
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<Value, TclError> {
        if self.eat("!") {
            let v = self.nested(Self::unary)?;
            return Ok(Value::Int((!v.truth()?) as i64));
        }
        if self.peek_str("-") {
            self.pos += 1;
            return match self.nested(Self::unary)?.numeric()? {
                Value::Int(i) => i
                    .checked_neg()
                    .map(Value::Int)
                    .ok_or_else(|| TclError::Expr("integer overflow".into())),
                Value::Float(f) => Ok(Value::Float(-f)),
                Value::Str(s) => Err(TclError::Expr(format!("`{s}` is not a number"))),
            };
        }
        if self.eat("+") {
            return self.nested(Self::unary)?.numeric();
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
                // Tcl rounds integer division toward minus infinity. The
                // one overflowing case, i64::MIN / -1, is an error.
                let q = floor_div(x, y).ok_or_else(overflow)?;
                if op == '/' {
                    q
                } else {
                    q.checked_mul(y)
                        .and_then(|qy| x.checked_sub(qy))
                        .ok_or_else(overflow)?
                }
            }
            '^' => {
                if y < 0 {
                    return Ok(Value::Float((x as f64).powf(y as f64)));
                }
                let e = u32::try_from(y).map_err(|_| overflow())?;
                x.checked_pow(e).ok_or_else(overflow)?
            }
            other => return Err(TclError::Expr(format!("operator `{other}`"))),
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
        other => return Err(TclError::Expr(format!("operator `{other}`"))),
    }))
}

/// Integer division rounded toward minus infinity, or `None` when it
/// overflows or divides by zero.
fn floor_div(x: i64, y: i64) -> Option<i64> {
    let q = x.checked_div(y)?;
    if (x.checked_rem(y)? != 0) && ((x < 0) != (y < 0)) {
        q.checked_sub(1)
    } else {
        Some(q)
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
            Some(Value::Int(i)) => i
                .checked_abs()
                .map(Value::Int)
                .ok_or_else(|| TclError::Expr("integer overflow".into())),
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

    /// xschem 3.4.5 and later: the `simulator_commands` format asks with
    /// `catch` whether there is a `sim_is_` command for the simulator.
    #[test]
    fn simulator_commands_with_catch() {
        let text = |sim: &str| {
            format!(
                "tcleval(\n[if {{[catch {{sim_is_{sim} }} retval] }} {{\n  return {{}} }} elseif {{$retval}} {{ return {{\n.op\n}}\n}} else {{ return {{}} }}\n])\n"
            )
        };
        assert_eq!(tcleval(&text("ngspice"), &env()).unwrap(), "\n\n.op\n\n");
        assert_eq!(tcleval(&text("xyce"), &env()).unwrap(), "\n\n");
    }

    /// `catch` gives 0 or, after `return`, 2, and sets its variable for the
    /// rest of the evaluation only.
    #[test]
    fn catch_reports_how_the_body_ended() {
        assert_eq!(subst("[catch {expr {1 + 1}} r]$r", &env()).unwrap(), "02");
        assert_eq!(subst("[catch {return x} r] $::r", &env()).unwrap(), "2 x");
        assert_eq!(subst("[catch {expr 1}]", &env()).unwrap(), "0");
        assert_eq!(
            subst("[catch {sim_is_xyce} r][catch {return $r} r]$r", &env()).unwrap(),
            "020"
        );
        assert_eq!(
            subst("$r", &env()),
            Err(TclError::UnknownVariable("r".into()))
        );
    }

    /// `catch` hides nothing: a body this subset cannot evaluate, or one
    /// Tcl itself would fail on, is an error, not a 1. Its variable is a
    /// plain name aispice's own variables do not use.
    #[test]
    fn catch_never_swallows_a_refusal() {
        for bad in [
            "[catch {exec rm -rf /tmp/x} r]",
            "[catch {open /etc/passwd} r]",
            "[catch {set x 1}]",
            "[catch {source evil.tcl} r]",
            "[catch {expr {1 / 0}} r]",
            "[catch {expr {system(1)}} r]",
            "[catch {return $env(HOME)} r]",
            "[if {[catch {exec ls} r]} {return a} else {return b}]",
            "[catch {expr 1} MODELS_NGSPICE]",
            "[catch {expr 1} ::r]",
            "[catch {expr 1} a(b)]",
            "[catch {expr 1} 1r]",
            "[catch {expr 1} {}]",
            "[catch {expr 1} r opts]",
            "[catch]",
        ] {
            assert!(subst(bad, &env()).is_err(), "{bad}");
        }
        assert!(matches!(
            subst("[catch {exec ls} r]", &env()),
            Err(TclError::Unsupported(c)) if c == "exec"
        ));
        assert_eq!(subst("$::MODELS_NGSPICE", &env()).unwrap(), "/pdk/models");
    }

    /// What `catch` keeps is bounded, by count and by size together.
    #[test]
    fn catch_variables_are_bounded() {
        let many: String = (0..=MAX_LOCALS)
            .map(|i| format!("[catch {{expr 1}} v{i}]"))
            .collect();
        assert_eq!(subst(&many, &env()), Err(TclError::Limit));
        let few: String = (0..MAX_LOCALS)
            .map(|i| format!("[catch {{expr 1}} v{i}]"))
            .collect();
        assert_eq!(subst(&few, &env()).unwrap(), "0".repeat(MAX_LOCALS));
        // Each value fits, together they do not.
        let big = format!(
            "[catch {{return {{{}}}}} a][catch {{return $a$a}} b][catch {{return $b$b}} c]",
            "x".repeat(50_000)
        );
        assert_eq!(subst(&big, &env()), Err(TclError::Limit));
        // Setting the same variable again replaces its value.
        let again = format!(
            "{}$a",
            format!("[catch {{return {{{}}}}} a]", "x".repeat(50_000)).repeat(4)
        );
        assert_eq!(subst(&again, &env()).unwrap().len(), 4 + 50_000);
    }

    #[test]
    fn expressions() {
        let e = |s: &str| {
            let env = env();
            let mut i = Interp::new(&env);
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
            "[catch {exec ls} r]",
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

    /// Input that would recurse once per character must stop at the
    /// nesting limit, quickly, instead of overflowing the stack.
    #[test]
    fn hostile_nesting_is_an_error_not_a_crash() {
        const N: usize = 100_000;
        let start = std::time::Instant::now();
        // Each case stays under the input cap, so it is the nesting limit
        // that stops it.
        let cases = [
            format!("[expr {{{}1{}}}]", "(".repeat(N), ")".repeat(N)),
            format!("[expr {{{}1}}]", "!".repeat(N)),
            format!("[expr {{{}1}}]", "-".repeat(N)),
            format!("[expr {{{}1}}]", "+ ".repeat(N)),
            format!("[expr {{1{}}}]", "**1".repeat(N / 2)),
            format!("[expr {{{}1{}}}]", "1?".repeat(N / 2), ":1".repeat(N / 2)),
            format!("[expr {{{}1{}}}]", "abs(".repeat(N / 4), ")".repeat(N / 4)),
            format!("{}1{}", "[".repeat(N), "]".repeat(N)),
            format!("{}{}", "[if 1 {".repeat(N / 4), "}]".repeat(N / 4)),
            format!("{}{}", "[ev {".repeat(N / 4), "}]".repeat(N / 4)),
            format!("{}{}", "[catch {".repeat(N / 4), "}]".repeat(N / 4)),
        ];
        for case in &cases {
            assert!(case.len() <= MAX_OUTPUT, "{}", &case[..40]);
            assert_eq!(subst(case, &env()), Err(TclError::Limit), "{}", &case[..40]);
        }
        // Braces are scanned without recursion: deep but balanced braces
        // are just a long word.
        let braces = format!("[expr {{{}x{}}}]", "{".repeat(N), "}".repeat(N));
        assert!(subst(&braces, &env()).is_ok());
        let braces = format!("[return {}x{}]", "{".repeat(N), "}".repeat(N));
        assert!(subst(&braces, &env()).is_ok());
        // Unbalanced input ends in an error at the end of the text.
        for case in [
            format!("[expr {}", "{".repeat(N)),
            format!("[expr {{\"{}}}]", "\"(".repeat(N)),
            "[".repeat(N),
            format!("[return \"{}]", "x".repeat(N)),
            format!("${{{}", "a".repeat(N)),
        ] {
            assert!(subst(&case, &env()).is_err(), "{}", &case[..20]);
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn integer_overflow_is_an_error() {
        for bad in [
            "[expr {(-9223372036854775807 - 1) / -1}]",
            "[expr {(-9223372036854775807 - 1) % -1}]",
            "[expr {-(-9223372036854775807 - 1)}]",
            "[expr {abs(-9223372036854775807 - 1)}]",
            "[expr {9223372036854775807 + 1}]",
            "[expr {3037000500 * 3037000500}]",
            "[expr {2 ** 64}]",
            "[expr {0x-8000000000000000 / -1}]",
        ] {
            assert!(subst(bad, &env()).is_err(), "{bad}");
        }
        assert_eq!(
            subst("[expr {-9223372036854775807 - 1}]", &env()).unwrap(),
            "-9223372036854775808"
        );
        assert_eq!(subst("[expr {-7 % -2}]", &env()).unwrap(), "-1");
        assert_eq!(subst("[expr {7 % -2}]", &env()).unwrap(), "-1");
    }

    #[test]
    fn size_limits() {
        let big = "x".repeat(MAX_OUTPUT + 1);
        assert_eq!(subst(&big, &env()), Err(TclError::Limit));
        assert_eq!(tcleval(&big, &env()), Err(TclError::Limit));
        // A word built from many variables stops at the cap.
        let many = format!("[return {}]", "$::MODELS_NGSPICE".repeat(20_000));
        assert_eq!(subst(&many, &env()), Err(TclError::Limit));
        // Many small words are checked with a running total, in linear time.
        let words = format!("[return {}]", "a ".repeat(100_000));
        let start = std::time::Instant::now();
        assert!(subst(&words, &env()).is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    /// Random text and cut-off real constructs never panic.
    #[test]
    fn random_and_truncated_input_never_panics() {
        let corpus = [
            "tcleval( .lib $::MODELS_NGSPICE/cornerHBT.lib hbt_typ\n )\n",
            "tcleval(R5 a b ptap1 R=[ev {  1.0 / ( 1.0 / ( 9.8e-10 / ( 10.0e-6 * 1.0e-6 ) ) + 1.0 ) }] w=1 )",
            "tcleval([expr {false ? \"V1 a b dc 1\n.save I( ?1 V1 )\" : \"V1 a b dc 1 \"}])",
            "tcleval(\n[if {[ sim_is_ngspice ]} { return {\n.op\n}} elseif {1} then {return x} else {return y}]\n)",
            "[expr {max(1, 2.5, 0x1f) + abs(-3) * 2 ** 3 % 5 - int(2.7) / 1.5e3 eq \"a\" ne {b}}]",
            "\\x41\\u00e9\\101\\\n  ${MODELS_NGSPICE} $a(b) \\[ ]",
            "tcleval(\n[if {[catch {sim_is_ngspice } retval] } {\n  return {} } elseif {$retval} { return {\n.op\n}\n} else { return {} }\n])",
        ];
        let mut rng = crate::xschem::test_rng(0x7c1e_5eed);
        let alphabet: Vec<char> = "[]{}()\"\\$:;# \n\tx01e.-+*/%!?<>=&|,ab\u{e9}\u{1f600}"
            .chars()
            .collect();
        for case in 0..4000 {
            let text: String = if case % 2 == 0 {
                let base = corpus[case / 2 % corpus.len()];
                let cut = rng.below(base.len() + 1);
                // Cut on a character boundary, then add a few random characters.
                let cut = (0..=cut)
                    .rev()
                    .find(|i| base.is_char_boundary(*i))
                    .unwrap_or(0);
                let mut t = base[..cut].to_string();
                for _ in 0..rng.below(4) {
                    t.push(alphabet[rng.below(alphabet.len())]);
                }
                t
            } else {
                (0..rng.below(200))
                    .map(|_| alphabet[rng.below(alphabet.len())])
                    .collect()
            };
            let _ = subst(&text, &env());
            let _ = tcleval(&text, &env());
        }
    }
}
