// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The process tree: which process is whose child, which process group and
//! session each is in, and what each has to tell its parent's `wait`.
//!
//! Every process a POSIX program can name is a [`Member`], found by its
//! number. A member's parent is the process that forked it (or ran `exec`
//! on it, for a process made with `process.create`), or none: a process
//! the kernel or a loader started, or one whose parent ended before it.
//! A process with no parent is never waited for, so when it ends it is
//! reaped at once: its parent's part falls to the kernel, which is Spark's
//! auto-reaping as the root of the tree. So a member that has ended (a
//! zombie) always has a parent that is alive.
//!
//! A process group is named by its leader's number and a session by its
//! leader's; both live as long as any member is in them, the leader or
//! not. These invariants hold after every operation, and the property
//! tests check them:
//!
//! - numbers are unique;
//! - a member's parent, if it has one, is a member that has not ended;
//! - every member of a process group is in the same session;
//! - a session leader leads its own process group.
//!
//! The tree holds a handle of the caller's for each member (the kernel's
//! process object), and gives it back when the member goes.

use alloc::vec::Vec;
use core::alloc::Allocator;

use crate::status::{CONTINUED, Ended};

/// Where a member is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Life {
    /// Running, or waiting.
    Alive,
    /// Stopped by a signal, until SIGCONT.
    Stopped,
    /// Ended, until its parent waits for it.
    Zombie {
        /// How.
        ended: Ended,
        /// The CPU time it used, in user mode and in all, in nanoseconds.
        times: (u64, u64),
    },
}

/// What a member has to tell its parent's `wait` besides its end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// It stopped with this signal, and `wait` has not said so yet.
    pub stopped: Option<u32>,
    /// It continued, and `wait` has not said so yet.
    pub continued: bool,
}

/// A process in the tree.
#[derive(Clone, Debug)]
pub struct Member<T> {
    /// Its number.
    pub pid: u64,
    /// Its parent's number, or 0 for none.
    pub parent: u64,
    /// Its process group.
    pub group: u64,
    /// Its session.
    pub session: u64,
    /// Whether it has run `exec` since it was forked, after which its
    /// parent may no longer move it to another process group.
    pub execed: bool,
    /// Where it is in its life.
    pub life: Life,
    /// What its parent's `wait` has yet to hear.
    pub report: Report,
    /// The caller's handle.
    pub handle: T,
}

/// Which children `wait` waits for, as its `pid` argument names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    /// Any child.
    Any,
    /// The child with this number.
    Pid(u64),
    /// Any child in this process group.
    Group(u64),
}

impl Which {
    /// What `pid` names, for a caller in process group `group`: -1 any
    /// child, 0 any in the caller's process group, a negative number any
    /// in that process group, a positive one that child.
    pub const fn from_wait(pid: i64, group: u64) -> Which {
        match pid {
            -1 => Which::Any,
            0 => Which::Group(group),
            p if p < 0 => Which::Group(p.unsigned_abs()),
            p => Which::Pid(p as u64),
        }
    }
}

/// What `wait` may report besides children that have ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WaitFor {
    /// Children that have stopped.
    pub stopped: bool,
    /// Children that have continued.
    pub continued: bool,
    /// Leave the child as it is, to be waited for again.
    pub no_reap: bool,
}

/// What `wait` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Found<T> {
    /// A child to report: its number, wait status and CPU times, and its
    /// handle if it was reaped (it had ended, and `no_reap` was not
    /// asked).
    Child {
        /// Its number.
        pid: u64,
        /// Its wait status.
        status: u32,
        /// The CPU time it used, in user mode and in all (zero unless it
        /// ended).
        times: (u64, u64),
        /// Its handle, if it left the tree.
        reaped: Option<T>,
    },
    /// Children that could be reported later, but none to report now.
    NotYet,
    /// No child the call could wait for.
    NoChild,
}

/// Why a change to the tree was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// No member has the number.
    NoSuchProcess,
    /// The caller may not do that (EPERM).
    NotPermitted,
    /// The caller's child has run `exec` (EACCES).
    Execed,
    /// An argument makes no sense (EINVAL).
    Invalid,
    /// No memory for another member.
    NoMemory,
    /// A member has that number already.
    Exists,
}

