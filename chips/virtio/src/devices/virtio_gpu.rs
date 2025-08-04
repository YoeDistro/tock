// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2022.

use core::cell::Cell;

use kernel::deferred_call::{DeferredCall, DeferredCallClient};
use kernel::utilities::cells::{OptionalCell, TakeCell};
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
    mut src: impl Iterator<Item = T>,
) {
    while let Some(e) = src.next() {
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

pub trait GPUClient {}

#[derive(Copy, Clone, Debug)]
pub enum VirtIOGPUState {
    Uninitialized,
    InitializingResourceCreate2D,
    InitializingResourceAttachBacking,
    InitializingSetScanout,
    InitializingTransferToHost2D,
    InitializingResourceFlush,
    Idle,
}

pub struct VirtIOGPU<'a, 'b> {
    control_queue: &'a SplitVirtqueue<'a, 'b, 2>,
    deferred_call: DeferredCall,
    client: OptionalCell<&'a dyn GPUClient>,
    state: Cell<VirtIOGPUState>,
    frame_buffer: TakeCell<'a, [u8]>,
    width: u32,
    height: u32,
    req_resp_buffers: OptionalCell<(&'b mut [u8; MAX_REQ_SIZE], &'b mut [u8; MAX_RESP_SIZE])>,
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
            control_queue,
            deferred_call: DeferredCall::new(),
            client: OptionalCell::empty(),
            state: Cell::new(VirtIOGPUState::Uninitialized),
            frame_buffer: TakeCell::new(frame_buffer),
            width,
            height,
            req_resp_buffers: OptionalCell::new((req_buffer, resp_buffer)),
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
            format: VideoFormat::R8G8B8A8Unorm,
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
        resp: ResourceCreate2DResp,
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
        resp: ResourceAttachBackingResp,
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
        resp: SetScanoutResp,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        // Initialization done!

        // As one final step, we draw the contents of the framebuffer that was
        // passed to us initially:
        let cmd_transfer_to_host_2d_req = TransferToHost2DReq {
            ctrl_header: CtrlHeader {
                ctrl_type: TransferToHost2DReq::CTRL_TYPE,
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

        self.state.set(VirtIOGPUState::InitializingTransferToHost2D);
    }

    fn initialize_transfer_to_host_2d_resp(
        &self,
        resp: TransferToHost2DResp,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        // As one final step, we draw the contents of the framebuffer that was
        // passed to us initially:
        let cmd_resource_flush_req = ResourceFlushReq {
            ctrl_header: CtrlHeader {
                ctrl_type: ResourceFlushReq::CTRL_TYPE,
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

        self.state.set(VirtIOGPUState::InitializingResourceFlush);
    }

    fn initialize_resource_flush_resp(
        &self,
        resp: ResourceFlushResp,
        req_buffer: &'b mut [u8; MAX_REQ_SIZE],
        resp_buffer: &'b mut [u8; MAX_RESP_SIZE],
    ) {
        self.req_resp_buffers.replace((req_buffer, resp_buffer));
        self.state.set(VirtIOGPUState::Idle);
    }

    fn buffer_chain_callback(
        &self,
        buffer_chain: &mut [Option<VirtqueueBuffer<'b>>],
        bytes_used: usize,
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
            ) => {
                // Parse the remainder of the response:
                let resp = TransferToHost2DResp::from_byte_iter_post_ctrl_header(
                    ctrl_header,
                    &mut resp_iter,
                )
                .expect("Failed to parse VirtIO GPU TransferToHost2DResp");

                // Continue the initialization routine:
                self.initialize_transfer_to_host_2d_resp(resp, req_array, resp_array);
            }

            (VirtIOGPUState::InitializingResourceFlush, ResourceFlushResp::EXPECTED_CTRL_TYPE) => {
                // Parse the remainder of the response:
                let resp =
                    ResourceFlushResp::from_byte_iter_post_ctrl_header(ctrl_header, &mut resp_iter)
                        .expect("Failed to parse VirtIO GPU ResourceFlushResp");

                // Continue the initialization routine:
                self.initialize_resource_flush_resp(resp, req_array, resp_array);
            }

            (VirtIOGPUState::Uninitialized, _)
            | (VirtIOGPUState::InitializingResourceCreate2D, _)
            | (VirtIOGPUState::InitializingResourceAttachBacking, _)
            | (VirtIOGPUState::InitializingSetScanout, _)
            | (VirtIOGPUState::InitializingTransferToHost2D, _)
            | (VirtIOGPUState::InitializingResourceFlush, _)
            | (VirtIOGPUState::Idle, _) => {
                panic!("Received unexpected VirtIO GPU device response. Device state: {:?}, ctrl hader: {:?}", self.state.get(), ctrl_header);
            }
        }
    }
}

// impl<'a> GPU<'a> for VirtIOGPU<'a, '_> {
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
        todo!()
        // // Try to extract a descriptor chain
        // if let Some((mut chain, bytes_used)) = self.virtqueue.pop_used_buffer_chain() {
        //     self.buffer_chain_callback(&mut chain, bytes_used)
        // } else {
        //     // If we don't get a buffer, this must be a race condition
        //     // which should not occur
        //     //
        //     // Prior to setting a deferred call, all virtqueue
        //     // interrupts must be disabled so that no used buffer is
        //     // removed before the deferred call callback
        //     panic!("VirtIO GPU: deferred call callback with empty queue");
        // }
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
