// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use core::cell::Cell;

use super::*;

/// A kernel stand-in that records the last call and answers with fixed
/// registers.
struct Mock {
    last: Cell<(u64, [u64; 10])>,
    status: u64,
    results: [u64; 10],
}

impl Syscalls for Mock {
    fn syscall(&self, number: u64, args: [u64; 10]) -> (u64, [u64; 10]) {
        self.last.set((number, args));
        (self.status, self.results)
    }
}

fn mock(status: u64, results: [u64; 10]) -> Mock {
    Mock { last: Cell::new((0, [0; 10])), status, results }
}

#[test]
fn methods_go_through_cap_call_with_their_label() {
    let sys = mock(0, [0; 10]);
    memory::retype(&sys, 3, capability::ObjectKind::Frame, 2, 40, 4).expect("retype");
    let (number, args) = sys.last.get();
    assert_eq!(number, syscalls::CAP_CALL);
    assert_eq!(args[0], 3, "the capability");
    assert_eq!(args[1], u64::from(memory::RETYPE) | (4 << endpoint::INFO_WORDS_SHIFT));
    assert_eq!(&args[2..6], &[1, 2, 40, 4]);
}

#[test]
fn results_come_from_the_data_words() {
    let sys = mock(0, [0, 2, 7, 3, 0, 0, 0, 0, 0, 0]);
    let info = memory::info(&sys, 3).expect("info");
    assert_eq!((info.pages, info.used), (7, 3));
}

#[test]
fn errors_keep_their_status() {
    let status = errors::Kernel::MissingRights.status(capability::Rights::WRITE.bits() as u32);
    let sys = mock(status.0, [0; 10]);
    let error = memory::info(&sys, 3).unwrap_err();
    assert_eq!(errors::Kernel::from_status(error), Some(errors::Kernel::MissingRights));
    assert_eq!(error.detail(), 2);
    assert_eq!(errors::Kernel::MissingRights.errno(), 13);
}

#[test]
fn decoding_checks_every_argument() {
    let mut words =
        memory::RetypeArgs { kind: capability::ObjectKind::Frame, order: 0, to: 1, count: 1 }
            .encode();
    assert!(memory::RetypeArgs::decode(&words).is_ok());
    words[2] = 1 << 32;
    assert_eq!(memory::RetypeArgs::decode(&words), Err(Invalid(2)), "a slot is 32 bits");
    words[2] = 1;
    words[0] = 200;
    assert_eq!(memory::RetypeArgs::decode(&words), Err(Invalid(0)), "no kind 200");
    let mut args = [0u64; 10];
    args[1] = 1 << 5;
    assert_eq!(syscalls::WaitArgs::decode(&args), Err(Invalid(1)), "an unknown flag");
}

#[test]
fn the_status_word_packs_domain_code_and_detail() {
    let status = Status::new(1, 8, 3);
    assert_eq!(status.0, (3 << 32) | (1 << 16) | 8);
    assert_eq!((status.domain(), status.code(), status.detail()), (1, 8, 3));
    assert_eq!(Status::from(Invalid(3)), errors::Kernel::InvalidArgument.status(3));
    for error in errors::Kernel::ALL {
        assert_eq!(errors::Kernel::from_status(error.status(u32::MAX)), Some(error));
        assert!(error.errno() > 0);
    }
}

#[test]
fn layouts_match_their_definitions() {
    assert_eq!(ring::Sqe::SIZE, 64);
    assert_eq!(ring::Cqe::SIZE, 32);
    assert_eq!(ring::RingIndexes::SIZE, 64);
    assert_eq!(ring::RingHeader::OFFSET_CQ, 64);
    assert_eq!(startup::Startup::SIZE, 96);
    assert_eq!(startup::Startup::OFFSET_IMAGE_BIAS, 64);
    assert_eq!(startup::Startup::OFFSET_BOOTFS_SIZE, 88);
}

#[test]
fn there_are_twelve_system_calls() {
    let names: [&str; 12] = core::array::from_fn(|n| syscalls::name(n as u64).unwrap_or(""));
    assert!(names.iter().all(|name| !name.is_empty()));
    assert_eq!(syscalls::name(12), None);
}
