//! Reader: source text to values. Code is data, so the reader's output is
//! what the evaluator and the macros operate on.

use crate::value::*;
use std::rc::Rc;

pub struct Reader {
    chars: Vec<char>,
    i: usize,
    line: u32,
    col: u32,
}

pub fn read_all(src: &str) -> Res<Vec<Value>> {
    let mut r = Reader { chars: src.chars().collect(), i: 0, line: 1, col: 1 };
    let mut out = vec![];
    loop {
        r.skip_ws()?;
        if r.peek().is_none() {
            return Ok(out);
        }
        out.push(r.read_form()?);
    }
}

fn is_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | ';')
}

impl Reader {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }
    fn peek2(&self) -> Option<char> {
        self.chars.get(self.i + 1).copied()
    }
    fn pos(&self) -> Pos {
        Pos { line: self.line, col: self.col }
    }
    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }
    fn err<T>(&self, pos: Pos, msg: impl Into<String>) -> Res<T> {
        Err(KilnError::new("K0001", msg).at(Some(pos)))
    }

    fn skip_ws(&mut self) -> Res<()> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() || c == ',' => {
                    self.next();
                }
                Some(';') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.next();
                    }
                }
                Some('#') if self.peek2() == Some('_') => {
                    self.next();
                    self.next();
                    self.skip_ws()?;
                    self.read_form()?; // discarded
                }
                _ => return Ok(()),
            }
        }
    }

    fn read_form(&mut self) -> Res<Value> {
        self.skip_ws()?;
        let pos = self.pos();
        let c = match self.peek() {
            Some(c) => c,
            None => return self.err(pos, "unexpected end of input"),
        };
        match c {
            '(' => {
                self.next();
                let items = self.read_seq(')', pos)?;
                Ok(Value::List(Rc::new(items), Some(pos)))
            }
            '[' => {
                self.next();
                Ok(Value::vector(self.read_seq(']', pos)?))
            }
            '{' => {
                self.next();
                let items = self.read_seq('}', pos)?;
                if items.len() % 2 != 0 {
                    return self.err(pos, "map literal needs an even number of forms");
                }
                let mut entries = Vec::with_capacity(items.len() / 2);
                let mut it = items.into_iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    entries.push((k, v));
                }
                Ok(Value::map(entries))
            }
            ')' | ']' | '}' => self.err(pos, format!("unexpected `{}`", c)),
            '"' => self.read_string(),
            '\'' => self.wrap("quote", pos),
            '`' => self.wrap("quasiquote", pos),
            '~' => {
                if self.peek2() == Some('@') {
                    self.next();
                    self.wrap("unquote-splicing", pos)
                } else {
                    self.wrap("unquote", pos)
                }
            }
            _ => self.read_atom(),
        }
    }

    fn wrap(&mut self, name: &str, pos: Pos) -> Res<Value> {
        self.next();
        let inner = self.read_form()?;
        Ok(Value::List(Rc::new(vec![Value::sym(name), inner]), Some(pos)))
    }

    fn read_seq(&mut self, close: char, open_pos: Pos) -> Res<Vec<Value>> {
        let mut items = vec![];
        loop {
            self.skip_ws()?;
            match self.peek() {
                None => {
                    return self.err(open_pos, format!("unclosed delimiter, expected `{}`", close))
                }
                Some(c) if c == close => {
                    self.next();
                    return Ok(items);
                }
                _ => items.push(self.read_form()?),
            }
        }
    }

    fn read_string(&mut self) -> Res<Value> {
        let pos = self.pos();
        self.next();
        let mut s = String::new();
        loop {
            match self.next() {
                None => return self.err(pos, "unterminated string"),
                Some('"') => return Ok(Value::str(s)),
                Some('\\') => match self.next() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some(c) => {
                        return self.err(self.pos(), format!("unknown string escape `\\{}`", c))
                    }
                    None => return self.err(pos, "unterminated string"),
                },
                Some(c) => s.push(c),
            }
        }
    }

    fn read_atom(&mut self) -> Res<Value> {
        let pos = self.pos();
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if is_delim(c) {
                break;
            }
            s.push(c);
            self.next();
        }
        match s.as_str() {
            "nil" => return Ok(Value::Nil),
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            _ => {}
        }
        let first = s.chars().next().unwrap();
        let second = s.chars().nth(1);
        let numeric = first.is_ascii_digit()
            || ((first == '-' || first == '+') && second.map_or(false, |c| c.is_ascii_digit()));
        if numeric {
            if let Ok(i) = s.parse::<i64>() {
                return Ok(Value::Int(i));
            }
            if let Ok(f) = s.parse::<f64>() {
                return Ok(Value::Float(f));
            }
            return self.err(pos, format!("invalid number `{}`", s));
        }
        if let Some(k) = s.strip_prefix(':') {
            if k.is_empty() {
                return self.err(pos, "empty keyword");
            }
            return Ok(Value::kw(k));
        }
        Ok(Value::sym(&s))
    }
}
