// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2025.

//! Support for the VirtIO Input Device
//!
//! <https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-3850008>

// use core::cell::Cell;

// use kernel::deferred_call::{DeferredCall, DeferredCallClient};
// use kernel::hil::rng::{Client as RngClient, Continue as RngCont, Rng};
use kernel::utilities::cells::OptionalCell;
// use kernel::ErrorCode;

use crate::devices::{VirtIODeviceDriver, VirtIODeviceType};
use crate::queues::split_queue::{SplitVirtqueue, SplitVirtqueueClient, VirtqueueBuffer};

pub struct VirtIOInput<'a> {
    // virtqueue: &'a SplitVirtqueue<'a, 'b, 1>,
    eventq: &'a SplitVirtqueue<'static, 'static, 3>,
    statusq: &'a SplitVirtqueue<'static, 'static, 1>,
    // tx_header: OptionalCell<&'static mut [u8; 12]>,
    // tx_frame_info: Cell<(u16, usize)>,
    // rx_header: OptionalCell<&'static mut [u8]>,
    event_buffer1: OptionalCell<&'static mut [u8]>,
    event_buffer2: OptionalCell<&'static mut [u8]>,
    event_buffer3: OptionalCell<&'static mut [u8]>,
    status_buffer: OptionalCell<&'static mut [u8]>,
    // client: OptionalCell<&'a dyn EthernetAdapterDatapathClient>,
    // rx_enabled: Cell<bool>,

    // buffer_capacity: Cell<usize>,
    // callback_pending: Cell<bool>,
    // deferred_call: DeferredCall,
    // client: OptionalCell<&'a dyn RngClient>,
}

// pub struct VirtIONet<'a> {
//     rxqueue: &'a SplitVirtqueue<'static, 'static, 2>,
//     txqueue: &'a SplitVirtqueue<'static, 'static, 2>,
//     tx_header: OptionalCell<&'static mut [u8; 12]>,
//     tx_frame_info: Cell<(u16, usize)>,
//     rx_header: OptionalCell<&'static mut [u8]>,
//     rx_buffer: OptionalCell<&'static mut [u8]>,
//     client: OptionalCell<&'a dyn EthernetAdapterDatapathClient>,
//     rx_enabled: Cell<bool>,
// }

impl<'a> VirtIOInput<'a> {
    pub fn new(
        eventq: &'a SplitVirtqueue<'static, 'static, 3>,
        statusq: &'a SplitVirtqueue<'static, 'static, 1>,
        // tx_header: &'static mut [u8; 12],
        // rxqueue: &'a SplitVirtqueue<'static, 'static, 2>,
        // rx_header: &'static mut [u8],
        event_buffer1: &'static mut [u8],
        event_buffer2: &'static mut [u8],
        event_buffer3: &'static mut [u8],
        status_buffer: &'static mut [u8],
    ) -> Self {
        eventq.enable_used_callbacks();
        // statusq.enable_used_callbacks();

        Self {
            eventq,
            statusq,
            event_buffer1: OptionalCell::new(event_buffer1),
            event_buffer2: OptionalCell::new(event_buffer2),
            event_buffer3: OptionalCell::new(event_buffer3),
            status_buffer: OptionalCell::new(status_buffer),
            // tx_header: OptionalCell::new(tx_header),
            // tx_frame_info: Cell::new((0, 0)),
            // rx_header: OptionalCell::new(rx_header),
            // rx_buffer: OptionalCell::new(rx_buffer),
            // client: OptionalCell::empty(),
            // rx_enabled: Cell::new(false),
        }
    }

