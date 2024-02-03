// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2022.

//! Traits and types for application credentials checkers, used to decide
//! whether an application can be loaded.
//!
//! See the [AppID TRD](../../doc/reference/trd-appid.md).

pub mod basic;

use core::cell::Cell;

use crate::config;
use crate::debug;
use crate::process::{Process, ShortID, State};
use crate::process_binary::{ProcessBinary, ProcessBinaryError};
use crate::process_loading::ProcessLoadError;
use crate::utilities::cells::{NumericCellExt, OptionalCell};
use crate::ErrorCode;
use tock_tbf::types::TbfFooterV2Credentials;
use tock_tbf::types::TbfParseError;

pub enum ProcessCheckError {
    /// The application checker requires credentials, but the TBF did
    /// not include a credentials that meets the checker's
    /// requirements. This can be either because the TBF has no
    /// credentials or the checker policy did not accept any of the
    /// credentials it has.
    CredentialsNotAccepted,

    /// The process contained a credentials which was rejected by the verifier.
    /// The u32 indicates which credentials was rejected: the first credentials
    /// after the application binary is 0, and each subsequent credentials increments
    /// this counter.
    CredentialsRejected(u32),

    InternalError,
}

/// What a AppCredentialsChecker decided a particular application's credential
/// indicates about the runnability of an application binary.
#[derive(Debug)]
pub enum CheckResult {
    /// Accept the credential and run the binary.
    Accept,
    /// Go to the next credential or in the case of the last one fall
    /// back to the default policy.
    Pass,
    /// Reject the credential and do not run the binary.
    Reject,
}

/// Receives callbacks on whether a credential was accepted or not.
pub trait Client<'a> {
    fn check_done(
        &self,
        result: Result<CheckResult, ErrorCode>,
        credentials: TbfFooterV2Credentials,
        binary: &'a [u8],
    );
}

/// Implements a Credentials Checking Policy.
pub trait AppCredentialsChecker<'a> {
    fn set_client(&self, _client: &'a dyn Client<'a>);
    fn require_credentials(&self) -> bool;
    fn check_credentials(
        &self,
        credentials: TbfFooterV2Credentials,
        binary: &'a [u8],
    ) -> Result<(), (ErrorCode, TbfFooterV2Credentials, &'a [u8])>;
}

/// Default implementation.
impl<'a> AppCredentialsChecker<'a> for () {
    fn set_client(&self, _client: &'a dyn Client<'a>) {}
    fn require_credentials(&self) -> bool {
        false
    }

    fn check_credentials(
        &self,
        credentials: TbfFooterV2Credentials,
        binary: &'a [u8],
    ) -> Result<(), (ErrorCode, TbfFooterV2Credentials, &'a [u8])> {
        Err((ErrorCode::NOSUPPORT, credentials, binary))
    }
}

