// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2025.

use crate::process;
use core::cell::Cell;

/// Represents a slot for a process in a [`ProcessArray`].
///
/// Holds both a reference to the [`process::Process`] as well as a
/// cached process identifier, which enables efficient selection as
/// well as invalidating a process without yet deallocating the
/// process itself.
///
/// # Invariants
///
/// If [valid_proc_id] is not [ID_INVALID], it should have the same value as [proc_ref]'s [ProcessId#identifier].
#[derive(Clone)]
pub struct ProcessSlot {
    /// Optionally points to a process.
    ///
    /// If [valid_proc_id] is not [ID_INVALID], this must be a [Some].
    pub(crate) proc: Cell<Option<&'static dyn process::Process>>,
}

impl ProcessSlot {
    /// Return the underlying [process::Process] if and only if [self]
    /// represents an active process.
    pub fn get_active(&self) -> Option<&'static dyn process::Process> {
        // if self.valid_proc_id.get() != Self::ID_INVALID {
        //     self.proc_ref.get()
        // } else {
        //     None
        // }

        self.proc.get()
    }

    pub fn is_valid_for(&self, identifier: usize) -> bool {
        match self.proc.get() {
            Some(process) => process.processid().id() == identifier,
            None => false,
        }
    }
}

// /// The type each board should allocate to hold processes.
// ///
// /// Boards should use this type, an use init_process_array to create
// /// an array so they don't need to pay too much attention to at this
// /// type actually is.
// pub type ProcessArray<const NUM_PROCS: usize> = [ProcEntry; NUM_PROCS];

// /// Create an empty array of processes required to construct a new kernel type
// pub const fn init_process_array<const NUM_PROCS: usize>() -> ProcessArray<NUM_PROCS> {
//     const INVALID_ENTRY: ProcEntry = ProcEntry {
//         valid_proc_id: Cell::new(ProcEntry::ID_INVALID),
//         proc_ref: Cell::new(None),
//     };
//     [INVALID_ENTRY; NUM_PROCS]
// }

pub struct ProcessArray<const NUM_PROCS: usize> {
    processes: [ProcessSlot; NUM_PROCS],
}

impl<const NUM_PROCS: usize> ProcessArray<NUM_PROCS> {
    pub const fn new() -> Self {
        const EMPTY: ProcessSlot = ProcessSlot {
            proc: Cell::new(None),
        };
        Self {
            processes: [EMPTY; NUM_PROCS],
        }
    }
}

impl<const NUM_PROCS: usize> From<ProcessArray<NUM_PROCS>> for [ProcessSlot; NUM_PROCS] {
    fn from(array: ProcessArray<NUM_PROCS>) -> Self {
        array.processes
    }
}

impl<const NUM_PROCS: usize> core::ops::Index<usize> for ProcessArray<NUM_PROCS> {
    type Output = ProcessSlot;

    fn index(&self, i: usize) -> &ProcessSlot {
        &self.processes[i]
    }
}
