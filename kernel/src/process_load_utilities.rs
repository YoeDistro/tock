//! Helper functions related to Tock processes by OTA_app.

use core::cell::Cell;
use core::cmp;

use crate::capabilities::MemoryAllocationCapability;
use crate::config;
use crate::create_capability;
use crate::debug;
use crate::grant::{AllowRoCount, AllowRwCount, Grant, UpcallCount};
use crate::kernel::Kernel;
use crate::platform::chip::Chip;
use crate::process::ProcessId;
use crate::process::{self, Process};
use crate::process_loading::ProcessLoadError;
use crate::process_policies::ProcessFaultPolicy;
use crate::process_standard::ProcessStandard;
use crate::syscall_driver::{CommandReturn, SyscallDriver};
use crate::utilities::cells::{MapCell, OptionalCell};
use crate::ErrorCode;

// use core::convert::TryInto;
// use core::fmt;

// use crate::capabilities::{ProcessApprovalCapability, ProcessManagementCapability};
// use crate::config;
// use crate::create_capability;
// use crate::debug;
// use crate::kernel::{Kernel, ProcessCheckerMachine};
// use crate::platform::chip::Chip;
// use crate::platform::platform::KernelResources;
// use crate::process::{Process, ShortID};
// use crate::process_checker::AppCredentialsChecker;
// use crate::process_policies::ProcessFaultPolicy;
// use crate::process_standard::ProcessStandard;

/// This interface supports loading processes at runtime.
pub trait DynamicProcessLoading {
    /// Call to request loading a new process.
    ///
    /// This informs the kernel we want to load a process and the size of the entire process binary.
    /// The kernel will try to find a suitable location in flash to store said process.
    ///
    /// Return value:
    /// - `Ok((start_address, length))`: If there is a place to load the
    ///   process, the function will return `Ok()` with the address to start at
    ///   and the size of the region to store the process.
    /// - `Err(ErrorCode)`: If there is nowhere to store the process a suitable
    ///   `ErrorCode` will be returned.
    fn setup(&self, app_length: usize) -> Result<(usize, usize), ErrorCode>;

    /// Instruct the kernel to load and execute the process.
    ///
    /// This tells the kernel to try to actually execute the new process that
    /// was just installed in flash from the preceding `setup()` call.
    fn load(&self) -> Result<(), ErrorCode>;
}

pub struct DynamicProcessLoader<C: 'static + Chip> {
    kernel: &'static Kernel,
    chip: &'static C,
    fault_policy: &'static dyn ProcessFaultPolicy,

    procs: MapCell<&'static mut [Option<&'static dyn process::Process>]>,
    flash: Cell<&'static [u8]>,
    app_memory: Cell<&'static mut [u8]>,
    new_process_flash: OptionalCell<&'static [u8]>,
}

impl<C: 'static + Chip> DynamicProcessLoader<C> {
    pub fn new(
        processes: &'static mut [Option<&'static dyn process::Process>],
        kernel: &'static Kernel,
        chip: &'static C,
        flash: &'static [u8],
        app_memory: &'static mut [u8],
        fault_policy: &'static dyn ProcessFaultPolicy,
    ) -> Self {
        Self {
            procs: MapCell::new(processes),
            kernel,
            chip,
            flash: Cell::new(flash),
            app_memory: Cell::new(app_memory),
            fault_policy,
            new_process_flash: OptionalCell::empty(),
        }
    }

    fn find_open_process_slot(&self) -> Option<usize> {
        self.procs.map_or(None, |procs| {
            for (i, p) in procs.iter().enumerate() {
                if p.is_none() {
                    return Some(i);
                }
            }
            None
        })
    }
}

