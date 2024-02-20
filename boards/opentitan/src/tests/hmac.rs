// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2022.

use crate::tests::run_kernel_op;
use crate::PERIPHERALS;
use core::cell::Cell;
use kernel::hil::digest::DigestAlgorithm;
#[allow(unused_imports)] // Can be unused if software only test
use kernel::hil::digest::DigestData;
use kernel::hil::digest::{self, Digest, DigestVerify, HmacSha256};
use kernel::static_init;
use kernel::utilities::cells::{MapCell, TakeCell};
use kernel::utilities::leasable_buffer::SubSlice;
use kernel::utilities::leasable_buffer::SubSliceMut;
use kernel::{debug, ErrorCode};

static KEY: [u8; 32] = [0xA1; 32];

struct HmacTestCallback {
    add_mut_data_done: Cell<bool>,
    verification_done: Cell<bool>,
    input_buffer: TakeCell<'static, [u8]>,
    digest_buffer: MapCell<&'static mut HmacSha256>,
}

unsafe impl Sync for HmacTestCallback {}

impl<'a> HmacTestCallback {
    fn new(input_buffer: &'static mut [u8], digest_buffer: &'static mut HmacSha256) -> Self {
        HmacTestCallback {
            add_mut_data_done: Cell::new(false),
            verification_done: Cell::new(false),
            input_buffer: TakeCell::new(input_buffer),
            digest_buffer: MapCell::new(digest_buffer),
        }
    }

    fn reset(&self) {
        self.add_mut_data_done.set(false);
        self.verification_done.set(false);
    }
}

impl<'a> digest::ClientData<HmacSha256> for HmacTestCallback {
    fn add_mut_data_done(&self, result: Result<(), ErrorCode>, data: SubSliceMut<'static, u8>) {
        self.add_mut_data_done.set(true);
        // Check that all of the data was accepted and the active slice is length 0
        assert_eq!(data.len(), 0);
        // Input data has been loaded, hold copy of data
        self.input_buffer.replace(data.take());
        assert_eq!(result, Ok(()));
    }

    fn add_data_done(&self, _result: Result<(), ErrorCode>, _data: SubSlice<'static, u8>) {
        unimplemented!()
    }
}

impl<'a> digest::ClientHash<HmacSha256> for HmacTestCallback {
    fn hash_done(&self, _result: Result<(), ErrorCode>, _digest: &'static mut HmacSha256) {
        unimplemented!()
    }
}

impl<'a> digest::ClientVerify<HmacSha256> for HmacTestCallback {
    fn verification_done(&self, result: Result<bool, ErrorCode>, compare: &'static mut HmacSha256) {
        self.digest_buffer.replace(compare);
        self.verification_done.set(true);
        assert_eq!(result, Ok(true));
    }
}

/// Static init an HmacTestCallback, with
/// respective buffers allocated for data fields.
macro_rules! static_init_test_cb {
    () => {{
        let input_data = static_init!([u8; 32], [32; 32]);
        let digest_data = static_init!(
            [u8; 32],
            [
                0xdc, 0x55, 0x51, 0x5e, 0x30, 0xac, 0x50, 0xc7, 0x65, 0xbd, 0xe, 0x2, 0x82, 0xf7,
                0x8b, 0xe1, 0xef, 0xd1, 0xb, 0xdc, 0xa8, 0xba, 0xe1, 0xfa, 0x11, 0x3f, 0xf6, 0xeb,
                0xaf, 0x58, 0x57, 0x40,
            ]
        );

        let digest_buf = static_init!(HmacSha256, HmacSha256::default());
        digest_buf.as_mut_slice()[..32].copy_from_slice(&digest_data[..32]);

        static_init!(
            HmacTestCallback,
            HmacTestCallback::new(input_data, digest_buf)
        )
    }};
}

#[test_case]
fn hmac_check_load_binary() {
    let perf = unsafe { PERIPHERALS.unwrap() };
    let hmac = &perf.hmac;

    let callback = unsafe { static_init_test_cb!() };
    let _buf = SubSliceMut::new(callback.input_buffer.take().unwrap());

    debug!("check hmac load binary... ");
    run_kernel_op(100);

    hmac.set_client(callback);
    callback.reset();

    #[cfg(feature = "hardware_tests")]
    assert_eq!(hmac.add_mut_data(_buf), Ok(()));

    run_kernel_op(1000);
    #[cfg(feature = "hardware_tests")]
    assert_eq!(callback.add_mut_data_done.get(), true);

    run_kernel_op(100);
    debug!("    [ok]");
    run_kernel_op(100);
}

#[test_case]
fn hmac_check_verify() {
    let perf = unsafe { PERIPHERALS.unwrap() };
    let hmac = &perf.hmac;

    let callback = unsafe { static_init_test_cb!() };

    let _buf = SubSliceMut::new(callback.input_buffer.take().unwrap());

    debug!("check hmac check verify... ");
    run_kernel_op(100);

    hmac.set_client(callback);
    callback.reset();
    assert_eq!(hmac.set_key(&KEY), Ok(()));

    #[cfg(feature = "hardware_tests")]
    assert_eq!(hmac.add_mut_data(_buf), Ok(()));

    run_kernel_op(1000);
    #[cfg(feature = "hardware_tests")]
    assert_eq!(callback.add_mut_data_done.get(), true);
    callback.reset();

    /* Get digest from callback digest buffer */
    assert!(hmac.verify(callback.digest_buffer.take().unwrap()) == Ok(()));

    run_kernel_op(1000);
    #[cfg(feature = "hardware_tests")]
    assert_eq!(callback.verification_done.get(), true);

    run_kernel_op(100);
    debug!("    [ok]");
    run_kernel_op(100);
}
