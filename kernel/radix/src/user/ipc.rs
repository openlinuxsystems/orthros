// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Endpoints: synchronous messages between threads, after the L4 family
//! and seL4.
//!
//! A message is an info word (label, word count, capability count, flags;
//! `radix_ipc::info`) and up to eight data words, all in registers: the
//! kernel copies them from the sender's saved registers straight into the
//! receiver's, never through memory. When the sender and receiver meet,
//! whichever came second hands the message over and wakes the other; the
//! one that came first waits in the endpoint's queue of senders or of
//! receivers, first come first served.
//!
//! A *call* (`cap_call` on an endpoint) sends and then waits for a reply,
//! which only the receiver of that call can give (`reply_recv`): the
//! receiver holds the caller until it replies, receives again, or exits,
//! in which case the caller gets `gone`.
//!
//! A message may carry up to four capabilities, named by the sender's first
//! data words, if the sender's endpoint capability has the grant right.
//! They are copied (or moved, with the move_caps flag) into the first
//! empty slots of the receiver's receive window, and the receiver gets
//! those slot numbers in place of the sender's. That happens under the
//! capability store's lock, which register-only messages never take.
//!
//! The fast path: a register-only message to a waiting receiver, and every
//! reply, wakes the other thread on this CPU (`sched::wake_sync`), where it
//! runs as soon as this thread blocks, without an interrupt to another CPU.
//!
//! Lock order: the store, an endpoint, a thread's IPC state (never two of
//! either at once).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use radix_abi::Status;
use radix_abi::endpoint::{MESSAGE_CAPS, MESSAGE_WORDS};
use radix_abi::errors::Kernel;
use radix_cap::{Derivation, SlotRef, SpaceId};
use radix_ipc::info::Info;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::SpinLock;

use super::{Fifo, Result, UserRef, UserThread, cap_error, error};
use crate::caps::{self, Store};
use crate::sched::{self, ThreadRef};

/// An endpoint, shared.
pub type EndpointRef = Arc<Endpoint, Kmalloc>;

/// An endpoint.
pub struct Endpoint {
    queues: SpinLock<Queues>,
    /// Queued senders whose messages carry capabilities: a receiver that
    /// sees any takes the store's lock first.
    cap_senders: AtomicUsize,
}

struct Queues {
    closed: bool,
    senders: Fifo<Sender>,
    receivers: Fifo<ThreadRef>,
}

/// A sender waiting in an endpoint's queue.
struct Sender {
    thread: ThreadRef,
    message: Message,
}

/// A message on its way.
#[derive(Clone, Copy)]
pub struct Message {
    pub info: Info,
    pub words: [u64; MESSAGE_WORDS as usize],
    /// The badge of the capability it was sent with.
    pub badge: u64,
    /// Whether the sender waits for a reply.
    pub call: bool,
    /// Whether the sender's capability had the grant right: its message
    /// may carry capabilities, and so may the reply to its call.
    pub grant: bool,
    /// The sender's capability space, where its capabilities are.
    pub cspace: SpaceId,
}

/// A thread waiting for a reply, held by the thread that received its call.
pub struct Caller {
    thread: ThreadRef,
    grant: bool,
}

/// What a thread received: a message or a reply.
pub struct Delivery {
    pub badge: u64,
    pub info: u64,
    pub words: [u64; MESSAGE_WORDS as usize],
    caller: Option<Caller>,
}

/// Where a thread is in IPC.
#[derive(Default)]
pub enum IpcState {
    /// Not in IPC.
    #[default]
    Idle,
    /// Queued on an endpoint, sending.
    Sending,
    /// Queued on an endpoint, receiving.
    Receiving,
    /// Its call was received; it waits for the reply.
    AwaitingReply,
    /// Its message was taken (a send, not a call).
    Taken,
    /// A message or a reply arrived.
    Delivered(Delivery),
    /// The operation failed on the other side.
    Failed(Status),
}

impl Endpoint {
    /// A new endpoint.
    pub fn new() -> Option<EndpointRef> {
        let endpoint = Endpoint {
            queues: SpinLock::new(Queues {
                closed: false,
                senders: Fifo::new(),
                receivers: Fifo::new(),
            }),
            cap_senders: AtomicUsize::new(0),
        };
        Arc::try_new_in(endpoint, HEAP.with(Flags::MAY_SLEEP)).ok()
    }

    /// Its last capability is gone: every thread waiting on it gets
    /// `gone`.
    pub fn close(&self) {
        let (senders, receivers) = {
            let mut queues = self.queues.lock();
            queues.closed = true;
            self.cap_senders.store(0, Ordering::Relaxed);
            (queues.senders.take(), queues.receivers.take())
        };
        for thread in senders.into_iter().map(|s| s.thread).chain(receivers) {
            fail(&thread, error(Kernel::Gone));
        }
    }
}

