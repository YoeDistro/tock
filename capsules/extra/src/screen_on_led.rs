// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2025.

//! Display virtual LEDs on a screen.

use core::cell::Cell;
use kernel::hil;
use kernel::utilities::cells::MapCell;
use kernel::utilities::leasable_buffer::SubSliceMut;
use kernel::ErrorCode;

pub struct ScreenOnLed<
    'a,
    S: hil::screen::Screen<'a>,
    const NUM_LEDS: usize,
    const SCREEN_WIDTH: usize,
    const SCREEN_HEIGHT: usize,
> {
    /// Underlying screen driver to use.
    screen: &'a S,

    buffer: MapCell<&'static mut [u8]>,

    initialized: Cell<bool>,
    // /// The first split screen user, for the kernel.
    // kernel_split: OptionalCell<&'a ScreenSplitSection<'a, S>>,

    // /// The second split screen user, for userspace apps.
    // userspace_split: OptionalCell<&'a ScreenSplitSection<'a, S>>,

    // /// Whether userspace or the kernel is currently executing a screen command.
    // current_user: OptionalCell<ScreenSplitUser>,
}

impl<
        'a,
        S: hil::screen::Screen<'a>,
        const NUM_LEDS: usize,
        const SCREEN_WIDTH: usize,
        const SCREEN_HEIGHT: usize,
    > ScreenOnLed<'a, S, NUM_LEDS, SCREEN_WIDTH, SCREEN_HEIGHT>
{
    pub const fn new(screen: &'a S, buffer: &'static mut [u8]) -> Self {
        Self {
            screen,
            buffer: MapCell::new(buffer),
            initialized: Cell::new(false),
        }
    }

    pub fn initialize_leds(&self) {
        self.buffer.take().map(|buffer| {
            self.render(buffer);
            let data = SubSliceMut::new(buffer);
            let _ = self.screen.write(data, false);
        });
    }

    fn render(&self, buffer: &mut [u8]) {
        for i in 0..NUM_LEDS {
            self.render_led(buffer, i);
        }
    }

    fn render_led(&self, buffer: &mut [u8], led_index: usize) {
        // Draw two squares, one on, then one inside that is off.

        let led_dimension: usize = self.get_size().1;
        let x_offset: usize =
            2 + self.get_led_width(led_dimension) + 1 + ((led_dimension + 1) * led_index);

        // Write the outside box fully on.
        self.write_square(buffer.as_mut(), x_offset, 1, led_dimension, 1);
        // Clear the inside to make just the border.
        self.write_square(buffer.as_mut(), x_offset + 1, 2, led_dimension - 2, 0);
    }

    fn write_square(&self, buffer: &mut [u8], x: usize, y: usize, dimension: usize, val: usize) {
        kernel::debug!(
            "write square x{} y{} dimension{} val{}",
            x,
            y,
            dimension,
            val
        );

        for i in 0..dimension {
            for j in 0..dimension {
                let pixel_x = i + x;
                let pixel_y = j + y;
                let byte = ((pixel_y / 8) * SCREEN_WIDTH) + pixel_x;
                let bit = pixel_y % 8;
                if val & 0x1 == 0x1 {
                    buffer[byte] |= 1 << bit;
                } else {
                    buffer[byte] &= !(1 << bit);
                }
            }
        }
    }

    // const fn get_led_dimension(&self) -> usize {
    //     SCREEN_HEIGHT - 2
    // }

    pub const fn get_size(&self) -> (usize, usize) {
        let mut width = SCREEN_WIDTH + 1;
        let mut led_dimension = SCREEN_HEIGHT - 1;

        while width > SCREEN_WIDTH {
            // Shrink LEDs by 1 pixel.
            led_dimension -= 1;

            let leds_width: usize = (led_dimension * NUM_LEDS) + (NUM_LEDS - 1);
            width = self.get_led_width(led_dimension) + 2 + leds_width;
        }

        // let led_dimension: usize = self.get_led_dimension();
        // let leds_width: usize = (led_dimension * NUM_LEDS) + (NUM_LEDS - 1);

        // let total_width = self.get_led_width() + 2 + leds_width;

        (width, led_dimension)
    }

    pub const fn get_led_width(&self, _height: usize) -> usize {
        40
    }
}

impl<
        'a,
        S: hil::screen::Screen<'a>,
        const NUM_LEDS: usize,
        const SCREEN_WIDTH: usize,
        const SCREEN_HEIGHT: usize,
    > hil::screen::ScreenClient for ScreenOnLed<'a, S, NUM_LEDS, SCREEN_WIDTH, SCREEN_HEIGHT>
{
    fn command_complete(&self, _r: Result<(), ErrorCode>) {
        // if r.is_err() {
        //     self.current_process.take().map(|process_id| {
        //         self.schedule_callback(process_id, kernel::errorcode::into_statuscode(r), 0, 0);
        //     });
        // }

        // self.run_next_command();
    }

    fn write_complete(&self, _data: SubSliceMut<'static, u8>, _r: Result<(), ErrorCode>) {
        // self.buffer.replace(data.take());

        // // Notify that the write is finished.
        // self.current_process.take().map(|process_id| {
        //     self.schedule_callback(process_id, kernel::errorcode::into_statuscode(r), 0, 0);
        // });

        // self.run_next_command();
    }

    fn screen_is_ready(&self) {
        if !self.initialized.get() {
            self.initialized.set(true);
            self.initialize_leds();
        }
    }
}
