// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Parsing a `.ridl` file into a [`Module`].

use crate::lex::{self, Tok, Token};
use crate::{Call, Dir, Error, Field, Item, ItemKind, Member, Module, Prim, Type};

struct Parser<'a> {
    path: &'a str,
    tokens: Vec<Token>,
    at: usize,
}

type Result<T> = std::result::Result<T, Error>;

impl Parser<'_> {
    fn line(&self) -> usize {
        self.tokens.get(self.at).or(self.tokens.last()).map_or(0, |t| t.line)
    }

    fn error<T>(&self, message: impl Into<String>) -> Result<T> {
        Err(Error { path: self.path.to_string(), line: self.line(), message: message.into() })
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.at).map(|t| &t.tok)
    }

    fn next(&mut self) -> Option<Tok> {
        let tok = self.tokens.get(self.at).map(|t| t.tok.clone());
        self.at += 1;
        tok
    }

    fn punct(&mut self, c: char) -> Result<()> {
        match self.next() {
            Some(Tok::Punct(p)) if p == c => Ok(()),
            other => {
                self.at -= 1;
                self.error(format!("expected `{c}`, found {}", describe(other.as_ref())))
            }
        }
    }

    fn eat_punct(&mut self, c: char) -> bool {
        if self.peek() == Some(&Tok::Punct(c)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.next() {
            Some(Tok::Ident(name)) => Ok(name),
            other => {
                self.at -= 1;
                self.error(format!("expected a name, found {}", describe(other.as_ref())))
            }
        }
    }

    fn keyword(&mut self, word: &str) -> Result<()> {
        match self.next() {
            Some(Tok::Ident(name)) if name == word => Ok(()),
            other => {
                self.at -= 1;
                self.error(format!("expected `{word}`, found {}", describe(other.as_ref())))
            }
        }
    }

    fn int(&mut self) -> Result<u64> {
        match self.next() {
            Some(Tok::Int(value)) => Ok(value),
            other => {
                self.at -= 1;
                self.error(format!("expected a number, found {}", describe(other.as_ref())))
            }
        }
    }

    fn docs(&mut self) -> Vec<String> {
        let mut doc = Vec::new();
        while let Some(Tok::Doc(text)) = self.peek() {
            doc.push(text.clone());
            self.at += 1;
        }
        doc
    }

    fn prim(&mut self) -> Result<Prim> {
        let name = self.ident()?;
        match Prim::parse(&name) {
            Some(prim) => Ok(prim),
            None => {
                self.at -= 1;
                self.error(format!("`{name}` is not a primitive type"))
            }
        }
    }

    fn ty(&mut self) -> Result<Type> {
        if self.eat_punct('[') {
            let inner = self.ty()?;
            self.punct(';')?;
            let len = self.int()?;
            self.punct(']')?;
            return Ok(Type::Array(Box::new(inner), len as usize));
        }
        let name = self.ident()?;
        Ok(match name.as_str() {
            "slot" => Type::Slot,
            "addr" => Type::Addr,
            "in" | "out" => {
                self.keyword("bytes")?;
                Type::Bytes(if name == "in" { Dir::In } else { Dir::Out })
            }
            _ => match Prim::parse(&name) {
                Some(prim) => Type::Prim(prim),
                None => Type::Named(name),
            },
        })
    }

    fn params(&mut self) -> Result<Vec<Field>> {
        self.punct('(')?;
        let mut fields = Vec::new();
        while !self.eat_punct(')') {
            let line = self.line();
            let name = self.ident()?;
            self.punct(':')?;
            let ty = self.ty()?;
            fields.push(Field { doc: Vec::new(), name, ty, line });
            if !self.eat_punct(',') {
                self.punct(')')?;
                break;
            }
        }
        Ok(fields)
    }

    fn call(&mut self, doc: Vec<String>) -> Result<Call> {
        let line = self.line();
        let name = self.ident()?;
        self.punct('=')?;
        let number = self.int()?;
        let args = self.params()?;
        match self.next() {
            Some(Tok::Arrow) => {}
            other => {
                self.at -= 1;
                return self.error(format!("expected `->`, found {}", describe(other.as_ref())));
            }
        }
        let results = self.params()?;
        let mut raises = Vec::new();
        if self.peek() == Some(&Tok::Ident("raises".into())) {
            self.at += 1;
            self.punct('[')?;
            while !self.eat_punct(']') {
                raises.push(self.ident()?);
                if !self.eat_punct(',') {
                    self.punct(']')?;
                    break;
                }
            }
        }
        self.punct(';')?;
        Ok(Call { doc, name, number, args, results, raises, line })
    }

    /// `{ /// doc  name = value [=> ERRNO]; ... }`
    fn members(&mut self, errno: bool) -> Result<Vec<Member>> {
        self.punct('{')?;
        let mut members = Vec::new();
        loop {
            let doc = self.docs();
            if self.eat_punct('}') {
                if !doc.is_empty() {
                    return self.error("documentation comment with nothing after it");
                }
                return Ok(members);
            }
            let line = self.line();
            let name = self.ident()?;
            self.punct('=')?;
            let value = self.int()?;
            let errno = if errno {
                match self.next() {
                    Some(Tok::FatArrow) => Some(self.ident()?),
                    other => {
                        self.at -= 1;
                        return self.error(format!(
                            "expected `=>` and an errno name, found {}",
                            describe(other.as_ref())
                        ));
                    }
                }
            } else {
                None
            };
            self.punct(';')?;
            members.push(Member { doc, name, value, errno, line });
        }
    }

    fn item(&mut self) -> Result<Item> {
        let doc = self.docs();
        let line = self.line();
        let keyword = self.ident()?;
        let (name, kind) = match keyword.as_str() {
            "const" => {
                let name = self.ident()?;
                self.punct(':')?;
                let ty = self.prim()?;
                self.punct('=')?;
                let value = self.int()?;
                self.punct(';')?;
                (name, ItemKind::Const { ty, value })
            }
            "enum" | "flags" => {
                let name = self.ident()?;
                self.punct(':')?;
                let repr = self.prim()?;
                let members = self.members(false)?;
                let kind = if keyword == "enum" {
                    ItemKind::Enum { repr, variants: members }
                } else {
                    ItemKind::Flags { repr, bits: members }
                };
                (name, kind)
            }
            "errors" => {
                let name = self.ident()?;
                self.punct('=')?;
                let domain = self.int()?;
                let errors = self.members(true)?;
                (name, ItemKind::Errors { domain, errors })
            }
            "struct" => {
                let name = self.ident()?;
                self.punct('{')?;
                let mut fields = Vec::new();
                loop {
                    let doc = self.docs();
                    if self.eat_punct('}') {
                        break;
                    }
                    let line = self.line();
                    let field = self.ident()?;
                    self.punct(':')?;
                    let ty = self.ty()?;
                    self.punct(';')?;
                    fields.push(Field { doc, name: field, ty, line });
                }
                (name, ItemKind::Struct { fields })
            }
            "syscall" => {
                let call = self.call(doc.clone())?;
                (call.name.clone(), ItemKind::Syscall(call))
            }
            "interface" => {
                let name = self.ident()?;
                self.punct('=')?;
                let id = self.int()?;
                self.keyword("version")?;
                let version = self.int()?;
                self.punct('{')?;
                let mut kind = None;
                let mut methods = Vec::new();
                loop {
                    let doc = self.docs();
                    if self.eat_punct('}') {
                        break;
                    }
                    match self.ident()?.as_str() {
                        "kind" if doc.is_empty() => {
                            self.punct('=')?;
                            kind = Some(self.ident()?);
                            self.punct(';')?;
                        }
                        "method" => methods.push(self.call(doc)?),
                        other => {
                            self.at -= 1;
                            return self
                                .error(format!("expected `kind` or `method`, found `{other}`"));
                        }
                    }
                }
                (name, ItemKind::Interface { id, version, kind, methods })
            }
            other => {
                self.at -= 1;
                return self.error(format!(
                    "expected const, enum, flags, errors, struct, syscall or interface, found `{other}`"
                ));
            }
        };
        Ok(Item { doc, name, kind, line })
    }
}

