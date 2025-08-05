// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2022.

use core::cell::Cell;
use core::ops::Range;

use kernel::deferred_call::{DeferredCall, DeferredCallClient};
use kernel::hil::screen::{Screen, ScreenClient, ScreenPixelFormat, ScreenRotation};
use kernel::utilities::cells::{OptionalCell, TakeCell};
use kernel::utilities::leasable_buffer::SubSliceMut;
use kernel::ErrorCode;

use super::super::devices::{VirtIODeviceDriver, VirtIODeviceType};
use super::super::queues::split_queue::{SplitVirtqueue, SplitVirtqueueClient, VirtqueueBuffer};

// Const version of `std::cmp::max` for an array of `usize`s.
//
// This is ... not great. `const fn`s are pretty restrictive still,
// and most subslicing or iterator operations can't be used. So this
// looks like its the best we can do, at least for now.
const fn max(elems: &[usize]) -> usize {
    const fn max_inner(elems: &[usize], idx: usize) -> usize {
        match elems.len() - idx {
            0 => usize::MIN,
            1 => elems[idx],
            _ => {
                let max_tail = max_inner(elems, idx + 1);
                if max_tail > elems[idx] {
                    max_tail
                } else {
                    elems[idx]
                }
            }
        }
    }
    max_inner(elems, 0)
}

#[inline]
fn copy_to_iter<'a, T: 'a>(
    dst: &mut impl Iterator<Item = &'a mut T>,
    src: impl Iterator<Item = T>,
) {
    for e in src {
        *dst.next().unwrap() = e;
    }
}

#[inline]
fn bytes_from_iter<const N: usize>(
    src: &mut impl Iterator<Item = u8>,
) -> Result<[u8; N], ErrorCode> {
    let mut dst: [u8; N] = [0; N];

    for d in dst.iter_mut() {
        *d = src.next().ok_or(ErrorCode::SIZE)?;
    }

    Ok(dst)
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct CtrlHeader {
    pub ctrl_type: CtrlType,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub padding: u32,
}

impl CtrlHeader {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();

    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>) {
        // Write out fields to iterator.
        //
        // This struct doesn't need any padding bytes.
        copy_to_iter(dst, u32::to_le_bytes(self.ctrl_type as u32).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.flags).into_iter());
        copy_to_iter(dst, u64::to_le_bytes(self.fence_id).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.ctx_id).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.padding).into_iter());
    }

    fn from_byte_iter(src: &mut impl Iterator<Item = u8>) -> Result<Self, ErrorCode> {
        let ctrl_type = CtrlType::try_from(u32::from_le_bytes(bytes_from_iter(src)?))
            .map_err(|()| ErrorCode::INVAL)?;
        let flags = u32::from_le_bytes(bytes_from_iter(src)?);
        let fence_id = u64::from_le_bytes(bytes_from_iter(src)?);
        let ctx_id = u32::from_le_bytes(bytes_from_iter(src)?);
        let padding = u32::from_le_bytes(bytes_from_iter(src)?);

        Ok(CtrlHeader {
            ctrl_type,
            flags,
            fence_id,
            ctx_id,
            padding,
        })
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct MemEntry {
    pub addr: u64,
    pub length: u32,
    pub padding: u32,
}

impl MemEntry {
    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>) {
        // Write out fields to iterator.
        //
        // This struct doesn't need any padding bytes.
        copy_to_iter(dst, u64::to_le_bytes(self.addr).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.length).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.padding).into_iter());
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn empty() -> Self {
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 && self.height == 0
    }

    // pub fn extend(&self, other: Rect) -> Rect {
    //     use core::cmp::{max, min};

    //     // If either one of the `Rect`s is empty, simply return the other:
    //     if self.is_empty() {
    //         other
    //     } else if other.is_empty() {
    //         *self
    //     } else {
    //         // Determine the "x1" for both self and other, so that we can calculate
    //         // the final width based on the distance of the larger of the two "x0"s
    //         // and the larger of the two "x1"s:
    //         let self_x1 = self.x.saturating_add(self.width);
    //         let other_x1 = other.x.saturating_add(other.width);

    //         // Same for "y1"s:
    //         let self_y1 = self.y.saturating_add(self.height);
    //         let other_y1 = other.y.saturating_add(other.height);

    //         // Now, build the rect:
    //         let new_x0 = min(self.x, other.x);
    //         let new_x1 = max(self_x1, other_x1);
    //         let new_y0 = min(self.y, other.y);
    //         let new_y1 = max(self_y1, other_y1);
    //         Rect {
    //             x: new_x0,
    //             y: new_y0,
    //             width: new_x1.saturating_sub(new_x0),
    //             height: new_y1.saturating_sub(new_y0),
    //         }
    //     }
    // }

    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>) {
        // Write out fields to iterator.
        //
        // This struct doesn't need any padding bytes.
        copy_to_iter(dst, u32::to_le_bytes(self.x).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.y).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.width).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.height).into_iter());
    }
}

trait VirtIOGPUReq {
    const ENCODED_SIZE: usize;
    const CTRL_TYPE: CtrlType;
    type ExpectedResponse;

    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>);
}

trait VirtIOGPUResp {
    const ENCODED_SIZE: usize;
    const EXPECTED_CTRL_TYPE: CtrlType;

    fn from_byte_iter_post_checked_ctrl_header(
        ctrl_header: CtrlHeader,
        src: &mut impl Iterator<Item = u8>,
    ) -> Result<Self, ErrorCode>
    where
        Self: Sized;

    fn from_byte_iter_post_ctrl_header(
        ctrl_header: CtrlHeader,
        src: &mut impl Iterator<Item = u8>,
    ) -> Result<Self, ErrorCode>
    where
        Self: Sized,
    {
        if ctrl_header.ctrl_type == Self::EXPECTED_CTRL_TYPE {
            Self::from_byte_iter_post_checked_ctrl_header(ctrl_header, src)
        } else {
            Err(ErrorCode::INVAL)
        }
    }

    #[allow(dead_code)]
    fn from_byte_iter(src: &mut impl Iterator<Item = u8>) -> Result<Self, ErrorCode>
    where
        Self: Sized,
    {
        let ctrl_header = CtrlHeader::from_byte_iter(src)?;
        Self::from_byte_iter_post_ctrl_header(ctrl_header, src)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct ResourceCreate2DReq {
    pub ctrl_header: CtrlHeader,
    pub resource_id: u32,
    pub format: VideoFormat,
    pub width: u32,
    pub height: u32,
}

impl VirtIOGPUReq for ResourceCreate2DReq {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const CTRL_TYPE: CtrlType = CtrlType::CmdResourceCreate2d;
    type ExpectedResponse = ResourceCreate2DResp;

    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>) {
        // Write out fields to iterator.
        //
        // This struct doesn't need any padding bytes.
        self.ctrl_header.write_to_byte_iter(dst);
        copy_to_iter(dst, u32::to_le_bytes(self.resource_id).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.format as u32).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.width).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.height).into_iter());
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct ResourceCreate2DResp {
    pub ctrl_header: CtrlHeader,
}

impl VirtIOGPUResp for ResourceCreate2DResp {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const EXPECTED_CTRL_TYPE: CtrlType = CtrlType::RespOkNoData;

