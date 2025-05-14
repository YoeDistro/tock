// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2025.

//! Data structure for storing `Process`es.

use crate::process;
use core::cell::Cell;

/// Represents a slot for a process in a [`ProcessArray`].
#[derive(Clone)]
pub struct ProcessSlot {
    /// Optionally points to a process.
    pub(crate) proc: Cell<Option<&'static dyn process::Process>>,
}

impl ProcessSlot {
    /// Return the underlying [process::Process] if and only if [self]
    /// represents an active process.
    pub fn get_active(&self) -> Option<&'static dyn process::Process> {
        self.proc.get()
    }

    pub fn is_valid_for(&self, identifier: usize) -> bool {
        match self.proc.get() {
            Some(process) => process.processid().id() == identifier,
            None => false,
        }
    }
}

/// Storage for an array of `Process`es.
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

    pub fn as_slice(&self) -> &[ProcessSlot] {
        &self.processes
    }
}

impl<const NUM_PROCS: usize> core::ops::Index<usize> for ProcessArray<NUM_PROCS> {
    type Output = ProcessSlot;

    fn index(&self, i: usize) -> &ProcessSlot {
        &self.processes[i]
    }
}
