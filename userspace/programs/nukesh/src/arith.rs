// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Arithmetic: `$(( ))`, `(( ))`, `let`, array subscripts and integer
//! variables.
//!
//! Integers are 64 bits and wrap, as in bash. The operators and their
//! precedence are C's, plus `**`; numbers can be decimal, octal (`010`),
//! hexadecimal (`0x10`) or in any base from 2 to 64 (`2#1010`). A
//! variable's value is itself evaluated as an expression, so `a=b b=3`
//! makes `$((a))` 3. The expression is parsed and evaluated in one pass;
//! the side of `&&`, `||` and `?:` that is not taken is parsed with
//! evaluation off, so its assignments and divisions do nothing.

/// What the expression reads and changes.
pub trait Env {
    /// The text of a variable, or an element of an array: `None` if it is
    /// unset.
    fn get(&mut self, name: &str, index: Option<Index>) -> Result<Option<String>, String>;
    fn set(&mut self, name: &str, index: Option<Index>, value: i64) -> Result<(), String>;

    /// What the subscript `text` of `name[text]` names, made once for each
    /// place the expression reads or writes, before the first: an indexed
    /// array's subscript is evaluated, keeping its side effects, so that
    /// `a[i++] += 1` changes the element it read and `$((a[i++]))` moves
    /// `i`, as in bash; an associative array's is its key as written. Here
    /// the subscript is passed on as it is, for `get` and `set` to make
    /// what they will of it.
    fn key(&mut self, _name: &str, text: &str) -> Result<String, String> {
        Ok(text.to_string())
    }
}

/// A subscript in an expression: `name[text]`.
#[derive(Clone, Copy)]
pub struct Index<'a> {
    /// As written, for messages.
    pub text: &'a str,
    /// What [`Env::key`] made of it.
    pub key: &'a str,
}

/// How deep variables whose values name other variables may go.
const MAX_DEPTH: u32 = 64;

/// How deep parentheses and unary operators may nest, as bash limits
/// them, so that a long run of `(` is an error and not a stack overflow.
const MAX_NEST: u32 = 1024;

/// Evaluates `text`; an empty expression is 0.
pub fn eval(text: &str, env: &mut dyn Env) -> Result<i64, String> {
    eval_depth(text, env, 0)
}

fn eval_depth(text: &str, env: &mut dyn Env, depth: u32) -> Result<i64, String> {
    if depth > MAX_DEPTH {
        let text = text.trim_start();
        return Err(format!(
            "{text}: expression recursion level exceeded (error token is \"{text}\")"
        ));
    }
    let chars: Vec<char> = text.chars().collect();
    let mut parser = Parser { chars: &chars, pos: 0, env, depth, nest: 0, text };
    parser.skip_space();
    if parser.pos == chars.len() {
        return Ok(0);
    }
    let value = parser.comma(true)?;
    parser.skip_space();
    if parser.pos < chars.len() {
        return Err(parser.error_after_operand("syntax error in expression"));
    }
    Ok(value)
}

/// Parses an integer constant as arithmetic reads one: `None` if `text`
/// is not one.
pub fn parse_number(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let value = number_value(digits).ok()?;
    Some(if negative { value.wrapping_neg() } else { value })
}

/// The value of a constant without a sign.
fn number_value(text: &str) -> Result<i64, String> {
    let bad = || format!("{text}: value too great for base");
    if text.is_empty() {
        return Err("operand expected".into());
    }
    let (base, digits) = if let Some((base, digits)) = text.split_once('#') {
        let base: u32 = base.parse().map_err(|_| format!("{text}: invalid arithmetic base"))?;
        if !(2..=64).contains(&base) {
            return Err(format!("{text}: invalid arithmetic base"));
        }
        (base, digits)
    } else if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        // `0x` alone is 0, as in bash.
        (16, hex)
    } else if text.len() > 1 && text.starts_with('0') {
        (8, &text[1..])
    } else {
        (10, text)
    };
    if digits.is_empty() && !text.contains(['x', 'X']) {
        return Err(format!("{text}: invalid integer constant"));
    }
    let mut value: i64 = 0;
    for c in digits.chars() {
        let digit = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 10,
            'A'..='Z' if base <= 36 => c as u32 - 'A' as u32 + 10,
            'A'..='Z' => c as u32 - 'A' as u32 + 36,
            '@' => 62,
            '_' => 63,
            _ => return Err(bad()),
        };
        if digit >= base {
            return Err(bad());
        }
        value = value.wrapping_mul(i64::from(base)).wrapping_add(i64::from(digit));
    }
    Ok(value)
}

