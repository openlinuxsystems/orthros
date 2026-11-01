// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What bundle(1) and bundled(8) say to each other on bundled's socket
//! (ADRs 0223, 0224 and 0225). A request is one line, tab-separated; an
//! `install` request comes with the archive's descriptor (SCM_RIGHTS).
//! bundled answers with lines too: the install sheet (`sheet N` and N bytes
//! of text), a question for a yes or a no, a listed bundle, the bundle to
//! run, what the asker said of its permissions, and last `ok` or `error`
//! with why. A `watch` connection then stays open, for the permissions
//! the person takes away while the app runs.

use std::io::{self, BufRead, Read};

use super::grants::{self, Grant, Said};
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
    /// Asks the person, through the trusted prompt, whether app `id`,
    /// whose signed list's SHA-256 is `list`, may have `permission`, as
    /// its manifest asks for it (ADR 0224).
    Ask { id: String, permission: String, list: String },
    /// Gives app `id` `permission`, with `words`, or as it asks for it
    /// with none, after the trusted prompt's yes.
    Grant { id: String, permission: String, words: Vec<String> },
    /// Takes `permission` away from app `id`, at once.
    Revoke { id: String, permission: String },
    /// Keeps the connection, to say each permission the person takes
    /// away from app `id` while it runs.
    Watch { id: String },
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

fn checked_permission(name: &str) -> Result<String, String> {
    manifest::bundle::permissions::find(name)
        .map(|p| p.name.to_string())
        .ok_or_else(|| format!("{name:?} is no permission of the list"))
}

/// A list's SHA-256, as 64 hex digits, in lowercase.
fn checked_list(list: &str) -> Result<String, String> {
    let hex = list.len() == 64 && list.bytes().all(|b| b.is_ascii_hexdigit());
    hex.then(|| list.to_ascii_lowercase()).ok_or_else(|| "not a list's SHA-256".into())
}

/// Words, separated by commas, `-` for none.
fn word_list(words: &[String]) -> String {
    if words.is_empty() { "-".into() } else { words.join(",") }
}

/// Words a request or an answer carries: each of a permission's words
/// as [`grants::check_word`] takes them.
fn read_words(permission: &str, field: &str) -> Result<Vec<String>, String> {
    if field == "-" {
        return Ok(Vec::new());
    }
    let found = manifest::bundle::permissions::find(permission)
        .ok_or_else(|| format!("{permission:?} is no permission of the list"))?;
    let words: Vec<String> = field.split(',').map(String::from).collect();
    for word in &words {
        grants::check_word(found, word)?;
    }
    Ok(words)
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
            Request::Ask { id, permission, list } => format!("ask\t{id}\t{permission}\t{list}\n"),
            Request::Grant { id, permission, words } => {
                format!("grant\t{id}\t{permission}\t{}\n", word_list(words))
            }
            Request::Revoke { id, permission } => format!("revoke\t{id}\t{permission}\n"),
            Request::Watch { id } => format!("watch\t{id}\n"),
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
            ["ask", id, permission, list] => Ok(Request::Ask {
                id: checked_id(id)?,
                permission: checked_permission(permission)?,
                list: checked_list(list)?,
            }),
            ["grant", id, permission, words] => {
                let permission = checked_permission(permission)?;
                let words = read_words(&permission, words)?;
                Ok(Request::Grant { id: checked_id(id)?, permission, words })
            }
            ["revoke", id, permission] => Ok(Request::Revoke {
                id: checked_id(id)?,
                permission: checked_permission(permission)?,
            }),
            ["watch", id] => Ok(Request::Watch { id: checked_id(id)? }),
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
    /// What the asker said of one of its permissions.
    Granted(Grant),
    /// A permission the person took away, on a `watch` connection.
    Revoked(String),
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
            Answer::Granted(g) => format!(
                "granted\t{}\t{}\t{}\t{}\t{}\n",
                g.permission,
                g.said.word(),
                g.since,
                word_list(&g.given),
                word_list(&g.asked)
            )
            .into_bytes(),
            Answer::Revoked(permission) => format!("revoked\t{permission}\n").into_bytes(),
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
            return Ok(Some(Answer::Found(Found {
                place: place(at)?,
                id: checked_id(id).map_err(|why| bad(&why))?,
                key: PublicKey::decode(key).map_err(|why| bad(&why))?,
                list: checked_list(list).map_err(|_| bad("a found bundle's list"))?,
            })));
        }
        if let ["granted", permission, said, since, given, asked] = words.as_slice() {
            let permission = checked_permission(permission).map_err(|why| bad(&why))?;
            return Ok(Some(Answer::Granted(Grant {
                said: Said::from_word(said).ok_or_else(|| bad("neither allow nor never"))?,
                since: since.parse().map_err(|_| bad("a grant's time"))?,
                given: read_words(&permission, given).map_err(|why| bad(&why))?,
                asked: read_words(&permission, asked).map_err(|why| bad(&why))?,
                permission,
            })));
        }
        if let ["revoked", permission] = words.as_slice() {
            let permission = checked_permission(permission).map_err(|why| bad(&why))?;
            return Ok(Some(Answer::Revoked(permission)));
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
            Request::Ask {
                id: "org.example.notes".into(),
                permission: "files.folder".into(),
                list: "ab".repeat(32),
            },
            Request::Grant {
                id: "org.example.notes".into(),
                permission: "files.folder".into(),
                words: vec!["documents=read".into(), "music=read-write".into()],
            },
            Request::Grant {
                id: "org.example.notes".into(),
                permission: "files.home".into(),
                words: Vec::new(),
            },
            Request::Revoke {
                id: "org.example.notes".into(),
                permission: "network.outbound".into(),
            },
            Request::Watch { id: "org.example.notes".into() },
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
            "ask\torg.example.notes\tfiles.folder\tab",
            "ask\torg.example.notes\tfiles.everything\t-",
            "grant\torg.example.notes\tfiles.folder\tsecrets=read",
            "grant\torg.example.notes\tfiles.home\tdocuments=read",
            "grant\torg.example.notes\tfiles.folder",
            "revoke\torg.example.notes",
            "watch\tNotes",
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
            Answer::Granted(Grant {
                permission: "files.folder".into(),
                said: Said::Allow,
                since: 1_793_000_000,
                given: vec!["documents=read".into()],
                asked: vec!["documents=read-write".into()],
            }),
            Answer::Granted(Grant {
                permission: "network.outbound".into(),
                said: Said::Never,
                since: 1,
                given: Vec::new(),
                asked: Vec::new(),
            }),
            Answer::Revoked("files.folder".into()),
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
            b"granted\tfiles.folder\tmaybe\t1\t-\t-\n",
            b"granted\tfiles.folder\tallow\t1\tsecrets=read\t-\n",
            b"revoked\tfiles.everything\n",
        ] {
            assert!(Answer::read(&mut io::Cursor::new(bad.to_vec())).is_err(), "{bad:?}");
        }
    }
}
