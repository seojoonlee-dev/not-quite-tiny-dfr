//! The virtual keyboard (uinput) the daemon types through, plus the libinput
//! `Interface` used to open the real input devices.

use crate::backlight;
use crate::config::ButtonAction;
use input::LibinputInterface;
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{input_event, input_id, timeval, uinput_setup};
use libc::{c_char, O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use std::{
    fs::{File, OpenOptions},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
};

pub(crate) struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let mode = flags & O_ACCMODE;

        OpenOptions::new()
            .custom_flags(flags)
            .read(mode == O_RDONLY || mode == O_RDWR)
            .write(mode == O_WRONLY || mode == O_RDWR)
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}

pub(crate) fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32)
where
    F: AsRawFd,
{
    uinput
        .write(&[input_event {
            value,
            type_: ty as u16,
            code,
            time: timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        }])
        .unwrap();
}

pub(crate) fn toggle_keys<F>(uinput: &mut UInputHandle<F>, codes: &Vec<ButtonAction>, value: i32)
where
    F: AsRawFd,
{
    if codes.is_empty() {
        return;
    }
    for action in codes {
        match action {
            // Handled inside the daemon; no input event leaves it.
            ButtonAction::TouchBarBrightnessUp | ButtonAction::TouchBarBrightnessDown => {
                if value <= 1 {
                    let delta = if *action == ButtonAction::TouchBarBrightnessUp {
                        1
                    } else {
                        -1
                    };
                    backlight::dim_button(delta, value == 1);
                }
            }
            ButtonAction::Key(kc) => emit(uinput, EventKind::Key, *kc as u16, value),
        }
    }
    emit(
        uinput,
        EventKind::Synchronize,
        SynchronizeKind::Report as u16,
        0,
    );
}

/// Set up the virtual keyboard. Created in main(), before the panic boundary:
/// /dev/uinput is only openable as root, and by the time real_main panics the
/// privileges are long dropped -- but the emergency Esc key still needs it.
pub(crate) fn create_uinput() -> UInputHandle<File> {
    let uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    uinput.set_evbit(EventKind::Key).unwrap();
    for k in Key::iter() {
        uinput.set_keybit(k).unwrap();
    }
    let mut dev_name_c = [0 as c_char; 80];
    let dev_name = "Dynamic Function Row Virtual Input Device".as_bytes();
    for i in 0..dev_name.len() {
        dev_name_c[i] = dev_name[i] as c_char;
    }
    uinput
        .dev_setup(&uinput_setup {
            id: input_id {
                bustype: 0x19,
                vendor: 0x1209,
                product: 0x316E,
                version: 1,
            },
            ff_effects_max: 0,
            name: dev_name_c,
        })
        .unwrap();
    uinput.dev_create().unwrap();
    uinput
}
