// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2025.

//! ECDSA Signature Verifier for P256 signatures.

use p256::ecdsa;
use p256::ecdsa::signature::hazmat::PrehashVerifier;

use core::cell::Cell;
use kernel::hil;
use kernel::utilities::cells::{MapCell, OptionalCell, TakeCell};

pub struct EcdsaP256SignatureVerifier<'a> {
    verified: Cell<bool>,
    client: OptionalCell<&'a dyn hil::public_key_crypto::signature::ClientVerify<32, 64>>,
    verifying_key: MapCell<ecdsa::VerifyingKey>,
    hash_storage: TakeCell<'static, [u8; 32]>,
    signature_storage: TakeCell<'static, [u8; 64]>,
    deferred_call: kernel::deferred_call::DeferredCall,
}

impl<'a> EcdsaP256SignatureVerifier<'a> {
    pub fn new(verifying_key_bytes: &[u8; 64]) -> Self {
        let ep = p256::EncodedPoint::from_untagged_bytes(verifying_key_bytes.into());
        let key = ecdsa::VerifyingKey::from_encoded_point(&ep);

        let verifying_key = key.map_or_else(|_e| MapCell::empty(), |v| MapCell::new(v));

        Self {
            verified: Cell::new(false),
            client: OptionalCell::empty(),
            verifying_key,
            hash_storage: TakeCell::empty(),
            signature_storage: TakeCell::empty(),
            deferred_call: kernel::deferred_call::DeferredCall::new(),
        }
    }
}

impl<'a> hil::public_key_crypto::signature::SignatureVerify<'a, 32, 64>
    for EcdsaP256SignatureVerifier<'a>
{
    fn set_verify_client(
        &self,
        client: &'a dyn hil::public_key_crypto::signature::ClientVerify<32, 64>,
    ) {
        self.client.replace(client);
    }

    fn verify(
        &self,
        hash: &'static mut [u8; 32],
        signature: &'static mut [u8; 64],
    ) -> Result<
        (),
        (
            kernel::ErrorCode,
            &'static mut [u8; 32],
            &'static mut [u8; 64],
        ),
    > {
        if self.verifying_key.is_some() {
            let sig = ecdsa::Signature::from_slice(signature);

            if sig.is_ok() {
                self.verifying_key
                    .map(|vkey| {
                        let sig = sig.unwrap();
                        self.verified.set(vkey.verify_prehash(hash, &sig).is_ok());
                        self.hash_storage.replace(hash);
                        self.signature_storage.replace(signature);
                        self.deferred_call.set();
                        Ok(())
                    })
                    .unwrap()
            } else {
                Err((kernel::ErrorCode::FAIL, hash, signature))
            }
        } else {
            Err((kernel::ErrorCode::FAIL, hash, signature))
        }
    }
}

impl<'a> kernel::deferred_call::DeferredCallClient for EcdsaP256SignatureVerifier<'a> {
    fn handle_deferred_call(&self) {
        self.client.map(|client| {
            self.hash_storage.take().map(|h| {
                self.signature_storage.take().map(|s| {
                    client.verification_done(Ok(self.verified.get()), h, s);
                });
            });
        });
    }

    fn register(&'static self) {
        self.deferred_call.register(self);
    }
}
