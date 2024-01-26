use crate::config;
use crate::debug;

pub struct ProcessBinary {
    /// Process flash segment. This is the region of nonvolatile flash that
    /// the process occupies.
    pub(crate) flash: &'static [u8],

    /// The footers of the process binary (may be zero-sized), which are metadata
    /// about the process not covered by integrity. Used, among other things, to
    /// store signatures.
    pub(crate) footers: &'static [u8],

    /// Collection of pointers to the TBF header in flash.
    pub(crate) header: tock_tbf::types::TbfHeader,
}

pub enum ProcessBinaryError {
    /// No TBF header was found.
    TbfHeaderNotFound,

    /// The TBF header for the process could not be successfully parsed.
    TbfHeaderParseFailure(tock_tbf::types::TbfParseError),

    /// Not enough flash remaining to parse a process and its header.
    NotEnoughFlash,

    /// A process requires a newer version of the kernel or did not specify
    /// a required version. Processes can include the KernelVersion TBF header stating
    /// their compatible kernel version (^major.minor).
    ///
    /// Boards may not require processes to include the KernelVersion TBF header, and
    /// the kernel supports ignoring a missing KernelVersion TBF header. In that case,
    /// this error will not be returned for a process missing a KernelVersion TBF
    /// header.
    ///
    /// `version` is the `(major, minor)` kernel version the process indicates it
    /// requires. If `version` is `None` then the process did not include the
    /// KernelVersion TBF header.
    IncompatibleKernelVersion {
        version: Option<(u16, u16)>,
    },

    /// A process specified that its binary must start at a particular address,
    /// and that is not the address the binary is actually placed at.
    IncorrectFlashAddress {
        actual_address: u32,
        expected_address: u32,
    },

    NotEnabledProcess,
}

impl From<tock_tbf::types::TbfParseError> for ProcessBinaryError {
    /// Convert between a TBF Header parse error and a process binary error.
    ///
    /// We note that the process binary error is because a TBF header failed to
    /// parse, and just pass through the parse error.
    fn from(error: tock_tbf::types::TbfParseError) -> Self {
        ProcessBinaryError::TbfHeaderParseFailure(error)
    }
}

// impl From<ProcessLoadError> for ProcessBinaryError {
//     fn from(e: ProcessLoadError) -> ProcessBinaryError {
//         w.0

//         pub enum ProcessLoadError {
//     /// No TBF header was found.
//     TbfHeaderNotFound,

//     /// The TBF header for the process could not be successfully parsed.
//     TbfHeaderParseFailure(tock_tbf::types::TbfParseError),

//     /// Not enough flash remaining to parse a process and its header.
//     NotEnoughFlash,

//     /// Not enough memory to meet the amount requested by a process. Modify the
//     /// process to request less memory, flash fewer processes, or increase the
//     /// size of the region your board reserves for process memory.
//     NotEnoughMemory,

//     /// A process was loaded with a length in flash that the MPU does not
//     /// support. The fix is probably to correct the process size, but this could
//     /// also be caused by a bad MPU implementation.
//     MpuInvalidFlashLength,

//     /// The MPU configuration failed for some other, unspecified reason. This
//     /// could be of an internal resource exhaustion, or a mismatch between the
//     /// (current) MPU constraints and process requirements.
//     MpuConfigurationError,

//     /// A process specified a fixed memory address that it needs its memory
//     /// range to start at, and the kernel did not or could not give the process
//     /// a memory region starting at that address.
//     MemoryAddressMismatch {
//         actual_address: u32,
//         expected_address: u32,
//     },

//     /// A process specified that its binary must start at a particular address,
//     /// and that is not the address the binary is actually placed at.
//     IncorrectFlashAddress {
//         actual_address: u32,
//         expected_address: u32,
//     },

//     /// A process requires a newer version of the kernel or did not specify
//     /// a required version. Processes can include the KernelVersion TBF header stating
//     /// their compatible kernel version (^major.minor).
//     ///
//     /// Boards may not require processes to include the KernelVersion TBF header, and
//     /// the kernel supports ignoring a missing KernelVersion TBF header. In that case,
//     /// this error will not be returned for a process missing a KernelVersion TBF
//     /// header.
//     ///
//     /// `version` is the `(major, minor)` kernel version the process indicates it
//     /// requires. If `version` is `None` then the process did not include the
//     /// KernelVersion TBF header.
//     IncompatibleKernelVersion { version: Option<(u16, u16)> },

//     /// The application checker requires credentials, but the TBF did
//     /// not include a credentials that meets the checker's
//     /// requirements. This can be either because the TBF has no
//     /// credentials or the checker policy did not accept any of the
//     /// credentials it has.
//     CredentialsNoAccept,

//     /// The process contained a credentials which was rejected by the verifier.
//     /// The u32 indicates which credentials was rejected: the first credentials
//     /// after the application binary is 0, and each subsequent credentials increments
//     /// this counter.
//     CredentialsReject(u32),

