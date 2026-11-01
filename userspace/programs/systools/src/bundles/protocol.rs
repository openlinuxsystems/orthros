// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What bundle(1) and bundled(8) say to each other on bundled's socket
//! (ADRs 0223 and 0225). A request is one line, tab-separated; an
//! `install` request comes with the archive's descriptor (SCM_RIGHTS).
//! bundled answers with lines too: the install sheet (`sheet N` and N bytes
//! of text), a question for a yes or a no, a listed bundle, the bundle to
//! run, and last `ok` or `error` with why.

use std::io::{self, BufRead, Read};

use super::minisign::PublicKey;
use super::registry::Place;

/// Where bundled answers.
pub const SOCKET: &str = "/system/live/bundled/socket";
/// The longest request line, and the longest answer line.
pub const LINE_MAX: usize = 4096;
/// The longest sheet.
pub const SHEET_MAX: usize = 64 * 1024;

/// What bundle asks bundled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Installs the archive sent with the request, for everyone or for the
    /// asker; with `yes`, without asking after the sheet.
    Install { everyone: bool, yes: bool },
    /// Lists the bundles the asker can run.
    List,
    /// Shows an installed bundle's sheet.
    Info { id: String },
    /// Removes a bundle, for everyone or the asker's; with `purge`, the
    /// asker's data of it too.
    Remove { id: String, everyone: bool, purge: bool },
    /// Keeps the asker's developer key.
    DeveloperKey { key: PublicKey },
    /// Finds the bundle to run, of the asker's or everyone's.
    Run { find: Find },
}

/// Which bundle `run` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Find {
    /// The bundle of this ID.
    Id(String),
    /// The bundle that adds this command.
    Command(String),
}

fn whose(everyone: bool) -> &'static str {
    if everyone { "everyone" } else { "me" }
}

fn checked_id(id: &str) -> Result<String, String> {
    manifest::bundle::check_id(id).map_err(|why| format!("{id:?}: {why}"))?;
    Ok(id.into())
}

impl Request {
    /// The request's line, with its newline.
    pub fn line(&self) -> String {
        match self {
            Request::Install { everyone, yes } => {
                format!("install\t{}\t{}\n", whose(*everyone), if *yes { "yes" } else { "ask" })
            }
            Request::List => "list\n".into(),
            Request::Info { id } => format!("info\t{id}\n"),
            Request::Remove { id, everyone, purge } => {
                let data = if *purge { "purge" } else { "keep-data" };
                format!("remove\t{id}\t{}\t{data}\n", whose(*everyone))
            }
            Request::DeveloperKey { key } => format!("developer-key\t{}\n", key.encoded()),
            Request::Run { find: Find::Id(id) } => format!("run\tid\t{id}\n"),
            Request::Run { find: Find::Command(name) } => format!("run\tcommand\t{name}\n"),
        }
    }

    /// Reads a request's line, without its newline.
    pub fn read(line: &str) -> Result<Request, String> {
        let words: Vec<&str> = line.split('\t').collect();
        let everyone = |word: &str| match word {
            "everyone" => Ok(true),
            "me" => Ok(false),
            _ => Err(format!("{word:?} is neither everyone nor me")),
        };
        match words.as_slice() {
            ["install", place, answer] => {
                let yes = match *answer {
                    "yes" => true,
                    "ask" => false,
                    _ => return Err(format!("{answer:?} is neither yes nor ask")),
                };
                Ok(Request::Install { everyone: everyone(place)?, yes })
            }
            ["list"] => Ok(Request::List),
            ["info", id] => Ok(Request::Info { id: checked_id(id)? }),
            ["remove", id, place, data] => {
                let purge = match *data {
                    "purge" => true,
                    "keep-data" => false,
                    _ => return Err(format!("{data:?} is neither purge nor keep-data")),
                };
                Ok(Request::Remove { id: checked_id(id)?, everyone: everyone(place)?, purge })
            }
            ["developer-key", key] => Ok(Request::DeveloperKey { key: PublicKey::decode(key)? }),
            ["run", "id", id] => Ok(Request::Run { find: Find::Id(checked_id(id)?) }),
            ["run", "command", name] => {
                manifest::bundle::check_command_name(name)?;
                Ok(Request::Run { find: Find::Command((*name).into()) })
            }
            _ => Err("not a request bundled knows".into()),
        }
    }
}

