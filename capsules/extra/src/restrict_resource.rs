// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2024.

//! Utility to partition SyscallDriver resources by app.

use kernel::syscall::{CommandReturn, SyscallDriver};
use kernel::ErrorCode;
use kernel::ProcessId;

/// Represents the permissions an app has to use the underlying resource.
///
/// The app is represented by its `ShortID` and this identifies the ranges of
/// the first argument to every command that is permitted for the identified
/// app.
pub struct AppPermissions {
    /// The identified app that these permissions are for.
    app_id: kernel::process::ShortID,
    /// The range of allowed arguments to argument 1 of the command syscall.
    permitted_arg1: core::ops::Range<usize>,
    /// The range of allowed arguments to argument 2 of the command syscall.
    permitted_arg2: core::ops::Range<usize>,
}

/// Capsule that restricts applications to only accessing commands with a subset
/// of arguments.
pub struct CommandRestrictions<'a, D: kernel::syscall::SyscallDriver> {
    /// Underlying `SyscallDriver` resource that is being restricted.
    driver: &'a D,
    /// Command num for the command that returns the count of the underlying
    /// resource. So for example, if `command_num==5` means return the number of
    /// GPIO pins, then this capsule should be configured with
    /// `command_num_num==5`.
    command_num_num: usize,
    /// Array of permissions granted to specific apps.
    permissions: &'a [AppPermissions],
}

impl<'a, D: kernel::syscall::SyscallDriver> CommandRestrictions<'a, D> {
    pub fn new(driver: &'a D, permissions: &'a [AppPermissions], command_num_num: usize) -> Self {
        Self {
            driver,
            command_num_num,
            permissions,
        }
    }

    fn get_app_permitted(&self, processid: ProcessId) -> Option<&AppPermissions> {
        for perm in self.permissions {
            if processid.short_app_id() == perm.app_id {
                return Some(&perm);
            }
        }
        None
    }
}

impl<'a, D: kernel::syscall::SyscallDriver> SyscallDriver for CommandRestrictions<'a, D> {
    fn command(
        &self,
        command_num: usize,
        arg1: usize,
        arg2: usize,
        processid: ProcessId,
    ) -> CommandReturn {
        match command_num {
            0 => self.driver.command(0, arg1, arg2, processid),

            _ => match self.get_app_permitted(processid) {
                Some(perm) => {
                    if command_num == self.command_num_num {
                        CommandReturn::success_u32(perm.permitted_arg1.len() as u32)
                    } else {
                        // For all other commands, we convert the arguments from
                        // the range used by the app to the full range used by
                        // the underlying resource and then call into the
                        // underlying resource.
                        let new_arg1 = perm.permitted_arg1.start + arg1;
                        let new_arg2 = perm.permitted_arg2.start + arg2;

                        // If that is within the approved range, call the
                        // command in the underlying resource.
                        if perm.permitted_arg1.contains(&new_arg1)
                            && perm.permitted_arg2.contains(&new_arg2)
                        {
                            self.driver.command(0, new_arg1, new_arg2, processid)
                        } else {
                            // Otherwise return a failure.
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