//     /// Process loading error due (likely) to a bug in the kernel. If you get
//     /// this error please open a bug report.
//     InternalError,
// }
//     }
// }

impl ProcessBinary {
    pub(crate) unsafe fn create(
        app_flash: &'static [u8],
        header_length: usize,
        tbf_version: u16,

        require_kernel_version: bool,
    ) -> Result<Self, ProcessBinaryError> {
        // Get a slice for just the app header.
        let header_flash = match app_flash.get(0..header_length) {
            Some(h) => h,
            None => return Err(ProcessBinaryError::NotEnoughFlash),
        };

        // Parse the full TBF header to see if this is a valid app. If the
        // header can't parse, we will error right here.
        let tbf_header = match tock_tbf::parse::parse_tbf_header(header_flash, tbf_version) {
            Ok(h) => h,
            Err(err) => return Err(err.into()),
        };

        let process_name = tbf_header.get_package_name();

        // // If this isn't an app (i.e. it is padding) or it is an app but it
        // // isn't enabled, then we can skip it and do not create a `Process`
        // // object.
        // if !tbf_header.is_app() || !tbf_header.enabled() {
        //     if config::CONFIG.debug_load_processes {
        //         if !tbf_header.is_app() {
        //             debug!(
        //                 "Padding in flash={:#010X}-{:#010X}",
        //                 app_flash.as_ptr() as usize,
        //                 app_flash.as_ptr() as usize + app_flash.len() - 1
        //             );
        //         }
        //         if !tbf_header.enabled() {
        //             debug!(
        //                 "Process not enabled flash={:#010X}-{:#010X} process={:?}",
        //                 app_flash.as_ptr() as usize,
        //                 app_flash.as_ptr() as usize + app_flash.len() - 1,
        //                 process_name.unwrap_or("(no name)")
        //             );
        //         }
        //     }
        //     // Return no process and the full memory slice we were given.
        //     return Ok(None);
        // }

        // If this is an app but it isn't enabled, then we can return an error.
        if !tbf_header.enabled() {
            if config::CONFIG.debug_load_processes {
                debug!(
                    "Process not enabled flash={:#010X}-{:#010X} process={:?}",
                    app_flash.as_ptr() as usize,
                    app_flash.as_ptr() as usize + app_flash.len() - 1,
                    process_name.unwrap_or("(no name)")
                );
            }
            return Err(ProcessBinaryError::NotEnabledProcess);
        }

        if let Some((major, minor)) = tbf_header.get_kernel_version() {
            // If the `KernelVersion` header is present, we read the requested
            // kernel version and compare it to the running kernel version.
            if crate::KERNEL_MAJOR_VERSION != major || crate::KERNEL_MINOR_VERSION < minor {
                // If the kernel major version is different, we prevent the
                // process from being loaded.
                //
                // If the kernel major version is the same, we compare the
                // kernel minor version. The current running kernel minor
                // version has to be greater or equal to the one that the
                // process has requested. If not, we prevent the process from
                // loading.
                if config::CONFIG.debug_load_processes {
                    debug!("WARN process {:?} not loaded as it requires kernel version >= {}.{} and < {}.0, (running kernel {}.{})",
                        process_name.unwrap_or("(no name)"),
                        major,
                        minor,
                        (major+1),
                        crate::KERNEL_MAJOR_VERSION,
                        crate::KERNEL_MINOR_VERSION);
                }
                return Err(ProcessBinaryError::IncompatibleKernelVersion {
                    version: Some((major, minor)),
                });
            }
        } else {
            if require_kernel_version {
                // If enforcing the kernel version is requested, and the
                // `KernelVersion` header is not present, we prevent the process
                // from loading.
                if config::CONFIG.debug_load_processes {
                    debug!("WARN process {:?} not loaded as it has no kernel version header, please upgrade to elf2tab >= 0.8.0",
                               process_name.unwrap_or ("(no name"));
                }
                return Err(ProcessBinaryError::IncompatibleKernelVersion { version: None });
            }
        }

        let binary_end = tbf_header.get_binary_end() as usize;
        let total_size = app_flash.len();

        // End of the portion of the application binary covered by
        // integrity. Now handle footers.
        let footer_region = match app_flash.get(binary_end..total_size) {
            Some(f) => f,
            None => return Err(ProcessBinaryError::NotEnoughFlash),
        };

        // Check that the process is at the correct location in
        // flash if the TBF header specified a fixed address. If there is a
        // mismatch we catch that early.
        if let Some(fixed_flash_start) = tbf_header.get_fixed_address_flash() {
            // The flash address in the header is based on the app binary,
            // so we need to take into account the header length.
            let actual_address = app_flash.as_ptr() as u32 + tbf_header.get_protected_size();
            let expected_address = fixed_flash_start;
            if actual_address != expected_address {
                return Err(ProcessBinaryError::IncorrectFlashAddress {
                    actual_address,
                    expected_address,
                });
            }
        }

        let a = Self {
            header: tbf_header,

            footers: footer_region,
            flash: app_flash,
        };

        Ok(a)
    }
}
