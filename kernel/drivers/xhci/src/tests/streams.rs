// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Streams, alternate settings and device resets, against a simulated USB
//! Attached SCSI device as QEMU's `usb-uas` is at SuperSpeed: its status,
//! data-in and data-out pipes with 16 streams each, behind a Bulk-Only
//! setting 0.

use super::*;
use crate::ring::{MAX_STREAMS, STREAM_RING_SIZE};

/// The device context indexes of its UAS setting's pipes: command, status,
/// data-in and data-out.
const COMMAND: u8 = 2;
const STATUS: u8 = 5;
const DATA_IN: u8 = 7;
const DATA_OUT: u8 = 8;
/// Those with streams.
const STREAMED: [u8; 3] = [STATUS, DATA_IN, DATA_OUT];

/// `device` on root port 1 (USB 3), attached and configured with its
/// alternate setting `alternate`'s endpoints.
fn configured_uas(sim: &Arc<Sim>, device: SimDevice, alternate: u8) -> (Xhci, TestHost, Device) {
    let endpoints = setting_of(&device, alternate);
    sim.state().devices[0] = Some(device);
    let (mut xhci, mut host) = start(sim);
    let mut device = attach(&mut xhci, &mut host, 1).unwrap();
    let mut below = Vec::new();
    device.configure(&mut xhci, &mut host, &mut alloc(&mut below), 1, &endpoints).unwrap();
    host.others.clear();
    (xhci, host, device)
}

/// The UAS device configured in its UAS setting, its pipes given streams.
fn streamed(sim: &Arc<Sim>) -> (Xhci, TestHost, Device) {
    let (mut xhci, mut host, mut device) = configured_uas(sim, uas(), 1);
    let mut below = Vec::new();
    let count =
        device.alloc_streams(&mut xhci, &mut host, &mut alloc(&mut below), &STREAMED, 256).unwrap();
    assert_eq!(count, 16);
    (xhci, host, device)
}

/// What the simulated controller's last `n` commands were, last first.
fn last_commands(sim: &Arc<Sim>, n: usize) -> Vec<u32> {
    sim.state().commands.iter().rev().take(n).map(|&(kind, _)| kind).collect()
}

#[test]
fn streams_are_as_many_as_the_endpoints_the_controller_and_the_driver_take() {
    let sim = Sim::new(State::new());
    let (_xhci, _host, device) = streamed(&sim);
    let slot = device.slot();
    {
        let s = sim.state();
        let entry = &s.slots[&slot];
        for index in STREAMED {
            // 16 streams and stream 0: 32 entries, a linear array.
            let streams = entry.endpoints[&index].streams.as_ref().unwrap();
            assert_eq!(streams.entries, 32, "endpoint {index}");
            for stream in 1..32u64 {
                let context = State::read64(streams.array + 16 * stream);
                if stream <= 16 {
                    // A Primary Transfer Ring at its stream's 2 KiB, its
                    // cycle state set.
                    let ring = streams.array + stream * STREAM_RING_SIZE as u64;
                    assert_eq!(context, ring | 1 << 1 | 1, "stream {stream}");
                } else {
                    assert_eq!(context, 0, "stream {stream}");
                }
            }
            assert_eq!(device.streams(index), 16);
        }
        // The command pipe has none.
        assert!(entry.endpoints[&COMMAND].streams.is_none());
        assert_eq!(device.streams(COMMAND), 0);
        // Dropped and added again together.
        assert_eq!(s.restarted, STREAMED.map(|index| (slot, index)));
        assert_eq!(s.commands.last().unwrap().0, kind::CONFIGURE_ENDPOINT);
    }
    // Fewer if asked for fewer; the controller's array may be the limit,
    // and the driver's.
    for (psa, wanted, streams, got) in
        [(7, 8, 4, 8), (4, 256, 4, 16), (4, 256, 6, 31), (7, 256, 6, MAX_STREAMS)]
    {
        let state = State { max_psa_size: psa, ..State::new() };
        let sim = Sim::new(state);
        let mut device = uas();
        // The streams each of its pipes says it has, a power of two.
        let companions: Vec<usize> = (0..device.config.len())
            .filter(|&at| device.config[at..].starts_with(&[6, 0x30]))
            .collect();
        for at in companions {
            if device.config[at + 3] != 0 {
                device.config[at + 3] = streams;
            }
        }
        let (mut xhci, mut host, mut device) = configured_uas(&sim, device, 1);
        let mut below = Vec::new();
        let count =
            device.alloc_streams(&mut xhci, &mut host, &mut alloc(&mut below), &STREAMED, wanted);
        assert_eq!(count, Ok(got), "MaxPSASize {psa}, {wanted} wanted, 2^{streams}");
    }
}

