// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the rings and the line discipline.

use proptest::prelude::*;
use radix_abi::signal::{SIGINT, SIGTSTP};
use radix_abi::terminal::{ECHO, ICANON, ISIG, VLNEXT, VMIN, VTIME};

use crate::{Ldisc, Received, Ring, Settings, TERMIOS_SIZE};

/// Types `bytes`, collecting the echo and the signals.
fn typed(ldisc: &mut Ldisc<[u8; 64]>, bytes: &[u8]) -> (Vec<u8>, Vec<u32>) {
    let mut echo = Vec::new();
    let mut signals = Vec::new();
    for &byte in bytes {
        if let Received::Signal(signal) =
            ldisc.receive(byte, &mut |out| echo.extend_from_slice(out))
        {
            signals.push(signal);
        }
    }
    (echo, signals)
}

/// The settings with ICANON off, VMIN and VTIME as given.
fn raw(ldisc: &Ldisc<[u8; 64]>, min: u8, time: u8) -> Settings {
    let mut raw = ldisc.settings;
    raw.lflag &= !(ICANON | ECHO);
    raw.cc[VMIN as usize] = min;
    raw.cc[VTIME as usize] = time;
    raw
}

fn read_all(ldisc: &mut Ldisc<[u8; 64]>) -> Vec<u8> {
    let mut out = Vec::new();
    ldisc
        .read_with::<()>(1024, &mut |run| {
            out.extend_from_slice(run);
            Ok(())
        })
        .unwrap();
    out
}

#[test]
fn rings_wrap_and_keep_order() {
    let mut ring = Ring::new([0u8; 8]);
    assert_eq!(ring.push(b"abcdef"), 6);
    let mut out = [0u8; 4];
    assert_eq!(ring.pop(&mut out), 4);
    assert_eq!(&out, b"abcd");
    assert_eq!(ring.push(b"ghijklmn"), 6, "only what fits");
    let mut rest = [0u8; 16];
    assert_eq!(ring.pop(&mut rest), 8);
    assert_eq!(&rest[..8], b"efghijkl");
    assert!(ring.is_empty());
    // A failing sink leaves its run in the ring.
    ring.push(b"xy");
    assert_eq!(ring.read_with(2, &mut |_| Err::<(), _>(7)), Err(7));
    assert_eq!(ring.len(), 2);
    assert_eq!(ring.pop_back(), Some(b'y'));
}

#[test]
fn canonical_input_is_edited_a_line_at_a_time() {
    let mut ldisc = Ldisc::new([0u8; 64]);
    let (echo, _) = typed(&mut ldisc, b"helo\x7flo wrold\x17world\r");
    assert_eq!(read_all(&mut ldisc), b"hello world\n");
    // Erasing echoes backspace, space, backspace; CR became NL, echoed as
    // CR NL.
    assert!(echo.ends_with(b"world\r\n"));
    assert!(echo.windows(3).any(|w| w == b"\x08 \x08"));
    // Nothing until the line is complete.
    typed(&mut ldisc, b"abc");
    assert!(!ldisc.ready());
    assert_eq!(ldisc.readable(), None);
    // Ctrl-U kills it; Ctrl-D ends a line without a NL, and on an empty
    // line reads as the end of the file.
    typed(&mut ldisc, b"\x15xyz\x04\x04");
    assert_eq!(read_all(&mut ldisc), b"xyz");
    assert!(ldisc.ready());
    assert_eq!(read_all(&mut ldisc), b"");
    assert!(!ldisc.ready());
}

#[test]
fn a_read_takes_one_line_and_a_short_read_leaves_the_rest() {
    let mut ldisc = Ldisc::new([0u8; 64]);
    typed(&mut ldisc, b"one\ntwo\n");
    let mut out = Vec::new();
    ldisc
        .read_with::<()>(2, &mut |run| {
            out.extend_from_slice(run);
            Ok(())
        })
        .unwrap();
    assert_eq!(out, b"on");
    assert_eq!(read_all(&mut ldisc), b"e\n");
    assert_eq!(read_all(&mut ldisc), b"two\n");
    assert_eq!(ldisc.pending(), 0);
}

#[test]
fn signal_characters_send_signals_and_drop_input() {
    let mut ldisc = Ldisc::new([0u8; 64]);
    let (echo, signals) = typed(&mut ldisc, b"partial\x03");
    assert_eq!(signals, [SIGINT]);
    assert!(echo.ends_with(b"^C"));
    typed(&mut ldisc, b"\n");
    assert_eq!(read_all(&mut ldisc), b"\n", "the partial line went");
    let (_, signals) = typed(&mut ldisc, b"\x1a");
    assert_eq!(signals, [SIGTSTP]);
    // Without ISIG they are plain bytes.
    let mut raw = ldisc.settings;
    raw.lflag &= !(ISIG | ICANON | ECHO);
    ldisc.set(raw);
    let (echo, signals) = typed(&mut ldisc, b"\x03q");
    assert!(signals.is_empty() && echo.is_empty());
    assert_eq!(read_all(&mut ldisc), b"\x03q");
}

#[test]
fn non_canonical_reads_wait_for_vmin() {
    let mut ldisc = Ldisc::new([0u8; 64]);
    let mut raw = ldisc.settings;
    raw.lflag &= !ICANON;
    raw.cc[VMIN as usize] = 3;
    ldisc.set(raw);
    typed(&mut ldisc, b"ab");
    assert!(!ldisc.ready());
    typed(&mut ldisc, b"c");
    assert!(ldisc.ready());
    assert_eq!(read_all(&mut ldisc), b"abc");
}