/// The process tree.
pub struct Tree<T, A: Allocator> {
    members: Vec<Member<T>, A>,
}

impl<T: Clone, A: Allocator> Tree<T, A> {
    /// An empty tree whose memory comes from `alloc`.
    pub fn new(alloc: A) -> Self {
        Tree { members: Vec::new_in(alloc) }
    }

    /// How many members.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether it has no member.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Every member.
    pub fn members(&self) -> impl Iterator<Item = &Member<T>> {
        self.members.iter()
    }

    fn index(&self, pid: u64) -> Option<usize> {
        self.members.iter().position(|m| m.pid == pid)
    }

    /// The member numbered `pid`.
    pub fn get(&self, pid: u64) -> Option<&Member<T>> {
        self.members.iter().find(|m| m.pid == pid)
    }

    /// The member numbered `pid`, to change what the tree does not check
    /// (`execed`, `report`).
    pub fn get_mut(&mut self, pid: u64) -> Option<&mut Member<T>> {
        self.members.iter_mut().find(|m| m.pid == pid)
    }

    /// Makes room for one more member, so that [`Tree::insert`] cannot
    /// fail for want of memory.
    pub fn reserve(&mut self) -> Result<(), TreeError> {
        self.members.try_reserve(1).map_err(|_| TreeError::NoMemory)
    }

    /// Adds a process that has no parent, as the leader of a new session
    /// and process group: one the kernel or a loader started, when it
    /// first does something that needs the tree.
    pub fn insert_root(&mut self, pid: u64, handle: T) -> Result<(), TreeError> {
        self.insert(Member {
            pid,
            parent: 0,
            group: pid,
            session: pid,
            execed: false,
            life: Life::Alive,
            report: Report::default(),
            handle,
        })
    }

    /// Adds `pid` as a child of `parent`, in its process group and
    /// session: a forked process, or one that `exec` started for it.
    pub fn insert_child(&mut self, pid: u64, parent: u64, handle: T) -> Result<(), TreeError> {
        let above = self.get(parent).ok_or(TreeError::NoSuchProcess)?;
        if above.life != Life::Alive && above.life != Life::Stopped {
            return Err(TreeError::NoSuchProcess);
        }
        let (group, session) = (above.group, above.session);
        self.insert(Member {
            pid,
            parent,
            group,
            session,
            execed: false,
            life: Life::Alive,
            report: Report::default(),
            handle,
        })
    }

    fn insert(&mut self, member: Member<T>) -> Result<(), TreeError> {
        if self.get(member.pid).is_some() {
            return Err(TreeError::Exists);
        }
        self.reserve()?;
        // Room was reserved above.
        let _ = self.members.push_within_capacity(member);
        Ok(())
    }

    /// Member `pid` has ended, `ended`, having used `times` of CPU time.
    /// Its children lose their parent, and those of them that had ended
    /// leave the tree; it becomes a zombie for its parent to wait for, or
    /// leaves the tree at once if it has no parent, or its parent does not
    /// wait for children (`auto_reap`, from SIGCHLD's action). Returns its
    /// parent, if it has one to tell, and the handles of the members that
    /// left, which the caller drops.
    pub fn exit(
        &mut self,
        pid: u64,
        ended: Ended,
        times: (u64, u64),
        auto_reap: bool,
        gone: &mut dyn FnMut(T),
    ) -> Option<u64> {
        let index = self.index(pid)?;
        let parent = self.members[index].parent;
        let member = &mut self.members[index];
        member.life = Life::Zombie { ended, times };
        member.report = Report::default();
        // Its children are orphans now; those that ended go.
        let mut i = 0;
        while i < self.members.len() {
            let child = &mut self.members[i];
            if child.parent == pid {
                child.parent = 0;
                if matches!(child.life, Life::Zombie { .. }) {
                    gone(self.members.swap_remove(i).handle);
                    continue;
                }
            }
            i += 1;
        }
        if parent == 0 || auto_reap {
            if let Some(index) = self.index(pid) {
                gone(self.members.swap_remove(index).handle);
            }
        }
        (parent != 0).then_some(parent)
    }