/// An lvalue: a variable, perhaps with a subscript.
struct Place {
    name: String,
    /// The subscript as written.
    index: Option<String>,
    /// What [`Env::key`] made of the subscript, at the place's first read
    /// or write: after the right side of `=`, before that of `+=`, as in
    /// bash.
    key: Option<String>,
}

impl Place {
    fn index(&self) -> Option<Index<'_>> {
        Some(Index { text: self.index.as_deref()?, key: self.key.as_deref()? })
    }
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
    env: &'a mut dyn Env,
    depth: u32,
    /// Parentheses and unary operators open now.
    nest: u32,
    text: &'a str,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        self.error_at(self.pos, what)
    }

    /// The error `what` where an operator should follow an operand, or
    /// bash's for a character that no operator starts with.
    fn error_after_operand(&self, what: &str) -> String {
        let next = self.chars[self.pos.min(self.chars.len())..].iter().find(|c| !c.is_whitespace());
        match next {
            Some(&c) if !c.is_alphanumeric() && c != '_' && !"=><+-*/%!()&|^~?:,".contains(c) => {
                self.error("syntax error: invalid arithmetic operator")
            }
            _ => self.error(what),
        }
    }

    /// An error, naming as bash does the expression and the rest of it
    /// from the token at `at`, or the last token when only space follows.
    fn error_at(&self, at: usize, what: &str) -> String {
        let end = self.chars.len();
        let mut at = at.min(end);
        while at < end && self.chars[at].is_whitespace() {
            at += 1;
        }
        if at == end {
            at = self.last_token();
        }
        let rest: String = self.chars[at..].iter().collect();
        let text = self.text.trim_start();
        if rest.is_empty() {
            format!("{text}: {what}")
        } else {
            format!("{text}: {what} (error token is \"{rest}\")")
        }
    }

    /// Where the expression's last token starts: a word or a number, or an
    /// operator of one or two characters.
    fn last_token(&self) -> usize {
        let word = |c: &char| c.is_ascii_alphanumeric() || matches!(c, '_' | '#' | '@');
        let mut at = self.chars.len();
        while at > 0 && self.chars[at - 1].is_whitespace() {
            at -= 1;
        }
        if at > 0 && word(&self.chars[at - 1]) {
            while at > 0 && word(&self.chars[at - 1]) {
                at -= 1;
            }
            return at;
        }
        const PAIRS: [&str; 18] = [
            "**", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "*=", "/=", "%=",
            "+=", "-=", "&=", "|=",
        ];
        if at >= 2 {
            let pair: String = self.chars[at - 2..at].iter().collect();
            if PAIRS.contains(&pair.as_str()) || pair == "^=" {
                return at - 2;
            }
        }
        at.saturating_sub(1)
    }

    fn skip_space(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_space();
        self.chars.get(self.pos).copied()
    }

    fn at(&mut self, op: &str) -> bool {
        self.skip_space();
        op.chars().enumerate().all(|(i, c)| self.chars.get(self.pos + i) == Some(&c))
    }

    /// Consumes `op` if it is next and is not the start of a longer
    /// operator in `longer`.
    fn eat(&mut self, op: &str, longer: &[&str]) -> bool {
        if !self.at(op) || longer.iter().any(|l| self.at(l)) {
            return false;
        }
        self.pos += op.chars().count();
        true
    }

    fn comma(&mut self, live: bool) -> Result<i64, String> {
        let mut value = self.assignment(live)?;
        while self.eat(",", &[]) {
            value = self.assignment(live)?;
        }
        Ok(value)
    }

    fn assignment(&mut self, live: bool) -> Result<i64, String> {
        self.nested(|p| p.assignment_here(live))
    }

    fn assignment_here(&mut self, live: bool) -> Result<i64, String> {
        let start = self.pos;
        if let Some(mut place) = self.place()? {
            const OPS: [&str; 11] =
                ["=", "*=", "/=", "%=", "+=", "-=", "<<=", ">>=", "&=", "^=", "|="];
            self.skip_space();
            for op in OPS {
                let longer: &[&str] = if op == "=" { &["=="] } else { &[] };
                if self.eat(op, longer) {
                    self.skip_space();
                    let right_at = self.pos;
                    let right = self.assignment(live)?;
                    if !live {
                        return Ok(0);
                    }
                    let value = if op == "=" {
                        right
                    } else {
                        let current = self.read(&mut place)?;
                        self.binary(&op[..op.len() - 1], current, right, right_at)?
                    };
                    self.write(&mut place, value)?;
                    return Ok(value);
                }
            }
        }
        self.pos = start;
        let value = self.conditional(live)?;
        self.skip_space();
        let assigns = ["=", "*=", "/=", "%=", "+=", "-=", "<<=", ">>=", "&=", "^=", "|="]
            .iter()
            .any(|op| self.at(op) && !self.at("=="));
        if assigns {
            return Err(self.error("attempted assignment to non-variable"));
        }
        Ok(value)
    }

    fn conditional(&mut self, live: bool) -> Result<i64, String> {
        let cond = self.logical_or(live)?;
        if !self.eat("?", &[]) {
            return Ok(cond);
        }
        if self.peek().is_none() {
            return Err(self.error("expression expected"));
        }
        let yes = self.comma(live && cond != 0)?;
        if !self.eat(":", &[]) {
            return Err(self.error_after_operand("`:' expected for conditional expression"));
        }
        if self.peek().is_none() {
            return Err(self.error("expression expected"));
        }
        let no = self.conditional(live && cond == 0)?;
        Ok(if cond != 0 { yes } else { no })
    }

    fn logical_or(&mut self, live: bool) -> Result<i64, String> {
        let mut value = self.logical_and(live)?;
        while self.eat("||", &[]) {
            let right = self.logical_and(live && value == 0)?;
            value = i64::from(value != 0 || right != 0);
        }
        Ok(value)
    }

    fn logical_and(&mut self, live: bool) -> Result<i64, String> {
        let mut value = self.binary_level(0, live)?;
        while self.eat("&&", &[]) {
            let right = self.binary_level(0, live && value != 0)?;
            value = i64::from(value != 0 && right != 0);
        }
        Ok(value)
    }

    /// The binary operators from `|` to `*`, lowest precedence first.
    fn binary_level(&mut self, level: usize, live: bool) -> Result<i64, String> {
        // Each level's operators, with the longer ones that start with them.
        const LEVELS: [&[(&str, &[&str])]; 9] = [
            &[("|", &["||", "|="])],
            &[("^", &["^="])],
            &[("&", &["&&", "&="])],
            &[("==", &[]), ("!=", &[])],
            &[("<=", &[]), (">=", &[]), ("<", &["<<", "<="]), (">", &[">>", ">="])],
            &[("<<", &["<<="]), (">>", &[">>="])],
            &[("+", &["++", "+="]), ("-", &["--", "-="])],
            &[("*", &["**", "*="]), ("/", &["/="]), ("%", &["%="])],
            &[],
        ];
        if LEVELS[level].is_empty() {
            return self.power(live);
        }
        let mut value = self.binary_level(level + 1, live)?;
        'more: loop {
            for (op, longer) in LEVELS[level] {
                if self.eat(op, longer) {
                    self.skip_space();
                    let right_at = self.pos;
                    let right = self.binary_level(level + 1, live)?;
                    value = if live { self.binary(op, value, right, right_at)? } else { 0 };
                    continue 'more;
                }
            }
            return Ok(value);
        }
    }

    /// `a op b`, where `b` started at `right_at`, which a division by 0
    /// names, as in bash.
    fn binary(&self, op: &str, a: i64, b: i64, right_at: usize) -> Result<i64, String> {
        Ok(match op {
            "|" => a | b,
            "^" => a ^ b,
            "&" => a & b,
            "==" => i64::from(a == b),
            "!=" => i64::from(a != b),
            "<" => i64::from(a < b),
            "<=" => i64::from(a <= b),
            ">" => i64::from(a > b),
            ">=" => i64::from(a >= b),
            "<<" => a.wrapping_shl(b as u32),
            ">>" => a.wrapping_shr(b as u32),
            "+" => a.wrapping_add(b),
            "-" => a.wrapping_sub(b),
            "*" => a.wrapping_mul(b),
            "/" | "%" if b == 0 => return Err(self.error_at(right_at, "division by 0")),
            "/" => a.wrapping_div(b),
            "%" => a.wrapping_rem(b),
            "**" => {
                if b < 0 {
                    return Err(self.error("exponent less than 0"));
                }
                let mut result: i64 = 1;
                let (mut base, mut exp) = (a, b as u64);
                while exp > 0 {
                    if exp & 1 == 1 {
                        result = result.wrapping_mul(base);
                    }
                    base = base.wrapping_mul(base);
                    exp >>= 1;
                }
                result
            }
            _ => unreachable!("not a binary operator: {op}"),
        })
    }

    /// `**`, which binds tighter than `*` and to the right.
    fn power(&mut self, live: bool) -> Result<i64, String> {
        let base = self.unary(live)?;
        if self.eat("**", &["**="]) {
            let exp = self.power(live)?;
            return if live { self.binary("**", base, exp, self.pos) } else { Ok(0) };
        }
        Ok(base)
    }

    /// Counts a level of nesting around `f`, which fails past
    /// [`MAX_NEST`].
    fn nested(&mut self, f: impl FnOnce(&mut Self) -> Result<i64, String>) -> Result<i64, String> {
        if self.nest >= MAX_NEST {
            return Err(self.error("expression recursion level exceeded"));
        }
        self.nest += 1;
        let result = f(self);
        self.nest -= 1;
        result
    }

    fn unary(&mut self, live: bool) -> Result<i64, String> {
        self.nested(|p| p.unary_here(live))
    }

    fn unary_here(&mut self, live: bool) -> Result<i64, String> {
        if self.at("++") || self.at("--") {
            let up = self.at("++");
            self.pos += 2;
            let Some(mut place) = self.place()? else {
                return Err(self.error("syntax error: operand expected"));
            };
            if !live {
                return Ok(0);
            }
            let value = self.read(&mut place)?.wrapping_add(if up { 1 } else { -1 });
            self.write(&mut place, value)?;
            return Ok(value);
        }
        if self.eat("-", &["-="]) {
            return Ok(self.unary(live)?.wrapping_neg());
        }
        if self.eat("+", &["+="]) {
            return self.unary(live);
        }
        if self.eat("!", &["!="]) {
            return Ok(i64::from(self.unary(live)? == 0));
        }
        if self.eat("~", &[]) {
            return Ok(!self.unary(live)?);
        }
        self.postfix(live)
    }

    fn postfix(&mut self, live: bool) -> Result<i64, String> {
        let start = self.pos;
        if let Some(mut place) = self.place()? {
            if self.at("++") || self.at("--") {
                let up = self.at("++");
                self.pos += 2;
                if !live {
                    return Ok(0);
                }
                let value = self.read(&mut place)?;
                self.write(&mut place, value.wrapping_add(if up { 1 } else { -1 }))?;
                return Ok(value);
            }
            return if live { self.read(&mut place) } else { Ok(0) };
        }
        self.pos = start;
        self.primary(live)
    }

    fn primary(&mut self, live: bool) -> Result<i64, String> {
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let value = self.comma(live)?;
                if !self.eat(")", &[]) {
                    return Err(self.error_after_operand("missing `)'"));
                }
                Ok(value)
            }
            Some(c) if c.is_ascii_digit() => {
                let start = self.pos;
                while let Some(&c) = self.chars.get(self.pos) {
                    if c.is_ascii_alphanumeric() || c == '#' || c == '@' || c == '_' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let text: String = self.chars[start..self.pos].iter().collect();
                number_value(&text).map_err(|message| {
                    let what = message.rsplit(": ").next().unwrap_or(&message);
                    if what == "invalid integer constant" {
                        // bash shows the expression up to the constant.
                        let before: String = self.chars[..self.pos].iter().collect();
                        let before = before.trim_start();
                        return format!("{before}: {what} (error token is \"{text}\")");
                    }
                    self.pos = start;
                    self.error(what)
                })
            }
            None => Err(self.error("syntax error: operand expected")),
            Some(_) => Err(self.error("syntax error: operand expected")),
        }
    }

    /// A variable name, with its subscript, if one is next.
    fn place(&mut self) -> Result<Option<Place>, String> {
        let Some(c) = self.peek() else { return Ok(None) };
        if !(c == '_' || c.is_ascii_alphabetic()) {
            return Ok(None);
        }
        let start = self.pos;
        while let Some(&c) = self.chars.get(self.pos) {
            if c == '_' || c.is_ascii_alphanumeric() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let name: String = self.chars[start..self.pos].iter().collect();
        let mut index = None;
        if self.chars.get(self.pos) == Some(&'[') {
            self.pos += 1;
            let begin = self.pos;
            let mut depth = 0;
            loop {
                match self.chars.get(self.pos) {
                    None => return Err(self.error_at(start, "bad array subscript")),
                    Some('[') => depth += 1,
                    Some(']') if depth == 0 => break,
                    Some(']') => depth -= 1,
                    _ => {}
                }
                self.pos += 1;
            }
            index = Some(self.chars[begin..self.pos].iter().collect());
            self.pos += 1;
        }
        Ok(Some(Place { name, index, key: None }))
    }

    /// Makes the key of `place`'s subscript, if it has one and it is not
    /// yet made.
    fn resolve(&mut self, place: &mut Place) -> Result<(), String> {
        if place.key.is_none()
            && let Some(text) = &place.index
        {
            place.key = Some(self.env.key(&place.name, text)?);
        }
        Ok(())
    }

    fn read(&mut self, place: &mut Place) -> Result<i64, String> {
        self.resolve(place)?;
        let text = self.env.get(&place.name, place.index())?;
        match text {
            None => Ok(0),
            Some(text) => {
                if let Some(value) = parse_number(&text) {
                    return Ok(value);
                }
                eval_depth(&text, self.env, self.depth + 1)
            }
        }
    }

    fn write(&mut self, place: &mut Place, value: i64) -> Result<(), String> {
        self.resolve(place)?;
        self.env.set(&place.name, place.index(), value)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct Vars(HashMap<String, String>);

    impl Env for Vars {
        fn get(&mut self, name: &str, index: Option<Index>) -> Result<Option<String>, String> {
            let key = match index {
                Some(i) => format!("{name}[{}]", i.key),
                None => name.to_string(),
            };
            Ok(self.0.get(&key).cloned())
        }

        fn set(&mut self, name: &str, index: Option<Index>, value: i64) -> Result<(), String> {
            let key = match index {
                Some(i) => format!("{name}[{}]", i.key),
                None => name.to_string(),
            };
            self.0.insert(key, value.to_string());
            Ok(())
        }

        fn key(&mut self, _name: &str, text: &str) -> Result<String, String> {
            Ok(eval(text, self)?.to_string())
        }
    }

    fn calc(text: &str) -> i64 {
        eval(text, &mut Vars::default()).unwrap()
    }

    #[test]
    fn follows_c_precedence() {
        assert_eq!(calc("1 + 2 * 3"), 7);
        assert_eq!(calc("(1 + 2) * 3"), 9);
        assert_eq!(calc("2 ** 3 ** 2"), 512);
        assert_eq!(calc("-2 ** 2"), 4);
        assert_eq!(calc("7 / 2 + 7 % 2"), 4);
        assert_eq!(calc("1 << 4 | 1"), 17);
        assert_eq!(calc("5 > 3 && 2 > 1"), 1);
        assert_eq!(calc("0 || 0"), 0);
        assert_eq!(calc("1 ? 2 : 3"), 2);
        assert_eq!(calc("0 ? 2 : 0 ? 3 : 4"), 4);
        assert_eq!(calc("1, 2, 3"), 3);
        assert_eq!(calc("~0"), -1);
        assert_eq!(calc("!5"), 0);
        assert_eq!(calc("3 - -2"), 5);
        assert_eq!(calc(""), 0);
    }

    #[test]
    fn reads_every_base() {
        assert_eq!(calc("010"), 8);
        assert_eq!(calc("0x1F"), 31);
        assert_eq!(calc("2#1010"), 10);
        assert_eq!(calc("36#z"), 35);
        assert_eq!(calc("64#_"), 63);
        assert_eq!(calc("10#08"), 8);
        assert!(eval("08", &mut Vars::default()).is_err());
        assert!(eval("2#2", &mut Vars::default()).is_err());
    }

    #[test]
    fn wraps_like_bash() {
        assert_eq!(calc("9223372036854775807 + 1"), i64::MIN);
        assert_eq!(calc("-9223372036854775808 / -1"), i64::MIN);
    }

    #[test]
    fn assigns_and_counts() {
        let mut vars = Vars::default();
        assert_eq!(eval("x = 5", &mut vars).unwrap(), 5);
        assert_eq!(eval("x += 2", &mut vars).unwrap(), 7);
        assert_eq!(eval("x++", &mut vars).unwrap(), 7);
        assert_eq!(eval("++x", &mut vars).unwrap(), 9);
        assert_eq!(eval("x--, x", &mut vars).unwrap(), 8);
        assert_eq!(eval("y = x <<= 1", &mut vars).unwrap(), 16);
        assert_eq!(vars.0["y"], "16");
        assert_eq!(eval("a[1+1] = 4, a[2] * 2", &mut vars).unwrap(), 8);
        assert_eq!(eval("x == 16", &mut vars).unwrap(), 1);
    }

    #[test]
    fn variables_hold_expressions() {
        let mut vars = Vars::default();
        vars.0.insert("a".into(), "b + 1".into());
        vars.0.insert("b".into(), "c".into());
        vars.0.insert("c".into(), "4".into());
        assert_eq!(eval("a * 2", &mut vars).unwrap(), 10);
        assert_eq!(eval("unset_one + 1", &mut vars).unwrap(), 1);
        vars.0.insert("loop".into(), "loop".into());
        assert!(eval("loop", &mut vars).unwrap_err().contains("recursion"));
    }

    #[test]
    fn the_side_not_taken_does_nothing() {
        let mut vars = Vars::default();
        assert_eq!(eval("0 && (x = 1)", &mut vars).unwrap(), 0);
        assert_eq!(eval("1 || (x = 1)", &mut vars).unwrap(), 1);
        assert_eq!(eval("1 ? 2 : (x = 1 / 0)", &mut vars).unwrap(), 2);
        assert!(!vars.0.contains_key("x"));
    }

    #[test]
    fn reports_mistakes() {
        let mut vars = Vars::default();
        assert!(eval("1 / 0", &mut vars).unwrap_err().contains("division by 0"));
        assert!(eval("2 ** -1", &mut vars).unwrap_err().contains("exponent"));
        assert!(eval("1 +", &mut vars).unwrap_err().contains("operand expected"));
        assert!(eval("(1", &mut vars).unwrap_err().contains("missing"));
        assert!(eval("1 2", &mut vars).unwrap_err().contains("syntax error"));
    }

    #[test]
    fn names_the_error_token_as_bash_does() {
        let mut vars = Vars::default();
        vars.0.insert("x".into(), "x".into());
        let mut error = |text: &str| eval(text, &mut vars).unwrap_err();
        assert_eq!(error("4 / 0 + 1"), "4 / 0 + 1: division by 0 (error token is \"0 + 1\")");
        assert_eq!(error(" 1 + "), "1 + : syntax error: operand expected (error token is \"+ \")");
        assert_eq!(error("1 ? 2 :"), "1 ? 2 :: expression expected (error token is \":\")");
        assert_eq!(
            error("4 ^= 1"),
            "4 ^= 1: attempted assignment to non-variable (error token is \"^= 1\")"
        );
        assert_eq!(error("a[1"), "a[1: bad array subscript (error token is \"a[1\")");
        assert_eq!(error("2 ** -1"), "2 ** -1: exponent less than 0 (error token is \"1\")");
        assert_eq!(
            error("1 + 10# * 2"),
            "1 + 10#: invalid integer constant (error token is \"10#\")"
        );
        assert_eq!(error("x"), "x: expression recursion level exceeded (error token is \"x\")");
        assert_eq!(eval("0x + 1", &mut vars), Ok(1));
    }

    #[test]
    fn evaluates_a_subscript_once_for_each_place() {
        let mut vars = Vars::default();
        eval("a[0] = 5, a[1] = 6, i = 0", &mut vars).unwrap();
        assert_eq!(eval("a[i++]", &mut vars), Ok(5));
        assert_eq!(eval("i", &mut vars), Ok(1));
        assert_eq!(eval("a[i++] += 10", &mut vars), Ok(16));
        assert_eq!(eval("a[1] + i * 100", &mut vars), Ok(216));
        assert_eq!(eval("a[--i]++", &mut vars), Ok(16));
        assert_eq!(eval("a[1] + i * 100", &mut vars), Ok(117));
        // After the right side of `=`, as bash.
        assert_eq!(eval("a[i++] = i", &mut vars), Ok(1));
        assert_eq!(eval("a[1] + i * 100", &mut vars), Ok(201));
        // The side not taken leaves its subscript alone.
        assert_eq!(eval("0 && a[i++]", &mut vars), Ok(0));
        assert_eq!(eval("i", &mut vars), Ok(2));
    }

    #[test]
    fn reads_constants() {
        assert_eq!(parse_number(" -12 "), Some(-12));
        assert_eq!(parse_number("0x10"), Some(16));
        assert_eq!(parse_number("abc"), None);
        assert_eq!(parse_number(""), None);
    }
}