/// The program side of a thread in an endpoint's queue.
fn user_of(thread: &ThreadRef) -> &UserRef {
    thread.user().expect("only program threads use endpoints")
}

/// Ends `thread`'s wait with `status`.
fn fail(thread: &ThreadRef, status: Status) {
    *user_of(thread).ipc.lock() = IpcState::Failed(status);
    sched::wake(thread);
}

/// Sends `message` on `endpoint` from the running thread, and for a call
/// waits for the reply. Returns the reply for a call.
pub fn send(
    me: &ThreadRef,
    user: &UserRef,
    endpoint: &EndpointRef,
    message: Message,
) -> Result<Option<Delivery>> {
    if message.info.caps > 0 && !message.grant {
        return Err(Kernel::MissingRights.status(u32::from(radix_cap::Rights::GRANT.bits())));
    }
    let queued = if message.info.caps > 0 {
        caps::with(|store| hand_over(Some(store), me, user, endpoint, message))
            .map_err(cap_error)??
    } else {
        hand_over(None, me, user, endpoint, message)?
    };
    if queued {
        match wait(me, user, Some(endpoint), "send")? {
            IpcState::Taken => return Ok(None),
            IpcState::AwaitingReply => {}
            // The receiver took the call and replied before this thread
            // looked.
            IpcState::Delivered(delivery) => return Ok(Some(delivery)),
            _ => unreachable!("a sender's wait ends taken, answered, awaiting a reply or failed"),
        }
    }
    if !message.call {
        return Ok(None);
    }
    match wait(me, user, None, "call")? {
        IpcState::Delivered(delivery) => Ok(Some(delivery)),
        _ => unreachable!("a caller's wait ends with a reply or a failure"),
    }
}

/// Hands `message` to a waiting receiver, or queues the sender. Returns
/// whether it queued. With the store locked when the message carries
/// capabilities.
fn hand_over(
    mut store: Option<&mut Store>,
    me: &ThreadRef,
    user: &UserRef,
    endpoint: &EndpointRef,
    message: Message,
) -> Result<bool> {
    let mut queues = endpoint.queues.lock();
    if queues.closed {
        return Err(error(Kernel::Gone));
    }
    let Some(receiver) = queues.receivers.pop_front() else {
        if message.info.no_block {
            return Err(error(Kernel::WouldBlock));
        }
        if queues.senders.push_back(Sender { thread: me.clone(), message }).is_err() {
            return Err(error(Kernel::NoMemory));
        }
        *user.ipc.lock() = IpcState::Sending;
        if message.info.caps > 0 {
            endpoint.cap_senders.fetch_add(1, Ordering::Relaxed);
        }
        return Ok(true);
    };
    let receiver_user = user_of(&receiver).clone();
    let mut words = message.words;
    if message.info.caps > 0 {
        let store = store.as_mut().expect("the store is locked for capabilities");
        match transfer(store, &message, &receiver_user) {
            Ok(slots) => words[..message.info.caps].copy_from_slice(&slots[..message.info.caps]),
            Err(err) => {
                // The receiver waits on for another message; it was just
                // taken, so there is room to put it back.
                if let Err(receiver) = queues.receivers.push_front(receiver) {
                    drop(queues);
                    fail(&receiver, error(Kernel::NoMemory));
                }
                return Err(err);
            }
        }
    }
    if message.call {
        *user.ipc.lock() = IpcState::AwaitingReply;
    }
    *receiver_user.ipc.lock() = IpcState::Delivered(Delivery {
        badge: message.badge,
        info: message.info.delivered(),
        words,
        caller: message.call.then(|| Caller { thread: me.clone(), grant: message.grant }),
    });
    drop(queues);
    sched::wake_sync(&receiver);
    Ok(false)
}

/// Receives a message on `endpoint` for the running thread, waiting for
/// one unless `no_block`. A call's caller is held for the reply.
pub fn recv(
    me: &ThreadRef,
    user: &UserRef,
    endpoint: &EndpointRef,
    no_block: bool,
) -> Result<Delivery> {
    abandon_caller(user);
    let mut delivery = loop {
        let taken = if endpoint.cap_senders.load(Ordering::Relaxed) > 0 {
            caps::with(|store| take(Some(store), me, user, endpoint, no_block))
                .map_err(cap_error)??
        } else {
            take(None, me, user, endpoint, no_block)?
        };
        match taken {
            Taken::Message(delivery) => break delivery,
            Taken::Queued => match wait(me, user, Some(endpoint), "receive")? {
                IpcState::Delivered(delivery) => break delivery,
                _ => unreachable!("a receiver's wait ends with a message or a failure"),
            },
            Taken::NeedStore => continue,
        }
    };
    if let Some(caller) = delivery.caller.take() {
        *user.reply.lock() = Some(caller);
    }
    Ok(delivery)
}