/// Return whether `process` can run given the identifiers, version
/// numbers, and execution state of other processes. A process is
/// runnable if its credentials have been approved, it is in the
/// Terminated state, and one of the following conditions hold:
///
///   1. Its Application Identifier and Short ID are different from
///   all other processes, or
///   2. For every other process that shares an Application Identifier
///   or Short ID:
///      2A. If it has a lower or equal version number, it is not running
///      2B. If it has a higher version number, it is in the Terminated,
///      CredentialsUnchecked, or CredentialsFailed state.
///
/// Case 2A is because if a lower version number is currently running, it
/// must be stopped before the higher version number can run. Case 2B is
/// so that a lower or equal version number can be run if the higher or equal
/// has been explicitly stopped (Terminated) or cannot run (Unchecked/Failed).
/// This second case is designed so that at boot the highest version number
/// will run (it will be in the CredentialsApproved state when this test
/// runs at boot), but it can be stopped to let a lower version number run.
pub fn is_runnable<AU: AppUniqueness>(
    process: &dyn Process,
    processes: &[Option<&dyn Process>],
    id_differ: &AU,
) -> bool {
    let len = processes.len();
    // A process is only runnable if it has approved credentials and
    // is not currently running.
    if process.get_state() != State::CredentialsApproved && process.get_state() != State::Terminated
    {
        return false;
    }

    // Note that this causes `process` to compare against itself;
    // however, since `process` is not running and its version number
    // is the same, it will not block itself from running.
    for i in 0..len {
        let other_process = processes[i];
        let other_name = other_process.map_or("None", |c| c.get_process_name());

        let blocks = other_process.map_or(false, |other| {
            let state = other.get_state();
            let creds_approve =
                state != State::CredentialsUnchecked && state != State::CredentialsFailed;
            let different = id_differ.different_identifier(process, other)
                && other.short_app_id() != process.short_app_id();
            let newer = other.binary_version() > process.binary_version();
            let running = other.is_running();
            let runnable = state != State::CredentialsUnchecked
                && state != State::CredentialsFailed
                && state != State::Terminated;
            // Other will block process from running if
            // 1) Other has approved credentials, and
            // 2) Other has the same ShortID or Application Identifier, and
            // 3) Other has a higher version number *or* the same version number and is running
            if config::CONFIG.debug_process_credentials {
                debug!(
                    "[{}]: creds_approve: {}, different: {}, newer: {}, runnable: {}, running: {}",
                    other.get_process_name(),
                    creds_approve,
                    different,
                    newer,
                    runnable,
                    running
                );
            }
            creds_approve && !different && ((newer && runnable) || running)
        });
        if blocks {
            if config::CONFIG.debug_process_credentials {
                debug!(
                    "Process {} blocks {}",
                    other_name,
                    process.get_process_name()
                );
            }
            return false;
        }
    }
    if config::CONFIG.debug_process_credentials {
        debug!(
            "No process blocks {}: it is runnable",
            process.get_process_name()
        );
    }
    // No process blocks this one from running -- it's runnable
    true
}

/// Whether two processes have the same Application Identifier; two
/// processes with the same Application Identifier cannot run concurrently.
pub trait AppUniqueness {
    /// Returns whether `process_a` and `process_b` have a different identifier,
    /// and so can run concurrently. If this returns `false`, the kernel
    /// will not run `process_a` and `process_b` at the same time.
    fn different_identifier(&self, _process_a: &dyn Process, _process_b: &dyn Process) -> bool;
}

/// Default implementation.
impl AppUniqueness for () {
    fn different_identifier(&self, _process_a: &dyn Process, _process_b: &dyn Process) -> bool {
        true
    }
}

/// Transforms Application Credentials into a corresponding ShortID.
pub trait Compress {
    fn to_short_id(&self, process: &dyn Process, credentials: &TbfFooterV2Credentials) -> ShortID;
}

impl Compress for () {
    fn to_short_id(
        &self,
        _process: &dyn Process,
        _credentials: &TbfFooterV2Credentials,
    ) -> ShortID {
        ShortID::LocallyUnique
    }
}

pub trait CredentialsCheckingPolicy<'a>:
    AppCredentialsChecker<'a> + Compress + AppUniqueness
{
}
impl<'a, T: AppCredentialsChecker<'a> + Compress + AppUniqueness> CredentialsCheckingPolicy<'a>
    for T
{
}

struct KernelProcessInitCapability {}
unsafe impl crate::capabilities::ProcessInitCapability for KernelProcessInitCapability {}

struct KernelProcessApprovalCapability {}
unsafe impl crate::capabilities::ProcessApprovalCapability for KernelProcessApprovalCapability {}

pub(crate) trait ProcessCheckerMachineClient {
    fn done(&self, process_binary: &'static ProcessBinary, result: Result<(), ProcessCheckError>);
}

/// Checks the footers for a `ProcessBinary` and decides whether to continue
/// loading the process based on the checking policy in `checker`.
pub struct ProcessCheckerMachine {
    footer_index: Cell<usize>,
    policy: OptionalCell<&'static dyn CredentialsCheckingPolicy<'static>>,
    process_binary: OptionalCell<ProcessBinary>,
}

#[derive(Debug)]
enum FooterCheckResult {
    /// A check has started
    Checking,
    /// There are no more footers, no check started
    PastLastFooter,
    /// The footer isn't a credential, no check started
    FooterNotCheckable,
    /// The footer is invalid, no check started
    BadFooter,
    /// An internal error occurred, no check started
    Error,
}

impl ProcessCheckerMachine {
    fn set_client(&self, client: &'static ProcessCheckerMachineClient) {
        self.client.set(client);
    }

