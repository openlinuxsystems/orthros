// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the index's changes ([`crate::tree`]) and the record
//! edits under them, on a store in memory.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::edit;
use crate::index::{self, ROOT_HEADER};
use crate::record::{self, STRIDE, u16_at, u32_at, u64_at};
use crate::tree::{self, Store};
use crate::upcase::Upcase;

/// ASCII's up-case table, as `mkntfs` writes its first 128 units.
fn ascii() -> Upcase {
    Upcase::load(256, |at, buf| {
        for (i, pair) in buf.chunks_mut(2).enumerate() {
            let unit = (at / 2) as u16 + i as u16;
            let upper = if (unit as u8).is_ascii_lowercase() { unit - 32 } else { unit };
            pair.copy_from_slice(&upper.to_le_bytes());
        }
        Ok(())
    })
    .unwrap()
}

/// Index blocks in memory, with a root that fits in `root_max` bytes.
struct MemStore {
    blocks: BTreeMap<u64, Vec<u8>>,
    free: BTreeSet<u64>,
    next: u64,
    size: usize,
    root_max: usize,
}

impl MemStore {
    fn new(size: usize, root_max: usize) -> MemStore {
        MemStore { blocks: BTreeMap::new(), free: BTreeSet::new(), next: 0, size, root_max }
    }
}

impl Store for MemStore {
    fn read(&mut self, vcn: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.blocks.get(&vcn).ok_or(Error::Corrupt)?;
        assert!(!self.free.contains(&vcn), "block {vcn} read while free");
        let mut on_disk = b.clone();
        record::fix_up(&mut on_disk, b"INDX")?;
        buf.copy_from_slice(&on_disk);
        Ok(())
    }

    fn write(&mut self, vcn: u64, buf: &[u8]) -> Result<()> {
        assert!(!self.free.contains(&vcn), "block {vcn} written while free");
        let mut on_disk = buf.to_vec();
        edit::fix_down(&mut on_disk)?;
        self.blocks.insert(vcn, on_disk);
        Ok(())
    }

    fn alloc(&mut self) -> Result<u64> {
        if let Some(&vcn) = self.free.iter().next() {
            self.free.remove(&vcn);
            return Ok(vcn);
        }
        let vcn = self.next;
        self.next += 1;
        Ok(vcn)
    }

    fn free(&mut self, vcn: u64) -> Result<()> {
        assert!(self.free.insert(vcn), "block {vcn} freed twice");
        Ok(())
    }

    fn root_fits(&self, len: usize) -> bool {
        len <= self.root_max
    }

    fn block_size(&self) -> usize {
        self.size
    }
}

/// A fresh root's value: `$FILE_NAME`s collated as names, blocks of
/// `size` bytes, no entries but the last.
fn fresh_root(size: usize) -> KVec<u8> {
    let mut r = vec![0u8; 0x30];
    r[0..4].copy_from_slice(&0x30u32.to_le_bytes());
    r[4..8].copy_from_slice(&1u32.to_le_bytes());
    r[8..12].copy_from_slice(&(size as u32).to_le_bytes());
    r[12] = 1;
    let h = ROOT_HEADER;
    r[h..h + 4].copy_from_slice(&0x10u32.to_le_bytes());
    r[h + 4..h + 8].copy_from_slice(&0x20u32.to_le_bytes());
    r[h + 8..h + 12].copy_from_slice(&0x20u32.to_le_bytes());
    // The last entry.
    r[0x20 + 8] = 0x10;
    r[0x20 + 0x0c] = 2;
    mem::copy(&r).unwrap()
}

/// A `$FILE_NAME` value for `name` in directory 5.
fn key(name: &str) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut k = vec![0u8; 0x42 + 2 * units.len()];
    k[0..8].copy_from_slice(&5u64.to_le_bytes());
    k[0x40] = units.len() as u8;
    k[0x41] = index::namespace::WIN32;
    for (i, u) in units.iter().enumerate() {
        k[0x42 + 2 * i..0x44 + 2 * i].copy_from_slice(&u.to_le_bytes());
    }
    k
}

fn reference(n: u64) -> u64 {
    (1u64 << 48) | (n + 100)
}

