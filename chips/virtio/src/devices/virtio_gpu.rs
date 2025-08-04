// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2022.

use core::cell::Cell;

use kernel::deferred_call::{DeferredCall, DeferredCallClient};
use kernel::hil::screen::{
    Dims as ScreenDims, InMemoryFrameBufferScreen, Rect as ScreenRect, Screen, ScreenClient,
    ScreenPixelFormat, ScreenRotation,
};
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

    pub fn extend(&self, other: Rect) -> Rect {
        use core::cmp::{max, min};

        // If either one of the `Rect`s is empty, simply return the other:
        if self.is_empty() {
            other
        } else if other.is_empty() {
            *self
        } else {
            // Determine the "x1" for both self and other, so that we can calculate
            // the final width based on the distance of the larger of the two "x0"s
            // and the larger of the two "x1"s:
            let self_x1 = self.x.saturating_add(self.width);
            let other_x1 = other.x.saturating_add(other.width);

            // Same for "y1"s:
            let self_y1 = self.y.saturating_add(self.height);
            let other_y1 = other.y.saturating_add(other.height);

            // Now, build the rect:
            let new_x0 = min(self.x, other.x);
            let new_x1 = max(self_x1, other_x1);
            let new_y0 = min(self.y, other.y);
            let new_y1 = max(self_y1, other_y1);
            Rect {
                x: new_x0,
                y: new_y0,
                width: new_x1.saturating_sub(new_x0),
                height: new_y1.saturating_sub(new_y0),
            }
        }
    }

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
]);