#[test]
fn streams_are_refused_where_they_cannot_be() {
    // A controller whose arrays are too small for Linux, as for Radix.
    let sim = Sim::new(State { max_psa_size: 3, ..State::new() });
    let (mut xhci, mut host, mut device) = configured_uas(&sim, uas(), 1);
    let mut below = Vec::new();
    let mut ask = |device: &mut Device, indexes: &[u8], below: &mut Vec<bool>| {
        device.alloc_streams(&mut xhci, &mut host, &mut alloc(below), indexes, 16)
    };
    assert!(matches!(ask(&mut device, &STREAMED, &mut below), Err(Error::Unsupported(_))));
    let sim = Sim::new(State::new());
    let (mut xhci, mut host, mut device) = configured_uas(&sim, uas(), 1);
    let mut ask = |device: &mut Device, indexes: &[u8], below: &mut Vec<bool>| {
        device.alloc_streams(&mut xhci, &mut host, &mut alloc(below), indexes, 16)
    };
    // The command pipe has none; endpoints it was not configured with,
    // none at all, too many, or one twice.
    assert!(matches!(ask(&mut device, &[COMMAND], &mut below), Err(Error::Unsupported(_))));
    for bad in [&[9][..], &[], &[STATUS, DATA_IN, DATA_OUT, COMMAND], &[STATUS, STATUS]] {
        assert_eq!(ask(&mut device, bad, &mut below), Err(Error::BadRequest), "{bad:?}");
    }
    // Twice.
    assert_eq!(ask(&mut device, &STREAMED, &mut below), Ok(16));
    assert_eq!(ask(&mut device, &[STATUS], &mut below), Err(Error::BadRequest));
    // An endpoint with streams is not recovered as one without.
    assert_eq!(device.clear_halt(&mut xhci, &mut host, STATUS), Err(Error::BadRequest));
    assert_eq!(device.recover(&mut xhci, &mut host, STATUS, code::STALL), Err(Error::BadRequest));
}