    fn from_byte_iter_post_checked_ctrl_header(
        ctrl_header: CtrlHeader,
        _src: &mut impl Iterator<Item = u8>,
    ) -> Result<Self, ErrorCode> {
        Ok(ResourceCreate2DResp { ctrl_header })
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct ResourceAttachBackingReq<const ENTRIES: usize> {
    pub ctrl_header: CtrlHeader,
    pub resource_id: u32,
    pub nr_entries: u32,
    pub entries: [MemEntry; ENTRIES],
}

impl<const ENTRIES: usize> VirtIOGPUReq for ResourceAttachBackingReq<ENTRIES> {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const CTRL_TYPE: CtrlType = CtrlType::CmdResourceAttachBacking;
    type ExpectedResponse = ResourceAttachBackingResp;

    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>) {
        // Write out fields to iterator.
        //
        // This struct doesn't need any padding bytes.
        self.ctrl_header.write_to_byte_iter(dst);
        copy_to_iter(dst, u32::to_le_bytes(self.resource_id).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.nr_entries).into_iter());

        for entry in self.entries {
            entry.write_to_byte_iter(dst);
        }
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct ResourceAttachBackingResp {
    pub ctrl_header: CtrlHeader,
}

impl VirtIOGPUResp for ResourceAttachBackingResp {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const EXPECTED_CTRL_TYPE: CtrlType = CtrlType::RespOkNoData;

    fn from_byte_iter_post_checked_ctrl_header(
        ctrl_header: CtrlHeader,
        _src: &mut impl Iterator<Item = u8>,
    ) -> Result<Self, ErrorCode> {
        Ok(ResourceAttachBackingResp { ctrl_header })
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct ResourceDetachBackingReq {
    pub ctrl_header: CtrlHeader,
    pub resource_id: u32,
    pub padding: u32,
}

impl VirtIOGPUReq for ResourceDetachBackingReq {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const CTRL_TYPE: CtrlType = CtrlType::CmdResourceDetachBacking;
    type ExpectedResponse = ResourceDetachBackingResp;

    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>) {
        // Write out fields to iterator.
        //
        // This struct doesn't need any padding bytes.
        self.ctrl_header.write_to_byte_iter(dst);
        copy_to_iter(dst, u32::to_le_bytes(self.resource_id).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.padding).into_iter());
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct ResourceDetachBackingResp {
    pub ctrl_header: CtrlHeader,
}

impl VirtIOGPUResp for ResourceDetachBackingResp {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const EXPECTED_CTRL_TYPE: CtrlType = CtrlType::RespOkNoData;

    fn from_byte_iter_post_checked_ctrl_header(
        ctrl_header: CtrlHeader,
        _src: &mut impl Iterator<Item = u8>,
    ) -> Result<Self, ErrorCode> {
        Ok(ResourceDetachBackingResp { ctrl_header })
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct SetScanoutReq {
    pub ctrl_header: CtrlHeader,
    pub r: Rect,
    pub scanout_id: u32,
    pub resource_id: u32,
}

impl VirtIOGPUReq for SetScanoutReq {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const CTRL_TYPE: CtrlType = CtrlType::CmdSetScanout;
    type ExpectedResponse = SetScanoutResp;

    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>) {
        // Write out fields to iterator.
        //
        // This struct doesn't need any padding bytes.
        self.ctrl_header.write_to_byte_iter(dst);
        self.r.write_to_byte_iter(dst);
        copy_to_iter(dst, u32::to_le_bytes(self.scanout_id).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.resource_id).into_iter());
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct SetScanoutResp {
    pub ctrl_header: CtrlHeader,
}

impl VirtIOGPUResp for SetScanoutResp {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const EXPECTED_CTRL_TYPE: CtrlType = CtrlType::RespOkNoData;

    fn from_byte_iter_post_checked_ctrl_header(
        ctrl_header: CtrlHeader,
        _src: &mut impl Iterator<Item = u8>,
    ) -> Result<Self, ErrorCode> {
        Ok(SetScanoutResp { ctrl_header })
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct TransferToHost2DReq {
    pub ctrl_header: CtrlHeader,
    pub r: Rect,
    pub offset: u64,
    pub resource_id: u32,
    pub padding: u32,
}

impl VirtIOGPUReq for TransferToHost2DReq {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const CTRL_TYPE: CtrlType = CtrlType::CmdTransferToHost2d;
    type ExpectedResponse = TransferToHost2DResp;

    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>) {
        // Write out fields to iterator.
        //
        // This struct doesn't need any padding bytes.
        self.ctrl_header.write_to_byte_iter(dst);
        self.r.write_to_byte_iter(dst);
        copy_to_iter(dst, u64::to_le_bytes(self.offset).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.resource_id).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.padding).into_iter());
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct TransferToHost2DResp {
    pub ctrl_header: CtrlHeader,
}

impl VirtIOGPUResp for TransferToHost2DResp {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const EXPECTED_CTRL_TYPE: CtrlType = CtrlType::RespOkNoData;

    fn from_byte_iter_post_checked_ctrl_header(
        ctrl_header: CtrlHeader,
        _src: &mut impl Iterator<Item = u8>,
    ) -> Result<Self, ErrorCode> {
        Ok(TransferToHost2DResp { ctrl_header })
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct ResourceFlushReq {
    pub ctrl_header: CtrlHeader,
    pub r: Rect,
    pub resource_id: u32,
    pub padding: u32,
}

impl VirtIOGPUReq for ResourceFlushReq {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const CTRL_TYPE: CtrlType = CtrlType::CmdResourceFlush;
    type ExpectedResponse = ResourceFlushResp;

    fn write_to_byte_iter<'a>(&self, dst: &mut impl Iterator<Item = &'a mut u8>) {
        // Write out fields to iterator.
        //
        // This struct doesn't need any padding bytes.
        self.ctrl_header.write_to_byte_iter(dst);
        self.r.write_to_byte_iter(dst);
        copy_to_iter(dst, u32::to_le_bytes(self.resource_id).into_iter());
        copy_to_iter(dst, u32::to_le_bytes(self.padding).into_iter());
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
struct ResourceFlushResp {
    pub ctrl_header: CtrlHeader,
}

impl VirtIOGPUResp for ResourceFlushResp {
    const ENCODED_SIZE: usize = core::mem::size_of::<Self>();
    const EXPECTED_CTRL_TYPE: CtrlType = CtrlType::RespOkNoData;

    fn from_byte_iter_post_checked_ctrl_header(
        ctrl_header: CtrlHeader,
        _src: &mut impl Iterator<Item = u8>,
    ) -> Result<Self, ErrorCode> {
        Ok(ResourceFlushResp { ctrl_header })
    }
}

pub const PIXEL_STRIDE: usize = 4;

pub const MAX_ATTACH_BACKING_REQ_MEMORY_ENTRIES: usize = 1;

pub const MAX_REQ_SIZE: usize = max(&[
    ResourceCreate2DReq::ENCODED_SIZE,
    ResourceAttachBackingReq::<{ MAX_ATTACH_BACKING_REQ_MEMORY_ENTRIES }>::ENCODED_SIZE,
    SetScanoutReq::ENCODED_SIZE,
    TransferToHost2DReq::ENCODED_SIZE,
    ResourceFlushReq::ENCODED_SIZE,
    ResourceDetachBackingReq::ENCODED_SIZE,
]);

pub const MAX_RESP_SIZE: usize = max(&[
    ResourceCreate2DResp::ENCODED_SIZE,
    ResourceAttachBackingResp::ENCODED_SIZE,
    SetScanoutResp::ENCODED_SIZE,
    ResourceFlushResp::ENCODED_SIZE,
    ResourceDetachBackingResp::ENCODED_SIZE,
]);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u32)]
#[allow(dead_code)]
enum CtrlType {
    /* 2d commands */
    CmdGetDisplayInfo = 0x0100,
    CmdResourceCreate2d,
    CmdResourceUref,
    CmdSetScanout,
    CmdResourceFlush,
    CmdTransferToHost2d,
    CmdResourceAttachBacking,
    CmdResourceDetachBacking,
    CmdGetCapsetInfo,
    CmdGetCapset,
    CmdGetEdid,

    /* cursor commands */
    CmdUpdateCursor = 0x0300,
    CmdMoveCursor,

    /* success responses */
    RespOkNoData = 0x1100,
    RespOkDisplayInfo,
    RespOkCapsetInfo,
    RespOkCapset,
    RespOkEdid,

    /* error responses */
    RespErrUnspec = 0x1200,
    RespErrOutOfMemory,
    RespErrInvalidScanoutId,
    RespErrInvalidResourceId,
    RespErrInvalidContextId,
    RespErrInvalidParameter,
}

impl TryFrom<u32> for CtrlType {
    type Error = ();

    fn try_from(int: u32) -> Result<Self, Self::Error> {
        match int {
            /* 2d commands */
            v if v == CtrlType::CmdGetDisplayInfo as u32 => Ok(CtrlType::CmdGetDisplayInfo),
            v if v == CtrlType::CmdResourceCreate2d as u32 => Ok(CtrlType::CmdResourceCreate2d),
            v if v == CtrlType::CmdResourceUref as u32 => Ok(CtrlType::CmdResourceUref),
            v if v == CtrlType::CmdSetScanout as u32 => Ok(CtrlType::CmdSetScanout),
            v if v == CtrlType::CmdResourceFlush as u32 => Ok(CtrlType::CmdResourceFlush),
            v if v == CtrlType::CmdTransferToHost2d as u32 => Ok(CtrlType::CmdTransferToHost2d),
            v if v == CtrlType::CmdResourceAttachBacking as u32 => {
                Ok(CtrlType::CmdResourceAttachBacking)
            }
            v if v == CtrlType::CmdResourceDetachBacking as u32 => {
                Ok(CtrlType::CmdResourceDetachBacking)
            }
            v if v == CtrlType::CmdGetCapsetInfo as u32 => Ok(CtrlType::CmdGetCapsetInfo),
            v if v == CtrlType::CmdGetCapset as u32 => Ok(CtrlType::CmdGetCapset),
            v if v == CtrlType::CmdGetEdid as u32 => Ok(CtrlType::CmdGetEdid),

            /* cursor commands */
            v if v == CtrlType::CmdUpdateCursor as u32 => Ok(CtrlType::CmdUpdateCursor),
            v if v == CtrlType::CmdMoveCursor as u32 => Ok(CtrlType::CmdMoveCursor),

            /* success responses */
            v if v == CtrlType::RespOkNoData as u32 => Ok(CtrlType::RespOkNoData),
            v if v == CtrlType::RespOkDisplayInfo as u32 => Ok(CtrlType::RespOkDisplayInfo),
            v if v == CtrlType::RespOkCapsetInfo as u32 => Ok(CtrlType::RespOkCapsetInfo),
            v if v == CtrlType::RespOkCapset as u32 => Ok(CtrlType::RespOkCapset),
            v if v == CtrlType::RespOkEdid as u32 => Ok(CtrlType::RespOkEdid),

            /* error responses */
            v if v == CtrlType::RespErrUnspec as u32 => Ok(CtrlType::RespErrUnspec),
            v if v == CtrlType::RespErrOutOfMemory as u32 => Ok(CtrlType::RespErrOutOfMemory),
            v if v == CtrlType::RespErrInvalidScanoutId as u32 => {
                Ok(CtrlType::RespErrInvalidScanoutId)
            }
            v if v == CtrlType::RespErrInvalidResourceId as u32 => {
                Ok(CtrlType::RespErrInvalidResourceId)
            }
            v if v == CtrlType::RespErrInvalidContextId as u32 => {
                Ok(CtrlType::RespErrInvalidContextId)
            }
            v if v == CtrlType::RespErrInvalidParameter as u32 => {
                Ok(CtrlType::RespErrInvalidParameter)
            }

            _ => Err(()),
        }
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(u32)]
#[allow(dead_code)]
enum VideoFormat {
    B8G8R8A8Unorm = 1,
    B8G8R8X8Unorm = 2,
    A8R8G8B8Unorm = 3,
    X8R8G8B8Unorm = 4,
    R8G8B8A8Unorm = 67,
    X8B8G8R8Unorm = 68,
    A8B8G8R8Unorm = 121,
    R8G8B8X8Unorm = 134,
}

#[derive(Copy, Clone, Debug)]
pub enum VirtIOGPUState {
    Uninitialized,
    InitializingResourceCreate2D,
    InitializingResourceAttachBacking,
    InitializingSetScanout,
    InitializingResourceDetachBacking,
    Idle,
    SettingWriteFrame,
    DrawResourceAttachBacking,
    DrawTransferToHost2D,
    DrawResourceFlush,
    DrawResourceDetachBacking,
}

#[derive(Copy, Clone)]
#[repr(usize)]
pub enum PendingDeferredCall {
    SetWriteFrame,
}

struct PendingDeferredCallMask(Cell<usize>);

impl PendingDeferredCallMask {
    pub fn new() -> Self {
        PendingDeferredCallMask(Cell::new(0))
    }

    pub fn get_copy_and_clear(&self) -> PendingDeferredCallMask {
        let old = PendingDeferredCallMask(self.0.clone());
        self.0.set(0);
        old
    }

    pub fn set(&self, call: PendingDeferredCall) {
        self.0.set(self.0.get() | (1 << (call as usize)));
    }

    pub fn is_set(&self, call: PendingDeferredCall) -> bool {
        (self.0.get() & (1 << (call as usize))) != 0
    }

    pub fn for_each_call(&self, mut f: impl FnMut(PendingDeferredCall)) {
        let mut check_and_invoke = |call| {
            if self.is_set(call) {
                f(call)
            }
        };

        check_and_invoke(PendingDeferredCall::SetWriteFrame);
    }
}

pub struct VirtIOGPU<'a, 'b> {
    // Misc driver state:
    client: OptionalCell<&'a dyn ScreenClient>,
    state: Cell<VirtIOGPUState>,
    deferred_call: DeferredCall,
    pending_deferred_call_mask: PendingDeferredCallMask,

    // VirtIO bus and buffers:
    control_queue: &'a SplitVirtqueue<'a, 'b, 2>,
    req_resp_buffers: OptionalCell<(&'b mut [u8; MAX_REQ_SIZE], &'b mut [u8; MAX_RESP_SIZE])>,

    // Video output parameters:
    width: u32,
    height: u32,

    // Set up by `Screen::set_write_frame`, and then later written to with a
    // call to `Screen::write`. It contains the `Rect` being written to, and the
    // current write offset in (x, y) coordinates:
    current_draw_area: Cell<(
        // Draw area:
        Rect,
        // Current draw offset, relative to the draw area itself:
        (u32, u32),
        // Optimization -- count the number of pixels remaining undrawn:
        usize,
    )>,

    // The client provides us a subslice, but we need to place a `&'static mut`
    // buffer into the VirtQueue. We store the client's bounds here. We can't
    // use a `Range<usize>` as it isn't `Copy`, and so have to store
    // `rnage.start` and `range.end` instead.
    write_buffer_subslice_range: Cell<(usize, usize)>,

    // We can only draw rectangles, but the client can ask us to do arbitrarily
    // sized partial writes. This means that sometimes we might need to perform
    // multiple writes in response to a single client request. This stores the
    // offset into the client's buffer we've processed so far:
    write_buffer_offset: Cell<usize>,

    // Slot for the client's write buffer, while it's attached to the GPU:
    write_buffer: TakeCell<'static, [u8]>,

    // Current rect being transfered to the host:
    current_transfer_area_pixels: Cell<(Rect, usize)>,
}

impl<'a, 'b> VirtIOGPU<'a, 'b> {
    pub fn new(
        control_queue: &'a SplitVirtqueue<'a, 'b, 2>,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
        width: usize,
        height: usize,
    ) -> Result<VirtIOGPU<'a, 'b>, ErrorCode> {
        let width: u32 = width.try_into().map_err(|_| ErrorCode::SIZE)?;
        let height: u32 = height.try_into().map_err(|_| ErrorCode::SIZE)?;

        Ok(VirtIOGPU {
            client: OptionalCell::empty(),
            state: Cell::new(VirtIOGPUState::Uninitialized),
            deferred_call: DeferredCall::new(),
            pending_deferred_call_mask: PendingDeferredCallMask::new(),

            control_queue,
            req_resp_buffers: OptionalCell::new((req_buffer, resp_buffer)),

            width,
            height,

            current_draw_area: Cell::new((Rect::empty(), (0, 0), 0)),
            write_buffer_subslice_range: Cell::new((0, 0)),
            write_buffer_offset: Cell::new(0),
            write_buffer: TakeCell::empty(),
            current_transfer_area_pixels: Cell::new((Rect::empty(), 0)),
        })
    }

    pub fn initialize(&self) -> Result<(), ErrorCode> {
        // We can't double-initialize this device:
        let VirtIOGPUState::Uninitialized = self.state.get() else {
            return Err(ErrorCode::ALREADY);
        };

        // Enable callbacks for used descriptors:
        self.control_queue.enable_used_callbacks();

        // Take the request and response buffers. They must be available during
        // initialization:
        let (req_buffer, resp_buffer) = self.req_resp_buffers.take().unwrap();

        // Step 1: Create host resource
        let cmd_resource_create_2d_req = ResourceCreate2DReq {
            ctrl_header: CtrlHeader {
                ctrl_type: ResourceCreate2DReq::CTRL_TYPE,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            resource_id: 1,
            format: VideoFormat::A8R8G8B8Unorm,
            width: self.width,
            height: self.height,
        };
        cmd_resource_create_2d_req.write_to_byte_iter(&mut req_buffer.iter_mut());

        let mut buffer_chain = [
            Some(VirtqueueBuffer {
                buf: req_buffer,
                len: ResourceCreate2DReq::ENCODED_SIZE,
                device_writeable: false,
            }),
            Some(VirtqueueBuffer {
                buf: resp_buffer,
                len: ResourceCreate2DResp::ENCODED_SIZE,
                device_writeable: true,
            }),
        ];
        self.control_queue
            .provide_buffer_chain(&mut buffer_chain)
            .unwrap();

        self.state.set(VirtIOGPUState::InitializingResourceCreate2D);

        Ok(())
    }

    fn initialize_resource_create_2d_resp(
        &self,
        _resp: ResourceCreate2DResp,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        // Step 2: Attach backing memory (our framebuffer)

        // At first, we attach a zero-sized dummy buffer:
        const ENTRIES: usize = 1;
        let cmd_resource_attach_backing_req: ResourceAttachBackingReq<{ ENTRIES }> =
            ResourceAttachBackingReq {
                ctrl_header: CtrlHeader {
                    ctrl_type: ResourceAttachBackingReq::<{ ENTRIES }>::CTRL_TYPE,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                resource_id: 1,
                nr_entries: ENTRIES as u32,
                entries: [MemEntry {
                    // TODO: use dummy buffer!
                    addr: 1,
                    length: 1,
                    padding: 0,
                }],
            };
        cmd_resource_attach_backing_req.write_to_byte_iter(&mut req_buffer.iter_mut());

        let mut buffer_chain = [
            Some(VirtqueueBuffer {
                buf: req_buffer,
                len: ResourceAttachBackingReq::<{ ENTRIES }>::ENCODED_SIZE,
                device_writeable: false,
            }),
            Some(VirtqueueBuffer {
                buf: resp_buffer,
                len: ResourceAttachBackingResp::ENCODED_SIZE,
                device_writeable: true,
            }),
        ];
        self.control_queue
            .provide_buffer_chain(&mut buffer_chain)
            .unwrap();

        self.state
            .set(VirtIOGPUState::InitializingResourceAttachBacking);
    }

    fn initialize_resource_attach_backing_resp(
        &self,
        _resp: ResourceAttachBackingResp,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        // Step 3: Set scanout
        let cmd_set_scanout_req = SetScanoutReq {
            ctrl_header: CtrlHeader {
                ctrl_type: SetScanoutReq::CTRL_TYPE,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r: Rect {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            },
            scanout_id: 0,
            resource_id: 1,
        };
        cmd_set_scanout_req.write_to_byte_iter(&mut req_buffer.iter_mut());

        let mut buffer_chain = [
            Some(VirtqueueBuffer {
                buf: req_buffer,
                len: SetScanoutReq::ENCODED_SIZE,
                device_writeable: false,
            }),
            Some(VirtqueueBuffer {
                buf: resp_buffer,
                len: SetScanoutResp::ENCODED_SIZE,
                device_writeable: true,
            }),
        ];
        self.control_queue
            .provide_buffer_chain(&mut buffer_chain)
            .unwrap();

        self.state.set(VirtIOGPUState::InitializingSetScanout);
    }

    fn initialize_set_scanout_resp(
        &self,
        _resp: SetScanoutResp,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        // Step 4: Detach resource
        let cmd_resource_detach_backing_req = ResourceDetachBackingReq {
            ctrl_header: CtrlHeader {
                ctrl_type: ResourceDetachBackingReq::CTRL_TYPE,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            resource_id: 1,
            padding: 0,
        };
        cmd_resource_detach_backing_req.write_to_byte_iter(&mut req_buffer.iter_mut());

        let mut buffer_chain = [
            Some(VirtqueueBuffer {
                buf: req_buffer,
                len: ResourceDetachBackingReq::ENCODED_SIZE,
                device_writeable: false,
            }),
            Some(VirtqueueBuffer {
                buf: resp_buffer,
                len: ResourceDetachBackingResp::ENCODED_SIZE,
                device_writeable: true,
            }),
        ];
        self.control_queue
            .provide_buffer_chain(&mut buffer_chain)
            .unwrap();

        self.state
            .set(VirtIOGPUState::InitializingResourceDetachBacking);
    }

    fn initialize_resource_detach_backing_resp(
        &self,
        _resp: ResourceDetachBackingResp,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        // Initialization done! Return the buffers:
        self.req_resp_buffers.replace((req_buffer, resp_buffer));

        // Set the device state:
        self.state.set(VirtIOGPUState::Idle);

        // Then issue the appropriate callback:
        self.client.map(|c| c.screen_is_ready());
    }

    fn continue_draw_transfer_to_host_2d(
        &self,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        // Now, the `TRANSFER_TO_HOST_2D` command can only copy rectangles.
        // However, when we performed a partial write (let's say of just one
        // pixel), then the current x offset will not perfectly line up with the
        // left boundary of the overall draw rectangle. Similarly, when the
        // buffer doesn't perfectly fill up the last row of pixels, we can't
        // draw them together with the previous rows of the rectangle. Thus, a
        // single `write` call may result in at most three underlying
        // `TRANSFER_TO_HOST_2D` commands.
        //
        // At this stage, we have the `write_buffer_subslice_range` set to the
        // client's range, `write_buffer_offset` contains the offset into this
        // subslice range that we've already drawn, and `current_draw_area` has
        // the correct offset into the rectangle on the host.
        let (draw_rect, current_draw_offset, remaining_pixels) = self.current_draw_area.get();
        let (write_buffer_subslice_range_start, write_buffer_subslice_range_end) =
            self.write_buffer_subslice_range.get();
        let write_buffer_subslice_range = Range {
            start: write_buffer_subslice_range_start,
            end: write_buffer_subslice_range_end,
        };
        let write_buffer_offset = self.write_buffer_offset.get();

        // Compute the remaining bytes left in the client-supplied buffer:
        let write_buffer_remaining_bytes = write_buffer_subslice_range
            .len()
            .checked_sub(write_buffer_offset)
            .unwrap();
        assert!(write_buffer_remaining_bytes % PIXEL_STRIDE == 0);
        let write_buffer_remaining_pixels = write_buffer_remaining_bytes / PIXEL_STRIDE;
        assert!(write_buffer_remaining_pixels <= remaining_pixels);

        // Check whether the current draw offset within the rectangle has an `x`
        // coordinate of zero. That means we can copy one or more full rows, or
        // the last partial row of the draw area:
        let transfer_pixels = if draw_rect.is_empty() {
            // Short-circuit an empty draw_rect, to avoid divide by zero
            // areas when using `rect.width` or `rect.height` as a divisor:
            0
        } else if current_draw_offset.0 == 0 {
            // Okay, we can start drawing the full rectangle. We want to try
            // drawing any full rows, if there are any left, and if not the
            // last partial row:
            assert!(current_draw_offset.1 <= draw_rect.height || remaining_pixels == 0);
            if current_draw_offset.1 >= draw_rect.height {
                // Just one row left to draw, and we start from `x ==
                // 0`. This means we can just copy however much more data
                // the client buffer holds. We've previously checked that
                // the client buffer fully fits into the draw area, but
                // re-check that assertion here:
                assert!(draw_rect.width as usize >= write_buffer_remaining_pixels);
                write_buffer_remaining_pixels
            } else {
                // There is more than one row left to copy, and we start
                // from `x == 0`. If the client buffer lines up with the end
                // of a row, we can copy them as a single
                // rectangle. Otherwise, we need two copies:
                write_buffer_remaining_pixels / (draw_rect.width as usize)
                    * (draw_rect.width as usize)
            }
        } else {
            // Our current draw offset is not zero. This means we must copy
            // the current row, and then potentially any subsequent
            // rows. Determine how much to copy based on the lower of the
            // remaining data in the slice, or the remaining row width:
            let remaining_row_width = draw_rect.width.checked_sub(current_draw_offset.0).unwrap();
            core::cmp::min(remaining_row_width as usize, write_buffer_remaining_pixels)
        };

        // If we've got nothing left to copy, great! We're done drawing, but
        // still need to detach the resource:
        if transfer_pixels == 0 {
            let cmd_resource_detach_backing_req = ResourceDetachBackingReq {
                ctrl_header: CtrlHeader {
                    ctrl_type: ResourceDetachBackingReq::CTRL_TYPE,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                resource_id: 1,
                padding: 0,
            };
            cmd_resource_detach_backing_req.write_to_byte_iter(&mut req_buffer.iter_mut());

            let mut buffer_chain = [
                Some(VirtqueueBuffer {
                    buf: req_buffer,
                    len: ResourceDetachBackingReq::ENCODED_SIZE,
                    device_writeable: false,
                }),
                Some(VirtqueueBuffer {
                    buf: resp_buffer,
                    len: ResourceDetachBackingResp::ENCODED_SIZE,
                    device_writeable: true,
                }),
            ];
            self.control_queue
                .provide_buffer_chain(&mut buffer_chain)
                .unwrap();

            self.state.set(VirtIOGPUState::DrawResourceDetachBacking);

            return;
        }

        // Otherwise, build the transfer rect from `transfer_pixels`,
        // `draw_rect` and the current draw offset:
        let transfer_rect = Rect {
            x: draw_rect.x.checked_add(current_draw_offset.0).unwrap(),
            y: draw_rect.y.checked_add(current_draw_offset.1).unwrap(),
            width: core::cmp::min(transfer_pixels, draw_rect.width as usize) as u32,
            height: transfer_pixels.div_ceil(draw_rect.width as usize) as u32,
        };
        self.current_transfer_area_pixels
            .set((transfer_rect, transfer_pixels));

        // Attach write buffer
        let cmd_transfer_to_host_2d_req = TransferToHost2DReq {
            ctrl_header: CtrlHeader {
                ctrl_type: TransferToHost2DReq::CTRL_TYPE,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r: transfer_rect,
            offset: write_buffer_offset as u64,
            resource_id: 1,
            padding: 0,
        };
        kernel::debug!(
            "Transfer to host {:?}, {:?}",
            transfer_rect,
            write_buffer_offset
        );
        cmd_transfer_to_host_2d_req.write_to_byte_iter(&mut req_buffer.iter_mut());

        let mut buffer_chain = [
            Some(VirtqueueBuffer {
                buf: req_buffer,
                len: TransferToHost2DReq::ENCODED_SIZE,
                device_writeable: false,
            }),
            Some(VirtqueueBuffer {
                buf: resp_buffer,
                len: TransferToHost2DResp::ENCODED_SIZE,
                device_writeable: true,
            }),
        ];
        self.control_queue
            .provide_buffer_chain(&mut buffer_chain)
            .unwrap();

        self.state.set(VirtIOGPUState::DrawTransferToHost2D);
    }

    fn continue_draw_resource_flush(
        &self,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        let (current_transfer_area, _) = self.current_transfer_area_pixels.get();

        let cmd_resource_flush_req = ResourceFlushReq {
            ctrl_header: CtrlHeader {
                ctrl_type: ResourceFlushReq::CTRL_TYPE,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r: current_transfer_area,
            resource_id: 1,
            padding: 0,
        };
        cmd_resource_flush_req.write_to_byte_iter(&mut req_buffer.iter_mut());

        let mut buffer_chain = [
            Some(VirtqueueBuffer {
                buf: req_buffer,
                len: ResourceFlushReq::ENCODED_SIZE,
                device_writeable: false,
            }),
            Some(VirtqueueBuffer {
                buf: resp_buffer,
                len: ResourceFlushResp::ENCODED_SIZE,
                device_writeable: true,
            }),
        ];
        self.control_queue
            .provide_buffer_chain(&mut buffer_chain)
            .unwrap();

        self.state.set(VirtIOGPUState::DrawResourceFlush);
    }

    fn continue_draw_resource_flushed(
        &self,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        // We've finished one write command, but there might be more to
        // come. Increment `current_draw_offset` and `write_buffer_offset`, and
        // decrement `remaining_pixels` accordingly.
        let (draw_rect, mut current_draw_offset, mut remaining_pixels) =
            self.current_draw_area.get();
        let mut write_buffer_offset = self.write_buffer_offset.get();

        // This is what we've just drawn:
        let (drawn_area, drawn_pixels) = self.current_transfer_area_pixels.get();

        // We always draw left -> right, top -> bottom, so we can simply set the
        // current `x` and `y` coordinates to the bottom-right most coordinates
        // we've just drawn (while wrapping and carrying the one):
        current_draw_offset.0 = drawn_area
            .x
            .checked_add(drawn_area.width)
            .and_then(|drawn_x1| drawn_x1.checked_sub(draw_rect.x))
            .unwrap();
        current_draw_offset.1 = drawn_area
            .y
            .checked_add(drawn_area.height)
            .and_then(|drawn_y1| drawn_y1.checked_sub(draw_rect.y))
            .unwrap();

        // Wrap to the next line when we've finished writing the column of our
        // last row drawn:
        assert!(current_draw_offset.0 <= draw_rect.width);
        if current_draw_offset.0 == draw_rect.width {
            current_draw_offset.0 = 0;
            current_draw_offset.1 = current_draw_offset.1.checked_add(1).unwrap();
        }

        // Subtract our drawn_pixels from `remaining_pixels`:
        assert!(remaining_pixels >= drawn_pixels);
        remaining_pixels -= drawn_pixels;

        // Add our drawn pixels * PIXEL_STRIDE to the buffer offset:
        write_buffer_offset += drawn_pixels.checked_mul(PIXEL_STRIDE).unwrap();

        // Write all of this back:
        self.current_draw_area
            .set((draw_rect, current_draw_offset, remaining_pixels));
        self.write_buffer_offset.set(write_buffer_offset);

        // And continue drawing:
        self.continue_draw_transfer_to_host_2d(req_buffer, resp_buffer);
    }

    fn continue_draw_resource_detached_backing(
        &self,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        self.req_resp_buffers.replace((req_buffer, resp_buffer));
        self.state.set(VirtIOGPUState::Idle);

        let (write_buffer_subslice_range_start, write_buffer_subslice_range_end) =
            self.write_buffer_subslice_range.get();
        let write_buffer_subslice_range = Range {
            start: write_buffer_subslice_range_start,
            end: write_buffer_subslice_range_end,
        };

        let mut subslice = SubSliceMut::new(self.write_buffer.take().unwrap());
        subslice.slice(write_buffer_subslice_range);

        self.client.map(|c| c.write_complete(subslice, Ok(())));
    }

    fn buffer_chain_callback(
        &self,
        buffer_chain: &mut [Option<VirtqueueBuffer<'b>>],
        _bytes_used: usize,
    ) {
        // Every response should return exactly two buffers: one
        // request buffer, and one response buffer.
        let req_buffer = buffer_chain
            .get_mut(0)
            .and_then(|opt_buf| opt_buf.take())
            .expect("Missing request buffer in VirtIO GPU buffer chain");
        let resp_buffer = buffer_chain
            .get_mut(1)
            .and_then(|opt_buf| opt_buf.take())
            .expect("Missing request buffer in VirtIO GPU buffer chain");

        // Convert the buffer slices back into arrays:
        let req_array: &mut [u8; MAX_REQ_SIZE] = req_buffer
            .buf
            .try_into()
            .expect("Returned VirtIO GPU request buffer has unexpected size!");

        let resp_length = resp_buffer.len;
        let resp_array: &mut [u8; MAX_RESP_SIZE] = resp_buffer
            .buf
            .try_into()
            .expect("Returned VirtIO GPU response buffer has unexpected size!");

        // Check that the response has a length we can parse into a CtrlHeader:
        if resp_length < CtrlHeader::ENCODED_SIZE {
            panic!(
                "VirtIO GPU returned response smaller than the CtrlHeader, \
                 which we cannot parse! Returned bytes: {}",
                resp_length
            )
        }

        // We progressively parse the response, starting with the CtrlHeader
        // shared across all messages, checking its type, and then parsing the
        // rest. We do so by reusing a common iterator across these operations:
        let mut resp_iter = resp_array.iter().copied();
        let ctrl_header = CtrlHeader::from_byte_iter(&mut resp_iter)
            .expect("Failed to parse VirtIO response CtrlHeader");

        // We now match the current device state with the ctrl_type
        // that was returned to continue parsing:
        match (self.state.get(), ctrl_header.ctrl_type) {
            (
                VirtIOGPUState::InitializingResourceCreate2D,
                ResourceCreate2DResp::EXPECTED_CTRL_TYPE,
            ) => {
                // Parse the remainder of the response:
                let resp = ResourceCreate2DResp::from_byte_iter_post_ctrl_header(
                    ctrl_header,
                    &mut resp_iter,
                )
                .expect("Failed to parse VirtIO GPU ResourceCreate2DResp");

                // Continue the initialization routine:
                self.initialize_resource_create_2d_resp(resp, req_array, resp_array);
            }

            (
                VirtIOGPUState::InitializingResourceAttachBacking,
                ResourceAttachBackingResp::EXPECTED_CTRL_TYPE,
            ) => {
                // Parse the remainder of the response:
                let resp = ResourceAttachBackingResp::from_byte_iter_post_ctrl_header(
                    ctrl_header,
                    &mut resp_iter,
                )
                .expect("Failed to parse VirtIO GPU ResourceAttachBackingResp");

                // Continue the initialization routine:
                self.initialize_resource_attach_backing_resp(resp, req_array, resp_array);
            }

            (VirtIOGPUState::InitializingSetScanout, SetScanoutResp::EXPECTED_CTRL_TYPE) => {
                // Parse the remainder of the response:
                let resp =
                    SetScanoutResp::from_byte_iter_post_ctrl_header(ctrl_header, &mut resp_iter)
                        .expect("Failed to parse VirtIO GPU SetScanoutResp");

                // Continue the initialization routine:
                self.initialize_set_scanout_resp(resp, req_array, resp_array);
            }

            (
                VirtIOGPUState::InitializingResourceDetachBacking,
                ResourceDetachBackingResp::EXPECTED_CTRL_TYPE,
            ) => {
                // Parse the remainder of the response:
                let resp = ResourceDetachBackingResp::from_byte_iter_post_ctrl_header(
                    ctrl_header,
                    &mut resp_iter,
                )
                .expect("Failed to parse VirtIO GPU ResourceDetachBackingResp");

                // Continue the initialization routine:
                self.initialize_resource_detach_backing_resp(resp, req_array, resp_array);
            }

            (
                VirtIOGPUState::DrawResourceAttachBacking,
                ResourceAttachBackingResp::EXPECTED_CTRL_TYPE,
            ) => {
                // Parse the remainder of the response:
                let _resp = ResourceAttachBackingResp::from_byte_iter_post_ctrl_header(
                    ctrl_header,
                    &mut resp_iter,
                )
                .expect("Failed to parse VirtIO GPU ResourceAttachBackingResp");

                // Continue the initialization routine:
                self.continue_draw_transfer_to_host_2d(req_array, resp_array);
            }

            (VirtIOGPUState::DrawTransferToHost2D, TransferToHost2DResp::EXPECTED_CTRL_TYPE) => {
                // Parse the remainder of the response:
                let _resp = TransferToHost2DResp::from_byte_iter_post_ctrl_header(
                    ctrl_header,
                    &mut resp_iter,
                )
                .expect("Failed to parse VirtIO GPU TransferToHost2DResp");

                // Continue the initialization routine:
                self.continue_draw_resource_flush(req_array, resp_array);
            }

            (VirtIOGPUState::DrawResourceFlush, ResourceFlushResp::EXPECTED_CTRL_TYPE) => {
                // Parse the remainder of the response:
                let _resp =
                    ResourceFlushResp::from_byte_iter_post_ctrl_header(ctrl_header, &mut resp_iter)
                        .expect("Failed to parse VirtIO GPU ResourceFlushResp");

                // Continue the initialization routine:
                self.continue_draw_resource_flushed(req_array, resp_array);
            }

            (
                VirtIOGPUState::DrawResourceDetachBacking,
                ResourceDetachBackingResp::EXPECTED_CTRL_TYPE,
            ) => {
                // Parse the remainder of the response:
                let _resp = ResourceDetachBackingResp::from_byte_iter_post_ctrl_header(
                    ctrl_header,
                    &mut resp_iter,
                )
                .expect("Failed to parse VirtIO GPU ResourceDetachBackingResp");

                // Continue the initialization routine:
                self.continue_draw_resource_detached_backing(req_array, resp_array);
            }

            (VirtIOGPUState::Uninitialized, _)
            | (VirtIOGPUState::InitializingResourceCreate2D, _)
            | (VirtIOGPUState::InitializingResourceAttachBacking, _)
            | (VirtIOGPUState::InitializingSetScanout, _)
            | (VirtIOGPUState::InitializingResourceDetachBacking, _)
            | (VirtIOGPUState::Idle, _)
            | (VirtIOGPUState::SettingWriteFrame, _)
            | (VirtIOGPUState::DrawResourceAttachBacking, _)
            | (VirtIOGPUState::DrawTransferToHost2D, _)
            | (VirtIOGPUState::DrawResourceFlush, _)
            | (VirtIOGPUState::DrawResourceDetachBacking, _) => {
                panic!("Received unexpected VirtIO GPU device response. Device state: {:?}, ctrl hader: {:?}", self.state.get(), ctrl_header);
            }
        }
    }
}

impl<'a> Screen<'a> for VirtIOGPU<'a, '_> {
    fn set_client(&self, client: &'a dyn ScreenClient) {
        self.client.replace(client);
    }

    fn get_resolution(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    fn get_pixel_format(&self) -> ScreenPixelFormat {
        ScreenPixelFormat::ARGB_8888
    }

    fn get_rotation(&self) -> ScreenRotation {
        ScreenRotation::Normal
    }

    fn set_write_frame(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> Result<(), ErrorCode> {
        // Make sure we're idle:
        let VirtIOGPUState::Idle = self.state.get() else {
            return Err(ErrorCode::BUSY);
        };

        kernel::debug!("set_write_frame({}, {}, {}, {})", x, y, width, height);

        // We first convert the coordinates to u32s:
        let x: u32 = x.try_into().map_err(|_| ErrorCode::INVAL)?;
        let y: u32 = y.try_into().map_err(|_| ErrorCode::INVAL)?;
        let width: u32 = width.try_into().map_err(|_| ErrorCode::INVAL)?;
        let height: u32 = height.try_into().map_err(|_| ErrorCode::INVAL)?;

        // Ensure that the draw area actually fits our screen:
        let x1 = x.checked_add(width).ok_or(ErrorCode::INVAL)?;
        let y1 = y.checked_add(height).ok_or(ErrorCode::INVAL)?;
        if x1 > self.width || y1 > self.height {
            return Err(ErrorCode::INVAL);
        }

        // Store the new drawing area as the bounding box and offset coordinates
        // for `write`:
        self.current_draw_area.set((
            // Draw area:
            Rect {
                x,
                y,
                width,
                height,
            },
            // Current draw offset, relative to the draw area itself:
            (0, 0),
            // Precompute the number of pixels in this draw area:
            (width as usize)
                .checked_mul(height as usize)
                .ok_or(ErrorCode::INVAL)?,
        ));

        // Set the device state to busy and issue the callback in a deferred
        // call:
        self.state.set(VirtIOGPUState::SettingWriteFrame);
        self.pending_deferred_call_mask
            .set(PendingDeferredCall::SetWriteFrame);
        self.deferred_call.set();

        Ok(())
    }

    fn write(
        &self,
        buffer: SubSliceMut<'static, u8>,
        continue_write: bool,
    ) -> Result<(), ErrorCode> {
        // Make sure we're idle:
        let VirtIOGPUState::Idle = self.state.get() else {
            return Err(ErrorCode::BUSY);
        };

        kernel::debug!("write(len = {}, {:?})", buffer.len(), continue_write);

        // If `continue_write` is false, we must reset `x_off` and
        // `y_off`. Otherwise we start at the stored offset.
        let (draw_rect, mut current_draw_offset, mut remaining_pixels) =
            self.current_draw_area.get();
        if !continue_write {
            current_draw_offset = (0, 0);
            // This multiplication must not overflow, as we've already performed
            // it before in `set_write_area`:
            remaining_pixels = (draw_rect.width as usize)
                .checked_mul(draw_rect.height as usize)
                .unwrap();
        }
        self.current_draw_area
            .set((draw_rect, current_draw_offset, remaining_pixels));

        // Ensure that this buffer is evenly divisible by PIXEL_STRIDE and that
        // it can fit into the remaining part of the draw area:
        if buffer.len() % PIXEL_STRIDE != 0 {
            return Err(ErrorCode::INVAL);
        }
        if buffer.len() / PIXEL_STRIDE > remaining_pixels {
            return Err(ErrorCode::SIZE);
        }

        // Now, the `TRANSFER_TO_HOST_2D` command can only copy rectangles.
        // However, when we performed a partial write (let's say of just one
        // pixel), then the current x offset will not perfectly line up with the
        // left boundary of the overall draw rectangle. Similarly, when the
        // buffer doesn't perfectly fill up the last row of pixels, we can't
        // draw them together with the previous rows of the rectangle. Thus, a
        // single `write` call may result in at most three underlying
        // `TRANSFER_TO_HOST_2D` commands.
        //
        // We use a common subroutine to identify the next data to copy. We
        // first store the overall subslice active range, and the offset in this
        // subslice (0 right now!), and then let that subroutine handle the rest:
        let write_buffer_subslice_range = buffer.active_range();
        self.write_buffer_subslice_range.set((
            write_buffer_subslice_range.start,
            write_buffer_subslice_range.end,
        ));
        self.write_buffer_offset.set(0);

        let (req_buffer, resp_buffer) = self.req_resp_buffers.take().unwrap();

        // Now, attach the user-supplied buffer to this device:
        let buffer_slice = buffer.take();

        const ENTRIES: usize = 1;
        let cmd_resource_attach_backing_req: ResourceAttachBackingReq<{ ENTRIES }> =
            ResourceAttachBackingReq {
                ctrl_header: CtrlHeader {
                    ctrl_type: ResourceAttachBackingReq::<{ ENTRIES }>::CTRL_TYPE,
                    flags: 0,
                    fence_id: 0,
                    ctx_id: 0,
                    padding: 0,
                },
                resource_id: 1,
                nr_entries: ENTRIES as u32,
                entries: [MemEntry {
                    addr: buffer_slice.as_ptr() as u64 + write_buffer_subslice_range.start as u64,
                    length: write_buffer_subslice_range.len() as u32,
                    padding: 0,
                }],
            };
        cmd_resource_attach_backing_req.write_to_byte_iter(&mut req_buffer.iter_mut());

        assert!(self.write_buffer.replace(buffer_slice).is_none());

        let mut buffer_chain = [
            Some(VirtqueueBuffer {
                buf: req_buffer,
                len: ResourceAttachBackingReq::<{ ENTRIES }>::ENCODED_SIZE,
                device_writeable: false,
            }),
            Some(VirtqueueBuffer {
                buf: resp_buffer,
                len: ResourceAttachBackingResp::ENCODED_SIZE,
                device_writeable: true,
            }),
        ];
        self.control_queue
            .provide_buffer_chain(&mut buffer_chain)
            .unwrap();

        self.state.set(VirtIOGPUState::DrawResourceAttachBacking);

        Ok(())
    }

    // fn write(
    //     &self,
    //     mut _buffer: SubSliceMut<'static, u8>,
    //     _continue_write: bool,
    // ) -> Result<(), ErrorCode> {

    //     // // Make sure the buffer has a length compatible with our pixel mode:
    //     // if buffer.len() % PIXEL_STRIDE != 0 {
    //     //     // TODO: this error code is not yet supported in the HIL:
    //     //     return Err(ErrorCode::INVAL);
    //     // }
    //     // let buffer_pixels = buffer.len() / PIXEL_STRIDE;

    //     // // Check whether this buffer will fit the remaining draw area:
    //     // if buffer_pixels > pixels_remaining {
    //     //     return Err(ErrorCode::SIZE);
    //     // }

    //     // This following code is wrong, it needs to draw row-by-row instead.
    //     todo!();

    //     // // Okay, looks good, we can start drawing! Calculate the start offset
    //     // // into our framebuffer.
    //     // let fb_start_byte_offset = (x_off as usize)
    //     //     .checked_mul(self.width as usize)
    //     //     .and_then(|o| o.checked_add(y_off as usize))
    //     //     .and_then(|o| o.checked_mul(PIXEL_STRIDE))
    //     //     .unwrap();
    //     // let fb_end_byte_offset = fb_start_byte_offset.checked_add(buffer.len()).unwrap();

    //     // // The frame buffer must be accessible here. We never "take" it for
    //     // // longer than a single, synchronous method call:
    //     // self.frame_buffer
    //     //     .map(|fb| {
    //     //         fb[fb_start_byte_offset..fb_end_byte_offset].copy_from_slice(buffer.as_slice())
    //     //     })
    //     //     .unwrap();

    //     // // Update the offset in the draw area, and the number of pixels
    //     // // remaining:
    //     // self.current_draw_area.set((
    //     //     draw_rect,
    //     //     (
    //     //         x_off + u32::try_from(buffer_pixels / self.width as usize).unwrap(),
    //     //         y_off + u32::try_from(buffer_pixels % self.width as usize).unwrap(),
    //     //     ),
    //     //     pixels_remaining - buffer_pixels,
    //     // ));

    //     // // Extend the pending draw area by the drawn bytes.
    //     // //
    //     // // TODO: this could be made more efficient by actually respecting the
    //     // // offsets and length of the buffer written. For now, we just flush the
    //     // // whole `draw_rect`:
    //     // self.pending_draw_area
    //     //     .set(self.pending_draw_area.get().extend(draw_rect));

    //     // // Store the client's buffer. We must hold on to it until we issue the
    //     // // callback:
    //     // assert!(self.client_write_buffer.replace(buffer).is_none());

    //     // // Tell the screen to draw, please. This will also transition the GPU
    //     // // device state:
    //     // self.draw_frame_buffer(DrawMode::Write);

    //     // Ok(())
    // }

    fn set_brightness(&self, _brightness: u16) -> Result<(), ErrorCode> {
        // nop, not supported
        Ok(())
    }

    fn set_power(&self, enabled: bool) -> Result<(), ErrorCode> {
        if !enabled {
            Err(ErrorCode::INVAL)
        } else {
            Ok(())
        }
    }

    fn set_invert(&self, _enabled: bool) -> Result<(), ErrorCode> {
        Err(ErrorCode::NOSUPPORT)
    }
}

// impl<'a> InMemoryFrameBufferScreen<'a> for VirtIOGPU<'a, '_> {
//     fn write_to_frame_buffer(
//         &self,
//         f: impl FnOnce(ScreenDims, ScreenPixelFormat, &mut [u8]) -> Result<ScreenRect, ErrorCode>,
//     ) -> Result<(), ErrorCode> {
//         // Check that we're not busy. We allow multiple calls to this method, as
//         // per its documentation.
//         let idle = match self.state.get() {
//             VirtIOGPUState::Idle => true,
//             VirtIOGPUState::DrawTransferToHost2D(DrawMode::WriteToFrameBuffer) => false,
//             VirtIOGPUState::DrawResourceFlush(DrawMode::WriteToFrameBuffer) => false,
//             _ => return Err(ErrorCode::BUSY),
//         };

//         // Try to get a hold of the frame buffer. If it's already taken, this is
//         // likely because of a reentrant call to this function. Return `BUSY` in
//         // that case:
//         let Some(frame_buffer) = self.frame_buffer.take() else {
//             return Err(ErrorCode::BUSY);
//         };

//         // Pass it to the closure:
//         let closure_res = f(
//             ScreenDims {
//                 x: self.width as usize,
//                 y: self.height as usize,
//             },
//             ScreenPixelFormat::ARGB_8888,
//             frame_buffer,
//         );

//      let led_offset = (24 * self.width) as usize;
//         kernel::debug!("{:x?}", &frame_buffer[led_offset..(led_offset + 128)]);

//         // Replace the frame_buffer unconditionally:
//         self.frame_buffer.replace(frame_buffer);

//         match closure_res {
//             Err(e) => {
//                 // The closure returned an error, we do not need to emit a
//                 // callback.
//                 Err(e)
//             }

//             Ok(screen_rect) => {
//                 // The closure modified the frame buffer, issue a redraw of the
//                 // changed area. We first check that the to-draw area actually
//                 // fits:
//                 let x: u32 = screen_rect.x.try_into().map_err(|_| ErrorCode::SIZE)?;
//                 let y: u32 = screen_rect.y.try_into().map_err(|_| ErrorCode::SIZE)?;
//                 let width: u32 = screen_rect.width.try_into().map_err(|_| ErrorCode::SIZE)?;
//                 let height: u32 = screen_rect.height.try_into().map_err(|_| ErrorCode::SIZE)?;

//                 if x.checked_add(width).ok_or(ErrorCode::SIZE)? > self.width
//                     || y.checked_add(height).ok_or(ErrorCode::SIZE)? > self.height
//                 {
//                     return Err(ErrorCode::SIZE);
//                 }

//                 // Extend the to-redraw area:
//                 self.pending_draw_area
//                     .set(self.pending_draw_area.get().extend(Rect {
//                         x,
//                         y,
//                         width,
//                         height,
//                     }));

//                 let k = self.pending_draw_area.get();

//                 if height == 24 {
//                     kernel::debug!(
//                         "new pending_draw_area x{} y{} width{} height{}",
//                         k.x,
//                         k.y,
//                         k.width,
//                         k.height
//                     );
//                 }

//                 // If we're idle, issue a re-draw. Otherwise, one will
//                 // automatically be issued after the current draw operation:
//                 if idle {
//                  kernel::debug!("not idle");
//                     self.draw_frame_buffer(DrawMode::WriteToFrameBuffer);
//                 }

//                 Ok(())
//             }
//         }
//     }
// }

impl<'b> SplitVirtqueueClient<'b> for VirtIOGPU<'_, 'b> {
    fn buffer_chain_ready(
        &self,
        _queue_number: u32,
        buffer_chain: &mut [Option<VirtqueueBuffer<'b>>],
        bytes_used: usize,
    ) {
        self.buffer_chain_callback(buffer_chain, bytes_used)
    }
}

impl DeferredCallClient for VirtIOGPU<'_, '_> {
    fn register(&'static self) {
        self.deferred_call.register(self);
    }

    fn handle_deferred_call(&self) {
        let calls = self.pending_deferred_call_mask.get_copy_and_clear();
        calls.for_each_call(|call| match call {
            PendingDeferredCall::SetWriteFrame => {
                let VirtIOGPUState::SettingWriteFrame = self.state.get() else {
                    panic!(
                        "Unexpected VirtIOGPUState {:?} for SetWriteFrame deferred call",
                        self.state.get()
                    );
                };

                // Set the device staste back to idle:
                self.state.set(VirtIOGPUState::Idle);

                // Issue callback:
                self.client.map(|c| c.command_complete(Ok(())));
            }
        })
    }
}

impl VirtIODeviceDriver for VirtIOGPU<'_, '_> {
    fn negotiate_features(&self, _offered_features: u64) -> Option<u64> {
        // We don't support any special features and do not care about
        // what the device offers.
        Some(0)
    }

    fn device_type(&self) -> VirtIODeviceType {
        VirtIODeviceType::GPUDevice
    }
}
