// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2025.

//! Split a screen between userspace and the kernel.
//!
//! Userspace and the kernel are each given a rectangular region of the screen.
//!
//! This driver uses a subset of the API from `Screen`. It does not support any
//! screen config settings (brightness, invert) as those operations affect the
//! entire screen.

use core::cell::Cell;
use kernel::deferred_call::{DeferredCall, DeferredCallClient};
use kernel::hil;
use kernel::utilities::cells::OptionalCell;
use kernel::utilities::leasable_buffer::SubSliceMut;
use kernel::ErrorCode;

/// Pending asynchronous screen operation.
enum ScreenSplitOperation {
    /// Operation to set the writing frame.
    WriteSetFrame,
    /// Operation to write a buffer to the screen. `bool` is the continue_write
    /// argument.
    WriteBuffer(SubSliceMut<'static, u8>, bool),
}

/// Rectangular region of a screen.
#[derive(Default, Clone, Copy, PartialEq)]
pub struct Frame {
    /// X coordinate of the upper left corner of the frame.
    x: usize,
    /// Y coordinate of the upper left corner of the frame.
    y: usize,
    /// Width of the frame.
    width: usize,
    /// Height of the frame.
    height: usize,
}

/// An implementation of [`Screen`](kernel::hil::screen::Screen) for a subregion
/// of the actual screen.
pub struct ScreenSplitSection<'a, S: hil::screen::Screen<'a>> {
    /// The shared screen manager that serializes screen operations.
    screen_split: &'a ScreenSplit<'a, S>,
    /// The frame within the entire screen this split section has access to.
    frame: Frame,
    /// The frame inside of the split that is active. Defaults to the entire
    /// frame.
    write_frame: Cell<Frame>,
    /// What operation this section would like to do.[`ScreenSplitSection`] sets
    /// its intended operation here and then asks the [`ScreenSplit`] to
    /// execute it.
    pending: OptionalCell<ScreenSplitOperation>,
    /// Screen client.
    client: OptionalCell<&'a dyn hil::screen::ScreenClient>,
}

impl<'a, S: hil::screen::Screen<'a>> ScreenSplitSection<'a, S> {
    pub fn new(
        screen_split: &'a ScreenSplit<'a, S>,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> Self {
        let frame = Frame {
            x,
            y,
            width,
            height,
        };

        // Default the write frame to the entire frame provided for this split.
        Self {
            screen_split,
            frame,
            write_frame: Cell::new(Frame {
                x: 0,
                y: 0,
                width,
                height,
            }),
            pending: OptionalCell::empty(),
            client: OptionalCell::empty(),
        }
    }
}

impl<'a, S: hil::screen::Screen<'a>> hil::screen::Screen<'a> for ScreenSplitSection<'a, S> {
    fn set_client(&self, client: &'a dyn hil::screen::ScreenClient) {
        self.client.set(client);
    }

    fn get_resolution(&self) -> (usize, usize) {
        (self.frame.width, self.frame.height)
    }

    fn get_pixel_format(&self) -> hil::screen::ScreenPixelFormat {
        self.screen_split.screen.get_pixel_format()
    }

    fn get_rotation(&self) -> hil::screen::ScreenRotation {
        self.screen_split.screen.get_rotation()
    }