#[test]
fn each_stream_carries_its_own_transfers() {
    let sim = Sim::new(State::new());
    let (mut xhci, mut host, mut device) = streamed(&sim);
    let slot = device.slot();
    let (memory, frames) = buffers(2, 4096);
    let first = device.submit_stream(&xhci, DATA_IN, 1, &frames[..1]).unwrap();
    let second = device.submit_stream(&xhci, DATA_IN, 2, &frames[1..]).unwrap();
    // Each on its stream's ring, the doorbell naming the stream.
    let streams = sim.state().slots[&slot].endpoints[&DATA_IN].streams.clone().unwrap();
    assert_eq!(first.first, streams.array + STREAM_RING_SIZE as u64);
    assert_eq!(second.first, streams.array + 2 * STREAM_RING_SIZE as u64);
    assert_eq!(sim.state().doorbells[..], [(slot, DATA_IN, 1), (slot, DATA_IN, 2)]);
    assert_eq!(transfer(&mut xhci, &mut host), None);
    // The second's data comes first.
    sim.state().stream_report(1, 0, (DATA_IN, 2), &[2; 4096]);
    assert_eq!(
        transfer(&mut xhci, &mut host),
        Some(Event::Transfer {
            trb: second.last,
            code: code::SUCCESS,
            slot,
            endpoint: DATA_IN,
            residual: 0
        })
    );
    sim.state().stream_report(1, 0, (DATA_IN, 1), &[1; 100]);
    let Some(Event::Transfer { trb, code, residual, .. }) = transfer(&mut xhci, &mut host) else {
        panic!("no event");
    };
    assert_eq!((code, first.moved(&frames[..1], trb, residual)), (code::SHORT_PACKET, Some(100)));
    let mut came = [0u8; 4096];
    memory.read(0, &mut came[..100]);
    assert_eq!(came[..100], [1; 100]);
    memory.read(4096, &mut came);
    assert_eq!(came, [2; 4096]);
    // Out, on stream 16.
    memory.write(0, &[9; 4096]);
    let out = device.submit_stream(&xhci, DATA_OUT, 16, &frames[..1]).unwrap();
    assert_eq!(
        transfer(&mut xhci, &mut host),
        Some(Event::Transfer {
            trb: out.last,
            code: code::SUCCESS,
            slot,
            endpoint: DATA_OUT,
            residual: 0
        })
    );
    assert_eq!(
        sim.state().devices[0].as_ref().unwrap().received_on,
        [(DATA_OUT, 16, std::vec![9; 4096])]
    );
    // A stream's ring takes a TD of all but its Link TRB: after one of a
    // TRB, through the Link TRB.
    let (_memory, frames) = buffers(MAX_STREAM_TD_BUFFERS, 512);
    for (round, count) in [1, MAX_STREAM_TD_BUFFERS].into_iter().enumerate() {
        // The data waits for the TD, so the simulator reads it once.
        let report = std::vec![round as u8; 512 * count];
        sim.state().stream_report(1, 0, (DATA_IN, 3), &report);
        let td = device.submit_stream(&xhci, DATA_IN, 3, &frames[..count]).unwrap();
        assert_eq!(td.position(td.last), Some(count - 1));
        assert_eq!(round == 1, td.last < td.first, "round {round}");
        let event = transfer(&mut xhci, &mut host);
        assert!(
            matches!(event, Some(Event::Transfer { trb, code: code::SUCCESS, .. }) if trb == td.last),
            "round {round}: {event:?}"
        );
    }
    assert_eq!(sim.state().links_in_tds, [true]);
    // No more than that, no stream 0 or past the last, and none on an
    // endpoint without streams.
    let (_memory, frames) = buffers(MAX_STREAM_TD_BUFFERS + 1, 512);
    assert_eq!(device.submit_stream(&xhci, DATA_IN, 4, &frames), Err(Error::BadRequest));
    assert_eq!(device.submit_stream(&xhci, DATA_IN, 17, &frames[..1]), Err(Error::BadRequest));
    assert_eq!(device.submit_stream(&xhci, COMMAND, 1, &frames[..1]), Err(Error::BadRequest));
}