    pub fn start(&self, process_binary: &'static ProcessBinary) {
        self.footer_index.set(0);
        self.process_binary.set(process_binary);
        self.check();
    }

    /// Must be called from a callback context.
    fn check(&self) {
        loop {
            let policy = self.policy.get()?;
            let pb = self.process_binary.get();
            let footer_index = self.footer.get();

            let check_result = self.check_footer(policy, pb, footer_index);

            if config::CONFIG.debug_process_credentials {
                debug!(
                    "Checking: Check status for process {}, footer {}: {:?}",
                    pb.headers.get_process_name(),
                    footer_index,
                    check_result
                );
            }
            match check_result {
                FooterCheckResult::Checking => {
                    break;
                }
                FooterCheckResult::PastLastFooter | FooterCheckResult::BadFooter => {
                    // We reached the end of the footers without any
                    // credentials or all credentials were Pass: apply
                    // the checker policy to see if the process
                    // should be allowed to run.
                    self.policy.map(|policy| {
                        let requires = policy.require_credentials();

                        // TODO: verify we are doing this from an "interrupt"!!!
                        let result = if requires {
                            Err(ProcessCheckError::NoAcceptedCredentials)
                        } else {
                            Ok(())
                        };

                        self.client.map(|client| client.done(pb, result));
                    });
                    break;
                }
                FooterCheckResult::FooterNotCheckable => {
                    // Go to next footer
                    self.footer.increment();
                }
                FooterCheckResult::Error => {
                    self.client
                        .map(|client| client.done(pb, Err(ProcessCheckError::InternalError)));
                    break;
                }
            }
        }
    }

    pub fn set_policy(&self, policy: &'static dyn CredentialsCheckingPolicy<'static>) {
        self.policy.replace(policy);
    }

