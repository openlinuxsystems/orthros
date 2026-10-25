// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! column(1): lines put in columns, as util-linux's column does. Without
//! `-t` it fills the width of the terminal with the lines as entries,
//! column by column (or row by row, `-x`), at tab stops; with `-t` it
//! splits each line into cells and lines them up as a table, which can be
//! named (`-N`), reordered, hidden, aligned right, fitted to the width,
//! shown as a tree (`-r`), or written as JSON (`-J`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, BufWriter, Read, Write};

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail, usage, width};

/// Tab stops are this far apart.
const TAB: usize = 8;

/// What the options ask of a table.
#[derive(Default)]
struct Table {
    separators: Option<Vec<char>>,
    output: String,
    names: Vec<String>,
    right: Vec<String>,
    hide: Vec<String>,
    order: Vec<String>,
    truncate: Vec<String>,
    wrap: Vec<String>,
    no_headings: bool,
    limit: Option<usize>,
    keep_empty: bool,
    json: bool,
    name: Option<String>,
    tree: Option<String>,
    tree_id: Option<String>,
    tree_parent: Option<String>,
}

/// column(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut width: Option<usize> = None;
    let mut table_mode = false;
    let mut fill_rows = false;
    let mut t = Table { output: "  ".into(), ..Table::default() };
    let mut files: Vec<OsString> = Vec::new();
    // Lists leave out empty items, as util-linux's do: `-N A,,B` names two.
    let list = |text: String| {
        text.split(',').filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>()
    };
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('c') => width = Some(args.number()?),
            Arg::Long(l) if l == "output-width" || l == "columns" => width = Some(args.number()?),
            Arg::Short('t') => table_mode = true,
            Arg::Long(l) if l == "table" => table_mode = true,
            Arg::Short('x') => fill_rows = true,
            Arg::Long(l) if l == "fillrows" => fill_rows = true,
            Arg::Short('s') => t.separators = Some(args.text()?.chars().collect()),
            Arg::Long(l) if l == "separator" => t.separators = Some(args.text()?.chars().collect()),
            Arg::Short('o') => t.output = args.text()?,
            Arg::Long(l) if l == "output-separator" => t.output = args.text()?,
            Arg::Short('N') => t.names = list(args.text()?),
            Arg::Long(l) if l == "table-columns" => t.names = list(args.text()?),
            Arg::Short('R') => t.right.extend(list(args.text()?)),
            Arg::Long(l) if l == "table-right" => t.right.extend(list(args.text()?)),
            Arg::Short('H') => t.hide.extend(list(args.text()?)),
            Arg::Long(l) if l == "table-hide" => t.hide.extend(list(args.text()?)),
            Arg::Short('O') => t.order = list(args.text()?),
            Arg::Long(l) if l == "table-order" => t.order = list(args.text()?),
            Arg::Short('T') => t.truncate.extend(list(args.text()?)),
            Arg::Long(l) if l == "table-truncate" => t.truncate.extend(list(args.text()?)),
            Arg::Short('W') => t.wrap.extend(list(args.text()?)),
            Arg::Long(l) if l == "table-wrap" => t.wrap.extend(list(args.text()?)),
            Arg::Short('d') => t.no_headings = true,
            Arg::Long(l) if l == "table-noheadings" => t.no_headings = true,
            Arg::Short('l') => t.limit = Some(args.number()?),
            Arg::Long(l) if l == "table-columns-limit" => t.limit = Some(args.number()?),
            Arg::Short('L') => t.keep_empty = true,
            Arg::Long(l) if l == "keep-empty-lines" => t.keep_empty = true,
            Arg::Short('J') => (t.json, table_mode) = (true, true),
            Arg::Long(l) if l == "json" => (t.json, table_mode) = (true, true),
            Arg::Short('n') => t.name = Some(args.text()?),
            Arg::Long(l) if l == "table-name" => t.name = Some(args.text()?),
            Arg::Short('r') => (t.tree, table_mode) = (Some(args.text()?), true),
            Arg::Long(l) if l == "tree" => (t.tree, table_mode) = (Some(args.text()?), true),
            Arg::Short('i') => t.tree_id = Some(args.text()?),
            Arg::Long(l) if l == "tree-id" => t.tree_id = Some(args.text()?),
            Arg::Short('p') => t.tree_parent = Some(args.text()?),
            Arg::Long(l) if l == "tree-parent" => t.tree_parent = Some(args.text()?),
            // Repeating the header on each page, and which columns may
            // be narrowed past their widest cell: neither changes the
            // output here.
            Arg::Short('e') => {}
            Arg::Long(l) if l == "table-header-repeat" => {}
            Arg::Short('E') => {
                let _ = args.text()?;
            }
            Arg::Long(l) if l == "table-noextreme" => {
                let _ = args.text()?;
            }
            Arg::Operand(file) => files.push(file),
            other => return Err(args.unknown(&other)),
        }
    }
    let width = width.unwrap_or_else(|| util::terminal_size(1).map_or(80, |(columns, _)| columns));
    let (text, mut failed) = read(&files);
    let lines: Vec<&str> = text.split_terminator('\n').collect();
    let mut out = BufWriter::new(io::stdout().lock());
    if table_mode {
        let code = table(&mut out, &lines, &t, width)?;
        failed |= code != 0;
    } else {
        let entries: Vec<&str> = lines.into_iter().filter(|l| !l.is_empty()).collect();
        fill(&mut out, &entries, width, fill_rows)?;
    }
    out.flush()?;
    Ok(i32::from(failed))
}