fn describe(tok: Option<&Tok>) -> String {
    match tok {
        None => "the end of the file".into(),
        Some(Tok::Ident(name)) => format!("`{name}`"),
        Some(Tok::Int(value)) => format!("`{value}`"),
        Some(Tok::Doc(_)) => "a documentation comment".into(),
        Some(Tok::ModuleDoc(_)) => "a module documentation comment".into(),
        Some(Tok::Arrow) => "`->`".into(),
        Some(Tok::FatArrow) => "`=>`".into(),
        Some(Tok::Punct(c)) => format!("`{c}`"),
    }
}

/// Parses the file at `path` with contents `source`.
pub fn module(path: &str, source: &str) -> Result<Module> {
    let tokens = lex::tokens(source).map_err(|(line, message)| Error {
        path: path.to_string(),
        line,
        message,
    })?;
    let mut parser = Parser { path, tokens, at: 0 };
    let mut doc = Vec::new();
    while let Some(Tok::ModuleDoc(text)) = parser.peek() {
        doc.push(text.clone());
        parser.at += 1;
    }
    parser.keyword("module")?;
    let name = parser.ident()?;
    parser.punct(';')?;
    let mut items = Vec::new();
    while parser.peek().is_some() {
        items.push(parser.item()?);
    }
    Ok(Module { path: path.to_string(), name, doc, items })
}