pub const MAX_RESP_SIZE: usize = max(&[
    ResourceCreate2DResp::ENCODED_SIZE,
    ResourceAttachBackingResp::ENCODED_SIZE,
    SetScanoutResp::ENCODED_SIZE,
    ResourceFlushResp::ENCODED_SIZE,
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DrawMode {
    Write,
    WriteToFrameBuffer,
}

#[derive(Copy, Clone, Debug)]
pub enum VirtIOGPUState {
    Uninitialized,
    InitializingResourceCreate2D,
    InitializingResourceAttachBacking,
    InitializingSetScanout,
    InitializingTransferToHost2D,
    InitializingResourceFlush,
    Idle,
    SettingWriteFrame,
    DrawTransferToHost2D(DrawMode),
    DrawResourceFlush(DrawMode),
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

    // Frame buffer and output parameters:
    frame_buffer: TakeCell<'a, [u8]>,
    width: u32,
    height: u32,

    // Pending output update state:
    current_flush_area: Cell<Rect>,
    pending_draw_area: Cell<Rect>,

    // Set up by `Screen::set_write_frame`, and then later written to with a
    // call to `Screen::write`. It contains the `Rect` being written to, and the
    // current write offset in (x, y) coordinates:
    current_draw_area: Cell<(
        // Draw area:
        Rect,
        // Current draw offset:
        (u32, u32),
        // Optimization -- number of pixels left in the draw area, starting from
        // the offset:
        usize,
    )>,
    client_write_buffer: OptionalCell<SubSliceMut<'static, u8>>,
}

impl<'a, 'b> VirtIOGPU<'a, 'b> {
    pub fn new(
        control_queue: &'a SplitVirtqueue<'a, 'b, 2>,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
        frame_buffer: &'a mut [u8],
        width: usize,
        height: usize,
    ) -> Result<VirtIOGPU<'a, 'b>, ErrorCode> {
        let width: u32 = width.try_into().map_err(|_| ErrorCode::SIZE)?;
        let height: u32 = height.try_into().map_err(|_| ErrorCode::SIZE)?;
        let pixel_data_size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|p| p.checked_mul(PIXEL_STRIDE))
            .ok_or(ErrorCode::SIZE)?;
        if pixel_data_size != frame_buffer.len() {
            return Err(ErrorCode::SIZE);
        }

        Ok(VirtIOGPU {
            client: OptionalCell::empty(),
            state: Cell::new(VirtIOGPUState::Uninitialized),
            deferred_call: DeferredCall::new(),
            pending_deferred_call_mask: PendingDeferredCallMask::new(),

            control_queue,
            req_resp_buffers: OptionalCell::new((req_buffer, resp_buffer)),

            frame_buffer: TakeCell::new(frame_buffer),
            width,
            height,

            current_flush_area: Cell::new(Rect::empty()),
            pending_draw_area: Cell::new(Rect::empty()),
            current_draw_area: Cell::new((Rect::empty(), (0, 0), 0)),
            client_write_buffer: OptionalCell::empty(),
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

        // Mark the entire frame buffer as to be re-drawn:
        self.pending_draw_area.set(Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        });

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

        // We first determine the address of our framebuffer. Even
        // though it lives in a TakeCell and we can take it out and
        // put it back in, that only affects the reference, but no the
        // address of the underlying buffer. That stays constant for
        // as long as this driver instance lives:
        let (frame_buffer_addr, frame_buffer_length) = self
            .frame_buffer
            .map(|fb| (fb.as_mut_ptr(), fb.len()))
            .unwrap();

        // Now, tell inform the device of this buffer:
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
                    addr: frame_buffer_addr as u64,
                    length: frame_buffer_length as u32,
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
        // Initialization done! Return the buffers, first of all:
        self.req_resp_buffers.replace((req_buffer, resp_buffer));

        // As one final step, we draw the contents of the framebuffer that was
        // passed to us initially. We use the common `draw_frame_buffer_int`
        // method, but setting the appropriate state, to distinguish from a
        // regular draw command:
        self.state.set(VirtIOGPUState::InitializingTransferToHost2D);
        self.draw_frame_buffer_int();
    }

    fn draw_frame_buffer(&self, mode: DrawMode) {
        // Call the `draw_frame_buffer_int` shared with the initialization
        // routine, but setting a `DrawTransferToHost2D` state instead, which
        // communicates that we're not in the initialization routine any more:
        self.state.set(VirtIOGPUState::DrawTransferToHost2D(mode));
        self.draw_frame_buffer_int();
    }

    fn draw_frame_buffer_int(&self) {
        // Make sure we've entered the correct state before calling this method:
        match self.state.get() {
            VirtIOGPUState::DrawTransferToHost2D(_mode) => (),
            VirtIOGPUState::InitializingTransferToHost2D => (),
            s => panic!("Called draw_frame_buffer_int in invalid state {:?}", s),
        }

        // Transfer the `pending_draw_area` into the `current_flush_area`, and
        // reset the `pending_draw_area`. This allows concurrent calls to
        // `write_to_frame_buffer` to set up new redraw areas while we're
        // performing the flush:
        self.current_flush_area.set(self.pending_draw_area.get());
        self.pending_draw_area.set(Rect::empty());

        let (req_buffer, resp_buffer) = self.req_resp_buffers.take().unwrap();

        let cmd_transfer_to_host_2d_req = TransferToHost2DReq {
            ctrl_header: CtrlHeader {
                ctrl_type: TransferToHost2DReq::CTRL_TYPE,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r: self.current_flush_area.get(),
            offset: 0,
            resource_id: 1,
            padding: 0,
        };
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
    }

    fn draw_transfer_to_host_2d_resp(
        &self,
        _resp: TransferToHost2DResp,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        // Now draw the contents of the framebuffer that was passed to us
        // initially:
        let cmd_resource_flush_req = ResourceFlushReq {
            ctrl_header: CtrlHeader {
                ctrl_type: ResourceFlushReq::CTRL_TYPE,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r: self.current_flush_area.get(),
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

        match self.state.get() {
            VirtIOGPUState::InitializingTransferToHost2D => {
                self.state.set(VirtIOGPUState::InitializingResourceFlush);
            }
            VirtIOGPUState::DrawTransferToHost2D(mode) => {
                self.state.set(VirtIOGPUState::DrawResourceFlush(mode));
            }
            s => panic!(
                "Called draw_transfer_to_host_2d_resp in unexpected state {:?}",
                s
            ),
        }
    }

    fn draw_resource_flush_resp(
        &self,
        _resp: ResourceFlushResp,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        self.req_resp_buffers.replace((req_buffer, resp_buffer));

        // Reset the flush area:
        self.current_flush_area.set(Rect::empty());

        // Issue the appropriate callback:
        match self.state.get() {
            VirtIOGPUState::DrawResourceFlush(DrawMode::Write) => {
                self.client
                    .map(|c| c.write_complete(self.client_write_buffer.take().unwrap(), Ok(())));
            }
            VirtIOGPUState::DrawResourceFlush(DrawMode::WriteToFrameBuffer) => {
                self.client.map(|c| c.command_complete(Ok(())));
            }
            VirtIOGPUState::InitializingResourceFlush => {
                self.client.map(|c| c.screen_is_ready());
            }
            s => panic!(
                "Called draw_transfer_to_host_2d_resp in unexpected state {:?}",
                s
            ),
        }

        // Check if we have received more data to draw in the meantime. This is
        // only possible when using `write_to_frame_buffer`:
        if !self.pending_draw_area.get().is_empty() {
            // Start another draw operation:
            self.draw_frame_buffer(DrawMode::WriteToFrameBuffer);
        } else {
            // Return to idle:
            self.state.set(VirtIOGPUState::Idle);
        }
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
                VirtIOGPUState::InitializingTransferToHost2D,
                TransferToHost2DResp::EXPECTED_CTRL_TYPE,
            )
            | (VirtIOGPUState::DrawTransferToHost2D(_), TransferToHost2DResp::EXPECTED_CTRL_TYPE) =>
            {
                // Parse the remainder of the response:
                let resp = TransferToHost2DResp::from_byte_iter_post_ctrl_header(
                    ctrl_header,
                    &mut resp_iter,
                )
                .expect("Failed to parse VirtIO GPU TransferToHost2DResp");

                // Continue the initialization routine:
                self.draw_transfer_to_host_2d_resp(resp, req_array, resp_array);
            }

            (VirtIOGPUState::InitializingResourceFlush, ResourceFlushResp::EXPECTED_CTRL_TYPE)
            | (VirtIOGPUState::DrawResourceFlush(_), ResourceFlushResp::EXPECTED_CTRL_TYPE) => {
                // Parse the remainder of the response:
                let resp =
                    ResourceFlushResp::from_byte_iter_post_ctrl_header(ctrl_header, &mut resp_iter)
                        .expect("Failed to parse VirtIO GPU ResourceFlushResp");

                // Continue the initialization routine:
                self.draw_resource_flush_resp(resp, req_array, resp_array);
            }

            (VirtIOGPUState::Uninitialized, _)
            | (VirtIOGPUState::InitializingResourceCreate2D, _)
            | (VirtIOGPUState::InitializingResourceAttachBacking, _)
            | (VirtIOGPUState::InitializingSetScanout, _)
            | (VirtIOGPUState::InitializingTransferToHost2D, _)
            | (VirtIOGPUState::InitializingResourceFlush, _)
            | (VirtIOGPUState::Idle, _)
            | (VirtIOGPUState::SettingWriteFrame, _)
            | (VirtIOGPUState::DrawTransferToHost2D(_), _)
            | (VirtIOGPUState::DrawResourceFlush(_), _) => {
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

        // Calculate the overall number of pixels in the draw area:
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .ok_or(ErrorCode::INVAL)?;

        // We don't extend the pending draw area with this rect right now, only
        // doing so for actual calls to `write`. However, we do store the new
        // drawing area as the bounding box and offset coordinates for `write`:
        self.current_draw_area.set((
            // Draw area:
            Rect {
                x,
                y,
                width,
                height,
            },
            // Current draw offset:
            (0, 0),
            // Pixels left to draw:
            pixels,
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
        mut _buffer: SubSliceMut<'static, u8>,
        _continue_write: bool,
    ) -> Result<(), ErrorCode> {
        // // Make sure we're idle:
        // let VirtIOGPUState::Idle = self.state.get() else {
        //     return Err(ErrorCode::BUSY);
        // };

        // // Write the contents of `buffer` to the internal frame buffer, in the
        // // draw area set by `set_write_frame`.
        // //
        // // If `continue_write` is false, we must reset `x_off`, `y_off` and the
        // // `pixels_remaining` value. Otherwise we start at the stored offset.
        // let (draw_rect, (x_off, y_off), pixels_remaining) = if continue_write {
        //     self.current_draw_area.get()
        // } else {
        //     let (draw_rect, _, _) = self.current_draw_area.get();

        //     // This multiplication must not overflow, as it hasn't overflowed
        //     // when we performed it in `set_write_frame`:
        //     (
        //         draw_rect,
        //         (0, 0),
        //         (draw_rect.width as usize)
        //             .checked_mul(draw_rect.height as usize)
        //             .unwrap(),
        //     )
        // };

        // // Make sure the buffer has a length compatible with our pixel mode:
        // if buffer.len() % PIXEL_STRIDE != 0 {
        //     // TODO: this error code is not yet supported in the HIL:
        //     return Err(ErrorCode::INVAL);
        // }
        // let buffer_pixels = buffer.len() / PIXEL_STRIDE;

        // // Check whether this buffer will fit the remaining draw area:
        // if buffer_pixels > pixels_remaining {
        //     return Err(ErrorCode::SIZE);
        // }

        // This following code is wrong, it needs to draw row-by-row instead.
        todo!();

        // // Okay, looks good, we can start drawing! Calculate the start offset
        // // into our framebuffer.
        // let fb_start_byte_offset = (x_off as usize)
        //     .checked_mul(self.width as usize)
        //     .and_then(|o| o.checked_add(y_off as usize))
        //     .and_then(|o| o.checked_mul(PIXEL_STRIDE))
        //     .unwrap();
        // let fb_end_byte_offset = fb_start_byte_offset.checked_add(buffer.len()).unwrap();

        // // The frame buffer must be accessible here. We never "take" it for
        // // longer than a single, synchronous method call:
        // self.frame_buffer
        //     .map(|fb| {
        //         fb[fb_start_byte_offset..fb_end_byte_offset].copy_from_slice(buffer.as_slice())
        //     })
        //     .unwrap();

        // // Update the offset in the draw area, and the number of pixels
        // // remaining:
        // self.current_draw_area.set((
        //     draw_rect,
        //     (
        //         x_off + u32::try_from(buffer_pixels / self.width as usize).unwrap(),
        //         y_off + u32::try_from(buffer_pixels % self.width as usize).unwrap(),
        //     ),
        //     pixels_remaining - buffer_pixels,
        // ));

        // // Extend the pending draw area by the drawn bytes.
        // //
        // // TODO: this could be made more efficient by actually respecting the
        // // offsets and length of the buffer written. For now, we just flush the
        // // whole `draw_rect`:
        // self.pending_draw_area
        //     .set(self.pending_draw_area.get().extend(draw_rect));

        // // Store the client's buffer. We must hold on to it until we issue the
        // // callback:
        // assert!(self.client_write_buffer.replace(buffer).is_none());

        // // Tell the screen to draw, please. This will also transition the GPU
        // // device state:
        // self.draw_frame_buffer(DrawMode::Write);

        // Ok(())
    }

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

impl<'a> InMemoryFrameBufferScreen<'a> for VirtIOGPU<'a, '_> {
    fn write_to_frame_buffer(
        &self,
        f: impl FnOnce(ScreenDims, ScreenPixelFormat, &mut [u8]) -> Result<ScreenRect, ErrorCode>,
    ) -> Result<(), ErrorCode> {
        // Check that we're not busy. We allow multiple calls to this method, as
        // per its documentation.
        let idle = match self.state.get() {
            VirtIOGPUState::Idle => true,
            VirtIOGPUState::DrawTransferToHost2D(DrawMode::WriteToFrameBuffer) => false,
            VirtIOGPUState::DrawResourceFlush(DrawMode::WriteToFrameBuffer) => false,
            _ => return Err(ErrorCode::BUSY),
        };

        // Try to get a hold of the frame buffer. If it's already taken, this is
        // likely because of a reentrant call to this function. Return `BUSY` in
        // that case:
        let Some(frame_buffer) = self.frame_buffer.take() else {
            return Err(ErrorCode::BUSY);
        };

        // Pass it to the closure:
        let closure_res = f(
            ScreenDims {
                x: self.width as usize,
                y: self.height as usize,
            },
            ScreenPixelFormat::ARGB_8888,
            frame_buffer,
        );

        kernel::debug!("{:x?}", &frame_buffer[..256]);

        // Replace the frame_buffer unconditionally:
        self.frame_buffer.replace(frame_buffer);

        match closure_res {
            Err(e) => {
                // The closure returned an error, we do not need to emit a
                // callback.
                Err(e)
            }

            Ok(screen_rect) => {
                // The closure modified the frame buffer, issue a redraw of the
                // changed area. We first check that the to-draw area actually
                // fits:
                let x: u32 = screen_rect.x.try_into().map_err(|_| ErrorCode::SIZE)?;
                let y: u32 = screen_rect.y.try_into().map_err(|_| ErrorCode::SIZE)?;
                let width: u32 = screen_rect.width.try_into().map_err(|_| ErrorCode::SIZE)?;
                let height: u32 = screen_rect.height.try_into().map_err(|_| ErrorCode::SIZE)?;

                if x.checked_add(width).ok_or(ErrorCode::SIZE)? > self.width
                    || y.checked_add(height).ok_or(ErrorCode::SIZE)? > self.height
                {
                    return Err(ErrorCode::SIZE);
                }

                // Extend the to-redraw area:
                self.pending_draw_area
                    .set(self.pending_draw_area.get().extend(Rect {
                        x,
                        y,
                        width,
                        height,
                    }));

                // If we're idle, issue a re-draw. Otherwise, one will
                // automatically be issued after the current draw operation:
                if idle {
                    self.draw_frame_buffer(DrawMode::WriteToFrameBuffer);
                }

                Ok(())
            }
        }
    }
}

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