    /// Takes member `pid` out of the tree whatever it is doing: for a
    /// process that goes before it ends, such as one a failed `fork` made.
    /// Its children lose their parent as in [`Tree::exit`].
    pub fn remove(&mut self, pid: u64, gone: &mut dyn FnMut(T)) {
        let _ = self.exit(pid, Ended::Exited(0), (0, 0), true, gone);
    }

    /// Looks for a child of `caller` that `which` names and has something
    /// to report: an end, or with `for_` a stop or a continue. An ended
    /// child is reaped unless `no_reap`.
    pub fn wait(&mut self, caller: u64, which: Which, for_: WaitFor) -> Found<T> {
        let mut any = false;
        let mut found = None;
        for (index, member) in self.members.iter().enumerate() {
            let named = member.parent == caller
                && match which {
                    Which::Any => true,
                    Which::Pid(pid) => member.pid == pid,
                    Which::Group(group) => member.group == group,
                };
            if !named {
                continue;
            }
            any = true;
            let ready = match member.life {
                Life::Zombie { .. } => true,
                _ => {
                    (for_.stopped && member.report.stopped.is_some())
                        || (for_.continued && member.report.continued)
                }
            };
            if ready {
                found = Some(index);
                break;
            }
        }
        let Some(index) = found else {
            return if any { Found::NotYet } else { Found::NoChild };
        };
        let member = &mut self.members[index];
        let pid = member.pid;
        match member.life {
            Life::Zombie { ended, times } => {
                let reaped = if for_.no_reap {
                    None
                } else {
                    Some(self.members.swap_remove(index).handle)
                };
                Found::Child { pid, status: ended.status(), times, reaped }
            }
            _ => {
                let status = match member.report.stopped {
                    Some(signal) if for_.stopped => {
                        if !for_.no_reap {
                            member.report.stopped = None;
                        }
                        crate::status::stopped(signal)
                    }
                    _ => {
                        if !for_.no_reap {
                            member.report.continued = false;
                        }
                        CONTINUED
                    }
                };
                Found::Child { pid, status, times: (0, 0), reaped: None }
            }
        }
    }

    /// Member `pid` stopped with `signal`: its parent's `wait` hears of it.
    /// Returns its parent, if it has one.
    pub fn stopped(&mut self, pid: u64, signal: u32) -> Option<u64> {
        let member = self.get_mut(pid)?;
        if member.life != Life::Alive {
            return None;
        }
        member.life = Life::Stopped;
        member.report = Report { stopped: Some(signal), continued: false };
        (member.parent != 0).then_some(member.parent)
    }

    /// Member `pid` continued after a stop: its parent's `wait` hears of
    /// it. Returns its parent, if it has one, or `None` if it was not
    /// stopped.
    pub fn continued(&mut self, pid: u64) -> Option<u64> {
        let member = self.get_mut(pid)?;
        if member.life != Life::Stopped {
            return None;
        }
        member.life = Life::Alive;
        member.report = Report { stopped: None, continued: true };
        (member.parent != 0).then_some(member.parent)
    }

    /// Whether `sender` may send signals to `target`: itself, a process
    /// below it in the tree, or one in its session. (Until Phase 13 brings
    /// users, these stand in for POSIX's checks of user numbers.)
    pub fn may_signal(&self, sender: u64, target: u64) -> bool {
        if sender == target {
            return true;
        }
        let (Some(from), Some(to)) = (self.get(sender), self.get(target)) else {
            return false;
        };
        if from.session == to.session {
            return true;
        }
        // Up from the target, at most as many steps as there are members.
        let mut at = to.parent;
        for _ in 0..self.members.len() {
            if at == 0 {
                break;
            }
            if at == sender {
                return true;
            }
            at = self.get(at).map_or(0, |m| m.parent);
        }
        false
    }