enum Taken {
    Message(Delivery),
    Queued,
    /// The first sender carries capabilities: again, with the store.
    NeedStore,
}

/// Takes the first sender's message, or queues the receiver.
fn take(
    mut store: Option<&mut Store>,
    me: &ThreadRef,
    user: &UserRef,
    endpoint: &EndpointRef,
    no_block: bool,
) -> Result<Taken> {
    let mut queues = endpoint.queues.lock();
    if queues.closed {
        return Err(error(Kernel::Gone));
    }
    while let Some(sender) = queues.senders.pop_front() {
        let message = sender.message;
        let mut words = message.words;
        if message.info.caps > 0 {
            let Some(store) = store.as_deref_mut() else {
                if let Err(sender) = queues.senders.push_front(sender) {
                    drop(queues);
                    fail(&sender.thread, error(Kernel::NoMemory));
                }
                return Ok(Taken::NeedStore);
            };
            endpoint.cap_senders.fetch_sub(1, Ordering::Relaxed);
            match transfer(store, &message, user) {
                Ok(slots) => {
                    words[..message.info.caps].copy_from_slice(&slots[..message.info.caps])
                }
                Err(err) => {
                    // The sender gets the error; the next one may fit.
                    fail(&sender.thread, err);
                    continue;
                }
            }
        }
        let sender_user = user_of(&sender.thread);
        *sender_user.ipc.lock() =
            if message.call { IpcState::AwaitingReply } else { IpcState::Taken };
        drop(queues);
        if !message.call {
            sched::wake(&sender.thread);
        }
        return Ok(Taken::Message(Delivery {
            badge: message.badge,
            info: message.info.delivered(),
            words,
            caller: message.call.then(|| Caller { thread: sender.thread, grant: message.grant }),
        }));
    }
    if no_block {
        return Err(error(Kernel::WouldBlock));
    }
    if queues.receivers.push_back(me.clone()).is_err() {
        return Err(error(Kernel::NoMemory));
    }
    *user.ipc.lock() = IpcState::Receiving;
    Ok(Taken::Queued)
}

/// Replies to the caller the running thread holds. A caller that stopped
/// waiting is skipped.
pub fn reply(user: &UserRef, info: Info, words: [u64; MESSAGE_WORDS as usize]) -> Result<()> {
    let Some(caller) = user.reply.lock().take() else {
        return Err(error(Kernel::NoCaller));
    };
    let caller_user = user_of(&caller.thread).clone();
    let mut words = words;
    if info.caps > 0 {
        if !caller.grant {
            return Err(Kernel::MissingRights.status(u32::from(radix_cap::Rights::GRANT.bits())));
        }
        let message =
            Message { info, words, badge: 0, call: false, grant: true, cspace: user.cspace };
        caps::with(|store| -> Result<()> {
            let slots = transfer(store, &message, &caller_user)?;
            if !deliver_reply(&caller, &caller_user, info, {
                words[..info.caps].copy_from_slice(&slots[..info.caps]);
                words
            }) {
                // Nobody waits for it: the capabilities go back.
                undo(store, &message, &caller_user, &slots);
            }
            Ok(())
        })
        .map_err(cap_error)??;
    } else {
        deliver_reply(&caller, &caller_user, info, words);
    }
    Ok(())
}

/// Gives a waiting caller its reply. Returns whether it was waiting.
fn deliver_reply(
    caller: &Caller,
    caller_user: &UserThread,
    info: Info,
    words: [u64; MESSAGE_WORDS as usize],
) -> bool {
    {
        let mut state = caller_user.ipc.lock();
        if !matches!(*state, IpcState::AwaitingReply) {
            return false;
        }
        *state =
            IpcState::Delivered(Delivery { badge: 0, info: info.delivered(), words, caller: None });
    }
    sched::wake_sync(&caller.thread);
    true
}

/// A thread that receives again, or exits, without replying lets its
/// caller go with `gone`.
fn abandon_caller(user: &UserThread) {
    let caller = user.reply.lock().take();
    if let Some(caller) = caller {
        let caller_user = user_of(&caller.thread);
        let mut state = caller_user.ipc.lock();
        if matches!(*state, IpcState::AwaitingReply) {
            *state = IpcState::Failed(error(Kernel::Gone));
            drop(state);
            sched::wake(&caller.thread);
        }
    }
}

