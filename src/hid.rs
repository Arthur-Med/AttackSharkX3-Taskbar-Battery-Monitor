//! Win32 HID device discovery and overlapped telemetry reader for Attack Shark X3.

use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetPreparsedData,
    HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, PHIDP_PREPARSED_DATA,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, HANDLE, HWND, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForMultipleObjects};
use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::protocol::{
    parse_telemetry_packet, BatteryState, ATTACK_SHARK_VID, PID_WIRED, PID_WIRELESS,
    SLEEP_WATCHDOG_MS, TELEMETRY_USAGE_PAGE,
};

struct SafeHandle(HANDLE);

impl SafeHandle {
    fn new(handle: HANDLE) -> Option<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(Self(handle))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for SafeHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct SafeDevInfo(HDEVINFO);

impl Drop for SafeDevInfo {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

struct SafePreparsed(PHIDP_PREPARSED_DATA);

impl Drop for SafePreparsed {
    fn drop(&mut self) {
        unsafe {
            HidD_FreePreparsedData(self.0);
        }
    }
}

#[derive(Default)]
struct DiscoveredDevices {
    wired_present: bool,
    wireless_telemetry: Option<(Vec<u16>, usize)>,
}

pub fn hid_interface_guid() -> GUID {
    let mut guid = GUID {
        data1: 0,
        data2: 0,
        data3: 0,
        data4: [0; 8],
    };
    unsafe {
        HidD_GetHidGuid(&mut guid);
    }
    guid
}

fn contains_ascii_case_insensitive(wide: &[u16], needle: &[u8]) -> bool {
    if wide.len() < needle.len() {
        return false;
    }
    wide.windows(needle.len()).any(|win| {
        win.iter()
            .zip(needle.iter())
            .all(|(&w, &b)| (w as u8).to_ascii_lowercase() == b && w <= 0x7F)
    })
}

fn discover_devices() -> DiscoveredDevices {
    let mut result = DiscoveredDevices::default();
    let hid_guid = hid_interface_guid();

    let info_set = unsafe {
        SetupDiGetClassDevsW(
            &hid_guid,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if info_set == INVALID_HANDLE_VALUE as isize {
        return result;
    }
    let _dev_info_guard = SafeDevInfo(info_set);

    let mut if_data = SP_DEVICE_INTERFACE_DATA {
        cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        InterfaceClassGuid: GUID {
            data1: 0,
            data2: 0,
            data3: 0,
            data4: [0; 8],
        },
        Flags: 0,
        Reserved: 0,
    };

    let mut index = 0u32;
    loop {
        let ok = unsafe {
            SetupDiEnumDeviceInterfaces(info_set, null(), &hid_guid, index, &mut if_data)
        };
        if ok == 0 {
            break;
        }
        index += 1;

        let mut req_size = 0u32;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                info_set,
                &if_data,
                null_mut(),
                0,
                &mut req_size,
                null_mut(),
            );
        }
        if req_size < 8 {
            continue;
        }

        let mut detail_buf = vec![0u8; req_size as usize];
        let cb_size: u32 = if cfg!(target_pointer_width = "64") { 8 } else { 6 };
        detail_buf[..4].copy_from_slice(&cb_size.to_ne_bytes());

        let mut dummy = 0u32;
        let ok_detail = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                info_set,
                &if_data,
                detail_buf.as_mut_ptr().cast(),
                req_size,
                &mut dummy,
                null_mut(),
            )
        };
        if ok_detail == 0 {
            continue;
        }

        let path_bytes = &detail_buf[4..];
        let mut path_w: Vec<u16> = path_bytes
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .take_while(|&ch| ch != 0)
            .collect();
        path_w.push(0);

        // Quick pre-filter before opening device handles to avoid probing unrelated HID devices.
        if !contains_ascii_case_insensitive(&path_w, b"vid_1d57") {
            continue;
        }

        let meta_handle = unsafe {
            CreateFileW(
                path_w.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        let Some(handle) = SafeHandle::new(meta_handle) else {
            continue;
        };

        let mut attrs = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            VendorID: 0,
            ProductID: 0,
            VersionNumber: 0,
        };
        if !unsafe { HidD_GetAttributes(handle.raw(), &mut attrs) } {
            continue;
        }
        if attrs.VendorID != ATTACK_SHARK_VID {
            continue;
        }

        if attrs.ProductID == PID_WIRED {
            result.wired_present = true;
            continue;
        }

        if attrs.ProductID != PID_WIRELESS {
            continue;
        }

        let mut preparsed: PHIDP_PREPARSED_DATA = 0;
        if !unsafe { HidD_GetPreparsedData(handle.raw(), &mut preparsed) } || preparsed == 0 {
            continue;
        }
        let _preparsed_guard = SafePreparsed(preparsed);

        let mut caps = unsafe { std::mem::zeroed::<HIDP_CAPS>() };
        let status = unsafe { HidP_GetCaps(preparsed, &mut caps) };
        if status != HIDP_STATUS_SUCCESS {
            continue;
        }

        if caps.UsagePage == TELEMETRY_USAGE_PAGE && caps.InputReportByteLength >= 5 {
            result.wireless_telemetry = Some((path_w, caps.InputReportByteLength as usize));
        }
    }