#[test]
fn a_stream_cancelled_skips_its_td_while_the_others_go_on() {
    let sim = Sim::new(State::new());
    let (mut xhci, mut host, mut device) = streamed(&sim);
    let slot = device.slot();
    let (memory, frames) = buffers(3, 512);
    let cancelled = device.submit_stream(&xhci, DATA_IN, 1, &frames[..1]).unwrap();
    let other = device.submit_stream(&xhci, DATA_IN, 2, &frames[1..2]).unwrap();
    sim.state().doorbells.clear();
    device.cancel_stream(&mut xhci, &mut host, DATA_IN, 1, &|stream| stream == 2).unwrap();
    assert_eq!(last_commands(&sim, 2), [kind::SET_TR_DEQUEUE, kind::STOP_ENDPOINT]);
    {
        let s = sim.state();
        // Past the cancelled TD, on its stream; the other's doorbell rung
        // again.
        assert_eq!(s.stream_dequeues[..], [(slot, DATA_IN, 1, cancelled.last + 16)]);
        assert_eq!(s.doorbells[..], [(slot, DATA_IN, 2)]);
    }
    // Both said they stopped.
    let stopped: Vec<u64> = host
        .others
        .drain(..)
        .filter_map(|event| match event {
            Event::Transfer { trb, code: code::STOPPED, .. } => Some(trb),
            _ => None,
        })
        .collect();
    assert_eq!(stopped.len(), 2);
    assert!(stopped.contains(&cancelled.first) && stopped.contains(&other.first));
    // The other goes on; the stream cancelled takes its next TD, and its
    // data goes there, not to the TD taken back.
    sim.state().stream_report(1, 0, (DATA_IN, 2), &[2; 512]);
    assert!(matches!(
        transfer(&mut xhci, &mut host),
        Some(Event::Transfer { trb, code: code::SUCCESS, .. }) if trb == other.last
    ));
    let next = device.submit_stream(&xhci, DATA_IN, 1, &frames[2..]).unwrap();
    sim.state().stream_report(1, 0, (DATA_IN, 1), &[3; 512]);
    assert!(matches!(
        transfer(&mut xhci, &mut host),
        Some(Event::Transfer { trb, code: code::SUCCESS, .. }) if trb == next.last
    ));
    let mut came = [0u8; 512];
    memory.read(0, &mut came);
    assert_eq!(came, [0; 512]);
    memory.read(2 * 4096, &mut came);
    assert_eq!(came, [3; 512]);
    // Only a stream it has, of an endpoint with streams.
    let never = |_: usize| false;
    assert_eq!(
        device.cancel_stream(&mut xhci, &mut host, DATA_IN, 17, &never),
        Err(Error::BadRequest)
    );
    assert_eq!(
        device.cancel_stream(&mut xhci, &mut host, COMMAND, 1, &never),
        Err(Error::BadRequest)
    );
}

#[test]
fn an_alternate_setting_brings_its_own_endpoints() {
    let sim = Sim::new(State::new());
    let (mut xhci, mut host, mut device) = configured_uas(&sim, uas(), 0);
    let slot = device.slot();
    let uas_setting = setting_of(&sim.state().devices[0].clone().unwrap(), 1);
    let bot_setting = setting_of(&sim.state().devices[0].clone().unwrap(), 0);
    assert_eq!(sim.state().slots[&slot].endpoints.len(), 2);
    let mut below = Vec::new();
    device
        .set_interface(&mut xhci, &mut host, &mut alloc(&mut below), (0, 1), &[3, 4], &uas_setting)
        .unwrap();
    {
        let s = sim.state();
        let entry = &s.slots[&slot];
        let mut indexes: Vec<u8> = entry.endpoints.keys().copied().collect();
        indexes.sort_unstable();
        assert_eq!(indexes, [COMMAND, STATUS, DATA_IN, DATA_OUT]);
        assert_eq!(entry.entries, DATA_OUT);
        // Setting 0's endpoints dropped as setting 1's were added.
        assert_eq!(s.restarted, [(slot, 3), (slot, 4)]);
        assert_eq!(s.requests.last().unwrap().1, Setup::set_interface(0, 1));
        assert_eq!(s.devices[0].as_ref().unwrap().alternate, 1);
    }
    let count = device.alloc_streams(&mut xhci, &mut host, &mut alloc(&mut below), &STREAMED, 16);
    assert_eq!(count, Ok(16));
    // And back, the streams going with their endpoints.
    device
        .set_interface(
            &mut xhci,
            &mut host,
            &mut alloc(&mut below),
            (0, 0),
            &[COMMAND, STATUS, DATA_IN, DATA_OUT],
            &bot_setting,
        )
        .unwrap();
    assert_eq!(device.streams(STATUS), 0);
    {
        let s = sim.state();
        let mut indexes: Vec<u8> = s.slots[&slot].endpoints.keys().copied().collect();
        indexes.sort_unstable();
        assert_eq!(indexes, [3, 4]);
        assert_eq!(s.slots[&slot].entries, 4);
        assert_eq!(s.devices[0].as_ref().unwrap().alternate, 0);
    }
    let (_memory, frames) = buffers(1, 512);
    let td = device.submit_frames(&xhci, 4, &frames).unwrap();
    assert!(matches!(
        transfer(&mut xhci, &mut host),
        Some(Event::Transfer { trb, code: code::SUCCESS, .. }) if trb == td.last
    ));
    // An endpoint it does not have, one it has kept, or too many; nothing
    // changes then.
    let mut set = |drop: &[u8], add: &[radix_usb::EndpointDescriptor]| {
        device.set_interface(&mut xhci, &mut host, &mut alloc(&mut below), (0, 1), drop, add)
    };
    assert_eq!(set(&[9], &uas_setting), Err(Error::BadRequest));
    assert_eq!(set(&[], &bot_setting), Err(Error::BadRequest));
    let many: Vec<radix_usb::EndpointDescriptor> = (1..=MAX_ENDPOINTS as u8 + 1)
        .map(|number| radix_usb::EndpointDescriptor {
            address: number,
            attributes: 2,
            max_packet_raw: 512,
            interval: 0,
            max_burst: 0,
            companion_attributes: 0,
        })
        .collect();
    assert_eq!(set(&[3, 4], &many), Err(Error::BadRequest));
    assert_eq!(sim.state().slots[&slot].endpoints.len(), 2);
}