    // Returns whether a footer is being checked or not, and if not, why.
    // Iterates through the footer list until if finds `next_footer` or
    // it reached the end of the footer region.
    fn check_footer(
        process_binary: &'static ProcessBinary,
        policy: &'static dyn CredentialsCheckingPolicy<'static>,
        next_footer: usize,
    ) -> FooterCheckResult {
        if config::CONFIG.debug_process_credentials {
            debug!(
                "Checking: Checking {} footer {}",
                process_binary.get_package_name.get_process_name(),
                next_footer
            );
        }
        // let footers_position_ptr = process.get_addresses().flash_integrity_end;
        // let mut footers_position = footers_position_ptr as usize;

        // let flash_start_ptr = process.get_addresses().flash_start as *const u8;
        // let flash_start = flash_start_ptr as usize;
        // let flash_integrity_len = footers_position - flash_start;
        // let flash_end = process.get_addresses().flash_end;
        // let footers_len = flash_end - footers_position;

        // let mut current_footer = 0;
        // let mut footer_slice = unsafe { slice::from_raw_parts(footers_position_ptr, footers_len) };
        // let binary_slice = unsafe { slice::from_raw_parts(flash_start_ptr, flash_integrity_len) };

        let integrity_slice = process_binary.get_integrity_region_slice();
        let footer_slice = process_binary.footer;

        if config::CONFIG.debug_process_credentials {
            debug!(
                "Checking: Integrity region is {:x}-{:x}; footers at {:x}-{:x}",
                integrity_slice.as_ptr() as usize,
                integrity_slice.as_ptr() as usize + integrity_slice.len(),
                footer_slice.as_ptr() as usize,
                footer_slice.as_ptr() as usize + footer_slice.len(),
            );
        }

        let mut current_footer = 0;
        // let mut footers_position = footer_slice.as_ptr() as usize;

        // while current_footer <= next_footer && footers_position < flash_end {
        while current_footer <= next_footer {
            let parse_result = tock_tbf::parse::parse_tbf_footer(footer_slice);
            match parse_result {
                Err(TbfParseError::NotEnoughFlash) => {
                    if config::CONFIG.debug_process_credentials {
                        debug!("Checking: Not enough flash for a footer");
                    }
                    return FooterCheckResult::PastLastFooter;
                }
                Err(TbfParseError::BadTlvEntry(t)) => {
                    if config::CONFIG.debug_process_credentials {
                        debug!("Checking: Bad TLV entry, type: {:?}", t);
                    }
                    return FooterCheckResult::BadFooter;
                }
                Err(e) => {
                    if config::CONFIG.debug_process_credentials {
                        debug!("Checking: Error parsing footer: {:?}", e);
                    }
                    return FooterCheckResult::BadFooter;
                }
                Ok((footer, len)) => {
                    let slice_result = footer_slice.get(len as usize + 4..);
                    // if config::CONFIG.debug_process_credentials {
                    //     debug!(
                    //         "ProcessLoad: @{:x} found a len {} footer: {:?}",
                    //         footers_position,
                    //         len,
                    //         footer.format()
                    //     );
                    // }
                    // footers_position = footers_position + len as usize + 4;
                    match slice_result {
                        None => {
                            return FooterCheckResult::BadFooter;
                        }
                        Some(slice) => {
                            footer_slice = slice;
                            if current_footer == next_footer {
                                match policy.check_credentials(footer, integrity_slice) {
                                    Ok(()) => {
                                        if config::CONFIG.debug_process_credentials {
                                            debug!("Checking: Found {}, checking", current_footer);
                                        }
                                        return FooterCheckResult::Checking;
                                    }
                                    Err((ErrorCode::NOSUPPORT, _, _)) => {
                                        if config::CONFIG.debug_process_credentials {
                                            debug!(
                                                "Checking: Found {}, not supported",
                                                current_footer
                                            );
                                        }
                                        return FooterCheckResult::FooterNotCheckable;
                                    }
                                    Err((ErrorCode::ALREADY, _, _)) => {
                                        if config::CONFIG.debug_process_credentials {
                                            debug!("Checking: Found {}, already", current_footer);
                                        }
                                        return FooterCheckResult::FooterNotCheckable;
                                    }
                                    Err(e) => {
                                        if config::CONFIG.debug_process_credentials {
                                            debug!(
                                                "Checking: Found {}, error {:?}",
                                                current_footer, e
                                            );
                                        }
                                        return FooterCheckResult::Error;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            current_footer += 1;
        }
        FooterCheckResult::PastLastFooter
    }
}

impl process_checker::Client<'static> for ProcessCheckerMachine {
    fn check_done(
        &self,
        result: Result<CheckResult, ErrorCode>,
        credentials: TbfFooterV2Credentials,
        _binary: &'static [u8],
    ) {
        if config::CONFIG.debug_process_credentials {
            debug!("Checking: check_done gave result {:?}", result);
        }
        match result {
            Ok(CheckResult::Accept) => {
                // self.processes[self.process.get()].map(|p| {
                //     let short_id = self.policy.map_or(ShortID::LocallyUnique, |policy| {
                //         policy.to_short_id(p, &credentials)
                //     });
                //     let _r =
                //         p.mark_credentials_pass(Some(credentials), short_id, &self.approve_cap);
                // });
                // self.process.set(self.process.get() + 1);

                self.client.map(|client| {
                    let pb = self.process_binary.take();

                    client.done(pb, Ok(()))
                });
            }
            Ok(CheckResult::Pass) => {
                self.footer_index.increment();
            }
            Ok(CheckResult::Reject) => {
                // self.processes[self.process.get()].map(|p| {
                //     p.mark_credentials_fail(&self.approve_cap);
                // });
                // self.process.set(self.process.get() + 1);

                self.client.map(|client| {
                    let pb = self.process_binary.take();

                    client.done(pb, Err(ProcessCheckError::CredentialRejected))
                });
            }
            Err(e) => {
                if config::CONFIG.debug_process_credentials {
                    debug!("Checking: error checking footer {:?}", e);
                }
                self.footer_index.increment();
            }
        }
        let cont = self.next();
        match cont {
            Ok(true) => { /* processing next footer, do nothing */ }
            Ok(false) => {}
            Err(_e) => {}
        }
    }
}