impl<C: 'static + Chip> DynamicProcessLoading for DynamicProcessLoader<C> {
    fn setup(&self, app_length: usize) -> Result<(usize, usize), ErrorCode> {
        // EXAMPLE: very simple fixed location
        // Ok((0x50000, app_length))

        let flash_start = self.flash.get().as_ptr() as usize;
        let offset = 0x50000 - flash_start;

        let new_process_flash = self
            .flash
            .get()
            .get(offset..offset + app_length)
            .ok_or(ErrorCode::FAIL)?;
        let new_process_flash_start = new_process_flash.as_ptr() as usize;

        self.new_process_flash.set(new_process_flash);

        Ok((new_process_flash_start, app_length))
    }

    fn load(&self) -> Result<(), ErrorCode> {
        let index = self.find_open_process_slot().ok_or(ErrorCode::FAIL)?;
        let process_flash = self.new_process_flash.take().ok_or(ErrorCode::FAIL)?;
        let remaining_memory = self.app_memory.take();

        // Get the first eight bytes of flash to check if there is another app.
        let test_header_slice = match process_flash.get(0..8) {
            Some(s) => s,
            None => {
                // Not enough flash to test for another app. This just means
                // we are at the end of flash, and there are no more apps to
                // load. => This case is error in loading app by ota_app, because it means that there is no valid tbf header!
                return Err(ErrorCode::FAIL);
            }
        };

        // Pass the first eight bytes to tbfheader to parse out the length of
        // the tbf header and app. We then use those values to see if we have
        // enough flash remaining to parse the remainder of the header.
        let (version, header_length, entry_length) = match tock_tbf::parse::parse_tbf_header_lengths(
            test_header_slice.try_into().or(Err(ErrorCode::FAIL))?,
        ) {
            Ok((v, hl, el)) => (v, hl, el),
            Err(tock_tbf::types::InitialTbfParseError::InvalidHeader(_entry_length)) => {
                // If we could not parse the header, then we want to skip over
                // this app and look for the next one. => This case is error in loading app by ota_app
                return Err(ErrorCode::FAIL);
            }
            Err(tock_tbf::types::InitialTbfParseError::UnableToParse) => {
                // Since Tock apps use a linked list, it is very possible the
                // header we started to parse is intentionally invalid to signal
                // the end of apps. This is ok and just means we have finished
                // loading apps. => This case is error in loading app by ota_app
                return Err(ErrorCode::FAIL);
            }
        };

        // Now we can get a slice which only encompasses the length of flash
        // described by this tbf header.  We will either parse this as an actual
        // app, or skip over this region.
        let entry_flash = process_flash
            .get(0..entry_length as usize)
            .ok_or(ErrorCode::FAIL)?;

        // Need to reassign remaining_memory in every iteration so the compiler
        // knows it will not be re-borrowed.
        if header_length > 0 {
            // If we found an actual app header, try to create a `Process`
            // object. We also need to shrink the amount of remaining memory
            // based on whatever is assigned to the new process if one is
            // created.

            // Try to create a process object from that app slice. If we don't
            // get a process and we didn't get a loading error (aka we got to
            // this point), then the app is a disabled process or just padding.
            let process_option = unsafe {
                let result = ProcessStandard::create(
                    self.kernel,
                    self.chip,
                    entry_flash,
                    header_length as usize,
                    version,
                    remaining_memory,
                    self.fault_policy,
                    true,
                    index,
                );
                match result {
                    Ok((process_option, unused_memory)) => {
                        self.app_memory.set(unused_memory);
                        process_option
                    }
                    Err((_err, unused_memory)) => {
                        self.app_memory.set(unused_memory);
                        return Err(ErrorCode::FAIL);
                    }
                }
            };
            process_option.map(|process| {
                if config::CONFIG.debug_load_processes {
                    let addresses = process.get_addresses();
                        debug!(
                        "Loaded process[{}] from flash={:#010X}-{:#010X} into sram={:#010X}-{:#010X} = {:?}",
                        index,
                        entry_flash.as_ptr() as usize,
                        entry_flash.as_ptr() as usize + entry_flash.len() - 1,
                        addresses.sram_start,
                        addresses.sram_end - 1,
                        process.get_process_name()
                    );
                }
            });

            // //we return sram_end_addresses
            // let addresses = process.get_addresses();
            // sram_end_addresses = addresses.sram_end;

            // //we return process_copy
            // process_copy = Some(process);

            // self.app_memory.set(unused_memory);
            self.procs.map(|procs| procs[index] = process_option);

            let capability = create_capability!(crate::capabilities::ProcessApprovalCapability);
            self.procs.map(|procs| {
                procs[index].map(|p| {
                    p.mark_credentials_pass(
                        None,
                        crate::process::ShortID::LocallyUnique,
                        &capability,
                    );
                    if config::CONFIG.debug_process_credentials {
                        debug!("Running {}", p.get_process_name());
                    }
                });
            });
        } else {
            //header length 0 means invalid header
            return Err(ErrorCode::FAIL);
        }

        Ok(())
    }
}

