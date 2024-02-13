// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2022.

//! This provides kernel and userspace access to nonvolatile memory.
//!
//! This is an initial implementation that does not provide safety for
//! individual userland applications. Each application has full access to
//! the entire memory space that has been provided to userland. Future revisions
//! should update this to limit applications to only their allocated regions.
//!
//! However, the kernel accessible memory does not have to be the same range
//! as the userspace accessible address space. The kernel memory can overlap
//! if desired, or can be a completely separate range.
//!
//! Here is a diagram of the expected stack with this capsule:
//! Boxes are components and between the boxes are the traits that are the
//! interfaces between components. This capsule provides both a kernel and
//! userspace interface.
//!
//! ```text
//! +--------------------------------------------+     +--------------+
//! |                                            |     |              |
//! |                  kernel                    |     |  userspace   |
//! |                                            |     |              |
//! +--------------------------------------------+     +--------------+
//!  hil::nonvolatile_storage::NonvolatileStorage       kernel::Driver
//! +-----------------------------------------------------------------+
//! |                                                                 |
//! | capsules::nonvolatile_storage_driver::NonvolatileStorage (this) |
//! |                                                                 |
//! +-----------------------------------------------------------------+
//!            hil::nonvolatile_storage::NonvolatileStorage
//! +-----------------------------------------------------------------+
//! |                                                                 |
//! |               Physical nonvolatile storage driver               |
//! |                                                                 |
//! +-----------------------------------------------------------------+
//! ```
//!
//! Example instantiation:
//!
//! ```rust
//! # use kernel::static_init;
//!
//! let nonvolatile_storage = static_init!(
//!     capsules::nonvolatile_storage_driver::NonvolatileStorage<'static>,
//!     capsules::nonvolatile_storage_driver::NonvolatileStorage::new(
//!         fm25cl,                      // The underlying storage driver.
//!         board_kernel.create_grant(&grant_cap),     // Storage for app-specific state.
//!         3000,                        // The byte start address for the userspace
//!                                      // accessible memory region.
//!         2000,                        // The length of the userspace region.
//!         0,                           // The byte start address of the region
//!                                      // that is accessible by the kernel.
//!         3000,                        // The length of the kernel region.
//!         &mut capsules::nonvolatile_storage_driver::BUFFER));
//! hil::nonvolatile_storage::NonvolatileStorage::set_client(fm25cl, nonvolatile_storage);
//! ```

use core::cell::Cell;
use core::cmp;

use kernel::grant::{AllowRoCount, AllowRwCount, Grant, UpcallCount};
use kernel::hil;
use kernel::processbuffer::{ReadableProcessBuffer, WriteableProcessBuffer};
use kernel::syscall::{CommandReturn, SyscallDriver};
use kernel::utilities::cells::{OptionalCell, TakeCell};
use kernel::{ErrorCode, ProcessId};
// use kernel::debug;

/// Syscall driver number.
use capsules_core::driver;
pub const DRIVER_NUM: usize = driver::NUM::NvmStorage as usize;

/// IDs for subscribed upcalls.
mod upcall {
    /// Read done callback.
    pub const READ_DONE: usize = 0;
    /// Write done callback.
    pub const WRITE_DONE: usize = 1;
    /// Number of upcalls.
    pub const COUNT: u8 = 2;
}

/// Ids for read-only allow buffers
mod ro_allow {
    /// Setup a buffer to write bytes to the nonvolatile storage.
    pub const WRITE: usize = 0;
    /// The number of allow buffers the kernel stores for this grant
    pub const COUNT: u8 = 1;
}

/// Ids for read-write allow buffers
mod rw_allow {
    /// Setup a buffer to read from the nonvolatile storage into.
    pub const READ: usize = 0;
    /// The number of allow buffers the kernel stores for this grant
    pub const COUNT: u8 = 1;
}

pub const BUF_LEN: usize = 512;

#[derive(Clone, Copy, PartialEq)]
pub enum NonvolatileCommand {
    UserspaceRead,
    UserspaceWrite,
    KernelRead,
    KernelWrite,
}

#[derive(Clone, Copy)]
pub enum NonvolatileUser {
    App { processid: ProcessId },
    Kernel,
}