/// Walks the tree in order: its names, checking that every node is in
/// order, every entry of a node above has a subtree and every block but
/// the root has a name or a child, and that the blocks in use are the
/// store's.
fn walk(root: &[u8], store: &mut MemStore, upcase: &Upcase) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = BTreeSet::new();
    visit(root, ROOT_HEADER, store, &mut names, &mut seen, 0, true);
    let in_use: BTreeSet<u64> =
        store.blocks.keys().copied().filter(|v| !store.free.contains(v)).collect();
    assert_eq!(seen, in_use, "blocks reached are not the blocks in use");
    for pair in names.windows(2) {
        let a: Vec<u16> = pair[0].encode_utf16().collect();
        let order = tree::collate(upcase, &a, pair[1].encode_utf16());
        assert_eq!(order, std::cmp::Ordering::Less, "{} before {}", pair[0], pair[1]);
    }
    names
}

fn visit(
    node: &[u8],
    header: usize,
    store: &mut MemStore,
    names: &mut Vec<String>,
    seen: &mut BTreeSet<u64>,
    depth: usize,
    is_root: bool,
) {
    assert!(depth < 32, "too deep");
    let has_children = node[header + 12] & 1 != 0;
    let mut count = 0;
    let mut any_child = false;
    for entry in index::entries(node, header) {
        let entry = entry.unwrap();
        assert_eq!(entry.subnode().is_some(), has_children, "children not as the header says");
        if let Some(vcn) = entry.subnode() {
            any_child = true;
            assert!(seen.insert(vcn), "block {vcn} reached twice");
            let mut buf = vec![0u8; store.size];
            store.read(vcn, &mut buf).unwrap();
            assert_eq!(u64_at(&buf, 0x10), vcn);
            visit(&buf, index::BLOCK_HEADER, store, names, seen, depth + 1, false);
        }
        if entry.is_last() {
            break;
        }
        count += 1;
        let name = entry.name().unwrap();
        names.push(String::from_utf16(&name.units().collect::<Vec<_>>()).unwrap());
    }
    if !is_root {
        assert!(count > 0 || any_child, "an empty block");
        assert!(count > 0, "a block with no names");
    }
}

/// Puts each of `names` in, then takes out those `out` says, checking the
/// tree after each.
fn run(names: &[String], out: &[bool], size: usize, root_max: usize) {
    let upcase = ascii();
    let mut store = MemStore::new(size, root_max);
    let mut root = fresh_root(size);
    let mut have = BTreeMap::new();
    for (i, name) in names.iter().enumerate() {
        // The index tells names apart by case; a directory refuses one
        // the same but for case before it gets here.
        let upper = name.clone();
        let e = tree::make_entry(reference(i as u64), &key(name), None).unwrap();
        let result = tree::insert(&mut root, &mut store, &upcase, &e);
        if have.contains_key(&upper) {
            assert_eq!(result.unwrap_err(), Error::Exists);
            continue;
        }
        result.unwrap();
        have.insert(upper, (name.clone(), i as u64));
        let listed = walk(&root, &mut store, &upcase);
        assert_eq!(listed.len(), have.len());
        if i % 16 == 0 {
            assert_eq!(walked(&root, &mut store, &upcase, None, usize::MAX), listed);
            let mid = &listed[listed.len() / 2];
            let units: Vec<u16> = mid.encode_utf16().collect();
            let after = walked(&root, &mut store, &upcase, Some(&units), usize::MAX);
            assert_eq!(after, listed[listed.len() / 2 + 1..]);
        }
    }
    let mut left: BTreeMap<String, (String, u64)> = have.clone();
    for (n, (upper, (name, i))) in have.iter().enumerate() {
        if !out.get(n).copied().unwrap_or(false) {
            continue;
        }
        let units: Vec<u16> = name.encode_utf16().collect();
        tree::remove(&mut root, &mut store, &upcase, &units, reference(*i)).unwrap();
        left.remove(upper);
        let listed = walk(&root, &mut store, &upcase);
        let want: Vec<String> = left.values().map(|(n, _)| n.clone()).collect();
        assert_eq!(listed.len(), want.len());
        let found: BTreeSet<String> = listed.iter().cloned().collect();
        let expect: BTreeSet<String> = left.keys().cloned().collect();
        assert_eq!(found, expect);
    }
}

/// The names [`tree::walk`] gives after `after`, `max` of them at most.
fn walked(
    root: &[u8],
    store: &mut MemStore,
    upcase: &Upcase,
    after: Option<&[u16]>,
    max: usize,
) -> Vec<String> {
    let mut names = Vec::new();
    tree::walk(root, store, upcase, after, &mut |e| {
        let len = usize::from(u16_at(e, 0x0a));
        let name = index::FileName::new(&e[0x10..0x10 + len]).unwrap();
        names.push(String::from_utf16(&name.units().collect::<Vec<u16>>()).unwrap());
        Ok(names.len() < max)
    })
    .unwrap();
    names
}