    fn set_write_frame(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> Result<(), ErrorCode> {
        if self.pending.is_some() {
            Err(ErrorCode::BUSY)
        } else {
            let frame = Frame {
                x,
                y,
                width,
                height,
            };

            self.write_frame.set(frame);

            // Just mark this operation as intended and then ask the shared
            // split manager to execute it.
            self.pending.set(ScreenSplitOperation::WriteSetFrame);
            self.screen_split.request_operation()
        }
    }

    fn write(
        &self,
        buffer: SubSliceMut<'static, u8>,
        continue_write: bool,
    ) -> Result<(), ErrorCode> {
        if self.pending.is_some() {
            Err(ErrorCode::BUSY)
        } else {
            // Just mark this operation as intended and then ask the shared
            // split manager to execute it.
            self.pending
                .set(ScreenSplitOperation::WriteBuffer(buffer, continue_write));
            self.screen_split.request_operation()
        }
    }

    fn set_brightness(&self, _brightness: u16) -> Result<(), ErrorCode> {
        Err(ErrorCode::NOSUPPORT)
    }

    fn set_power(&self, _enabled: bool) -> Result<(), ErrorCode> {
        Err(ErrorCode::NOSUPPORT)
    }

    fn set_invert(&self, _enabled: bool) -> Result<(), ErrorCode> {
        Err(ErrorCode::NOSUPPORT)
    }
}

impl<'a, S: hil::screen::Screen<'a>> hil::screen::ScreenClient for ScreenSplitSection<'a, S> {
    fn command_complete(&self, r: Result<(), ErrorCode>) {
        self.pending.take();

        self.client.map(|client| {
            client.command_complete(r);
        });
    }

    fn write_complete(&self, data: SubSliceMut<'static, u8>, r: Result<(), ErrorCode>) {
        self.pending.take();

        self.client.map(|client| {
            client.write_complete(data, r);
        });
    }

    fn screen_is_ready(&self) {
        self.client.map(|client| {
            client.screen_is_ready();
        });
    }
}

/// What the screen split mux is currently working on.
enum ScreenSplitState {
    /// Setting the frame is just recording the write frame, so this just needs
    /// to simulate a callback.
    SetFrame,
    /// Do a write to the screen. First step is setting the frame.
    WriteSetFrame(SubSliceMut<'static, u8>, bool),
    /// Do a write to the screen. Second step is actually writing the buffer
    /// contents.
    WriteBuffer,
}

/// Split-screen manager.
///
/// This enables two users (e.g., the kernel and all userspace apps) to share
/// a single physical screen. Each split screen is assigned a fixed region.
pub struct ScreenSplit<'a, S: hil::screen::Screen<'a>> {
    /// Underlying screen driver to use.
    screen: &'a S,

    /// The first split screen user, for the kernel.
    kernel_split: OptionalCell<&'a ScreenSplitSection<'a, S>>,

    /// The second split screen user, for userspace apps.
    userspace_split: OptionalCell<&'a ScreenSplitSection<'a, S>>,

    /// What is using the split screen and what state this mux is in.
    current_user: OptionalCell<(&'a ScreenSplitSection<'a, S>, ScreenSplitState)>,

    /// Simulate interrupt callbacks for setting the frame.
    deferred_call: DeferredCall,
}

impl<'a, S: hil::screen::Screen<'a>> ScreenSplit<'a, S> {
    pub fn new(screen: &'a S) -> Self {
        Self {
            screen,
            current_user: OptionalCell::empty(),
            kernel_split: OptionalCell::empty(),
            userspace_split: OptionalCell::empty(),
            deferred_call: DeferredCall::new(),
        }
    }

    /// Set the first user for the kernel.
    pub fn set_kernel_split(&self, kernel_split: &'a ScreenSplitSection<'a, S>) {
        self.kernel_split.set(kernel_split)
    }

    /// Set the second user for userspace.
    pub fn set_userspace_split(&self, userspace_split: &'a ScreenSplitSection<'a, S>) {
        self.userspace_split.set(userspace_split)
    }

