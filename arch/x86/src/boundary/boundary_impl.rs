// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2024.

use core::fmt::Write;

use crate::registers::bits32::eflags::{EFlags, EFLAGS};

use kernel::process::FunctionCall;
use kernel::syscall::{ContextSwitchReason, Syscall, SyscallReturn, UserspaceKernelBoundary};
use kernel::ErrorCode;

use crate::interrupts::{IDT_RESERVED_EXCEPTIONS, SYSCALL_VECTOR};
use crate::segmentation::{USER_CODE, USER_DATA};

use super::UserContext;

/// Defines the usermode-kernelmode ABI for x86 platforms.
pub struct Boundary;

impl Default for Boundary {
    fn default() -> Self {
        Self::new()
    }
}

impl Boundary {
    /// Minimum required size for initial process memory.
    ///
    /// Need at least 5 dwords of initial stack space for CRT 0:
    ///
    /// - 1 dword for initial upcall return address (although this will be zero for init_fn)
    /// - 4 dwords of scratch space for invoking memop syscalls
    const MIN_APP_BRK: u32 = 5 * core::mem::size_of::<usize>() as u32;

    /// Constructs a new instance of `SysCall`.
    pub fn new() -> Self {
        Self
    }
}

impl UserspaceKernelBoundary for Boundary {
    type StoredState = UserContext;

    fn initial_process_app_brk_size(&self) -> usize {
        Self::MIN_APP_BRK as usize
    }

    unsafe fn initialize_process(
        &self,
        accessible_memory_start: *const u8,
        app_brk: *const u8,
        state: &mut Self::StoredState,
    ) -> Result<(), ()> {
        if (app_brk as u32 - accessible_memory_start as u32) < Self::MIN_APP_BRK {
            return Err(());
        }

        // We pre-allocate 16 bytes on the stack for initial upcall arguments.
        let esp = (app_brk as u32) - 16;

        let mut eflags = EFlags::new();
        eflags.0.modify(EFLAGS::FLAGS_IF::SET);

        state.eax = 0;
        state.ebx = 0;
        state.ecx = 0;
        state.edx = 0;
        state.esi = 0;
        state.edi = 0;
        state.ebp = 0;
        state.esp = esp;
        state.eip = 0;
        state.eflags = eflags.0.get();
        state.cs = USER_CODE.bits() as u32;
        state.ss = USER_DATA.bits() as u32;
        state.ds = USER_DATA.bits() as u32;
        state.es = USER_DATA.bits() as u32;
        state.fs = USER_DATA.bits() as u32;
        state.gs = USER_DATA.bits() as u32;

        Ok(())
    }

    unsafe fn set_syscall_return_value(
        &self,
        _accessible_memory_start: *const u8,
        _app_brk: *const u8,
        state: &mut Self::StoredState,
        return_value: SyscallReturn,
    ) -> Result<(), ()> {
        kernel::utilities::arch_helpers::encode_syscall_return_trd104(
            &kernel::utilities::arch_helpers::TRD104SyscallReturn::from_syscall_return(
                return_value,
            ),
            &mut state.ebx,
            &mut state.ecx,
            &mut state.edx,
            &mut state.edi,
        );

        Ok(())
    }

    unsafe fn set_process_function(
        &self,
        _accessible_memory_start: *const u8,
        _app_brk: *const u8,
        state: &mut Self::StoredState,
        upcall: FunctionCall,
    ) -> Result<(), ()> {
        state.ebx = upcall.argument0 as u32;
        state.ecx = upcall.argument1 as u32;
        state.edx = upcall.argument2 as u32;
        state.edi = upcall.argument3.as_usize() as u32;

        // The next time we switch to this process, we will directly jump to the upcall. When the
        // upcall issues `ret`, it will return to wherever the yield syscall was invoked.
        state.eip = upcall.pc.addr() as u32;

        Ok(())
    }

    unsafe fn switch_to_process(
        &self,
        _accessible_memory_start: *const u8,
        _app_brk: *const u8,
        state: &mut Self::StoredState,
    ) -> (ContextSwitchReason, Option<*const u8>) {
        // Sanity check: don't try to run a faulted app
        if state.exception != 0 || state.err_code != 0 {
            let stack_ptr = state.esp as *mut u8;
            return (ContextSwitchReason::Fault, Some(stack_ptr));
        }

        let mut err_code = 0;
        let int_num = unsafe { super::switch_to_user(state, &mut err_code) };

        let reason = match int_num as u8 {
            0..IDT_RESERVED_EXCEPTIONS => {
                state.exception = int_num as u8;
                state.err_code = err_code;
                ContextSwitchReason::Fault
            }

            SYSCALL_VECTOR => {
                let num = state.eax as u8;

                let arg0 = state.ebx;
                let arg1 = state.ecx;
                let arg2 = state.edx;
                let arg3 = state.edi;

                Syscall::from_register_arguments(
                    num,
                    arg0 as usize,
                    (arg1 as usize).into(),
                    (arg2 as usize).into(),
                    (arg3 as usize).into(),
                )
                .map_or(ContextSwitchReason::Fault, |syscall| {
                    ContextSwitchReason::SyscallFired { syscall }
                })
            }
            _ => ContextSwitchReason::Interrupted,
        };

        let stack_ptr = state.esp as *const u8;

        (reason, Some(stack_ptr))
    }

    unsafe fn print_context(
        &self,
        _accessible_memory_start: *const u8,
        _app_brk: *const u8,
        state: &Self::StoredState,
        writer: &mut dyn Write,
    ) {
        let _ = writeln!(writer, "{}", state);
    }

    fn store_context(
        &self,
        _state: &Self::StoredState,
        _out: &mut [u8],
    ) -> Result<usize, ErrorCode> {
        unimplemented!()
    }
}