/// A listing goes on after the last name it gave, though the names it
/// gave have gone meanwhile, as `rm -r` takes them: each name comes once.
#[test]
fn listings_go_on_after_names_taken_out() {
    let upcase = ascii();
    let mut store = MemStore::new(1024, 400);
    let mut root = fresh_root(1024);
    let names: Vec<String> = (0..700).map(|i| format!("entry-{:05}", (i * 389) % 700)).collect();
    for (i, name) in names.iter().enumerate() {
        let e = tree::make_entry(reference(i as u64), &key(name), None).unwrap();
        tree::insert(&mut root, &mut store, &upcase, &e).unwrap();
    }
    let number: BTreeMap<&String, u64> = names.iter().zip(0..).collect();
    let mut seen = Vec::new();
    let mut last: Option<Vec<u16>> = None;
    loop {
        let batch = walked(&root, &mut store, &upcase, last.as_deref(), 37);
        if batch.is_empty() {
            break;
        }
        for name in &batch {
            let units: Vec<u16> = name.encode_utf16().collect();
            tree::remove(&mut root, &mut store, &upcase, &units, reference(number[name])).unwrap();
        }
        last = Some(batch.last().unwrap().encode_utf16().collect());
        seen.extend(batch);
    }
    let mut want = names.clone();
    want.sort();
    assert_eq!(seen, want);
    assert!(walk(&root, &mut store, &upcase).is_empty());
}

/// A key changes in place, wherever its entry is.
#[test]
fn keys_change_in_place() {
    let upcase = ascii();
    let mut store = MemStore::new(1024, 400);
    let mut root = fresh_root(1024);
    let names: Vec<String> = (0..200).map(|i| format!("k{i:03}")).collect();
    for (i, name) in names.iter().enumerate() {
        let e = tree::make_entry(reference(i as u64), &key(name), None).unwrap();
        tree::insert(&mut root, &mut store, &upcase, &e).unwrap();
    }
    for (i, name) in names.iter().enumerate() {
        let units: Vec<u16> = name.encode_utf16().collect();
        tree::update(&mut root, &mut store, &upcase, &units, reference(i as u64), &mut |k| {
            k[0x30..0x38].copy_from_slice(&(i as u64 * 10).to_le_bytes());
        })
        .unwrap();
    }
    let mut sizes = Vec::new();
    tree::walk(&root, &mut store, &upcase, None, &mut |e| {
        sizes.push(u64_at(e, 0x10 + 0x30));
        Ok(true)
    })
    .unwrap();
    assert_eq!(sizes, (0..200).map(|i| i * 10).collect::<Vec<u64>>());
    let units: Vec<u16> = "k001".encode_utf16().collect();
    let wrong = tree::update(&mut root, &mut store, &upcase, &units, reference(7), &mut |_| {});
    assert_eq!(wrong.unwrap_err(), Error::NotFound);
    let units: Vec<u16> = "nope".encode_utf16().collect();
    let missing = tree::update(&mut root, &mut store, &upcase, &units, reference(1), &mut |_| {});
    assert_eq!(missing.unwrap_err(), Error::NotFound);
}

#[test]
fn names_go_in_and_out_in_order() {
    let names: Vec<String> =
        (0..600).map(|i| format!("file-{:04}-{}", (i * 7919) % 600, "x".repeat(i % 40))).collect();
    let out: Vec<bool> = (0..600).map(|i| i % 3 != 1).collect();
    run(&names, &out, 1024, 480);
    run(&names, &out, 4096, 900);
}

#[test]
fn every_name_can_go() {
    let names: Vec<String> = (0..300).map(|i| format!("n{}", (i * 37) % 300)).collect();
    run(&names, &vec![true; 300], 1024, 300);
}

#[test]
fn long_names_split_blocks() {
    let names: Vec<String> = (0..80).map(|i| format!("{:03}{}", i, "long".repeat(60))).collect();
    let out: Vec<bool> = (0..80).map(|i| i % 2 == 0).collect();
    run(&names, &out, 4096, 1200);
}