/// Variables that are stored in OTA_app grant region to support dynamic app load
#[derive(Default)]
struct ProcLoaderData{
    //Index points the position where the entry point of a new app is written into PROCESS global array 
    index: usize,
    // App size requested by ota app
    rquested_app_size: usize,
    // dynamic_flash_start_addr points the start address that a new app will be loaded
    dynamic_flash_start_addr: usize,
    // dynamic_unsued_sram_start_addr points the start address that a new app will use
    dynamic_unsued_sram_start_addr: usize,
}

pub struct App {
    pending_command: bool,
    command: NonvolatileCommand,
    offset: usize,
    length: usize,
}

impl Default for App {
    fn default() -> App {
        App {
            pending_command: false,
            command: NonvolatileCommand::UserspaceRead,
            offset: 0,
            length: 0,
        }
    }
}

pub struct DynamicProcessLoader<'a> {
    // The underlying physical storage device.
    driver: &'a dyn hil::nonvolatile_storage::NonvolatileStorage<'a>,
    // Per-app state.
    apps: Grant<
        App,
        UpcallCount<{ upcall::COUNT }>,
        AllowRoCount<{ ro_allow::COUNT }>,
        AllowRwCount<{ rw_allow::COUNT }>,
    >,

    // Internal buffer for copying appslices into.
    buffer: TakeCell<'static, [u8]>,
    // What issued the currently executing call. This can be an app or the kernel.
    current_user: OptionalCell<NonvolatileUser>,

    // The first byte that is accessible from userspace.
    userspace_start_address: usize,
    // How many bytes allocated to userspace.
    userspace_length: usize,
    // The first byte that is accessible from the kernel.
    kernel_start_address: usize,
    // How many bytes allocated to kernel.
    kernel_length: usize,

    // Optional client for the kernel. Only needed if the kernel intends to use
    // this nonvolatile storage.
    kernel_client: OptionalCell<&'a dyn hil::nonvolatile_storage::NonvolatileStorageClient>,
    // Whether the kernel is waiting for a read/write.
    kernel_pending_command: Cell<bool>,
    // Whether the kernel wanted a read/write.
    kernel_command: Cell<NonvolatileCommand>,
    // Holder for the buffer passed from the kernel in case we need to wait.
    kernel_buffer: TakeCell<'static, [u8]>,
    // How many bytes to read/write from the kernel buffer.
    kernel_readwrite_length: Cell<usize>,
    // Where to read/write from the kernel request.
    kernel_readwrite_address: Cell<usize>,

    // Supported number of process
    supported_process_num: usize,
    // Start address of a process loaded
    process_region_start_address: &'static [usize],
    // Size of a process loaded
    process_region_size: &'static [usize],
}

