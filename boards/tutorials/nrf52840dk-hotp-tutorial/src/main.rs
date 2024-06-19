// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2024.

//! Tock kernel for the Nordic Semiconductor nRF52840 development kit (DK).

#![no_std]
// Disable this attribute when documenting, as a workaround for
// https://github.com/rust-lang/rust/issues/62184.
#![cfg_attr(not(doc), no_main)]
#![deny(missing_docs)]

use core::ptr::addr_of_mut;
use kernel::component::Component;
use kernel::hil::usb::Client;
use kernel::platform::{KernelResources, SyscallDriverLookup};
use kernel::static_init;
use kernel::{capabilities, create_capability};
use nrf52840dk_lib::{self, PROCESSES};

// State for loading and holding applications.
// How should the kernel respond when a process faults.
const FAULT_RESPONSE: capsules_system::process_policies::PanicFaultPolicy =
    capsules_system::process_policies::PanicFaultPolicy {};

// USB Keyboard HID - for nRF52840dk
type UsbHw = nrf52840::usbd::Usbd<'static>; // For any nRF52840 board.
type KeyboardHidDriver = components::keyboard_hid::KeyboardHidComponentType<UsbHw>;

// HMAC
type HmacSha256Software = components::hmac::HmacSha256SoftwareComponentType<
    capsules_extra::sha256::Sha256Software<'static>,
>;
type HmacDriver = components::hmac::HmacComponentType<HmacSha256Software, 32>;

struct Platform {
    keyboard_hid_driver: &'static KeyboardHidDriver,
    hmac: &'static HmacDriver,
    base: nrf52840dk_lib::Platform,
}

const KEYBOARD_HID_DRIVER_NUM: usize = capsules_core::driver::NUM::KeyboardHid as usize;

impl SyscallDriverLookup for Platform {
    fn with_driver<F, R>(&self, driver_num: usize, f: F) -> R
    where
        F: FnOnce(Option<&dyn kernel::syscall::SyscallDriver>) -> R,
    {
        match driver_num {
            capsules_extra::hmac::DRIVER_NUM => f(Some(self.hmac)),
            KEYBOARD_HID_DRIVER_NUM => f(Some(self.keyboard_hid_driver)),
            _ => self.base.with_driver(driver_num, f),
        }
    }
}

type Chip = nrf52840dk_lib::Chip;

impl KernelResources<Chip> for Platform {
    type SyscallDriverLookup = Self;
    type SyscallFilter = <nrf52840dk_lib::Platform as KernelResources<Chip>>::SyscallFilter;
    type ProcessFault = <nrf52840dk_lib::Platform as KernelResources<Chip>>::ProcessFault;
    type Scheduler = <nrf52840dk_lib::Platform as KernelResources<Chip>>::Scheduler;
    type SchedulerTimer = <nrf52840dk_lib::Platform as KernelResources<Chip>>::SchedulerTimer;
    type WatchDog = <nrf52840dk_lib::Platform as KernelResources<Chip>>::WatchDog;
    type ContextSwitchCallback =
        <nrf52840dk_lib::Platform as KernelResources<Chip>>::ContextSwitchCallback;

    fn syscall_driver_lookup(&self) -> &Self::SyscallDriverLookup {
        self
    }
    fn syscall_filter(&self) -> &Self::SyscallFilter {
        self.base.syscall_filter()
    }
    fn process_fault(&self) -> &Self::ProcessFault {
        self.base.process_fault()
    }
    fn scheduler(&self) -> &Self::Scheduler {
        self.base.scheduler()
    }
    fn scheduler_timer(&self) -> &Self::SchedulerTimer {
        self.base.scheduler_timer()
    }
    fn watchdog(&self) -> &Self::WatchDog {
        self.base.watchdog()
    }
    fn context_switch_callback(&self) -> &Self::ContextSwitchCallback {
        self.base.context_switch_callback()
    }
}

/// Main function called after RAM initialized.
#[no_mangle]
pub unsafe fn main() {
    let main_loop_capability = create_capability!(capabilities::MainLoopCapability);

    // Create the base board:
    let (board_kernel, base_platform, chip, nrf52840_peripherals, _mux_alarm) =
        nrf52840dk_lib::start();

    //--------------------------------------------------------------------------
    // HMAC-SHA256
    //--------------------------------------------------------------------------

    let sha256_sw = components::sha::ShaSoftware256Component::new()
        .finalize(components::sha_software_256_component_static!());

    let hmac_sha256_sw = components::hmac::HmacSha256SoftwareComponent::new(sha256_sw).finalize(
        components::hmac_sha256_software_component_static!(capsules_extra::sha256::Sha256Software),
    );

    let hmac = components::hmac::HmacComponent::new(
        board_kernel,
        capsules_extra::hmac::DRIVER_NUM,
        hmac_sha256_sw,
    )
    .finalize(components::hmac_component_static!(HmacSha256Software, 32));

    //--------------------------------------------------------------------------
    // KEYBOARD
    //--------------------------------------------------------------------------

    // Create the strings we include in the USB descriptor.
    let strings = static_init!(
        [&str; 3],
        [
            "Nordic Semiconductor", // Manufacturer
            "nRF52840dk - TockOS",  // Product
            "serial0001",           // Serial number
        ]
    );

    let usb_device = &nrf52840_peripherals.usbd;

    // Generic HID Keyboard component usage
    let (keyboard_hid, keyboard_hid_driver) = components::keyboard_hid::KeyboardHidComponent::new(
        board_kernel,
        capsules_core::driver::NUM::KeyboardHid as usize,
        usb_device,
        0x1915, // Nordic Semiconductor
        0x503a,
        strings,
    )
    .finalize(components::keyboard_hid_component_static!(UsbHw));

    keyboard_hid.enable();
    keyboard_hid.attach();

    //--------------------------------------------------------------------------
    // Credential Checking
    //--------------------------------------------------------------------------

    // Create the credential checker.
    let checking_policy = components::appid::checker_null::AppCheckerNullComponent::new()
        .finalize(components::app_checker_null_component_static!());

    // Create the AppID assigner.
    let assigner = components::appid::assigner_name::AppIdAssignerNamesComponent::new()
        .finalize(components::appid_assigner_names_component_static!());

    // Create the process checking machine.
    let checker = components::appid::checker::ProcessCheckerMachineComponent::new(checking_policy)
        .finalize(components::process_checker_machine_component_static!());

    //--------------------------------------------------------------------------
    // STORAGE PERMISSIONS
    //--------------------------------------------------------------------------

    let storage_permissions_policy =
        components::storage_permissions::individual::StoragePermissionsIndividualComponent::new()
            .finalize(components::storage_permissions_individual_component_static!(Chip));

    //--------------------------------------------------------------------------
    // PLATFORM SETUP, PROCESS LOADING, AND START KERNEL LOOP
    //--------------------------------------------------------------------------

    let platform = Platform {
        base: base_platform,
        keyboard_hid_driver,
        hmac,
    };

    // Create and start the asynchronous process loader.
    let _loader = components::loader::sequential::ProcessLoaderSequentialComponent::new(
        checker,
        &mut *addr_of_mut!(PROCESSES),
        board_kernel,
        chip,
        &FAULT_RESPONSE,
        assigner,
        storage_permissions_policy,
    )
    .finalize(components::process_loader_sequential_component_static!(
        Chip,
        nrf52840dk_lib::NUM_PROCS
    ));

    board_kernel.kernel_loop(
        &platform,
        chip,
        Some(&platform.base.ipc),
        &main_loop_capability,
    );
}