//
//
//
//
// legacy
//
//
//
//
//

/// Variables that are stored in OTA_app grant region to support dynamic app load
#[derive(Default)]
struct ProcLoaderData {
    //Index points the position where the entry point of a new app is written into PROCESS global array
    index: usize,
    // App size requested by ota app
    appsize_requested_by_ota_app: usize,
    // dynamic_flash_start_addr points the start address that a new app will be loaded
    dynamic_flash_start_addr: usize,
    // dynamic_unsued_sram_start_addr points the start address that a new app will use
    dynamic_unsued_sram_start_addr: usize,
}

pub struct ProcessLoader<C: 'static + Chip> {
    kernel: &'static Kernel,
    chip: &'static C,
    fault_policy: &'static dyn ProcessFaultPolicy,
    ptr_process: *mut Option<&'static (dyn Process + 'static)>,
    ptr_process_region_start_address: *mut usize,
    ptr_process_region_size: *mut usize,
    supported_process_num: usize,
    start_app: usize,
    end_app: usize,
    end_appmem: usize,
    dynamic_unused_ram_start_addr_init_val: &'static usize,
    index_init_val: &'static usize,
}

impl<C: 'static + Chip> ProcessLoader<C> {
    pub fn init(
        kernel: &'static Kernel,
        chip: &'static C,
        fault_policy: &'static dyn ProcessFaultPolicy,
        memcapability: &dyn MemoryAllocationCapability,
        ptr_process: *mut Option<&'static (dyn Process + 'static)>,
        ptr_process_region_start_address: *mut usize,
        ptr_process_region_size: *mut usize,
        supported_process_num: usize,
        start_app: usize,
        end_app: usize,
        end_appmem: usize,
        dynamic_unused_ram_start_addr_init_val: &'static usize,
        index_init_val: &'static usize,
    ) -> ProcessLoader<C> {
        ProcessLoader {
            kernel: kernel,
            chip: chip,
            fault_policy: fault_policy,
            ptr_process: ptr_process,
            ptr_process_region_start_address: ptr_process_region_start_address,
            ptr_process_region_size: ptr_process_region_size,
            supported_process_num: supported_process_num,
            start_app: start_app,
            end_app: end_app,
            end_appmem: end_appmem,
            dynamic_unused_ram_start_addr_init_val: dynamic_unused_ram_start_addr_init_val,
            index_init_val: index_init_val,
        }
    }

    // This function is implemented based on load_processes_advanced
    // the purpose is to load an application flashed from OTA_app into PROCESS global array
    fn load_processes_advanced_air(
        &self,
        proc_data: &mut ProcLoaderData,
    ) -> Result<(usize, Option<&'static dyn Process>), ProcessLoadError> {
        let appstart = proc_data.dynamic_flash_start_addr as *const u8;
        let appsramstart = proc_data.dynamic_unsued_sram_start_addr as *mut u8;

        let mut sram_end_addresses = 0;
        let mut process_copy: Option<&'static dyn Process> = None;

        //Todo: self.eapps has to be replaced by the end address of the flahsed app? (can reduce the ram usage)
        let remaining_flash =
            unsafe { core::slice::from_raw_parts(appstart, self.end_app - appstart as usize) };

        let remaining_memory = unsafe {
            core::slice::from_raw_parts_mut(appsramstart, self.end_appmem - appsramstart as usize)
        };

        if proc_data.index < self.supported_process_num {
            // Get the first eight bytes of flash to check if there is another app.
            let test_header_slice = match remaining_flash.get(0..8) {
                Some(s) => s,
                None => {
                    // Not enough flash to test for another app. This just means
                    // we are at the end of flash, and there are no more apps to
                    // load. => This case is error in loading app by ota_app, because it means that there is no valid tbf header!
                    return Err(ProcessLoadError::InternalError);
                }
            };

            // Pass the first eight bytes to tbfheader to parse out the length of
            // the tbf header and app. We then use those values to see if we have
            // enough flash remaining to parse the remainder of the header.
            let (version, header_length, entry_length) =
                match tock_tbf::parse::parse_tbf_header_lengths(
                    test_header_slice
                        .try_into()
                        .or(Err(ProcessLoadError::InternalError))?,
                ) {
                    Ok((v, hl, el)) => (v, hl, el),
                    Err(tock_tbf::types::InitialTbfParseError::InvalidHeader(_entry_length)) => {
                        // If we could not parse the header, then we want to skip over
                        // this app and look for the next one. => This case is error in loading app by ota_app
                        return Err(ProcessLoadError::InternalError);
                    }
                    Err(tock_tbf::types::InitialTbfParseError::UnableToParse) => {
                        // Since Tock apps use a linked list, it is very possible the
                        // header we started to parse is intentionally invalid to signal
                        // the end of apps. This is ok and just means we have finished
                        // loading apps. => This case is error in loading app by ota_app
                        return Err(ProcessLoadError::InternalError);
                    }
                };

            // Now we can get a slice which only encompasses the length of flash
            // described by this tbf header.  We will either parse this as an actual
            // app, or skip over this region.
            let entry_flash = remaining_flash
                .get(0..entry_length as usize)
                .ok_or(ProcessLoadError::NotEnoughFlash)?;

            // Need to reassign remaining_memory in every iteration so the compiler
            // knows it will not be re-borrowed.
            if header_length > 0 {
                // If we found an actual app header, try to create a `Process`
                // object. We also need to shrink the amount of remaining memory
                // based on whatever is assigned to the new process if one is
                // created.

                // Try to create a process object from that app slice. If we don't
                // get a process and we didn't get a loading error (aka we got to
                // this point), then the app is a disabled process or just padding.
                let (process_option, _unused_memory) = unsafe {
                    let result = ProcessStandard::create(
                        self.kernel,
                        self.chip,
                        entry_flash,
                        header_length as usize,
                        version,
                        remaining_memory,
                        self.fault_policy,
                        true,
                        proc_data.index,
                    );
                    match result {
                        Ok(tuple) => tuple,
                        Err((err, _memory)) => {
                            return Err(err);
                        }
                    }
                };
                process_option.map(|process| {
                    if config::CONFIG.debug_load_processes {
                        let addresses = process.get_addresses();
                            debug!(
                            "Loaded process[{}] from flash={:#010X}-{:#010X} into sram={:#010X}-{:#010X} = {:?}",
                            proc_data.index,
                            entry_flash.as_ptr() as usize,
                            entry_flash.as_ptr() as usize + entry_flash.len() - 1,
                            addresses.sram_start,
                            addresses.sram_end - 1,
                            process.get_process_name()
                        );
                    }

                    //we return sram_end_addresses
                    let addresses = process.get_addresses();
                    sram_end_addresses = addresses.sram_end;

                    //we return process_copy
                    process_copy = Some(process);
                });
            } else {
                //header length 0 means invalid header
                return Err(ProcessLoadError::InternalError);
            }
        }

        Ok((sram_end_addresses, process_copy))
    }

    // In order to match the result value of command
    fn load_processes_air(&self, proc_data: &mut ProcLoaderData) -> Result<(), ErrorCode> {
        let res = self.load_processes_advanced_air(proc_data);

        // Without alignment error, we only store the entry point, the start address, and the size of the flashed application
        match res {
            Ok((sram_end, process_copy)) => {
                // This variable will be used, when loading the another app at next load work by ota app
                // This is necessary to prevent the access violation of sram memory whilch are already used by kernel and other apps.
                proc_data.dynamic_unsued_sram_start_addr = sram_end;

                // Store the entry point, the start address, and the size of the flashed application into PROCESS global array
                // Although I used unsafe keyword, I think it's okay, becasue we pass the exact pointer of PROCESS global array
                unsafe {
                    *self.ptr_process.offset(proc_data.index.try_into().unwrap()) = process_copy;

                    // We also save process region information to check the validity of 'proc_data.dynamic_flash_start_addr' in future load work
                    *self
                        .ptr_process_region_start_address
                        .offset(proc_data.index.try_into().unwrap()) =
                        proc_data.dynamic_flash_start_addr;
                    *self
                        .ptr_process_region_size
                        .offset(proc_data.index.try_into().unwrap()) =
                        proc_data.appsize_requested_by_ota_app;
                }

                // We increase the index for next load work by OTA app
                proc_data.index += 1;

                return Ok(());
            }
            Err(_e) => {
                // If there is an error caused by misalignment,
                // 'proc_data.dynamic_unsued_sram_start_addr' and 'proc_data.index' will hold current unused sram start address
                return Err(ErrorCode::FAIL);
            }
        }
    }

    // This function is implemented based on load_processes_advanced
    // the purpose is to parse the dynamically changing start address of flash memory satisfying MPU rules
    fn find_dynamic_start_address_of_writable_flash_advanced(
        &self,
        proc_data: &mut ProcLoaderData,
        start_app: usize,
    ) -> Result<(), ProcessLoadError> {
        proc_data.dynamic_flash_start_addr = start_app;

        while proc_data.dynamic_flash_start_addr < self.end_app {
            let mut is_padding_app: bool = false;
            let mut is_empty_region: bool = false;
            let mut is_remnant_region: bool = true;

            // 1. Check whether or not app_start_address points an padding app
            let res_padding_app = self.is_padding_app(proc_data.dynamic_flash_start_addr);
            match res_padding_app {
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
            match res_empty_region {
                Ok((isempty, entry_length)) => {
                    if isempty == true {
                        // If we found this is empty flash region, we save the new app from here.
                        // Before return Ok, do validity check
                        is_empty_region = true;
                    } else {
                        // 3. Check whether or not the start address points a remnant app which is not erased by tockloader erase-apps command
                        let mut index = 0;
                        while index < proc_data.index {
                            if proc_data.dynamic_flash_start_addr
                                == unsafe {
                                    *self
                                        .ptr_process_region_start_address
                                        .offset(index.try_into().unwrap())
                                }
                            {
                                // If we found this is the remnant region(app), we save the new app from here.
                                // Before return Ok, do validity check
                                is_remnant_region = false;
                                break;
                            }
                            index += 1;
                        }

                        // We only increase proc_data.dynamic_flash_start_addr, when there are the existing apps which are actually loaded into PROCESS global array
                        if is_remnant_region == false {
                            // Jump to the maximum length based on power of 2
                            // If 'tockloader' offers flashing app bundles with MPU subregion rules (e.g., 16k + 32k consecutive), we need more logic!
                            // We have to check whether or not there is subregion, and jump to the start address of 32k (Todo!)
                            proc_data.dynamic_flash_start_addr +=
                                cmp::max(proc_data.appsize_requested_by_ota_app, entry_length);
                        }
                    }
                }
                Err(_e) => {
                    return Err(ProcessLoadError::InternalError);
                }
            }

            // 4. Check whether or not the start address invades the other regions occupied by the existing apps
            if is_padding_app == true || is_empty_region == true || is_remnant_region == true {
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

    fn is_padding_app(&self, start_addr: usize) -> Result<bool, ProcessLoadError> {
        //We only need tbf header information to get the size of app which is already loaded
        let header_info = unsafe { core::slice::from_raw_parts(start_addr as *const u8, 8) };

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
        let (version, header_length, _entry_length) =
            match tock_tbf::parse::parse_tbf_header_lengths(
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
        let header_flash =
            unsafe { core::slice::from_raw_parts(start_addr as *const u8, header_length as usize) };

        let tbf_header = tock_tbf::parse::parse_tbf_header(header_flash, version)?;

        // If this isn't an app (i.e. it is padding)
        if !tbf_header.is_app() {
            return Ok(true);
        }

        return Ok(false);
    }

    fn is_empty_flash_region(&self, start_addr: usize) -> Result<(bool, usize), ProcessLoadError> {
        //We only need tbf header information to get the size of app which is already loaded
        let header_info = unsafe { core::slice::from_raw_parts(start_addr as *const u8, 8) };

        let test_header_slice = match header_info.get(0..8) {
            Some(s) => s,
            None => {
                // Not enough flash to test for another app. This just means
                // We are at the end of flash (0x80000). => This case is Error!
                // But we can't reach out to here in this while statement!
                return Err(ProcessLoadError::NotEnoughFlash);
            }
        };

        let (_version, _header_length, entry_length) =
            match tock_tbf::parse::parse_tbf_header_lengths(
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
        if proc_data.index >= self.supported_process_num {
            return Err(ErrorCode::FAIL);
        }

        //If there is enough room in PROCESS array, we start to find a start address satisfying MPU rules
        let res =
            self.find_dynamic_start_address_of_writable_flash_advanced(proc_data, self.start_app);

        match res {
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
    ) -> Result<(), (usize, ProcessLoadError)> {
        let mut index = 0;
        let new_process_start_address = proc_data.dynamic_flash_start_addr;
        let new_process_end_address =
            proc_data.dynamic_flash_start_addr + proc_data.appsize_requested_by_ota_app - 1;

        while index < proc_data.index {
            let process_start_address = unsafe {
                *self
                    .ptr_process_region_start_address
                    .offset(index.try_into().unwrap())
            };
            let process_end_address = unsafe {
                (*self
                    .ptr_process_region_start_address
                    .offset(index.try_into().unwrap())
                    + *self
                        .ptr_process_region_size
                        .offset(index.try_into().unwrap()))
                    - 1
            };

            //debug!("process_start_address, process_end_address, {:#010X} {:#010X}", process_start_address, process_end_address);
            //debug!("new_process_start_address, new_process_end_address, {:#010X} {:#010X}", new_process_start_address, new_process_end_address);

            if new_process_end_address >= process_start_address
                && new_process_end_address <= process_end_address
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
            } else if new_process_start_address >= process_start_address
                && new_process_start_address <= process_end_address
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
    fn cal_crc32_posix(&self, start_address: usize, mode: usize) -> u32 {
        let appstart = start_address as *const u8;

        //Only parse the header information (8byte)
        let header_slice = unsafe { core::slice::from_raw_parts(appstart, 8) };

        let entry_length = if mode == 1 {
            u32::from_le_bytes([
                header_slice[4],
                header_slice[5],
                header_slice[6],
                header_slice[7],
            ])
        } else {
            // In case of Crc32 for an padding app, we calculate only 1 pasge size (512)
            512
        };

        let data =
            unsafe { core::slice::from_raw_parts(appstart, entry_length.try_into().unwrap()) };

        // let mut crc32_instance = tickv::crc32::Crc32::new();
        // crc32_instance.update(data);

        // let crc32_rst = crc32_instance.finalise();

        return crate::utilities::helpers::crc32_posix(data);
    }

    fn kernel_version(&self) -> Result<u32, ErrorCode> {
        let header_info = unsafe { core::slice::from_raw_parts(self.start_app as *const u8, 8) };

        let header_slice = match header_info.get(0..8) {
            Some(s) => s,
            None => {
                return Err(ErrorCode::FAIL);
            }
        };

        let (version, _header_length, _entry_length) =
            match tock_tbf::parse::parse_tbf_header_lengths(
                header_slice.try_into().or(Err(ErrorCode::FAIL))?,
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
}