#[test]
fn poll_wants_vmin_bytes_only_without_vtime() {
    // As Linux's input_available_p: with VTIME a read returns at its
    // timer, so a byte makes the terminal readable.
    let mut ldisc = Ldisc::new([0u8; 64]);
    ldisc.set(raw(&ldisc, 3, 5));
    typed(&mut ldisc, b"a");
    assert!(ldisc.ready());
    ldisc.set(raw(&ldisc, 3, 0));
    assert!(!ldisc.ready());
    ldisc.set(raw(&ldisc, 0, 0));
    assert!(ldisc.ready());
}

#[test]
fn raw_input_past_its_room_waits_for_a_reader() {
    let mut ldisc = Ldisc::new([0u8; 64]);
    ldisc.set(raw(&ldisc, 1, 0));
    for i in 0..64u8 {
        assert_eq!(ldisc.receive(b'a' + i % 26, &mut |_| {}), Received::Taken);
    }
    assert!(!ldisc.has_room());
    assert_eq!(ldisc.receive(b'z', &mut |_| {}), Received::Full);
    assert_eq!(ldisc.pending(), 64);
    let mut first = [0u8; 4];
    let mut at = 0;
    ldisc
        .read_with::<()>(4, &mut |run| {
            first[at..at + run.len()].copy_from_slice(run);
            at += run.len();
            Ok(())
        })
        .unwrap();
    assert_eq!(&first, b"abcd");
    assert!(ldisc.has_room());
    assert_eq!(ldisc.receive(b'z', &mut |_| {}), Received::Taken);
    assert_eq!(ldisc.pending(), 61);
}

#[test]
fn canonical_input_past_its_room_waits_only_behind_a_line() {
    let mut ldisc = Ldisc::new([0u8; 64]);
    // A line of 62 and its end fill the input: the next waits for it to
    // be read, but ^C needs no room.
    typed(&mut ldisc, &[b'x'; 62]);
    typed(&mut ldisc, b"\n");
    assert!(!ldisc.has_room());
    assert_eq!(ldisc.receive(b'y', &mut |_| {}), Received::Full);
    assert_eq!(ldisc.receive(0x03, &mut |_| {}), Received::Signal(SIGINT));
    // The interrupt flushed the input: there is room again.
    assert!(ldisc.has_room() && ldisc.pending() == 0);
    // A line being edited that fills the input, with nothing to read
    // before it, drops what is typed past it: nothing could make room.
    typed(&mut ldisc, &[b'x'; 63]);
    assert!(ldisc.has_room());
    assert_eq!(ldisc.receive(b'y', &mut |_| {}), Received::Dropped);
    assert_eq!(ldisc.receive(b'\n', &mut |_| {}), Received::Taken);
    assert_eq!(read_all(&mut ldisc).len(), 64);
}

#[test]
fn entering_canonical_mode_makes_typeahead_a_line() {
    let mut ldisc = Ldisc::new([0u8; 64]);
    let canonical = ldisc.settings;
    ldisc.set(raw(&ldisc, 1, 0));
    typed(&mut ldisc, b"ls");
    ldisc.set(canonical);
    assert!(ldisc.ready());
    assert_eq!(read_all(&mut ldisc), b"ls");
    // A pending LNEXT is forgotten across the change.
    typed(&mut ldisc, &[canonical.cc[VLNEXT as usize]]);
    ldisc.set(raw(&ldisc, 1, 0));
    ldisc.set(canonical);
    let (_, signals) = typed(&mut ldisc, b"\x03");
    assert_eq!(signals, [SIGINT]);
}

#[test]
fn output_turns_nl_into_cr_nl_within_its_room() {
    let mut ldisc = Ldisc::new([0u8; 64]);
    let mut out = Vec::new();
    assert_eq!(ldisc.output(b"a\nb", 10, &mut |run| out.extend_from_slice(run)), 3);
    assert_eq!(out, b"a\r\nb");
    out.clear();
    // Room for "a" and not for CR NL: the NL waits.
    assert_eq!(ldisc.output(b"a\nb", 2, &mut |run| out.extend_from_slice(run)), 1);
    assert_eq!(out, b"a");
}

#[test]
fn settings_round_trip_through_struct_termios() {
    let settings = Settings::new();
    let bytes = settings.to_bytes();
    assert_eq!(bytes.len(), TERMIOS_SIZE);
    assert_eq!(TERMIOS_SIZE, 60);
    assert_eq!(Settings::from_bytes(&bytes), settings);
}

proptest! {
    #[test]
    fn rings_are_fifos(ops in proptest::collection::vec((any::<bool>(), 0usize..20), 1..100)) {
        let mut ring = Ring::new([0u8; 16]);
        let mut model = std::collections::VecDeque::new();
        let mut next = 0u8;
        for (push, n) in ops {
            if push {
                let bytes: Vec<u8> = (0..n).map(|_| { next = next.wrapping_add(1); next }).collect();
                let done = ring.push(&bytes);
                prop_assert_eq!(done, n.min(16 - model.len()));
                model.extend(&bytes[..done]);
            } else {
                let mut out = vec![0u8; n];
                let done = ring.pop(&mut out);
                let expected: Vec<u8> = model.drain(..n.min(model.len())).collect();
                prop_assert_eq!(&out[..done], &expected[..]);
            }
            prop_assert_eq!(ring.len(), model.len());
        }
    }

    #[test]
    fn typing_never_panics_and_reads_add_up(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        let mut ldisc = Ldisc::new([0u8; 64]);
        for byte in bytes {
            ldisc.receive(byte, &mut |_| {});
            prop_assert!(ldisc.pending() <= 64);
            if ldisc.ready() {
                read_all(&mut ldisc);
            }
        }
    }
}