    fn request_operation(&self) -> Result<(), ErrorCode> {
        // Check if we are busy with an existing operation. If so, just return
        // OK and we will handle the operation later.
        if self.current_user.is_some() {
            return Ok(());
        }

        // Check if the kernel has work to do.
        let kernel_ret = if let Some(kernel_user) = self.kernel_split.get() {
            if let Some(operation) = kernel_user.pending.take() {
                Some(self.call_screen(kernel_user, operation))
            } else {
                None
            }
        } else {
            None
        };

        // If kernel did work, then we return. Otherwise, we check the userspace
        // split.
        if let Some(kernel_ret) = kernel_ret {
            kernel_ret
        } else {
            if let Some(userspace_user) = self.userspace_split.get() {
                if let Some(operation) = userspace_user.pending.take() {
                    self.call_screen(userspace_user, operation)
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            }
        }
    }

    fn call_screen(
        &self,
        split: &'a ScreenSplitSection<'a, S>,
        operation: ScreenSplitOperation,
    ) -> Result<(), ErrorCode> {
        match operation {
            ScreenSplitOperation::WriteSetFrame => {
                // Just need to set a deferred call since we only write the
                // frame if we are going to write the screen.
                self.current_user.set((split, ScreenSplitState::SetFrame));
                self.deferred_call.set();
                Ok(())
            }
            ScreenSplitOperation::WriteBuffer(subslice, continue_write) => {
                // First we need to set the frame.
                let absolute_frame =
                    self.calculate_absolute_frame(split.frame, split.write_frame.get());

                self.screen
                    .set_write_frame(
                        absolute_frame.x,
                        absolute_frame.y,
                        absolute_frame.width,
                        absolute_frame.height,
                    )
                    .inspect(|_| {
                        self.current_user.set((
                            split,
                            ScreenSplitState::WriteSetFrame(subslice, continue_write),
                        ))
                    })
            }
        }
    }

    /// Calculate the frame within the entire screen that the split is currently
    /// trying to use. This is the `active_frame` within the split's allocated
    /// `split_frame`.
    fn calculate_absolute_frame(&self, split_frame: Frame, active_frame: Frame) -> Frame {
        // x and y are sums
        let mut absolute_x = split_frame.x + active_frame.x;
        let mut absolute_y = split_frame.y + active_frame.y;
        // width and height are simply the app_frame width and height.
        let mut absolute_w = active_frame.width;
        let mut absolute_h = active_frame.height;

        // Make sure that the calculate frame is within the allocated region.
        absolute_x = core::cmp::min(split_frame.x + split_frame.width, absolute_x);
        absolute_y = core::cmp::min(split_frame.y + split_frame.height, absolute_y);
        absolute_w = core::cmp::min(split_frame.x + split_frame.width - absolute_x, absolute_w);
        absolute_h = core::cmp::min(split_frame.y + split_frame.height - absolute_y, absolute_h);

        Frame {
            x: absolute_x,
            y: absolute_y,
            width: absolute_w,
            height: absolute_h,
        }
    }
}

impl<'a, S: hil::screen::Screen<'a>> hil::screen::ScreenClient for ScreenSplit<'a, S> {
    fn command_complete(&self, _r: Result<(), ErrorCode>) {
        if let Some((current_user, state)) = self.current_user.take() {
            match state {
                ScreenSplitState::WriteSetFrame(subslice, continue_write) => {
                    let _ = self.screen.write(subslice, continue_write).inspect(|_| {
                        self.current_user
                            .set((current_user, ScreenSplitState::WriteBuffer))
                    });
                }
                _ => {
                    // No other state will trigger this callback.
                }
            }
        }
    }

    fn write_complete(&self, data: SubSliceMut<'static, u8>, r: Result<(), ErrorCode>) {
        if let Some((current_user, _state)) = self.current_user.take() {
            current_user.write_complete(data, r);
        }

        let _ = self.request_operation();
    }

    fn screen_is_ready(&self) {
        if let Some(kernel_user) = self.kernel_split.get() {
            kernel_user.screen_is_ready();
        }

        if let Some(userspace_user) = self.userspace_split.get() {
            userspace_user.screen_is_ready();
        }
    }
}

impl<'a, S: hil::screen::Screen<'a>> DeferredCallClient for ScreenSplit<'a, S> {
    fn handle_deferred_call(&self) {
        // All we have to do is trigger the set frame callback.
        if let Some((current_user, _state)) = self.current_user.take() {
            hil::screen::ScreenClient::command_complete(current_user, Ok(()));
        }
    }

    fn register(&'static self) {
        self.deferred_call.register(self);
    }
}
