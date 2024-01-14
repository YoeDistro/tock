// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2024.

//! Utility to partition SyscallDriver resources by app.

use kernel::syscall::{CommandReturn, SyscallDriver};
use kernel::ErrorCode;
use kernel::ProcessId;

pub struct AppPermittedData {
    app_id: kernel::process::ShortID,
    range_start: usize,
    range_end: usize,
}

/// Holds the array of LEDs and implements a `Driver` interface to
/// control them.
pub struct RestrictResource<'a, D: kernel::syscall::SyscallDriver> {
    driver: &'a D,
    command_num_num: usize,
    permitted: &'a [AppPermittedData],
}

impl<'a, D: kernel::syscall::SyscallDriver> RestrictResource<'a, D> {
    pub fn new(driver: &'a D, permitted: &'a [AppPermittedData], command_num_num: usize) -> Self {
        Self {
            driver,
            command_num_num,
            permitted,
        }
    }

    fn get_app_permitted(&self, processid: ProcessId) -> Option<&AppPermittedData> {
        for perm in self.permitted {
            if processid.short_app_id() == perm.app_id {
                return Some(&perm);
            }
        }
        None
    }
}

impl<'a, D: kernel::syscall::SyscallDriver> SyscallDriver for RestrictResource<'a, D> {
    fn command(
        &self,
        command_num: usize,
        data: usize,
        arg2: usize,
        processid: ProcessId,
    ) -> CommandReturn {
        match command_num {
            0 => self.driver.command(0, data, arg2, processid),

            _ => match self.get_app_permitted(processid) {
                Some(perm) => {
                    if command_num == self.command_num_num {
                        CommandReturn::success_u32((perm.range_end - perm.range_start) as u32)
                    } else {
                        let new_data = perm.range_start;
                        if new_data < perm.range_end {
                            self.driver.command(0, new_data, arg2, processid)
                        } else {
                            CommandReturn::failure(ErrorCode::NOSUPPORT)
                        }
                    }
                }
                None => CommandReturn::failure(ErrorCode::NOSUPPORT),
            },
        }
    }

    fn allocate_grant(&self, processid: ProcessId) -> Result<(), kernel::process::Error> {
        self.driver.allocate_grant(processid)
    }
}