    pub fn reinsert_virtqueue_receive_buffer(&self) {
        // // Don't reinsert receive buffer when reception is disabled. The buffers
        // // will be reinserted on the next call to `enable_receive`:
        // if !self.rx_enabled.get() {
        //     return;
        // }

        // // Place the event buffers into the device's VirtQueue
        // if let Some(event_buffer1) = self.event_buffer1.take() {
        //     if let Some(event_buffer2) = self.event_buffer2.take() {
        //         if let Some(event_buffer3) = self.event_buffer3.take() {
        //             let event_buffer1_len = event_buffer1.len();
        //             let event_buffer2_len = event_buffer2.len();
        //             let event_buffer3_len = event_buffer3.len();

        //             let mut buffer_chain = [
        //                 Some(VirtqueueBuffer {
        //                     buf: event_buffer1,
        //                     len: event_buffer1_len,
        //                     device_writeable: true,
        //                 }),
        //                 Some(VirtqueueBuffer {
        //                     buf: event_buffer2,
        //                     len: event_buffer2_len,
        //                     device_writeable: true,
        //                 }),
        //                 Some(VirtqueueBuffer {
        //                     buf: event_buffer3,
        //                     len: event_buffer3_len,
        //                     device_writeable: true,
        //                 }),
        //             ];

        //             self.eventq.provide_buffer_chain(&mut buffer_chain).unwrap();

        //             kernel::debug!("reinsert ");

        //             // a.unwrap();
        //         }
        //     }
        // }

        if let Some(event_buffer) = self.event_buffer1.take() {
            let event_buffer_len = event_buffer.len();

            let mut buffer_chain = [Some(VirtqueueBuffer {
                buf: event_buffer,
                len: event_buffer_len,
                device_writeable: true,
            })];

            self.eventq.provide_buffer_chain(&mut buffer_chain).unwrap();

            kernel::debug!("reinsert1 ");
        }

        if let Some(event_buffer) = self.event_buffer2.take() {
            let event_buffer_len = event_buffer.len();

            let mut buffer_chain = [Some(VirtqueueBuffer {
                buf: event_buffer,
                len: event_buffer_len,
                device_writeable: true,
            })];

            self.eventq.provide_buffer_chain(&mut buffer_chain).unwrap();

            kernel::debug!("reinsert1 ");
        }

        if let Some(event_buffer) = self.event_buffer3.take() {
            let event_buffer_len = event_buffer.len();

            let mut buffer_chain = [Some(VirtqueueBuffer {
                buf: event_buffer,
                len: event_buffer_len,
                device_writeable: true,
            })];

            self.eventq.provide_buffer_chain(&mut buffer_chain).unwrap();

            kernel::debug!("reinsert1 ");
        }

        // if let Some(status_buffer) = self.status_buffer.take() {
        //     let status_buffer_len = status_buffer.len();

        //     let mut buffer_chain = [Some(VirtqueueBuffer {
        //         buf: status_buffer,
        //         len: status_buffer_len,
        //         device_writeable: true,
        //     })];

        //     self.statusq
        //         .provide_buffer_chain(&mut buffer_chain)
        //         .unwrap();

        //     // kernel::debug!("reinsert status");

        //     // a.unwrap();
        // }
    }
}

