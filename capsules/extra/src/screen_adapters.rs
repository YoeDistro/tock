use core::cell::Cell;

use kernel::deferred_call::{DeferredCall, DeferredCallClient};
use kernel::hil::screen::{
    Dims, InMemoryFrameBufferScreen, Rect, Screen, ScreenClient, ScreenPixelFormat, ScreenRotation,
};
use kernel::utilities::cells::OptionalCell;
use kernel::utilities::leasable_buffer::SubSliceMut;
use kernel::ErrorCode;

pub struct ScreenARGB8888ToMono8BitPage<'a, S: InMemoryFrameBufferScreen<'a>> {
    screen: &'a S,
    deferred_call: DeferredCall,
    write_frame: Cell<(Rect, Dims, usize)>,
    client_buffer: OptionalCell<SubSliceMut<'static, u8>>,
    client: OptionalCell<&'a dyn ScreenClient>,
    _phantom_lifetime: core::marker::PhantomData<&'a ()>,
}

impl<'a, S: InMemoryFrameBufferScreen<'a>> ScreenARGB8888ToMono8BitPage<'a, S> {
    pub fn new(screen: &'a S) -> Self {
        ScreenARGB8888ToMono8BitPage {
            screen,
            deferred_call: DeferredCall::new(),
            write_frame: Cell::new((Rect::EMPTY, Dims { x: 0, y: 0 }, 0)),
            client_buffer: OptionalCell::empty(),
            client: OptionalCell::empty(),
            _phantom_lifetime: core::marker::PhantomData,
        }
    }
}

impl<'a, S: InMemoryFrameBufferScreen<'a>> Screen<'a> for ScreenARGB8888ToMono8BitPage<'a, S> {
    fn set_client(&self, client: &'a dyn ScreenClient) {
        self.client.replace(client);
    }

    fn get_resolution(&self) -> (usize, usize) {
        self.screen.get_resolution()
    }

    fn get_pixel_format(&self) -> ScreenPixelFormat {
        ScreenPixelFormat::Mono_8BitPage
    }

    fn get_rotation(&self) -> ScreenRotation {
        self.screen.get_rotation()
    }

    fn set_write_frame(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> Result<(), ErrorCode> {
        let screen_res = self.get_resolution();
        if x.checked_add(width).ok_or(ErrorCode::SIZE)? > screen_res.0
            || y.checked_add(height).ok_or(ErrorCode::SIZE)? > screen_res.1
        {
            return Err(ErrorCode::SIZE);
        }

        // We can only write 8 rows at a time:
        if y % 8 != 0 || height % 8 != 0 {
            return Err(ErrorCode::INVAL);
        }

        self.write_frame.set((
            Rect {
                x,
                y,
                width,
                height,
            },
            Dims { x, y },
            width.checked_mul(height).ok_or(ErrorCode::SIZE)?,
        ));

        self.deferred_call.set();

        Ok(())
    }

    fn write(
        &self,
        mut buffer: SubSliceMut<'static, u8>,
        continue_write: bool,
    ) -> Result<(), ErrorCode> {
        fn into_bits(byte: u8) -> [bool; 8] {
            let mut dst = [false; 8];
            for (i, d) in dst.iter_mut().enumerate() {
                *d = (byte & (1 << i)) != 0;
            }
            dst
        }

        let write_res = self
            .screen
            .write_to_frame_buffer(|screen_dims, _pixel_mode, fb| {
                // Every byte in the buffer represents 8 pixels. Ensure that this
                // fits into the remaining bytes within our `write_frame`:
                let (write_frame, mut offset, mut pixels_remaining) = self.write_frame.get();

                // Reset the offset and bytes remaining if we're starting anew:
                if !continue_write {
                    offset = Dims {
                        x: write_frame.x,
                        y: write_frame.y,
                    };
                    // This multiplication must not overflow, as we've already
                    // perfomed it successfully in `set_write_frame`:
                    pixels_remaining = write_frame.width.checked_mul(write_frame.height).unwrap();
                }

                // Check if this write will overflow the write frame:
                if buffer.len().checked_mul(8).ok_or(ErrorCode::SIZE)? > pixels_remaining {
                    return Err(ErrorCode::SIZE);
                }

                // For now, we write one input byte, so 8 pixels at a time.
                for mono_8bit_page in buffer.as_slice().iter() {
                    // When reaching the end of an "8 set" of rows, do a Carriage
                    // Return + Line Feed style operation:
                    if offset.x >= write_frame.x.checked_add(write_frame.width).unwrap() {
                        offset.x = write_frame.x;
                        offset.y += 8;
                    }

                    // This loop must never be able to run past the write_frame:
                    assert!(pixels_remaining > 0);
                    assert!(offset.y < write_frame.y.checked_add(write_frame.height).unwrap());

                    // Now, write an "8 set" of rows:
                    for (y_set_off, v) in into_bits(*mono_8bit_page).into_iter().enumerate() {
                        let pixel_byte_offset = offset
                            .y
                            .checked_add(y_set_off)
                            .and_then(|rows| screen_dims.x.checked_mul(rows))
                            .and_then(|row_offset| row_offset.checked_add(offset.x))
                            .and_then(|pixel_offset| pixel_offset.checked_mul(4))
                            .unwrap();
                        fb[pixel_byte_offset..(pixel_byte_offset.checked_add(4).unwrap())]
                            .copy_from_slice(&[
                                0x00,
                                0xFF * (v as u8),
                                0xFF * (v as u8),
                                0xFF * (v as u8),
                            ]);
                    }

                    // Add to the offset:
                    offset.x += 1;

                    // Indicate we've written 8 pixels:
                    pixels_remaining -= 8;
                }

                // Store the new offset and pixels remaining:
                self.write_frame
                    .set((write_frame, offset, pixels_remaining));

                // Indicate that we've written `write_frame` pixels:
                Ok(write_frame)
            });

        // TODO: error handling?
        write_res.unwrap();

        self.client_buffer.replace(buffer);

        Ok(())
    }

    fn set_brightness(&self, brightness: u16) -> Result<(), ErrorCode> {
        self.screen.set_brightness(brightness)
    }

    fn set_power(&self, enabled: bool) -> Result<(), ErrorCode> {
        self.screen.set_power(enabled)
    }

    fn set_invert(&self, enabled: bool) -> Result<(), ErrorCode> {
        self.screen.set_invert(enabled)
    }
}

impl<'a, S: InMemoryFrameBufferScreen<'a>> ScreenClient for ScreenARGB8888ToMono8BitPage<'a, S> {
    fn command_complete(&self, _result: Result<(), ErrorCode>) {
        self.client
            .map(|c| c.write_complete(self.client_buffer.take().unwrap(), Ok(())));
    }

    fn write_complete(&self, _buffer: SubSliceMut<'static, u8>, _result: Result<(), ErrorCode>) {
        unreachable!();
    }

    fn screen_is_ready(&self) {
        self.client.map(|c| c.screen_is_ready());
    }
}

impl<'a, S: InMemoryFrameBufferScreen<'a>> DeferredCallClient
    for ScreenARGB8888ToMono8BitPage<'a, S>
{
    fn register(&'static self) {
        self.deferred_call.register(self);
    }

    fn handle_deferred_call(&self) {
        self.client.map(|c| c.command_complete(Ok(())));
    }
}
