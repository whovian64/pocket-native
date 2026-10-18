//! cam:u, the cameras. there are none, so they see black: each frame a port
//! that is capturing fills what it was asked to receive and signals the end
//! of it, and its vsync, with the transfer sizes worked out as Citra does.

use crate::kernel::ipc::{CommandBuffer, Descriptor, Header};
use crate::kernel::object::{KObject, ObjectId};
use crate::kernel::sync::ResetType;
use crate::System;

/// the smallest and largest piece a port moves a picture in, in bytes.
const MIN_TRANSFER: u32 = 256;
const MAX_TRANSFER: u32 = 5120;

#[derive(Default, Clone, Copy)]
struct Port {
    capturing: bool,
    /// where the next picture goes and its size, until it came.
    receiving: Option<(u32, u32)>,
    transfer_bytes: u32,
    trimming: bool,
    /// the receive end, buffer error and vsync events.
    events: Option<[ObjectId; 3]>,
}

#[derive(Default)]
pub struct CamState {
    ports: [Port; 2],
    /// pictures come as RGB565 rather than YUV422.
    rgb: bool,
}

pub fn handle(system: &mut System, buffer: &CommandBuffer, header: Header) -> bool {
    let command = header.command_id();
    let word = |system: &mut System, index: u32| buffer.get(&mut system.memory, index);
    let reply = |system: &mut System, values: &[u32]| buffer.reply(&mut system.memory, command, values);
    // a command's port, one bit each, the first word
    let ports = |system: &mut System| {
        let mask = word(system, 1);
        (0..2).filter(move |port| mask >> port & 1 != 0).collect::<Vec<usize>>()
    };
    let port = |system: &mut System| (word(system, 1).trailing_zeros() as usize).min(1);
    match command {
        // StartCapture, StopCapture
        0x0001 | 0x0002 => {
            for port in ports(system) {
                system.services.cam.ports[port].capturing = command == 0x0001;
            }
            reply(system, &[]);
        }
        // IsBusy
        0x0003 => {
            let busy = ports(system).into_iter().any(|port| system.services.cam.ports[port].capturing);
            reply(system, &[busy as u32]);
        }
        // GetVsyncInterruptEvent, GetBufferErrorInterruptEvent
        0x0005 | 0x0006 => {
            let port = port(system);
            let object = events(system, port)[if command == 0x0005 { 2 } else { 1 }];
            reply_handle(system, buffer, command, object);
        }
        // SetReceiving(destination, port, size, transfer unit, process)
        0x0007 => {
            let (destination, port, size) = (word(system, 1), (word(system, 2).trailing_zeros() as usize).min(1), word(system, 3));
            system.services.cam.ports[port].receiving = Some((destination, size));
            let object = events(system, port)[0];
            system.kernel.clear_event(object);
            reply_handle(system, buffer, command, object);
        }
        // IsFinishedReceiving
        0x0008 => {
            let port = port(system);
            let finished = system.services.cam.ports[port].receiving.is_none();
            reply(system, &[finished as u32]);
        }
        // SetTransferLines(port, lines, width, height), SetTransferBytes(port,
        // bytes, width, height)
        0x0009 | 0x000B => {
            let bytes = if command == 0x0009 { word(system, 2) * word(system, 3) * 2 } else { word(system, 2) };
            for port in ports(system) {
                system.services.cam.ports[port].transfer_bytes = bytes;
            }
            reply(system, &[]);
        }
        // GetMaxLines(width, height)
        0x000A => {
            let (width, height) = (word(system, 1).max(1), word(system, 2));
            let mut lines = (MAX_TRANSFER / width).min(height);
            while lines > 0 && (height % lines != 0 || lines * width * 2 % MIN_TRANSFER != 0) {
                lines -= 1;
            }
            reply(system, &[lines]);
        }
        // GetTransferBytes
        0x000C => {
            let port = port(system);
            let bytes = system.services.cam.ports[port].transfer_bytes;
            reply(system, &[bytes]);
        }
        // GetMaxBytes(width, height)
        0x000D => {
            let size = word(system, 1) * word(system, 2) * 2;
            let mut bytes = MAX_TRANSFER;
            while size % MIN_TRANSFER == 0 && size % bytes != 0 {
                bytes -= MIN_TRANSFER;
            }
            reply(system, &[bytes]);
        }
        // SetTrimming, IsTrimming
        0x000E => {
            let trimming = word(system, 2) & 0xFF != 0;
            for port in ports(system) {
                system.services.cam.ports[port].trimming = trimming;
            }
            reply(system, &[]);
        }
        0x000F => {
            let port = port(system);
            let trimming = system.services.cam.ports[port].trimming;
            reply(system, &[trimming as u32]);
        }
        // SetOutputFormat(cameras, format, context)
        0x0025 => {
            system.services.cam.rgb = word(system, 2) & 0xFF == 1;
            reply(system, &[]);
        }
        // GetStereoCameraCalibrationData, what Citra gives: scale 1, both
        // views 64.6875 degrees wide, a chart 250 mm off, the cameras 35 mm
        // apart, 640x480
        0x002B => {
            let float = |value: f32| value.to_bits();
            let mut data = [0u32; 16];
            data[1] = float(1.0);
            data[7] = float(64.6875);
            data[8] = float(64.6875);
            data[9] = float(250.0);
            data[10] = float(35.0);
            data[11] = 640 | 480 << 16;
            reply(system, &data);
        }
        // ClearBuffer, the trimming and picture settings, SwitchContext,
        // Activate, SynchronizeVsyncTiming, DriverInitialize and
        // DriverFinalize, which a black picture takes no notice of
        0x0004 | 0x0010 | 0x0012..=0x0024 | 0x0026..=0x0029 | 0x0033..=0x003A | 0x003D | 0x003E => {
            if command == 0x0039 {
                system.services.cam.ports = Default::default();
            }
            reply(system, &[]);
        }
        _ => return false,
    }
    true
}