impl<'a> DynamicProcessLoader<'a> {
    pub fn new(
        driver: &'a dyn hil::nonvolatile_storage::NonvolatileStorage<'a>,
        grant: Grant<
            App,
            UpcallCount<{ upcall::COUNT }>,
            AllowRoCount<{ ro_allow::COUNT }>,
            AllowRwCount<{ rw_allow::COUNT }>,
        >,
        userspace_start_address: usize,
        userspace_length: usize,
        kernel_start_address: usize,
        kernel_length: usize,
        buffer: &'static mut [u8],
        supported_process_num: usize,
        process_region_start_address: &'static [usize],
        process_region_size: &'static [usize],
    ) -> DynamicProcessLoader<'a> {
        DynamicProcessLoader {
            driver: driver,
            apps: grant,
            buffer: TakeCell::new(buffer),
            current_user: OptionalCell::empty(),
            userspace_start_address: userspace_start_address,
            userspace_length: userspace_length,
            kernel_start_address: kernel_start_address,
            kernel_length: kernel_length,
            kernel_client: OptionalCell::empty(),
            kernel_pending_command: Cell::new(false),
            kernel_command: Cell::new(NonvolatileCommand::KernelRead),
            kernel_buffer: TakeCell::empty(),
            kernel_readwrite_length: Cell::new(0),
            kernel_readwrite_address: Cell::new(0),
            supported_process_num: supported_process_num,
            process_region_start_address: process_region_start_address,
            process_region_size: process_region_size,
        }
    }

    // Check so see if we are doing something. If not, go ahead and do this
    // command. If so, this is queued and will be run when the pending
    // command completes.
    fn enqueue_command(
        &self,
        command: NonvolatileCommand,
        offset: usize,
        length: usize,
        processid: Option<ProcessId>,
    ) -> Result<(), ErrorCode> {
        // Do bounds check.
        match command {
            NonvolatileCommand::UserspaceRead | NonvolatileCommand::UserspaceWrite => {
                // Userspace sees memory that starts at address 0 even if it
                // is offset in the physical memory.
                if offset >= self.userspace_length
                    || length > self.userspace_length
                    || offset + length > self.userspace_length
                {
                    return Err(ErrorCode::INVAL);
                }
            }
            NonvolatileCommand::KernelRead | NonvolatileCommand::KernelWrite => {
                // Because the kernel uses the NonvolatileStorage interface,
                // its calls are absolute addresses.
                if offset < self.kernel_start_address
                    || offset >= self.kernel_start_address + self.kernel_length
                    || length > self.kernel_length
                    || offset + length > self.kernel_start_address + self.kernel_length
                {
                    return Err(ErrorCode::INVAL);
                }
            }
        }

        // Do very different actions if this is a call from userspace
        // or from the kernel.
        match command {
            NonvolatileCommand::UserspaceRead | NonvolatileCommand::UserspaceWrite => {
                processid.map_or(Err(ErrorCode::FAIL), |processid| {
                    self.apps
                        .enter(processid, |app, kernel_data| {
                            // Get the length of the correct allowed buffer.
                            let allow_buf_len = match command {
                                NonvolatileCommand::UserspaceRead => kernel_data
                                    .get_readwrite_processbuffer(rw_allow::READ)
                                    .map_or(0, |read| read.len()),
                                NonvolatileCommand::UserspaceWrite => kernel_data
                                    .get_readonly_processbuffer(ro_allow::WRITE)
                                    .map_or(0, |read| read.len()),
                                _ => 0,
                            };

                            // Check that it exists.
                            if allow_buf_len == 0 || self.buffer.is_none() {
                                return Err(ErrorCode::RESERVE);
                            }

                            // Shorten the length if the application gave us nowhere to
                            // put it.
                            let active_len = cmp::min(length, allow_buf_len);

                            // First need to determine if we can execute this or must
                            // queue it.
                            if self.current_user.is_none() {
                                // No app is currently using the underlying storage.
                                // Mark this app as active, and then execute the command.
                                self.current_user.set(NonvolatileUser::App {
                                    processid: processid,
                                });

                                // Need to copy bytes if this is a write!
                                if command == NonvolatileCommand::UserspaceWrite {
                                    let _ = kernel_data
                                        .get_readonly_processbuffer(ro_allow::WRITE)
                                        .and_then(|write| {
                                            write.enter(|app_buffer| {
                                                self.buffer.map(|kernel_buffer| {
                                                    // Check that the internal buffer and the buffer that was
                                                    // allowed are long enough.
                                                    let write_len =
                                                        cmp::min(active_len, kernel_buffer.len());

                                                    let d = &app_buffer[0..write_len];
                                                    for (i, c) in kernel_buffer[0..write_len]
                                                        .iter_mut()
                                                        .enumerate()
                                                    {
                                                        *c = d[i].get();
                                                    }
                                                });
                                            })
                                        });
                                }

                                self.userspace_call_driver(command, offset, active_len)
                            } else {
                                // Some app is using the storage, we must wait.
                                if app.pending_command == true {
                                    // No more room in the queue, nowhere to store this
                                    // request.
                                    Err(ErrorCode::NOMEM)
                                } else {
                                    // We can store this, so lets do it.
                                    app.pending_command = true;
                                    app.command = command;
                                    app.offset = offset;
                                    app.length = active_len;
                                    Ok(())
                                }
                            }
                        })
                        .unwrap_or_else(|err| Err(err.into()))
                })
            }
            NonvolatileCommand::KernelRead | NonvolatileCommand::KernelWrite => {
                self.kernel_buffer
                    .take()
                    .map_or(Err(ErrorCode::NOMEM), |kernel_buffer| {
                        let active_len = cmp::min(length, kernel_buffer.len());

                        // Check if there is something going on.
                        if self.current_user.is_none() {
                            // Nothing is using this, lets go!
                            self.current_user.set(NonvolatileUser::Kernel);

                            match command {
                                NonvolatileCommand::KernelRead => {
                                    self.driver.read(kernel_buffer, offset, active_len)
                                }
                                NonvolatileCommand::KernelWrite => {
                                    self.driver.write(kernel_buffer, offset, active_len)
                                }
                                _ => Err(ErrorCode::FAIL),
                            }
                        } else {
                            if self.kernel_pending_command.get() == true {
                                Err(ErrorCode::NOMEM)
                            } else {
                                self.kernel_pending_command.set(true);
                                self.kernel_command.set(command);
                                self.kernel_readwrite_length.set(active_len);
                                self.kernel_readwrite_address.set(offset);
                                self.kernel_buffer.replace(kernel_buffer);
                                Ok(())
                            }
                        }
                    })
            }
        }
    }

    fn userspace_call_driver(
        &self,
        command: NonvolatileCommand,
        offset: usize,
        length: usize,
    ) -> Result<(), ErrorCode> {
        // Calculate where we want to actually read from in the physical
        // storage.
        let physical_address = offset + self.userspace_start_address;

        self.buffer
            .take()
            .map_or(Err(ErrorCode::RESERVE), |buffer| {
                // Check that the internal buffer and the buffer that was
                // allowed are long enough.
                let active_len = cmp::min(length, buffer.len());

                // self.current_app.set(Some(processid));
                match command {
                    NonvolatileCommand::UserspaceRead => {
                        self.driver.read(buffer, physical_address, active_len)
                    }
                    NonvolatileCommand::UserspaceWrite => {
                        self.driver.write(buffer, physical_address, active_len)
                    }
                    _ => Err(ErrorCode::FAIL),
                }
            })
    }

    fn check_queue(&self) {
        // Check if there are any pending events.
        if self.kernel_pending_command.get() {
            self.kernel_buffer.take().map(|kernel_buffer| {
                self.kernel_pending_command.set(false);
                self.current_user.set(NonvolatileUser::Kernel);

                match self.kernel_command.get() {
                    NonvolatileCommand::KernelRead => self.driver.read(
                        kernel_buffer,
                        self.kernel_readwrite_address.get(),
                        self.kernel_readwrite_length.get(),
                    ),
                    NonvolatileCommand::KernelWrite => self.driver.write(
                        kernel_buffer,
                        self.kernel_readwrite_address.get(),
                        self.kernel_readwrite_length.get(),
                    ),
                    _ => Err(ErrorCode::FAIL),
                }
            });
        } else {
            // If the kernel is not requesting anything, check all of the apps.
            for cntr in self.apps.iter() {
                let processid = cntr.processid();
                let started_command = cntr.enter(|app, _| {
                    if app.pending_command {
                        app.pending_command = false;
                        self.current_user.set(NonvolatileUser::App {
                            processid: processid,
                        });
                        if let Ok(()) =
                            self.userspace_call_driver(app.command, app.offset, app.length)
                        {
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                });
                if started_command {
                    break;
                }
            }
        }
    }

    fn check_offset_is_in_processes(
        &self,
        offset: usize,
    ) -> Result<(), ErrorCode> {

        // debug!("offset: {}", offset);

        let mut index = 0;

        while index < self.supported_process_num
        {
            //We only refer to the two arrays
            let process_start_address =  self.process_region_start_address[index];
            let process_end_address = self.process_region_start_address[index] + self.process_region_size[index];
            // debug!("Process start address: {}, process end address: {}", process_start_address, process_end_address);

            let target = self.userspace_start_address + offset;
            // debug!("userspace address: {}", self.userspace_start_address);
            // debug!("target address: {}", target);

            if target >= process_start_address && target < process_end_address
            {
                return Err(ErrorCode::INVAL);
            }

            index += 1;

            // debug!("Index: {}", index);
        }

        return Ok(());
    }
}

/// This is the callback client for the underlying physical storage driver.
impl hil::nonvolatile_storage::NonvolatileStorageClient for NonvolatileStorage<'_> {
    fn read_done(&self, buffer: &'static mut [u8], length: usize) {
        // Switch on which user of this capsule generated this callback.
        self.current_user.take().map(|user| {
            match user {
                NonvolatileUser::Kernel => {
                    self.kernel_client.map(move |client| {
                        client.read_done(buffer, length);
                    });
                }
                NonvolatileUser::App { processid } => {
                    let _ = self.apps.enter(processid, move |_, kernel_data| {
                        // Need to copy in the contents of the buffer
                        let _ = kernel_data
                            .get_readwrite_processbuffer(rw_allow::READ)
                            .and_then(|read| {
                                read.mut_enter(|app_buffer| {
                                    let read_len = cmp::min(app_buffer.len(), length);

                                    let d = &app_buffer[0..(read_len as usize)];
                                    for (i, c) in buffer[0..read_len].iter().enumerate() {
                                        d[i].set(*c);
                                    }
                                })
                            });

                        // Replace the buffer we used to do this read.
                        self.buffer.replace(buffer);

                        // And then signal the app.
                        kernel_data
                            .schedule_upcall(upcall::READ_DONE, (length, 0, 0))
                            .ok();
                    });
                }
            }
        });

        self.check_queue();
    }

    fn write_done(&self, buffer: &'static mut [u8], length: usize) {
        // Switch on which user of this capsule generated this callback.
        self.current_user.take().map(|user| {
            match user {
                NonvolatileUser::Kernel => {
                    self.kernel_client.map(move |client| {
                        client.write_done(buffer, length);
                    });
                }
                NonvolatileUser::App { processid } => {
                    let _ = self.apps.enter(processid, move |_app, kernel_data| {
                        // Replace the buffer we used to do this write.
                        self.buffer.replace(buffer);

                        // And then signal the app.
                        kernel_data
                            .schedule_upcall(upcall::WRITE_DONE, (length, 0, 0))
                            .ok();
                    });
                }
            }
        });

        self.check_queue();
    }

     // This function is implemented based on load_processes_advanced
    // the purpose is to parse the dynamically changing start address of flash memory satisfying MPU rules
    fn find_dynamic_start_address_of_writable_flash_advanced(
        &self,
        proc_data: &mut ProcLoaderData,
        start_app: usize,
    ) -> Result<(), ProcessLoadError> {

        proc_data.dynamic_flash_start_addr = start_app;

        while proc_data.dynamic_flash_start_addr < self.end_app 
        {
            let mut is_padding_app: bool = false;
            let mut is_empty_region: bool = false;
            let mut is_remnant_region: bool = true;

            // 1. Check whether or not app_start_address points an padding app
            let res_padding_app = self.is_padding_app(proc_data.dynamic_flash_start_addr);
            match res_padding_app{
                Ok(ispadding) => {
                    if ispadding == true {
                        // If we found this is an padding app, we save the new app from here.
                        // Before return Ok, do validity check
                        is_padding_app = true;
                    }
                }
                Err(_e) => {
                    return Err(ProcessLoadError::InternalError);
                }
            }

            //2. Check whether or not app_start_address points empty flash region (unable to parse)
            let res_empty_region = self.is_empty_flash_region(proc_data.dynamic_flash_start_addr);
            match res_empty_region{
                Ok((isempty, entry_length)) => {
                    if isempty == true {
                        // If we found this is empty flash region, we save the new app from here.
                        // Before return Ok, do validity check
                        is_empty_region = true;
                    }
                    else
                    {
                        // 3. Check whether or not the start address points a remnant app which is not erased by tockloader erase-apps command
                        let mut index = 0;
                        while index < proc_data.index
                        {
                            if proc_data.dynamic_flash_start_addr == unsafe { *self.ptr_process_region_start_address.offset(index.try_into().unwrap()) }
                            {
                                // If we found this is the remnant region(app), we save the new app from here.
                                // Before return Ok, do validity check
                                is_remnant_region = false;
                                break;
                            }
                            index += 1;
                        }

                        // We only increase proc_data.dynamic_flash_start_addr, when there are the existing apps which are actually loaded into PROCESS global array
                        if is_remnant_region == false
                        {   
                            // Jump to the maximum length based on power of 2
                            // If 'tockloader' offers flashing app bundles with MPU subregion rules (e.g., 16k + 32k consecutive), we need more logic!
                            // We have to check whether or not there is subregion, and jump to the start address of 32k (Todo!)
                            proc_data.dynamic_flash_start_addr += cmp::max(proc_data.appsize_requested_by_ota_app, entry_length);
                        }
                        
                    }
                }
                Err(_e) => {
                    return Err(ProcessLoadError::InternalError);
                }
            }

            // 4. Check whether or not the start address invades the other regions occupied by the existing apps
            if is_padding_app == true || is_empty_region == true || is_remnant_region == true
            {
                let address_validity_check = self.check_overlap_region(proc_data);

                    match address_validity_check {
                        Ok(()) => {
                            return Ok(());
                        }
                        Err((new_start_addr, _e)) => {
                            // We try to parse again from the end address + 1 of a existing app (new_start_addr)
                            proc_data.dynamic_flash_start_addr = new_start_addr;
                        }
                    }
            }
        }

        //If we cannot parse a vaild start address satisfying MPU rules, we return Error.
        return Err(ProcessLoadError::NotEnoughFlash);
    }

    fn is_padding_app(
        &self,
        start_addr: usize,
    )-> Result<bool, ProcessLoadError> {
        
        //We only need tbf header information to get the size of app which is already loaded
        let header_info = unsafe {
            core::slice::from_raw_parts(
                start_addr as *const u8,
                8,
            )
        };

        let test_header_slice = match header_info.get(0..8) {
            Some(s) => s,
            None => {
                // Not enough flash to test for another app. This just means
                // We are at the end of flash (0x80000). => This case is Error!
                // But we can't reach out to here in this while statement!
                return Err(ProcessLoadError::InternalError);
            }
        };

        // Pass the first eight bytes to tbfheader to parse out the length of
        // the tbf header and app. We then use those values to see if we have
        // enough flash remaining to parse the remainder of the header.
        let (version, header_length, _entry_length) = match tock_tbf::parse::parse_tbf_header_lengths(
            test_header_slice
                .try_into()
                .or(Err(ProcessLoadError::InternalError))?,
        ) {
            Ok((v, hl, el)) => (v, hl, el),
            Err(tock_tbf::types::InitialTbfParseError::InvalidHeader(_entry_length)) => {
                // If we could not parse the header, then we want to skip over
                // this app and look for the next one.
                return Err(ProcessLoadError::InternalError); 
            }
            Err(tock_tbf::types::InitialTbfParseError::UnableToParse) => {
                // Since Tock apps use a linked list, it is very possible the
                // header we started to parse is intentionally invalid to signal
                // the end of apps. This is ok and just means we have finished
                // loading apps.
                // => After increasing start_addr, it means that it is empty region!
                return Ok(false);
            }
        };


        //If a padding app is exist at the start address satisfying MPU rules, we load the new app from here!
        let header_flash = unsafe {
            core::slice::from_raw_parts(
                start_addr as *const u8,
                header_length as usize,
            )
        };

        let tbf_header = tock_tbf::parse::parse_tbf_header(header_flash, version)?;

        // If this isn't an app (i.e. it is padding)
        if !tbf_header.is_app() {
            return Ok(true);
        }

        return Ok(false);
    }

    fn is_empty_flash_region(
        &self,
        start_addr: usize,
    )-> Result<(bool, usize), ProcessLoadError> {
        
        //We only need tbf header information to get the size of app which is already loaded
        let header_info = unsafe {
            core::slice::from_raw_parts(
                start_addr as *const u8,
                8,
            )
        };

        let test_header_slice = match header_info.get(0..8) {
            Some(s) => s,
            None => {
                // Not enough flash to test for another app. This just means
                // We are at the end of flash (0x80000). => This case is Error!
                // But we can't reach out to here in this while statement!
                return Err(ProcessLoadError::NotEnoughFlash);
            }
        };

        let (_version, _header_length, entry_length) = match tock_tbf::parse::parse_tbf_header_lengths(
            test_header_slice
                .try_into()
                .or(Err(ProcessLoadError::InternalError))?,
        ) {
            Ok((v, hl, el)) => (v, hl, el),
            Err(tock_tbf::types::InitialTbfParseError::InvalidHeader(_entry_length)) => {
                // If we could not parse the header, then we want to skip over
                // this app and look for the next one.
                return Err(ProcessLoadError::InternalError);
            }
            Err(tock_tbf::types::InitialTbfParseError::UnableToParse) => {
                // Since Tock apps use a linked list, it is very possible the
                // header we started to parse is intentionally invalid to signal
                // the end of apps. This is ok and just means we have finished
                // loading apps. => This case points the writable 'start_addr' satisfying MPU rules for an new app
                return Ok((true, 0));
            }
        };

        return Ok((false, entry_length as usize));
    }

    // In order to match the result value of command
    fn find_dynamic_start_address_of_writable_flash(
        &self,
        proc_data: &mut ProcLoaderData,
    ) -> Result<(), ErrorCode> {
        
        //First, we check Index validity  
        if proc_data.index >= self.supported_process_num
        {
            return Err(ErrorCode::FAIL); 
        }

        //If there is enough room in PROCESS array, we start to find a start address satisfying MPU rules
        let res = self.find_dynamic_start_address_of_writable_flash_advanced(proc_data, self.start_app);

        match res{
            Ok(()) => {
                return Ok(());
            }
            Err(_e) => {
                return Err(ErrorCode::FAIL);
            }
        }
    }

    // Check validity of 'proc_data.dynamic_flash_start_addr'
    fn check_overlap_region(
        &self,
        proc_data: &mut ProcLoaderData,
    ) -> Result<(), (usize, ProcessLoadError)>{
        
        let mut index = 0;
        let new_process_start_address = proc_data.dynamic_flash_start_addr;
        // debug!("New proc start addr = {}", new_process_start_address);
        let new_process_end_address = proc_data.dynamic_flash_start_addr + proc_data.appsize_requested_by_ota_app - 1;
        // debug!("New proc end addr = {}", new_process_end_address);

        while index < proc_data.index
        {
            let process_start_address = unsafe { *self.ptr_process_region_start_address.offset(index.try_into().unwrap()) };
            let process_end_address = unsafe{ (*self.ptr_process_region_start_address.offset(index.try_into().unwrap()) + *self.ptr_process_region_size.offset(index.try_into().unwrap())) -1};

            debug!("process_start_address, process_end_address, {:#010X} {:#010X}", process_start_address, process_end_address);
            debug!("new_process_start_address, new_process_end_address, {:#010X} {:#010X}", new_process_start_address, new_process_end_address);

            if new_process_end_address >= process_start_address && new_process_end_address <= process_end_address          
            {
                /* Case 1
                *              _________________          _______________           _________________
                *  ___________|__               |        |              _|_________|__               |
                * |           |  |              |        |             | |         |  |              |
                * |   new app |  |  app2        |   or   |   app1      | | new app |  |  app2        | 
                * |___________|__|              |        |             |_|_________|__|              |
                *             |_________________|        |_______________|         |_________________|
                * 
                * ^...........^                                           ^........^
                * In this case, we discard this region, and we try to find another start address from the end address + 1 of app2
                */
                
                return Err((process_end_address + 1, ProcessLoadError::NotEnoughFlash));
            }

            else if new_process_start_address >= process_start_address && new_process_start_address <= process_end_address
            {
                /* Case 2
                *              _________________
                *  ___________|__               |    _______________
                * |           |  |              |   |               |
                * |   app2    |  |  new app     |   |     app3      |         
                * |___________|__|              |   |_______________|
                *             |_________________|
                * 
                *                 ^
                *                 | In this case, the start address of new app is replaced by 'the end address + 1' of app2, and retry to find another start address from the end address + 1 of app2
                */
                return Err((process_end_address + 1, ProcessLoadError::NotEnoughFlash));
            }

            index += 1;
        }

        return Ok(());
    }

    // CRC32_POSIX
    fn cal_crc32_posix(
        &self,
        start_address: usize,
        mode: usize,
    ) -> u32 {

        let appstart = start_address as *const u8;

        //Only parse the header information (8byte)
        let header_slice =  unsafe {
            core::slice::from_raw_parts(
            appstart,
            8,
        )};
    
        let entry_length = if mode == 1 {
            u32::from_le_bytes([header_slice[4], header_slice[5], header_slice[6], header_slice[7]])
        }
        else {
            // In case of Crc32 for an padding app, we calculate only 1 pasge size (512)
            512 
        };
        
        let data =  unsafe {
            core::slice::from_raw_parts(
            appstart,
            entry_length.try_into().unwrap(),
        )};

        let mut crc32_instance = tickv::crc32::Crc32::new();
        crc32_instance.update(data);
        
        let crc32_rst = crc32_instance.finalise();

        return crc32_rst;
    }

    fn kernel_version(
        &self,
    ) -> Result<u32, ErrorCode> {
        
        let header_info = unsafe {
            core::slice::from_raw_parts(
                self.start_app as *const u8,
                8,
            )
        };

        let header_slice = match header_info.get(0..8) {
            Some(s) => s,
            None => {
                return Err(ErrorCode::FAIL);
            }
        };
        
        let (version, _header_length, _entry_length) = match tock_tbf::parse::parse_tbf_header_lengths(
            header_slice
                .try_into()
                .or(Err(ErrorCode::FAIL))?,
        ) {
            Ok((v, hl, el)) => (v, hl, el),
            Err(tock_tbf::types::InitialTbfParseError::InvalidHeader(_entry_length)) => {
                return Err(ErrorCode::FAIL);
            }
            Err(tock_tbf::types::InitialTbfParseError::UnableToParse) => {
                return Err(ErrorCode::FAIL);
            }
        };

        return Ok(version as u32);
    }

    fn get_flash_addr_and_len(
        &self,
        app_size: usize
    ) -> Result<(), ErrorCode> {
        
    }

}