/// A bundle as `list` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub place: Place,
    pub id: String,
    pub version: String,
    pub key: PublicKey,
    /// When it was installed, in seconds since 1970.
    pub when: u64,
    /// Its name, as it says it, shown.
    pub name: String,
}

/// The bundle to run, as `run` finds it: where, and what bundled
/// recorded of it, which the runner checks the folder against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub place: Place,
    pub id: String,
    pub key: PublicKey,
    /// The SHA-256 of its signed list, in hex.
    pub list: String,
}

/// What bundled answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The install sheet's text.
    Sheet(String),
    /// Whether to go on: bundle answers `yes` or `no`.
    Question,
    /// A bundle of a list.
    Listed(Listed),
    /// The bundle to run.
    Found(Found),
    /// Done, and what was done.
    Ok(String),
    /// Not done, and why.
    Error(String),
}

/// Text on one line, as an answer carries it.
fn one_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

impl Answer {
    /// The answer's bytes.
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            Answer::Sheet(text) => {
                let text: String = text.chars().filter(|&c| c == '\n' || !c.is_control()).collect();
                format!("sheet {}\n{text}", text.len()).into_bytes()
            }
            Answer::Question => b"question\n".to_vec(),
            Answer::Listed(l) => format!(
                "listed\t{}\t{}\t{}\t{}\t{}\t{}\n",
                l.place,
                l.id,
                one_line(&l.version),
                l.key.encoded(),
                l.when,
                one_line(&l.name)
            )
            .into_bytes(),
            Answer::Found(f) => {
                format!("found\t{}\t{}\t{}\t{}\n", f.place, f.id, f.key.encoded(), f.list)
                    .into_bytes()
            }
            Answer::Ok(what) => format!("ok {}\n", one_line(what)).into_bytes(),
            Answer::Error(why) => format!("error {}\n", one_line(why)).into_bytes(),
        }
    }

    /// Reads the next answer from `input`; none at its end.
    pub fn read(input: &mut impl BufRead) -> io::Result<Option<Answer>> {
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
        let mut line = Vec::new();
        let n = input.by_ref().take(LINE_MAX as u64 + 1).read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(None);
        }
        if line.pop() != Some(b'\n') {
            return Err(bad("bundled's answer has no end"));
        }
        let line = String::from_utf8(line).map_err(|_| bad("bundled's answer is not text"))?;
        if let Some(length) = line.strip_prefix("sheet ") {
            let length: usize = length.parse().map_err(|_| bad("a sheet without its length"))?;
            if length > SHEET_MAX {
                return Err(bad("bundled's sheet is too long"));
            }
            let mut text = vec![0; length];
            input.read_exact(&mut text)?;
            let text = String::from_utf8(text).map_err(|_| bad("bundled's sheet is not text"))?;
            return Ok(Some(Answer::Sheet(
                text.chars().filter(|&c| c == '\n' || !c.is_control()).collect(),
            )));
        }
        if line == "question" {
            return Ok(Some(Answer::Question));
        }
        if let Some(what) = line.strip_prefix("ok ") {
            return Ok(Some(Answer::Ok(one_line(what))));
        }
        if let Some(why) = line.strip_prefix("error ") {
            return Ok(Some(Answer::Error(one_line(why))));
        }
        let words: Vec<&str> = line.split('\t').collect();
        let place = |word: &str| match word {
            "everyone" => Ok(Place::Everyone),
            uid => uid.parse().map(Place::User).map_err(|_| bad("a bundle's place")),
        };
        if let ["found", at, id, key, list] = words.as_slice() {
            let hex = list.len() == 64 && list.bytes().all(|b| b.is_ascii_hexdigit());
            if !hex {
                return Err(bad("a found bundle's list"));
            }
            return Ok(Some(Answer::Found(Found {
                place: place(at)?,
                id: checked_id(id).map_err(|why| bad(&why))?,
                key: PublicKey::decode(key).map_err(|why| bad(&why))?,
                list: list.to_ascii_lowercase(),
            })));
        }
        if let ["listed", place_word, id, version, key, when, name] = words.as_slice() {
            let place = place(place_word)?;
            return Ok(Some(Answer::Listed(Listed {
                place,
                id: checked_id(id).map_err(|why| bad(&why))?,
                version: one_line(version),
                key: PublicKey::decode(key).map_err(|why| bad(&why))?,
                when: when.parse().map_err(|_| bad("a listed bundle's time"))?,
                name: one_line(name),
            })));
        }
        Err(bad("not an answer bundle knows"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundles::minisign::SecretKey;

    fn key() -> PublicKey {
        let mut random = [9u8; 40];
        random[0] = 1;
        SecretKey::generate(&random).unwrap().public()
    }

    #[test]
    fn requests_read_back() {
        for request in [
            Request::Install { everyone: false, yes: false },
            Request::Install { everyone: true, yes: true },
            Request::List,
            Request::Info { id: "org.example.notes".into() },
            Request::Remove { id: "org.example.notes".into(), everyone: false, purge: true },
            Request::Remove { id: "org.example.notes".into(), everyone: true, purge: false },
            Request::DeveloperKey { key: key() },
            Request::Run { find: Find::Id("org.example.notes".into()) },
            Request::Run { find: Find::Command("notes".into()) },
        ] {
            let line = request.line();
            assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
            assert_eq!(Request::read(line.trim_end_matches('\n')), Ok(request));
        }
        for bad in [
            "",
            "install\tme",
            "install\tall\tyes",
            "install\tme\tsure",
            "info\tNotes",
            "info\torg.example.notes\textra",
            "remove\torg.example.notes\tme\tall",
            "developer-key\tRWnope",
            "uninstall\torg.example.notes",
            "run\tid\tNotes",
            "run\tcommand\t-rf",
            "run\tcommand\ta/b",
            "run\tprogram\tnotes",
        ] {
            assert!(Request::read(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn answers_read_back() {
        let listed = Listed {
            place: Place::User(1000),
            id: "org.example.notes".into(),
            version: "0.1.0".into(),
            key: key(),
            when: 1_793_000_000,
            name: "Notes".into(),
        };
        let answers = [
            Answer::Sheet("Notes 0.1.0, for yourself\n  Signed by key RW...\n".into()),
            Answer::Question,
            Answer::Listed(listed),
            Answer::Found(Found {
                place: Place::Everyone,
                id: "org.example.notes".into(),
                key: key(),
                list: "ab".repeat(32),
            }),
            Answer::Ok("installed org.example.notes 0.1.0".into()),
            Answer::Error("it is not signed".into()),
        ];
        let bytes: Vec<u8> = answers.iter().flat_map(Answer::bytes).collect();
        let mut input = io::Cursor::new(bytes);
        for answer in &answers {
            assert_eq!(Answer::read(&mut input).unwrap().as_ref(), Some(answer));
        }
        assert_eq!(Answer::read(&mut input).unwrap(), None);
        // What could draw on the terminal never arrives.
        let sneaky = Answer::Sheet("a\u{1b}[2Jb\n".into()).bytes();
        let read = Answer::read(&mut io::Cursor::new(sneaky)).unwrap();
        assert_eq!(read, Some(Answer::Sheet("a[2Jb\n".into())));
        let sneaky = Answer::Error("x\ny\u{7}".into()).bytes();
        assert_eq!(sneaky, b"error x y \n");
        for bad in [
            &b"sheet 5\nab"[..],
            b"sheet x\n",
            b"maybe\n",
            b"ok fine",
            b"sheet 99999999\n",
            b"found\teveryone\torg.example.notes\tRWnope\tab\n",
        ] {
            assert!(Answer::read(&mut io::Cursor::new(bad.to_vec())).is_err(), "{bad:?}");
        }
    }
}