impl SplitVirtqueueClient<'static> for VirtIOInput<'_> {
    fn buffer_chain_ready(
        &self,
        queue_number: u32,
        buffer_chain: &mut [Option<VirtqueueBuffer<'static>>],
        _bytes_used: usize,
    ) {
        // kernel::debug!("bcr {}", queue_number);
        // kernel::debug!("bcr qn {:?}", self.eventq.queue_number());
        if queue_number == self.eventq.queue_number().unwrap() {
            // Received an input device event
            kernel::debug!("bcr input event");

            let event_buffer = buffer_chain[0].take().expect("No event buffer").buf;

            let event_type = u16::from_le_bytes([event_buffer[0], event_buffer[1]]);
            let event_code = u16::from_le_bytes([event_buffer[2], event_buffer[3]]);
            let event_value = u32::from_le_bytes([
                event_buffer[4],
                event_buffer[5],
                event_buffer[6],
                event_buffer[7],
            ]);

            kernel::debug!(
                "VirtIO Input Event: t:{}, c:{}, v:{}",
                event_type,
                event_code,
                event_value
            );

            // // TODO: do something with the header
            // self.rx_header.replace(rx_header);

            // let rx_buffer = buffer_chain[1].take().expect("No rx content buffer").buf;

            // if self.rx_enabled.get() {
            //     self.client
            //         .map(|client| client.received_frame(&rx_buffer[..(bytes_used - 12)], None));
            // }

            self.event_buffer1.replace(event_buffer);

            // Re-run enable RX to provide the RX buffer chain back to the
            // device (if reception is still enabled):
            self.reinsert_virtqueue_receive_buffer();
        } else if queue_number == self.statusq.queue_number().unwrap() {
            // Received an input device event
            // kernel::debug!("bcr input status");

            let status_buffer = buffer_chain[0].take().expect("No status buffer").buf;

            // let event_type = u16::from_le_bytes([status_buffer[0], status_buffer[1]]);
            // let event_code = u16::from_le_bytes([status_buffer[2], status_buffer[3]]);
            // let event_value = u32::from_le_bytes([
            //     status_buffer[4],
            //     status_buffer[5],
            //     status_buffer[6],
            //     status_buffer[7],
            // ]);

            // kernel::debug!(
            //     "VirtIO Input Status: t:{}, c:{}, v:{}",
            //     event_type,
            //     event_code,
            //     event_value
            // );

            // // TODO: do something with the header
            // self.rx_header.replace(rx_header);

            // let rx_buffer = buffer_chain[1].take().expect("No rx content buffer").buf;

            // if self.rx_enabled.get() {
            //     self.client
            //         .map(|client| client.received_frame(&rx_buffer[..(bytes_used - 12)], None));
            // }

            self.status_buffer.replace(status_buffer);

            // Re-run enable RX to provide the RX buffer chain back to the
            // device (if reception is still enabled):
            self.reinsert_virtqueue_receive_buffer();
        }

        // else if queue_number == self.txqueue.queue_number().unwrap() {
        //     // Sent an Ethernet frame

        //     let header_buf = buffer_chain[0].take().expect("No header buffer").buf;
        //     self.tx_header.replace(header_buf.try_into().unwrap());

        //     let frame_buf = buffer_chain[1].take().expect("No frame buffer").buf;

        //     let (frame_len, transmission_identifier) = self.tx_frame_info.get();

        //     self.client.map(move |client| {
        //         client.transmit_frame_done(
        //             Ok(()),
        //             frame_buf,
        //             frame_len,
        //             transmission_identifier,
        //             None,
        //         )
        //     });
        // } else {
        //     panic!("Callback from unknown queue");
        // }
    }
}

// impl VirtIODeviceDriver for VirtIONet<'_> {
//     fn negotiate_features(&self, offered_features: u64) -> Option<u64> {
//         let offered_features =
//             LocalRegisterCopy::<u64, VirtIONetFeatures::Register>::new(offered_features);
//         let mut negotiated_features = LocalRegisterCopy::<u64, VirtIONetFeatures::Register>::new(0);

//         if offered_features.is_set(VirtIONetFeatures::VirtIONetFMac) {
//             // VIRTIO_NET_F_MAC offered, which means that the device has a MAC
//             // address. Accept this feature, which is required for this driver
//             // for now.
//             negotiated_features.modify(VirtIONetFeatures::VirtIONetFMac::SET);
//         } else {
//             return None;
//         }

//         // TODO: QEMU doesn't offer this, but don't we need it? Does QEMU
//         // implicitly provide the feature but not offer it? Find out!
//         // if offered_features & (1 << 15) != 0 {
//         //     // VIRTIO_NET_F_MRG_RXBUF
//         //     //
//         //     // accept
//         //     negotiated_features |= 1 << 15;
//         // } else {
//         //     panic!("Missing NET_F_MRG_RXBUF");
//         // }

//         // Ignore everything else
//         Some(negotiated_features.get())
//     }

//     fn device_type(&self) -> VirtIODeviceType {
//         VirtIODeviceType::NetworkCard
//     }
// }

// impl<'a> EthernetAdapterDatapath<'a> for VirtIONet<'a> {
//     fn set_client(&self, client: &'a dyn EthernetAdapterDatapathClient) {
//         self.client.set(client);
//     }

//     fn enable_receive(&self) {
//         // Enable receive callbacks:
//         self.rx_enabled.set(true);

//         // Attempt to reinsert any driver-owned receive buffers into the receive
//         // queues. This will be a nop if reception was already enabled before
//         // this call:
//         self.reinsert_virtqueue_receive_buffer();
//     }

//     fn disable_receive(&self) {
//         // Disable receive callbacks:
//         self.rx_enabled.set(false);

//         // We don't "steal" any receive buffers out of the virtqueue, but the
//         // above flag will avoid reinserting buffers into the VirtQueue until
//         // reception is enabled again:
//     }

//     fn transmit_frame(
//         &self,
//         frame_buffer: &'static mut [u8],
//         len: u16,
//         transmission_identifier: usize,
//     ) -> Result<(), (ErrorCode, &'static mut [u8])> {
//         // Try to get a hold of the header buffer
//         //
//         // Otherwise, the device is currently busy transmitting a buffer
//         //
//         // TODO: Implement simultaneous transmissions
//         let mut frame_queue_buf = Some(VirtqueueBuffer {
//             buf: frame_buffer,
//             len: len as usize,
//             device_writeable: false,
//         });

//         let header_buf = self
//             .tx_header
//             .take()
//             .ok_or(ErrorCode::BUSY)
//             .map_err(|ret| (ret, frame_queue_buf.take().unwrap().buf))?;

//         // Write the header
//         //
//         // TODO: Can this be done more elegantly using a struct of registers?
//         header_buf[0] = 0; // flags -> we don't want checksumming
//         header_buf[1] = 0; // gso -> no checksumming or fragmentation
//         header_buf[2] = 0; // hdr_len_low
//         header_buf[3] = 0; // hdr_len_high
//         header_buf[4] = 0; // gso_size
//         header_buf[5] = 0; // gso_size
//         header_buf[6] = 0; // csum_start
//         header_buf[7] = 0; // csum_start
//         header_buf[8] = 0; // csum_offset
//         header_buf[9] = 0; // csum_offsetb
//         header_buf[10] = 0; // num_buffers
//         header_buf[11] = 0; // num_buffers

//         let mut buffer_chain = [
//             Some(VirtqueueBuffer {
//                 buf: header_buf,
//                 len: 12,
//                 device_writeable: false,
//             }),
//             frame_queue_buf.take(),
//         ];

//         self.tx_frame_info.set((len, transmission_identifier));

//         self.txqueue
//             .provide_buffer_chain(&mut buffer_chain)
//             .map_err(move |ret| (ret, buffer_chain[1].take().unwrap().buf))?;

//         Ok(())
//     }
// }

// impl<'a, 'b> VirtIOInput<'a, 'b> {
//     pub fn new(virtqueue: &'a SplitVirtqueue<'a, 'b, 1>) -> VirtIORng<'a, 'b> {
//         VirtIOInput {
//             virtqueue,
//             buffer_capacity: Cell::new(0),
//             callback_pending: Cell::new(false),
//             deferred_call: DeferredCall::new(),
//             client: OptionalCell::empty(),
//         }
//     }

//     pub fn provide_buffer(&self, buf: &'b mut [u8]) -> Result<usize, (&'b mut [u8], ErrorCode)> {
//         let len = buf.len();
//         if len < 4 {
//             // We don't yet support merging of randomness of multiple buffers
//             //
//             // Allowing a buffer with less than 4 elements will cause
//             // the callback to never be called, while the buffer is
//             // reinserted into the queue
//             return Err((buf, ErrorCode::INVAL));
//         }

//         let mut buffer_chain = [Some(VirtqueueBuffer {
//             buf,
//             len,
//             device_writeable: true,
//         })];

//         let res = self.virtqueue.provide_buffer_chain(&mut buffer_chain);

//         match res {
//             Err(ErrorCode::NOMEM) => {
//                 // Hand back the buffer, the queue MUST NOT write partial
//                 // buffer chains
//                 let buf = buffer_chain[0].take().unwrap().buf;
//                 Err((buf, ErrorCode::NOMEM))
//             }
//             Err(e) => panic!("Unexpected error {:?}", e),
//             Ok(()) => {
//                 let mut cap = self.buffer_capacity.get();
//                 cap += len;
//                 self.buffer_capacity.set(cap);
//                 Ok(cap)
//             }
//         }
//     }

//     fn buffer_chain_callback(
//         &self,
//         buffer_chain: &mut [Option<VirtqueueBuffer<'b>>],
//         bytes_used: usize,
//     ) {
//         // Disable further callbacks, until we're sure we need them
//         //
//         // The used buffers should stay in the queue until a client is
//         // ready to consume them
//         self.virtqueue.disable_used_callbacks();

//         // We only have buffer chains of a single buffer
//         let buf = buffer_chain[0].take().unwrap().buf;

//         // We have taken out a buffer, hence decrease the available capacity
//         assert!(self.buffer_capacity.get() >= buf.len());

//         // It could've happened that we don't require the callback any
//         // more, hence check beforehand
//         let cont = if self.callback_pending.get() {
//             // The callback is no longer pending
//             self.callback_pending.set(false);

//             let mut u32randiter = buf[0..bytes_used].chunks(4).filter_map(|slice| {
//                 if slice.len() < 4 {
//                     None
//                 } else {
//                     Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
//                 }
//             });

//             // For now we don't use left-over randomness and assume the
//             // client has consumed the entire iterator
//             self.client
//                 .map(|client| client.randomness_available(&mut u32randiter, Ok(())))
//                 .unwrap_or(RngCont::Done)
//         } else {
//             RngCont::Done
//         };

//         if let RngCont::More = cont {
//             // Returning more is the equivalent of calling .get() on
//             // the Rng trait.

//             // TODO: what if this call fails?
//             let _ = self.get();
//         }

//         // In any case, reinsert the buffer for further processing
//         self.provide_buffer(buf).expect("Buffer reinsertion failed");
//     }
// }

// impl<'a> Rng<'a> for VirtIORng<'a, '_> {
//     fn get(&self) -> Result<(), ErrorCode> {
//         // Minimum buffer capacity must be 4 bytes for a single 32-bit
//         // word
//         if self.buffer_capacity.get() < 4 {
//             Err(ErrorCode::FAIL)
//         } else if self.client.is_none() {
//             Err(ErrorCode::FAIL)
//         } else if self.callback_pending.get() {
//             Err(ErrorCode::OFF)
//         } else if self.virtqueue.used_descriptor_chains_count() < 1 {
//             // There is no buffer ready in the queue, so let's rely
//             // purely on queue callbacks to notify us of the next
//             // incoming one
//             self.callback_pending.set(true);
//             self.virtqueue.enable_used_callbacks();
//             Ok(())
//         } else {
//             // There is a buffer in the virtqueue, get it and return
//             // it to a client in a deferred call
//             self.callback_pending.set(true);
//             self.deferred_call.set();
//             Ok(())
//         }
//     }

//     fn cancel(&self) -> Result<(), ErrorCode> {
//         // Cancel by setting the callback_pending flag to false which
//         // MUST be checked prior to every callback
//         self.callback_pending.set(false);

//         // For efficiency reasons, also unsubscribe from the virtqueue
//         // callbacks, which will let the buffers remain in the queue
//         // for future use
//         self.virtqueue.disable_used_callbacks();

//         Ok(())
//     }

//     fn set_client(&self, client: &'a dyn RngClient) {
//         self.client.set(client);
//     }
// }

// impl<'b> SplitVirtqueueClient<'b> for VirtIORng<'_, 'b> {
//     fn buffer_chain_ready(
//         &self,
//         _queue_number: u32,
//         buffer_chain: &mut [Option<VirtqueueBuffer<'b>>],
//         bytes_used: usize,
//     ) {
//         self.buffer_chain_callback(buffer_chain, bytes_used)
//     }
// }

// impl DeferredCallClient for VirtIORng<'_, '_> {
//     fn register(&'static self) {
//         self.deferred_call.register(self);
//     }

//     fn handle_deferred_call(&self) {
//         // Try to extract a descriptor chain
//         if let Some((mut chain, bytes_used)) = self.virtqueue.pop_used_buffer_chain() {
//             self.buffer_chain_callback(&mut chain, bytes_used)
//         } else {
//             // If we don't get a buffer, this must be a race condition
//             // which should not occur
//             //
//             // Prior to setting a deferred call, all virtqueue
//             // interrupts must be disabled so that no used buffer is
//             // removed before the deferred call callback
//             panic!("VirtIO RNG: deferred call callback with empty queue");
//         }
//     }
// }

impl VirtIODeviceDriver for VirtIOInput<'_> {
    fn negotiate_features(&self, _offered_features: u64) -> Option<u64> {
        // kernel::debug!("feats");
        // We don't support any special features and do not care about
        // what the device offers.
        Some(0)
    }

    fn device_type(&self) -> VirtIODeviceType {
        VirtIODeviceType::InputDevice
    }
}
