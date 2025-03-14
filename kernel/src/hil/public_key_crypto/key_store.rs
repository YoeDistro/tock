// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2025.

//! Interface for a generic key store holding multiple cryptographic keys.

use crate::ErrorCode;

pub trait KeyStoreClient {
    fn activate_key_done(&self, index: usize, error: Result<(), ErrorCode>);
}

pub trait KeyStore<'a> {
    /// Return the number of keys held by the key store.
    ///
    /// Each key must be identifiable by a consistent index.
    fn get_key_count(&self) -> usize;

    /// Set the key identified by its index as the active key.
    ///
    /// Indices start at 0 and go to `get_key_count() - 1`.
    ///
    /// This operation is asynchronous and its completion is signaled by
    /// `activate_key_done()`.
    ///
    /// ## Return
    ///
    /// `Ok()` if the active operation was accepted. Otherwise:
    /// - `Err(ErrorCode::INVAL)` if the index is not valid.
    fn activate_key(&self, index: usize) -> Result<(), ErrorCode>;

    fn set_client(&self, client: &'a dyn KeyStoreClient);
}