// /// Provide an interface for the kernel.
// impl<'a> hil::nonvolatile_storage::NonvolatileStorage<'a> for NonvolatileStorage<'a> {
//     fn set_client(&self, client: &'a dyn hil::nonvolatile_storage::NonvolatileStorageClient) {
//         self.kernel_client.set(client);
//     }

//     fn read(
//         &self,
//         buffer: &'static mut [u8],
//         address: usize,
//         length: usize,
//     ) -> Result<(), ErrorCode> {
//         self.kernel_buffer.replace(buffer);
//         self.enqueue_command(NonvolatileCommand::KernelRead, address, length, None)
//     }

//     fn write(
//         &self,
//         buffer: &'static mut [u8],
//         address: usize,
//         length: usize,
//     ) -> Result<(), ErrorCode> {
//         self.kernel_buffer.replace(buffer);
//         self.enqueue_command(NonvolatileCommand::KernelWrite, address, length, None)
//     }
// }

/// Provide an interface for userland.
impl SyscallDriver for DynamicProcessLoader<'_> {
    /// Command interface.
    ///
    /// Commands are selected by the lowest 8 bits of the first argument.
    ///
    /// ### `command_num`
    ///
    /// - `0`: Return Ok(()) if this driver is included on the platform.
    /// - `1`: Return the number of bytes available to userspace.
    /// - `2`: Start a read from the nonvolatile storage.
    /// - `3`: Start a write to the nonvolatile_storage.
    fn command(
        &self,
        command_num: usize,
        arg1: usize,        // offset for nonvol
        arg2: usize,      // length for nonvol
        processid: ProcessId,
    ) -> CommandReturn {
        match command_num {
            0 => CommandReturn::success(),

            1 => {
                // provide with app name and size
                let res = self.get_flash_addr_and_len(arg2)
            }

            2 => {
                // write app to flash

            }

            1 => {
                // How many bytes are accessible from userspace 
                // TODO: Would break on 64-bit platforms
                CommandReturn::success_u32(self.userspace_length as u32)
            }

            2 => {
                // Issue a read command
                let res = self.enqueue_command(
                    NonvolatileCommand::UserspaceRead,
                    arg1,
                    arg2,
                    Some(processid),
                );

                match res {
                    Ok(()) => CommandReturn::success(),
                    Err(e) => CommandReturn::failure(e),
                }
            }

            3 => {
                // Issue a write command
                //     let res = self.enqueue_command(
                //         NonvolatileCommand::UserspaceWrite,
                //         offset,
                //         length,
                //         Some(processid),
                //     );

                //     match res {
                //         Ok(()) => CommandReturn::success(),
                //         Err(e) => CommandReturn::failure(e),
                //     }
                // }

                let offset_validity = self.check_offset_is_in_processes(arg1);
                // debug!("offset: {}, length: {}, pid: {:?}", offset, length, processid);
                // debug!("Offset validity: {:?}",offset_validity);
                match offset_validity {
                    Ok(()) => {
                        let res =
                            self.enqueue_command(
                                NonvolatileCommand::UserspaceWrite,
                                arg1,
                                arg2,
                                Some(processid),
                            );
                            // debug!("write command result: {:?}",res);

                        match res {
                            Ok(()) => CommandReturn::success(),
                            Err(e) => CommandReturn::failure(e),
                        }
                    }
                    Err(e) => CommandReturn::failure(e),
                }
            }

            4 => {
                //Find the start address and length dedicated for the process out writable flash */
                let res = self.data.enter(appid, |proc_data, _| {
                    proc_data.appsize_requested_by_ota_app = arg1;
                    self.find_dynamic_start_address_of_writable_flash(proc_data)
                })
                .map_err(ErrorCode::from);
        
                match res {
                    Ok(Ok(())) => CommandReturn::success(),
                    Ok(Err(e)) => CommandReturn::failure(e),
                    Err(e) => CommandReturn::failure(e),
                }

            }

            _ => CommandReturn::failure(ErrorCode::NOSUPPORT),
        }
    }

    fn allocate_grant(&self, processid: ProcessId) -> Result<(), kernel::process::Error> {
        self.apps.enter(processid, |_, _| {})
    }
}