/// a frame went by: every port capturing sees its vsync, and a picture it
/// waited for comes, black.
pub fn frame(system: &mut System) {
    for port in 0..2 {
        let Port { capturing, receiving, events, .. } = system.services.cam.ports[port];
        let Some(events) = events.filter(|_| capturing) else { continue };
        if let Some((destination, size)) = receiving {
            let black: &[u8] = if system.services.cam.rgb { &[0, 0] } else { &[0x10, 0x80] };
            let picture: Vec<u8> = black.iter().copied().cycle().take(size as usize).collect();
            system.memory.write_bytes(destination, &picture);
            system.services.cam.ports[port].receiving = None;
            system.kernel.signal_event(events[0]);
        }
        system.kernel.signal_event(events[2]);
    }
}

fn reply_handle(system: &mut System, buffer: &CommandBuffer, command: u16, object: ObjectId) {
    let handle = system.kernel.handles.create(&mut system.kernel.objects, object, "cam:u event");
    buffer.set(&mut system.memory, 0, Header::new(command, 1, 2).0);
    buffer.set(&mut system.memory, 1, 0);
    buffer.set(&mut system.memory, 2, Descriptor::handles(1));
    buffer.set(&mut system.memory, 3, handle);
}

/// a port's events, made the first time a title asks for one.
fn events(system: &mut System, port: usize) -> [ObjectId; 3] {
    if let Some(events) = system.services.cam.ports[port].events {
        return events;
    }
    let events = ["cam:u:Receive", "cam:u:BufferError", "cam:u:Vsync"].map(|name| {
        let object = system.kernel.objects.insert(KObject::Event(crate::kernel::sync::Event::new(ResetType::OneShot, name)));
        // the service keeps them, a title closing its handles must not take
        // them away
        system.kernel.objects.add_ref(object);
        object
    });
    system.services.cam.ports[port].events = Some(events);
    events
}
