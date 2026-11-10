/*
 * QEMU USB HID devices
 *
 * Copyright (c) 2005 Fabrice Bellard
 * Copyright (c) 2007 OpenMoko, Inc.  (andrew@openedhand.com)
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */
/*
 * QEMU USB emulation
 *
 * Copyright (c) 2005 Fabrice Bellard
 *
 * 2008 Generic packet handler rewrite by Max Krasnyansky
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */
// Adapted from QEMU hw/usb/dev-hid.c and hw/input/hid.c (HID report layout), and hw/usb/core.c (control stages).
// Source commit: bb779689554ca5e69e0ac0bb56f90c41c64a80bb (MIT).

use super::super::TICKS_PER_SECOND;
use alloc::collections::VecDeque;

pub(super) const IN: u8 = 0x69;
pub(super) const OUT: u8 = 0xe1;
pub(super) const SETUP: u8 = 0x2d;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reply {
    Nak,
    Stall,
    Babble,
    NoDevice,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Report {
    x: u16,
    y: u16,
    buttons: u8,
    wheel: i8,
}
impl Report {
    fn bytes(self) -> [u8; 6] {
        [
            self.buttons,
            self.x as u8,
            (self.x >> 8) as u8,
            self.y as u8,
            (self.y >> 8) as u8,
            self.wheel as u8,
        ]
    }
}
#[derive(Clone, Copy, Debug, Default)]
enum Stage {
    #[default]
    Idle,
    DataIn,
    StatusIn,
    Stall,
}
#[derive(Clone, Copy, Debug, Default)]
enum Action {
    #[default]
    None,
    Address(u8),
    Config(u8),
    Idle(u8),
    Halt(bool),
    Wake(bool),
    Interface,
}

#[derive(Debug)]
pub(super) struct Tablet {
    pub address: u8,
    config: u8,
    pub remote_wakeup: bool,
    halted: bool,
    idle: u8,
    last_report: u64,
    current: Report,
    queue: VecDeque<Report>,
    stage: Stage,
    action: Action,
    response: [u8; 128],
    length: usize,
    position: usize,
}
impl Default for Tablet {
    fn default() -> Self {
        Self {
            address: 0,
            config: 0,
            remote_wakeup: false,
            halted: false,
            idle: 0,
            last_report: 0,
            current: Report::default(),
            queue: VecDeque::new(),
            stage: Stage::Idle,
            action: Action::None,
            response: [0; 128],
            length: 0,
            position: 0,
        }
    }
}
impl Tablet {
    pub fn reset(&mut self) {
        let mut current = self.queue.back().copied().unwrap_or(self.current);
        current.wheel = 0;
        *self = Self::default();
        self.current = current;
    }
    pub fn input(&mut self, x: u16, y: u16, buttons: u8, wheel: i8) -> bool {
        let report = Report {
            x,
            y,
            buttons,
            wheel,
        };
        let last = self.queue.back().copied().unwrap_or(self.current);
        if report == last && wheel == 0 {
            return false;
        }
        // Motion can coalesce, but preserve queued press/release transitions.
        if let Some(last) = self.queue.back_mut()
            && last.buttons == buttons
            && last.wheel == 0
            && wheel == 0
        {
            *last = report;
            return true;
        }
        if self.queue.len() == 64 {
            self.queue.pop_front();
        }
        self.queue.push_back(report);
        true
    }
    pub fn ready(&self, now: u64) -> bool {
        self.config != 0
            && (!self.queue.is_empty()
                || (self.idle != 0
                    && now.saturating_sub(self.last_report)
                        >= (u64::from(self.idle) * 4 * TICKS_PER_SECOND).div_ceil(1000)))
    }
    fn report(&mut self, now: u64) -> [u8; 6] {
        if let Some(next) = self.queue.pop_front() {
            self.current = next;
        }
        let result = self.current.bytes();
        self.current.wheel = 0;
        self.last_report = now;
        result
    }
    fn answer(&mut self, data: &[u8]) {
        self.response[..data.len()].copy_from_slice(data);
        self.length = data.len();
    }
    fn string(&mut self, text: &str) {
        self.length = 2;
        self.response[1] = 3;
        for c in text.encode_utf16() {
            self.response[self.length..self.length + 2].copy_from_slice(&c.to_le_bytes());
            self.length += 2;
        }
        self.response[0] = self.length as u8;
    }
    fn setup(&mut self, data: &[u8], now: u64) -> Result<usize, Reply> {
        self.stage = Stage::Stall;
        self.action = Action::None;
        self.position = 0;
        self.length = 0;
        if data.len() != 8 {
            return Err(Reply::Stall);
        }
        let request = u16::from_be_bytes([data[0], data[1]]);
        let value = u16::from_le_bytes([data[2], data[3]]);
        let index = u16::from_le_bytes([data[4], data[5]]);
        let length = usize::from(u16::from_le_bytes([data[6], data[7]]));
        match request {
            0x8006 => match value >> 8 {
                1 if value & 255 == 0 && index == 0 => self.answer(&DEVICE),
                2 if value & 255 == 0 && index == 0 => self.answer(&CONFIG),
                3 => match value & 255 {
                    0 => self.answer(&[4, 3, 9, 4]),
                    1 if index == 0x409 => self.string("r86"),
                    2 if index == 0x409 => self.string("r86 USB Tablet"),
                    3 if index == 0x409 => self.string("r86-tablet-1"),
                    _ => return Err(Reply::Stall),
                },
                _ => return Err(Reply::Stall),
            },
            0x8106 if index == 0 => match value {
                0x2100 => self.answer(&CONFIG[18..27]),
                0x2200 => self.answer(&REPORT_DESCRIPTOR),
                _ => return Err(Reply::Stall),
            },
            0x8000 if value == 0 && index == 0 => {
                self.answer(&[u8::from(self.remote_wakeup) << 1, 0])
            }
            0x8100 if value == 0 && index == 0 && self.config != 0 => self.answer(&[0, 0]),
            0x8200 if value == 0 && matches!(index, 0 | 0x80 | 0x81) => {
                self.answer(&[u8::from(index == 0x81 && self.halted), 0])
            }
            0x0005 if value < 128 && index == 0 && length == 0 => {
                self.action = Action::Address(value as u8)
            }
            0x8008 if value == 0 && index == 0 => self.answer(&[self.config]),
            0x0009 if value <= 1 && index == 0 && length == 0 => {
                self.action = Action::Config(value as u8)
            }
            0x810a if value == 0 && index == 0 && self.config != 0 => self.answer(&[0]),
            0x010b if value == 0 && index == 0 && length == 0 && self.config != 0 => {
                self.action = Action::Interface
            }
            0x0001 | 0x0003 if value == 1 && index == 0 && length == 0 => {
                self.action = Action::Wake(request == 3)
            }
            0x0201 | 0x0203 if value == 0 && index == 0x81 && length == 0 && self.config != 0 => {
                self.action = Action::Halt(request == 0x0203)
            }
            0xa101 if value == 0x0100 && index == 0 => {
                let report = self.report(now);
                self.answer(&report);
            }
            0xa102 if value == 0 && index == 0 => self.answer(&[self.idle]),
            0x210a if value & 255 == 0 && index == 0 && length == 0 => {
                self.action = Action::Idle((value >> 8) as u8)
            }
            // This absolute tablet is not a boot-interface subclass. Boot
            // GET/SET_PROTOCOL and unsupported reports therefore stall.
            _ => return Err(Reply::Stall),
        }
        if data[0] & 0x80 != 0 {
            self.length = self.length.min(length);
            self.stage = Stage::DataIn;
        } else {
            self.stage = Stage::StatusIn;
        }
        Ok(8)
    }
    fn commit(&mut self, now: u64) {
        match self.action {
            Action::Address(address) => self.address = address,
            Action::Config(config) => {
                self.config = config;
                self.halted = false;
                if config != 0 && self.queue.is_empty() {
                    self.queue.push_back(self.current);
                }
            }
            Action::Idle(idle) => {
                self.idle = idle;
                self.last_report = now;
            }
            Action::Halt(halted) => self.halted = halted,
            Action::Wake(wake) => self.remote_wakeup = wake,
            Action::Interface => self.halted = false,
            Action::None => {}
        }
        self.action = Action::None;
        self.stage = Stage::Idle;
    }
    pub fn packet(
        &mut self,
        pid: u8,
        endpoint: u8,
        data: &mut [u8],
        now: u64,
    ) -> Result<usize, Reply> {
        if endpoint == 0 {
            match (pid, self.stage) {
                (SETUP, _) => self.setup(data, now),
                (IN, Stage::DataIn) => {
                    let len = (self.length - self.position).min(8);
                    if data.len() < len {
                        return Err(Reply::Babble);
                    }
                    data[..len].copy_from_slice(&self.response[self.position..self.position + len]);
                    self.position += len;
                    Ok(len)
                }
                (OUT, Stage::DataIn) if data.is_empty() => {
                    self.stage = Stage::Idle;
                    Ok(0)
                }
                (IN, Stage::StatusIn) => {
                    self.commit(now);
                    Ok(0)
                }
                _ => Err(Reply::Stall),
            }
        } else if endpoint == 1 && pid == IN && self.config != 0 && !self.halted {
            if !self.ready(now) {
                return Err(Reply::Nak);
            }
            if data.len() < 6 {
                return Err(Reply::Babble);
            }
            data[..6].copy_from_slice(&self.report(now));
            Ok(6)
        } else {
            Err(Reply::Stall)
        }
    }
}

// QEMU-compatible virtual device identity; no physical USB VID is allocated to r86.
const DEVICE: [u8; 18] = [18, 1, 0x10, 1, 0, 0, 0, 8, 0x27, 6, 1, 0, 0, 1, 1, 2, 3, 1];
const CONFIG: [u8; 34] = [
    9, 2, 34, 0, 1, 1, 0, 0xa0, 50, 9, 4, 0, 0, 1, 3, 0, 0, 0, 9, 0x21, 0x11, 1, 0, 1, 0x22, 74, 0,
    7, 5, 0x81, 3, 8, 0, 10,
];
const REPORT_DESCRIPTOR: [u8; 74] = [
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f,
    0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00,
    0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0, 0xc0,
];