/// The running thread is exiting.
pub fn exiting(user: &UserThread) {
    abandon_caller(user);
}

/// Waits until the running thread's IPC state settles, and returns it
/// (leaving it idle). A thread asked to stop leaves the endpoint's queue
/// (or stops waiting for a reply) and gets `interrupted`.
fn wait(
    me: &ThreadRef,
    user: &UserThread,
    endpoint: Option<&EndpointRef>,
    reason: &'static str,
) -> Result<IpcState> {
    loop {
        {
            let mut state = user.ipc.lock();
            match *state {
                IpcState::Delivered(_) | IpcState::Taken | IpcState::Failed(_) => {
                    return match core::mem::take(&mut *state) {
                        IpcState::Failed(status) => Err(status),
                        settled => Ok(settled),
                    };
                }
                // A queued call was taken: now the reply, which finds the
                // state as it is.
                IpcState::AwaitingReply if endpoint.is_some() => {
                    return Ok(IpcState::AwaitingReply);
                }
                _ => {}
            }
        }
        if me.should_stop() && cancel(me, user, endpoint) {
            return Err(error(Kernel::Interrupted));
        }
        sched::block(reason);
    }
}

/// Takes the running thread out of the wait it is in, if it still can:
/// out of the endpoint's queue, or no longer waiting for a reply. Returns
/// whether it did; if not, its partner is delivering right now.
fn cancel(me: &ThreadRef, user: &UserThread, endpoint: Option<&EndpointRef>) -> bool {
    match endpoint {
        Some(endpoint) => {
            let mut queues = endpoint.queues.lock();
            let before = queues.senders.len() + queues.receivers.len();
            let mut cap_sender = false;
            queues.senders.retain(|s| {
                let mine = ThreadRef::ptr_eq(&s.thread, me);
                cap_sender |= mine && s.message.info.caps > 0;
                !mine
            });
            queues.receivers.retain(|t| !ThreadRef::ptr_eq(t, me));
            if cap_sender {
                endpoint.cap_senders.fetch_sub(1, Ordering::Relaxed);
            }
            let removed = queues.senders.len() + queues.receivers.len() < before;
            if removed {
                *user.ipc.lock() = IpcState::Idle;
            }
            removed
        }
        None => {
            let mut state = user.ipc.lock();
            if matches!(*state, IpcState::AwaitingReply) {
                // The replier finds nobody waiting and skips the reply.
                *state = IpcState::Idle;
                true
            } else {
                false
            }
        }
    }
}

/// Copies or moves a message's capabilities from the sender's space into
/// the first empty slots of `to`'s receive window, and returns those
/// slots. All or nothing.
fn transfer(store: &mut Store, message: &Message, to: &UserThread) -> Result<[u64; 4]> {
    let count = message.info.caps;
    debug_assert!(count <= MESSAGE_CAPS as usize);
    let (first, window) = to.window();
    let mut slots = [0u64; 4];
    let mut next = first;
    for i in 0..count {
        let Ok(slot) = u32::try_from(message.words[i]) else {
            undo(store, message, to, &slots[..i]);
            return Err(error(Kernel::NoSuchSlot));
        };
        let from = SlotRef::new(message.cspace, slot);
        // The next empty slot of the window.
        loop {
            let room = next < first.saturating_add(window)
                && match store.get(SlotRef::new(to.cspace, next)) {
                    Err(radix_cap::CapError::Empty) => break,
                    Ok(_) => true,
                    Err(_) => false,
                };
            if !room {
                undo(store, message, to, &slots[..i]);
                return Err(error(Kernel::NoRoom));
            }
            next += 1;
        }
        let dest = SlotRef::new(to.cspace, next);
        let moved = if message.info.move_caps {
            store.move_cap(from, dest)
        } else {
            store.derive(from, dest, Derivation::COPY)
        };
        if let Err(err) = moved {
            undo(store, message, to, &slots[..i]);
            return Err(cap_error(err));
        }
        slots[i] = u64::from(next);
        next += 1;
    }
    Ok(slots)
}

/// Undoes a transfer of capabilities into `slots` of `to`'s space. A
/// copy's deletion never releases its object, whose capability the sender
/// still holds, so this never closes an object (it may run with an
/// endpoint locked).
fn undo(store: &mut Store, message: &Message, to: &UserThread, slots: &[u64]) {
    for (i, &slot) in slots.iter().enumerate() {
        let dest = SlotRef::new(to.cspace, slot as u32);
        if message.info.move_caps {
            let from = SlotRef::new(message.cspace, message.words[i] as u32);
            let _ = store.move_cap(dest, from);
        } else {
            let _ = store.delete(dest, &mut caps::release);
        }
    }
}