#[test]
fn update_sequences_go_on_and_come_off() {
    let mut b = vec![0u8; 1024];
    edit::format_record(&mut b, 42, 7, record::flags::IN_USE);
    b[510] = 0xab;
    b[1022] = 0xcd;
    let before = b.clone();
    edit::fix_down(&mut b).unwrap();
    assert_eq!(u16_at(&b, 510), u16_at(&b, edit::RECORD_USA));
    assert_ne!(b[510], 0xab);
    record::fix_up(&mut b, b"FILE").unwrap();
    // The number went up; the rest is as it was.
    assert_eq!(&b[..edit::RECORD_USA], &before[..edit::RECORD_USA]);
    // Past the array, which keeps what the strides ended in.
    assert_eq!(&b[edit::RECORD_USA + 6..], &before[edit::RECORD_USA + 6..]);
    // A torn write is told.
    edit::fix_down(&mut b).unwrap();
    b[1022] ^= 1;
    assert_eq!(record::fix_up(&mut b, b"FILE").unwrap_err(), Error::Corrupt);
    let _ = STRIDE;
}

#[test]
fn attributes_go_in_order() {
    let mut b = vec![0u8; 1024];
    edit::format_record(&mut b, 30, 1, record::flags::IN_USE);
    let data = edit::resident(record::kind::DATA, &[], 0, b"hello", false).unwrap();
    let si = edit::resident(record::kind::STANDARD_INFORMATION, &[], 1, &[0; 0x48], false).unwrap();
    let named = edit::resident(record::kind::DATA, &[b'a', 0], 2, b"stream", false).unwrap();
    edit::insert(&mut b, &data).unwrap();
    edit::insert(&mut b, &named).unwrap();
    edit::insert(&mut b, &si).unwrap();
    let rec = record::Record::new(&b).unwrap();
    let kinds: Vec<(u32, u16)> =
        rec.attrs().map(|a| a.unwrap()).map(|a| (a.kind(), a.id())).collect();
    assert_eq!(kinds, [(0x10, 1), (0x80, 0), (0x80, 2)]);
    let at = edit::find(&b, record::kind::DATA, &[]).unwrap();
    edit::set_value(&mut b, at, &[7u8; 300]).unwrap();
    let rec = record::Record::new(&b).unwrap();
    let values: Vec<usize> = rec.attrs().map(|a| a.unwrap().value().len()).collect();
    assert_eq!(values, [0x48, 300, 6]);
    edit::remove(&mut b, at);
    let rec = record::Record::new(&b).unwrap();
    assert_eq!(rec.attrs().count(), 2);
    let si_at = edit::find(&b, 0x10, &[]).unwrap();
    assert_eq!(edit::set_value(&mut b, si_at, &[0; 2000]), Err(Error::NoSpace));
    let _ = u32_at;
}

#[test]
fn run_lists_pack_as_they_read() {
    let mut runs = crate::runs::Runs::new();
    runs.append(0x1234, 10).unwrap();
    runs.append(crate::runs::HOLE, 5).unwrap();
    runs.append(0x10, 300).unwrap();
    runs.append(0x10 + 300, 2).unwrap();
    runs.append(0x7f_0000, 1).unwrap();
    let packed = runs.pack(0, runs.clusters()).unwrap();
    let mut back = crate::runs::Runs::new();
    back.add(&packed, 0, runs.clusters() - 1, 1 << 40).unwrap();
    assert_eq!(back.as_slice(), runs.as_slice());
    // A piece from the middle.
    let piece = runs.pack(12, 100).unwrap();
    let mut middle = crate::runs::Runs::new();
    middle.append(crate::runs::HOLE, 12).unwrap();
    let mut tail = crate::runs::Runs::new();
    tail.add(&piece, 0, 87, 1 << 40).unwrap();
    assert_eq!(tail.clusters(), 88);
    let freed = runs.truncate(14).unwrap();
    assert_eq!(runs.clusters(), 14);
    assert_eq!(freed.iter().map(|r| r.len).sum::<u64>(), 303);
    // A length's top bit is its sign, as for an offset: 137 clusters take
    // two bytes.
    let mut runs = crate::runs::Runs::new();
    runs.append(0x10, 137).unwrap();
    assert_eq!(&runs.pack(0, 137).unwrap()[..], &[0x12, 0x89, 0x00, 0x10, 0x00][..]);
}

proptest! {
    #[test]
    fn any_names_in_and_out(
        names in proptest::collection::vec("[a-zA-Z0-9]{1,40}", 1..250),
        out in proptest::collection::vec(any::<bool>(), 250),
    ) {
        run(&names, &out, 1024, 400);
    }
}