    /// The members `kill`'s `pid` names, for a sender numbered `sender` in
    /// process group `group`: a positive number that process, 0 the
    /// sender's process group, -1 every member but the sender, another
    /// negative number that process group. Gives each one's number and
    /// handle to `each`; returns how many there were.
    pub fn targets(
        &self,
        sender: u64,
        group: u64,
        pid: i64,
        each: &mut dyn FnMut(&Member<T>),
    ) -> usize {
        let mut count = 0;
        for member in &self.members {
            let named = match pid {
                -1 => member.pid != sender,
                0 => member.group == group,
                p if p < 0 => member.group == p.unsigned_abs(),
                p => member.pid == p as u64,
            };
            if named {
                each(member);
                count += 1;
            }
        }
        count
    }

    /// `setpgid`: `caller` puts `pid` (0: itself), itself or a child of
    /// its that has not run `exec`, in process group `group` (0: a new one
    /// numbered after `pid`), which must already be one of the caller's
    /// session unless it is that new one. A session leader stays where it
    /// is.
    pub fn set_group(&mut self, caller: u64, pid: u64, group: u64) -> Result<(), TreeError> {
        let pid = if pid == 0 { caller } else { pid };
        let group = if group == 0 { pid } else { group };
        let me = self.get(caller).ok_or(TreeError::NoSuchProcess)?;
        let session = me.session;
        let target = self.get(pid).ok_or(TreeError::NoSuchProcess)?;
        if pid != caller {
            if target.parent != caller {
                return Err(TreeError::NoSuchProcess);
            }
            if target.session != session {
                return Err(TreeError::NotPermitted);
            }
            if target.execed {
                return Err(TreeError::Execed);
            }
        }
        if matches!(target.life, Life::Zombie { .. }) {
            return Err(TreeError::NoSuchProcess);
        }
        if target.session == pid {
            return Err(TreeError::NotPermitted);
        }
        if group != pid
            && !self.members.iter().any(|m| m.group == group && m.session == session)
        {
            return Err(TreeError::NotPermitted);
        }
        if let Some(target) = self.get_mut(pid) {
            target.group = group;
        }
        Ok(())
    }

    /// `setsid`: `caller` leaves its session for a new one, of which it
    /// leads the only process group. Refused if it leads a process group
    /// already (so that the group would not span two sessions).
    pub fn new_session(&mut self, caller: u64) -> Result<u64, TreeError> {
        if self.members.iter().any(|m| m.group == caller) {
            return Err(TreeError::NotPermitted);
        }
        let me = self.get_mut(caller).ok_or(TreeError::NoSuchProcess)?;
        me.group = caller;
        me.session = caller;
        Ok(caller)
    }

    /// Whether process group `group` is orphaned: no member of it has a
    /// parent in another process group of the same session, so no shell
    /// is left to continue it.
    pub fn orphaned(&self, group: u64) -> bool {
        !self.members.iter().filter(|m| m.group == group && !matches!(m.life, Life::Zombie { .. }))
            .any(|m| {
                self.get(m.parent).is_some_and(|parent| {
                    parent.group != group && parent.session == m.session
                })
            })
    }

    /// Whether any member of process group `group` is stopped.
    pub fn has_stopped(&self, group: u64) -> bool {
        self.members.iter().any(|m| m.group == group && m.life == Life::Stopped)
    }

    /// Whether any live member is in process group `group`.
    pub fn group_exists(&self, group: u64) -> bool {
        self.members.iter().any(|m| m.group == group && !matches!(m.life, Life::Zombie { .. }))
    }

    /// Checks the invariants the module describes, for the tests.
    pub fn check(&self) -> Result<(), &'static str> {
        for (i, member) in self.members.iter().enumerate() {
            if self.members[i + 1..].iter().any(|m| m.pid == member.pid) {
                return Err("two members have the same number");
            }
            if member.parent != 0 {
                match self.get(member.parent) {
                    Some(parent) if !matches!(parent.life, Life::Zombie { .. }) => {}
                    _ => return Err("a member's parent is gone or has ended"),
                }
            }
            let mixed = self
                .members
                .iter()
                .any(|m| m.group == member.group && m.session != member.session);
            if mixed {
                return Err("a process group spans two sessions");
            }
            if member.session == member.pid && member.group != member.pid {
                return Err("a session leader is not its own group's leader");
            }
        }
        Ok(())
    }
}