/// The input, from the files in turn or standard input, and whether a
/// file could not be read.
fn read(files: &[OsString]) -> (String, bool) {
    let mut bytes = Vec::new();
    let mut failed = false;
    if files.is_empty()
        && let Err(e) = io::stdin().lock().read_to_end(&mut bytes)
    {
        eprintln!("column: stdin: {}", util::strip_os_error(&e));
        failed = true;
    }
    for file in files {
        match std::fs::read(file) {
            Ok(data) => {
                bytes.extend_from_slice(&data);
                if !data.is_empty() && !data.ends_with(b"\n") {
                    bytes.push(b'\n');
                }
            }
            Err(e) => {
                eprintln!("column: {}: {}", file.to_string_lossy(), util::strip_os_error(&e));
                failed = true;
            }
        }
    }
    (String::from_utf8_lossy(&bytes).into_owned(), failed)
}

/// The entries in columns `width` wide at most, at tab stops, as BSD
/// column puts them: down the columns first, or across the rows.
fn fill(out: &mut dyn Write, entries: &[&str], width: usize, rows_first: bool) -> io::Result<()> {
    let longest = entries.iter().map(|e| util::width(e)).max().unwrap_or(0);
    if entries.is_empty() {
        return Ok(());
    }
    if longest >= width {
        for entry in entries {
            writeln!(out, "{entry}")?;
        }
        return Ok(());
    }
    let column = (longest + TAB) & !(TAB - 1);
    let columns = (width / column).max(1);
    // Tabs from `at` to the next column, which ends at `end`.
    let tabs = |out: &mut dyn Write, mut at: usize, end: usize| -> io::Result<()> {
        loop {
            let next = (at + TAB) & !(TAB - 1);
            if next > end {
                return Ok(());
            }
            out.write_all(b"\t")?;
            at = next;
        }
    };
    if rows_first {
        for row in entries.chunks(columns) {
            let mut at = 0;
            for (n, entry) in row.iter().enumerate() {
                out.write_all(entry.as_bytes())?;
                at += util::width(entry);
                if n + 1 < row.len() {
                    tabs(out, at, column * (n + 1))?;
                    at = column * (n + 1);
                }
            }
            writeln!(out)?;
        }
        return Ok(());
    }
    let rows = entries.len().div_ceil(columns);
    for row in 0..rows {
        let mut at = 0;
        let mut n = row;
        for col in 0..columns {
            let entry = entries[n];
            out.write_all(entry.as_bytes())?;
            at += util::width(entry);
            n += rows;
            if n >= entries.len() {
                break;
            }
            tabs(out, at, column * (col + 1))?;
            at = column * (col + 1);
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Splits a line into at most `limit` cells: at runs of blanks, or at
/// each of the separators given, the last cell keeping the rest.
fn split(line: &str, separators: Option<&[char]>, limit: Option<usize>) -> Vec<String> {
    let mut cells = Vec::new();
    match separators {
        None => {
            let mut rest = line.trim_start_matches([' ', '\t']);
            while !rest.is_empty() {
                if limit.is_some_and(|l| cells.len() + 1 >= l) {
                    cells.push(rest.trim_end_matches([' ', '\t']).to_string());
                    break;
                }
                let end = rest.find([' ', '\t']).unwrap_or(rest.len());
                cells.push(rest[..end].to_string());
                rest = rest[end..].trim_start_matches([' ', '\t']);
            }
        }
        Some(separators) => {
            let mut rest = line;
            loop {
                if limit.is_some_and(|l| cells.len() + 1 >= l) {
                    cells.push(rest.to_string());
                    break;
                }
                match rest.find(separators) {
                    Some(at) => {
                        cells.push(rest[..at].to_string());
                        let separator = rest[at..].chars().next().map_or(1, char::len_utf8);
                        rest = &rest[at + separator..];
                    }
                    None => {
                        cells.push(rest.to_string());
                        break;
                    }
                }
            }
        }
    }
    cells
}

/// A column named or numbered (from 1) in `spec`.
fn find_column(spec: &str, names: &[String], columns: usize) -> Result<Option<usize>, Fail> {
    if let Ok(n) = spec.parse::<usize>() {
        return Ok((1..=columns).contains(&n).then(|| n - 1));
    }
    if spec.starts_with('-') && spec[1..].parse::<usize>().is_ok() {
        return Ok(None);
    }
    names
        .iter()
        .position(|n| n == spec)
        .or_else(|| names.iter().position(|n| n.eq_ignore_ascii_case(spec)))
        .map(Some)
        .ok_or_else(|| fail(format!("undefined column name '{spec}'")))
}

fn columns_in(specs: &[String], names: &[String], columns: usize) -> Result<Vec<usize>, Fail> {
    let mut found = Vec::new();
    for spec in specs {
        if let Some(n) = find_column(spec, names, columns)? {
            found.push(n);
        }
    }
    Ok(found)
}

/// A row of the table, and the rows under it in a tree.
struct Row {
    cells: Vec<String>,
    children: Vec<usize>,
}

/// Writes the table; returns the exit status.
fn table(out: &mut dyn Write, lines: &[&str], t: &Table, term_width: usize) -> Result<i32, Fail> {
    let mut rows: Vec<Row> = Vec::new();
    for line in lines {
        if line.is_empty() && !t.keep_empty {
            continue;
        }
        rows.push(Row {
            cells: split(line, t.separators.as_deref(), t.limit),
            children: Vec::new(),
        });
    }
    if rows.is_empty() {
        return Ok(0);
    }
    let columns = rows.iter().map(|r| r.cells.len()).max().unwrap_or(0).max(t.names.len());
    // The rows in the order they are shown, each with its depth and the
    // tree's lines before it.
    let mut shown: Vec<(usize, String)> = (0..rows.len()).map(|n| (n, String::new())).collect();
    let mut roots: Vec<usize> = (0..rows.len()).collect();
    let tree_column = match &t.tree {
        Some(spec) => {
            let (Some(id), Some(parent)) = (&t.tree_id, &t.tree_parent) else {
                return Err(usage(
                    "options --tree-id and --tree-parent are required for tree formatting".into(),
                ));
            };
            let tree = find_column(spec, &t.names, columns)?;
            let id = find_column(id, &t.names, columns)?;
            let parent = find_column(parent, &t.names, columns)?;
            if let (Some(id), Some(parent)) = (id, parent) {
                let ids: HashMap<String, usize> = rows
                    .iter()
                    .enumerate()
                    .rev()
                    .filter_map(|(n, r)| {
                        r.cells.get(id).filter(|c| !c.is_empty()).map(|c| (c.clone(), n))
                    })
                    .collect();
                roots.clear();
                for n in 0..rows.len() {
                    let up = rows[n]
                        .cells
                        .get(parent)
                        .and_then(|p| ids.get(p))
                        .copied()
                        .filter(|&p| p != n);
                    match up {
                        Some(p) => rows[p].children.push(n),
                        None => roots.push(n),
                    }
                }
                shown.clear();
                let mut seen = vec![false; rows.len()];
                for &root in &roots {
                    walk(&rows, root, "", None, &mut seen, &mut shown);
                }
                // Rows in a cycle have no root: show them as roots.
                for n in 0..rows.len() {
                    if !seen[n] {
                        roots.push(n);
                        walk(&rows, n, "", None, &mut seen, &mut shown);
                    }
                }
            }
            tree
        }
        None => None,
    };
    // `-` hides every column without a name.
    let named: Vec<String> = t.hide.iter().filter(|h| *h != "-").cloned().collect();
    let mut hidden = columns_in(&named, &t.names, columns)?;
    if t.hide.iter().any(|h| h == "-") {
        hidden.extend((0..columns).filter(|&n| t.names.get(n).is_none_or(String::is_empty)));
    }
    let mut order = columns_in(&t.order, &t.names, columns)?;
    for n in 0..columns {
        if !order.contains(&n) {
            order.push(n);
        }
    }
    order.retain(|n| !hidden.contains(n));
    let right = columns_in(&t.right, &t.names, columns)?;
    let truncate = columns_in(&t.truncate, &t.names, columns)?;
    let wrap = columns_in(&t.wrap, &t.names, columns)?;
    let cell = |row: usize, column: usize, prefix: &str| -> String {
        let text = rows[row].cells.get(column).map_or("", String::as_str);
        if tree_column == Some(column) { format!("{prefix}{text}") } else { text.to_string() }
    };
    if t.json {
        for &n in &order {
            if t.names.get(n).is_none_or(String::is_empty) {
                // As util-linux's column says it: one past the row where
                // the column first has a cell.
                let line = rows.iter().position(|r| r.cells.len() > n).unwrap_or(rows.len()) + 2;
                return Err(fail(format!(
                    "line {line}: for JSON the name of the column {} is required",
                    n + 1
                )));
            }
        }
        let name = t.name.as_deref().unwrap_or("table");
        writeln!(out, "{{\n   \"{}\": [", json_escape(name))?;
        let keys: Vec<String> =
            order.iter().map(|&n| json_escape(&t.names[n].to_lowercase())).collect();
        if tree_column.is_some() && t.tree_id.is_some() {
            write_json_rows(out, &rows, &roots, &order, &keys, 1)?;
        } else {
            let all: Vec<usize> = (0..rows.len()).collect();
            let flat: Vec<Row> =
                rows.iter().map(|r| Row { cells: r.cells.clone(), children: Vec::new() }).collect();
            write_json_rows(out, &flat, &all, &order, &keys, 1)?;
        }
        writeln!(out, "   ]\n}}")?;
        return Ok(0);
    }
    let header = !t.names.is_empty() && !t.no_headings;
    // The names count, as libsmartcols counts them, even unshown.
    let mut widths: Vec<usize> = vec![0; columns];
    for (n, name) in t.names.iter().enumerate() {
        widths[n] = width(name);
    }
    for (row, prefix) in &shown {
        for &n in &order {
            widths[n] = widths[n].max(width(&cell(*row, n, prefix)));
        }
    }
    // Narrow the columns that may be truncated or wrapped until the table
    // fits, widest first, but not below their names.
    let separator = width(&t.output);
    let total = |widths: &[usize]| {
        order.iter().map(|&n| widths[n]).sum::<usize>() + separator * order.len().saturating_sub(1)
    };
    let narrowable: Vec<usize> =
        order.iter().copied().filter(|n| truncate.contains(n) || wrap.contains(n)).collect();
    while total(&widths) > term_width {
        let least = |n: usize| t.names.get(n).map_or(1, |name| width(name).max(1));
        let Some(&widest) =
            narrowable.iter().filter(|&&n| widths[n] > least(n)).max_by_key(|&&n| widths[n])
        else {
            break;
        };
        widths[widest] -= 1;
    }
    let print = |out: &mut dyn Write, cells: Vec<String>| -> io::Result<()> {
        // A wrapped cell takes as many lines as it needs.
        let pieces: Vec<Vec<String>> = order
            .iter()
            .zip(&cells)
            .map(|(&n, text)| {
                if wrap.contains(&n) && width(text) > widths[n] {
                    chunks(text, widths[n])
                } else if truncate.contains(&n) && width(text) > widths[n] {
                    vec![cut(text, widths[n])]
                } else {
                    vec![text.clone()]
                }
            })
            .collect();
        let lines = pieces.iter().map(Vec::len).max().unwrap_or(1);
        for line in 0..lines {
            let mut text = String::new();
            for (i, (&n, piece)) in order.iter().zip(&pieces).enumerate() {
                if i > 0 {
                    text.push_str(&t.output);
                }
                let value = piece.get(line).map_or("", String::as_str);
                let pad = widths[n].saturating_sub(width(value));
                if right.contains(&n) {
                    text.extend(std::iter::repeat_n(' ', pad));
                    text.push_str(value);
                } else {
                    text.push_str(value);
                    if i + 1 < order.len() {
                        text.extend(std::iter::repeat_n(' ', pad));
                    }
                }
            }
            writeln!(out, "{text}")?;
        }
        Ok(())
    };
    if header {
        print(out, order.iter().map(|&n| t.names.get(n).cloned().unwrap_or_default()).collect())?;
    }
    for (row, prefix) in &shown {
        print(out, order.iter().map(|&n| cell(*row, n, prefix)).collect())?;
    }
    Ok(0)
}

/// Adds `row` and the rows under it to `shown`, with the tree's lines:
/// `last` says whether the row is the last of its parent's, or is a root.
fn walk(
    rows: &[Row],
    row: usize,
    lines: &str,
    last: Option<bool>,
    seen: &mut [bool],
    shown: &mut Vec<(usize, String)>,
) {
    if seen[row] {
        return;
    }
    seen[row] = true;
    let prefix = match last {
        None => String::new(),
        Some(true) => format!("{lines}└─"),
        Some(false) => format!("{lines}├─"),
    };
    shown.push((row, prefix));
    let below = match last {
        None => String::new(),
        Some(true) => format!("{lines}  "),
        Some(false) => format!("{lines}│ "),
    };
    let children = &rows[row].children;
    for (i, &child) in children.iter().enumerate() {
        walk(rows, child, &below, Some(i + 1 == children.len()), seen, shown);
    }
}

/// Writes rows as JSON objects, with the rows under each as its
/// `children`.
fn write_json_rows(
    out: &mut dyn Write,
    rows: &[Row],
    list: &[usize],
    order: &[usize],
    keys: &[String],
    depth: usize,
) -> io::Result<()> {
    let indent = "   ".repeat(depth + 1);
    for (i, &row) in list.iter().enumerate() {
        if i == 0 {
            writeln!(out, "{indent}{{")?;
        }
        let children = &rows[row].children;
        for (k, (&n, key)) in order.iter().zip(keys).enumerate() {
            let value = match rows[row].cells.get(n).filter(|c| !c.is_empty()) {
                Some(text) => format!("\"{}\"", json_escape(text)),
                None => "null".into(),
            };
            let comma = if k + 1 < order.len() || !children.is_empty() { "," } else { "" };
            writeln!(out, "{indent}   \"{key}\": {value}{comma}")?;
        }
        if !children.is_empty() {
            writeln!(out, "{indent}   \"children\": [")?;
            write_json_rows(out, rows, children, order, keys, depth + 2)?;
            writeln!(out, "{indent}   ]")?;
        }
        if i + 1 < list.len() {
            writeln!(out, "{indent}}},{{")?;
        } else {
            writeln!(out, "{indent}}}")?;
        }
    }
    Ok(())
}

fn json_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

/// `text` cut to `columns` columns.
fn cut(text: &str, columns: usize) -> String {
    let mut used = 0;
    let mut kept = String::new();
    for c in text.chars() {
        let w = width(c.encode_utf8(&mut [0; 4]));
        if used + w > columns {
            break;
        }
        used += w;
        kept.push(c);
    }
    kept
}

/// `text` in pieces of `columns` columns.
fn chunks(text: &str, columns: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let piece = cut(rest, columns.max(1));
        let piece =
            if piece.is_empty() { rest.chars().next().map_or_default(String::from) } else { piece };
        rest = &rest[piece.len()..];
        pieces.push(piece);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(entries: &[&str], width: usize, rows_first: bool) -> String {
        let mut out = Vec::new();
        fill(&mut out, entries, width, rows_first).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn tabled(text: &str, t: &Table) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let mut out = Vec::new();
        table(&mut out, &lines, t, 80).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn fills_at_tab_stops() {
        let numbers: Vec<String> = (1..=23).map(|n| n.to_string()).collect();
        let numbers: Vec<&str> = numbers.iter().map(String::as_str).collect();
        assert!(filled(&numbers, 30, false).starts_with("1\t9\t17\n2\t10\t18\n"));
        assert!(filled(&numbers, 30, true).starts_with("1\t2\t3\n4\t5\t6\n"));
        assert_eq!(
            filled(&["alpha", "beta", "gamma  ", "delta epsilon", "zeta\teta"], 40, false)
                .lines()
                .next(),
            Some("alpha\t\tdelta epsilon")
        );
        assert_eq!(filled(&["alpha", "delta epsilon"], 10, false), "alpha\ndelta epsilon\n");
    }

    #[test]
    fn tables() {
        let text = "name size kind\nfoo 12 file\nx 7 link extra field\n";
        let t = Table { output: "  ".into(), ..Table::default() };
        assert_eq!(
            tabled(text, &t),
            "name  size  kind         \nfoo   12    file         \nx     7     link  extra  field\n"
        );
        let t = Table {
            output: "  ".into(),
            names: vec!["A".into(), "B".into(), "C".into()],
            right: vec!["B".into()],
            order: vec!["C".into()],
            hide: vec!["4".into(), "5".into()],
            ..Table::default()
        };
        assert_eq!(
            tabled(text, &t),
            "C     A        B\nkind  name  size\nfile  foo     12\nlink  x        7\n"
        );
        let t = Table { output: "  ".into(), separators: Some(vec![':']), ..Table::default() };
        assert_eq!(tabled("a:b:c\n1::3\n", &t), "a  b  c\n1     3\n");
        let t = Table { output: "  ".into(), limit: Some(2), ..Table::default() };
        assert_eq!(tabled("x 7 link  extra\n", &t), "x  7 link  extra\n");
    }

    #[test]
    fn trees_and_json() {
        let t = Table {
            output: "  ".into(),
            names: vec!["ID".into(), "PARENT".into(), "NAME".into()],
            tree: Some("NAME".into()),
            tree_id: Some("ID".into()),
            tree_parent: Some("PARENT".into()),
            ..Table::default()
        };
        assert_eq!(
            tabled("1 0 root\n2 1 child\n3 1 kid2\n4 2 grandchild\n", &t),
            "ID  PARENT  NAME\n1   0       root\n2   1       ├─child\n4   2       │ └─grandchild\n3   1       └─kid2\n"
        );
        let t = Table {
            output: "  ".into(),
            names: vec!["A".into(), "B".into()],
            json: true,
            ..Table::default()
        };
        assert_eq!(
            tabled("a\nb \"c\"\n", &t),
            "{\n   \"table\": [\n      {\n         \"a\": \"a\",\n         \"b\": null\n      },{\n         \"a\": \"b\",\n         \"b\": \"\\\"c\\\"\"\n      }\n   ]\n}\n"
        );
        // What util-linux 2.39.3's column says of a column with no name.
        let t = Table { names: vec!["one".into()], json: true, ..Table::default() };
        let error = |text: &str| {
            let lines: Vec<&str> = text.lines().collect();
            table(&mut Vec::new(), &lines, &t, 80).err().map(|e| e.message)
        };
        let message =
            |line| Some(format!("line {line}: for JSON the name of the column 2 is required"));
        assert_eq!(error("a b\nc d e\nf\n"), message(2));
        assert_eq!(error("a\nb\nc d\n"), message(4));
        assert_eq!(error("x\n\n\nz w q\n"), message(3));
    }
}
