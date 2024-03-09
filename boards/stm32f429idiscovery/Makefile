# Licensed under the Apache License, Version 2.0 or the MIT License.
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Copyright Tock Contributors 2022.

# Makefile for building the tock kernel for the STM32F429i Discovery board
#
TARGET=thumbv7em-none-eabi
PLATFORM=stm32f429idiscovery

include ../Makefile.common

OPENOCD=openocd
OPENOCD_OPTIONS=-f openocd.cfg

# Default target for installing the kernel.
.PHONY: install
install: program

.PHONY: flash-debug
flash-debug: $(TOCK_ROOT_DIRECTORY)target/$(TARGET)/debug/$(PLATFORM).elf
	$(OPENOCD) $(OPENOCD_OPTIONS) -c "init; reset halt; flash write_image erase $<; verify_image $<; reset; shutdown"

.PHONY: flash
flash: $(TOCK_ROOT_DIRECTORY)target/$(TARGET)/release/$(PLATFORM).elf
	$(OPENOCD) $(OPENOCD_OPTIONS) -c "init; reset halt; flash write_image erase $<; verify_image $<; reset; shutdown"

.PHONY: program
program: $(TOCK_ROOT_DIRECTORY)target/$(TARGET)/release/$(PLATFORM).bin
	tockloader flash --address 0x08000000 $<