#[test]
fn a_device_whose_port_was_reset_comes_back_as_it_was() {
    let sim = Sim::new(State::new());
    let (mut xhci, mut host, mut device) = streamed(&sim);
    let slot = device.slot();
    let (memory, frames) = buffers(1, 512);
    // A transfer waits when the device is reset: it is gone with the
    // endpoint.
    let lost = device.submit_stream(&xhci, DATA_IN, 1, &frames).unwrap();
    device.reset(&mut xhci, &mut host, 1).unwrap();
    assert_eq!(
        last_commands(&sim, 3),
        [kind::CONFIGURE_ENDPOINT, kind::ADDRESS_DEVICE, kind::RESET_DEVICE]
    );
    {
        let s = sim.state();
        let entry = &s.slots[&slot];
        assert!(entry.addressed);
        assert_eq!(entry.endpoints.len(), 4);
        assert_eq!(entry.entries, DATA_OUT);
        for index in STREAMED {
            assert_eq!(entry.endpoints[&index].streams.as_ref().unwrap().entries, 32);
        }
        assert_eq!(s.requests.last().unwrap().1, Setup::set_configuration(1));
    }
    // Each ring starts over, empty.
    memory.write(0, &[0; 512]);
    let td = device.submit_stream(&xhci, DATA_IN, 1, &frames).unwrap();
    assert_eq!(td.first, lost.first);
    sim.state().stream_report(1, 0, (DATA_IN, 1), &[5; 512]);
    assert!(matches!(
        transfer(&mut xhci, &mut host),
        Some(Event::Transfer { trb, code: code::SUCCESS, .. }) if trb == td.last
    ));
    let mut came = [0u8; 512];
    memory.read(0, &mut came);
    assert_eq!(came, [5; 512]);
    let td = device.submit_frames(&xhci, COMMAND, &frames).unwrap();
    assert!(matches!(
        transfer(&mut xhci, &mut host),
        Some(Event::Transfer { trb, code: code::SUCCESS, .. }) if trb == td.last
    ));
    let mut bytes = [0u8; 18];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(device.control_in(&mut xhci, &mut host, setup, &mut bytes), Ok(18));
}