    result
}

fn publish_state(
    shared_state: &Arc<Mutex<BatteryState>>,
    hwnd: HWND,
    update_msg: u32,
    new_state: BatteryState,
) {
    let changed = {
        let mut guard = shared_state.lock().unwrap();
        if *guard != new_state {
            *guard = new_state;
            true
        } else {
            false
        }
    };
    if changed {
        unsafe {
            PostMessageW(hwnd, update_msg, 0, 0);
        }
    }
}

/// Spawns the background HID monitor thread.
///
/// `wake_event` and `stop_event` must remain valid until the returned `JoinHandle` is joined.
pub fn spawn_hid_monitor(
    shared_state: Arc<Mutex<BatteryState>>,
    hwnd_raw: usize,
    update_msg: u32,
    wake_event_raw: usize,
    stop_event_raw: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let hwnd = hwnd_raw as HWND;
        let wake_event = wake_event_raw as HANDLE;
        let stop_event = stop_event_raw as HANDLE;

        let mut last_percent: Option<u8> = None;
        let mut last_packet: Option<[u8; 5]> = None;

        'outer: loop {
            let devices = discover_devices();

            if devices.wired_present {
                publish_state(
                    &shared_state,
                    hwnd,
                    update_msg,
                    BatteryState::WiredCharging,
                );
                let handles = [wake_event, stop_event];
                let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 2_500) };
                if wait == WAIT_OBJECT_0 + 1 {
                    break 'outer;
                }
                continue;
            }

            let Some((path_w, report_len)) = devices.wireless_telemetry else {
                publish_state(
                    &shared_state,
                    hwnd,
                    update_msg,
                    BatteryState::Disconnected,
                );
                let handles = [wake_event, stop_event];
                let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 2_500) };
                if wait == WAIT_OBJECT_0 + 1 {
                    break 'outer;
                }
                continue;
            };

            let read_handle_raw = unsafe {
                CreateFileW(
                    path_w.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    null_mut(),
                )
            };
            let Some(read_handle) = SafeHandle::new(read_handle_raw) else {
                let handles = [wake_event, stop_event];
                let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 1_000) };
                if wait == WAIT_OBJECT_0 + 1 {
                    break 'outer;
                }
                continue;
            };

            let read_event_raw = unsafe { CreateEventW(null(), 1, 0, null()) };
            let Some(read_event) = SafeHandle::new(read_event_raw) else {
                break 'outer;
            };

            let mut last_packet_at = Instant::now();
            publish_state(
                &shared_state,
                hwnd,
                update_msg,
                BatteryState::Sleeping {
                    last_percent,
                    last_packet,
                },
            );

            let mut buf = vec![0u8; report_len.max(5)];
            let handles = [read_event.raw(), wake_event, stop_event];

            loop {
                unsafe { ResetEvent(read_event.raw()) };
                let mut overlapped = unsafe { std::mem::zeroed::<OVERLAPPED>() };
                overlapped.hEvent = read_event.raw();

                let mut bytes_read = 0u32;
                let ok = unsafe {
                    ReadFile(
                        read_handle.raw(),
                        buf.as_mut_ptr(),
                        buf.len() as u32,
                        &mut bytes_read,
                        &mut overlapped,
                    )
                };

                let got_packet = if ok != 0 {
                    true
                } else {
                    let err = unsafe { GetLastError() };
                    if err != ERROR_IO_PENDING {
                        break;
                    }
                    let wait = unsafe {
                        WaitForMultipleObjects(
                            3,
                            handles.as_ptr(),
                            0,
                            SLEEP_WATCHDOG_MS as u32,
                        )
                    };
                    if wait == WAIT_OBJECT_0 {
                        let res = unsafe {
                            GetOverlappedResult(read_handle.raw(), &overlapped, &mut bytes_read, 0)
                        };
                        res != 0
                    } else {
                        // Cancel pending IRP and wait for completion before reusing/dropping buffers.
                        unsafe {
                            CancelIo(read_handle.raw());
                            let mut cancelled_bytes = 0u32;
                            GetOverlappedResult(
                                read_handle.raw(),
                                &overlapped,
                                &mut cancelled_bytes,
                                1,
                            );
                        }
                        if wait == WAIT_OBJECT_0 + 2 {
                            break 'outer;
                        }
                        if wait == WAIT_OBJECT_0 + 1 {
                            break;
                        }
                        if wait == WAIT_TIMEOUT {
                            if discover_devices().wired_present {
                                break;
                            }
                            if last_packet_at.elapsed() >= Duration::from_millis(SLEEP_WATCHDOG_MS)
                            {
                                publish_state(
                                    &shared_state,
                                    hwnd,
                                    update_msg,
                                    BatteryState::Sleeping {
                                        last_percent,
                                        last_packet,
                                    },
                                );
                            }
                            continue;
                        }
                        break;
                    }
                };

                if !got_packet {
                    break;
                }

                let slice = &buf[..bytes_read as usize];
                if let Some(percent) = parse_telemetry_packet(slice) {
                    let pkt = [slice[0], slice[1], slice[2], slice[3], slice[4]];
                    last_percent = Some(percent);
                    last_packet = Some(pkt);
                    last_packet_at = Instant::now();
                    publish_state(
                        &shared_state,
                        hwnd,
                        update_msg,
                        BatteryState::Wireless {
                            percent,
                            last_packet: pkt,
                        },
                    );
                }
            }
        }
    })
}
