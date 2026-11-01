//! IOHIDManager listener: reports every keyboard key value together with the
//! device it came from. Devices are opened listen-only (never seized).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use core_foundation::base::{CFRelease, CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};
use core_foundation::string::CFString;
use io_kit_sys::hid::base::{IOHIDDeviceRef, IOHIDValueRef};
use io_kit_sys::hid::device::IOHIDDeviceGetProperty;
use io_kit_sys::hid::element::{IOHIDElementGetDevice, IOHIDElementGetUsage, IOHIDElementGetUsagePage};
use io_kit_sys::hid::keys::kIOHIDOptionsTypeNone;
use io_kit_sys::hid::manager::*;
use io_kit_sys::hid::value::{IOHIDValueGetElement, IOHIDValueGetIntegerValue, IOHIDValueGetTimeStamp};
use io_kit_sys::ret::IOReturn;

use super::{DeviceInfo, KeyAction, KeyEvent, KeySource, classify, is_key_usage};

#[derive(Debug, Clone)]
pub enum InputMsg {
    Key(KeyEvent),
    Devices {
        builtin: usize,
        external: usize,
    },
    /// A keyboard was connected (diagnostics for `probe` and logs).
    DeviceSeen {
        info: DeviceInfo,
        source: KeySource,
    },
}

/// Connected keyboards, keyed by IOHIDDevice pointer. Only the matching and
/// removal callbacks change it, so a key value that arrives after its device
/// was removed can never leave a stale entry behind.
#[derive(Default)]
pub struct DeviceRegistry {
    devices: HashMap<usize, KeySource>,
}

impl DeviceRegistry {
    pub fn matched(&mut self, id: usize, source: KeySource) {
        self.devices.insert(id, source);
    }

    pub fn removed(&mut self, id: usize) {
        self.devices.remove(&id);
    }

    pub fn source_of(&self, id: usize) -> Option<KeySource> {
        self.devices.get(&id).copied()
    }

    /// (built-in, external)
    pub fn counts(&self) -> (usize, usize) {
        let builtin = self.devices.values().filter(|s| **s == KeySource::BuiltIn).count();
        (builtin, self.devices.len() - builtin)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    Opened,
    /// No Input Monitoring permission: nothing will be delivered.
    NotPermitted,
    /// Some device couldn't be opened (e.g. held exclusively elsewhere); the
    /// others still deliver, so keep listening.
    Partial,
}

const K_IO_RETURN_NOT_PERMITTED: i32 = 0xe00002e2u32 as i32;

pub fn open_outcome(ret: IOReturn) -> OpenOutcome {
    match ret {
        0 => OpenOutcome::Opened,
        K_IO_RETURN_NOT_PERMITTED => OpenOutcome::NotPermitted,
        _ => OpenOutcome::Partial,
    }
}

struct Ctx {
    tx: Sender<InputMsg>,
    devices: DeviceRegistry,
}

impl Ctx {
    fn send_counts(&self) {
        let (builtin, external) = self.devices.counts();
        let _ = self.tx.send(InputMsg::Devices { builtin, external });
    }
}

fn prop(device: IOHIDDeviceRef, key: &'static str) -> Option<CFType> {
    let k = CFString::from_static_string(key);
    let r = unsafe { IOHIDDeviceGetProperty(device, k.as_concrete_TypeRef()) };
    if r.is_null() {
        None
    } else {
        Some(unsafe { CFType::wrap_under_get_rule(r) })
    }
}

fn device_info(device: IOHIDDeviceRef) -> DeviceInfo {
    let built_in = prop(device, "Built-In").and_then(|v| {
        if let Some(b) = v.downcast::<CFBoolean>() {
            Some(bool::from(b))
        } else {
            v.downcast::<CFNumber>().and_then(|n| n.to_i64()).map(|n| n != 0)
        }
    });
    let string = |key| {
        prop(device, key)
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string())
    };
    DeviceInfo {
        built_in,
        transport: string("Transport"),
        product: string("Product"),
    }
}

unsafe fn ctx<'a>(context: *mut c_void) -> &'a RefCell<Ctx> {
    unsafe { &*(context as *const RefCell<Ctx>) }
}

unsafe extern "C" fn on_match(context: *mut c_void, _r: IOReturn, _s: *mut c_void, device: IOHIDDeviceRef) {
    let ctx = unsafe { ctx(context) };
    let info = device_info(device);
    let source = classify(&info);
    let mut c = ctx.borrow_mut();
    c.devices.matched(device as usize, source);
    let _ = c.tx.send(InputMsg::DeviceSeen { info, source });
    c.send_counts();
}

unsafe extern "C" fn on_remove(context: *mut c_void, _r: IOReturn, _s: *mut c_void, device: IOHIDDeviceRef) {
    let ctx = unsafe { ctx(context) };
    let mut c = ctx.borrow_mut();
    c.devices.removed(device as usize);
    c.send_counts();
}

unsafe extern "C" fn on_value(context: *mut c_void, _r: IOReturn, _s: *mut c_void, value: IOHIDValueRef) {
    let ctx = unsafe { ctx(context) };
    let element = unsafe { IOHIDValueGetElement(value) };
    let (page, usage) = unsafe { (IOHIDElementGetUsagePage(element), IOHIDElementGetUsage(element)) };
    if !is_key_usage(page, usage) {
        return;
    }
    let device = unsafe { IOHIDElementGetDevice(element) };
    let c = ctx.borrow();
    let source = c
        .devices
        .source_of(device as usize)
        .unwrap_or_else(|| classify(&device_info(device)));
    let action = if unsafe { IOHIDValueGetIntegerValue(value) } != 0 {
        KeyAction::Down
    } else {
        KeyAction::Up
    };
    if std::env::var_os("RUSTIC_KEYS_TRACE").is_some() {
        trace_delivery(unsafe { IOHIDValueGetTimeStamp(value) });
    }
    let _ = c.tx.send(InputMsg::Key(KeyEvent { source, action, usage }));
}

/// Diagnostics: how long after the hardware event this callback ran.
#[allow(deprecated)] // libc mach time fns; avoids an extra mach2 dependency
fn trace_delivery(event_ticks: u64) {
    let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
    let now = unsafe {
        libc::mach_timebase_info(&mut tb);
        libc::mach_absolute_time()
    };
    let ns = now.saturating_sub(event_ticks) as u128 * tb.numer as u128 / tb.denom.max(1) as u128;
    eprintln!("[trace] HID delivery {:.2} ms", ns as f64 / 1e6);
}

/// Start the listener on its own thread with its own run loop. Returns once
/// the HID manager is open (or with the error that prevented it).
pub fn spawn_listener(tx: Sender<InputMsg>) -> Result<JoinHandle<()>> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let handle = std::thread::Builder::new()
        .name("hid-listener".into())
        .spawn(move || {
            // Owned by the run loop for the life of the process; freed only if
            // opening fails below.
            let ctx_ptr = Box::into_raw(Box::new(RefCell::new(Ctx {
                tx,
                devices: DeviceRegistry::default(),
            })));
            let context = ctx_ptr as *mut c_void;
            unsafe {
                let manager = IOHIDManagerCreate(std::ptr::null(), kIOHIDManagerOptionNone);
                if manager.is_null() {
                    drop(Box::from_raw(ctx_ptr));
                    let _ = ready_tx.send(Err(anyhow!("IOHIDManagerCreate failed")));
                    return;
                }
                let matching = CFDictionary::from_CFType_pairs(&[
                    (CFString::from_static_string("DeviceUsagePage"), CFNumber::from(1i32)),
                    (CFString::from_static_string("DeviceUsage"), CFNumber::from(6i32)),
                ]);
                IOHIDManagerSetDeviceMatching(manager, matching.as_concrete_TypeRef() as _);
                IOHIDManagerRegisterDeviceMatchingCallback(manager, Some(on_match), context);
                IOHIDManagerRegisterDeviceRemovalCallback(manager, Some(on_remove), context);
                IOHIDManagerRegisterInputValueCallback(manager, Some(on_value), context);
                IOHIDManagerScheduleWithRunLoop(
                    manager,
                    CFRunLoop::get_current().as_concrete_TypeRef(),
                    kCFRunLoopDefaultMode,
                );
                let ret = IOHIDManagerOpen(manager, kIOHIDOptionsTypeNone);
                match open_outcome(ret) {
                    OpenOutcome::Opened => {}
                    OpenOutcome::Partial => {
                        eprintln!(
                            "[rustic-keys] some keyboards couldn't be opened (0x{ret:08x}); listening to the rest"
                        );
                    }
                    OpenOutcome::NotPermitted => {
                        // Tear everything down so retries don't leak.
                        IOHIDManagerUnscheduleFromRunLoop(
                            manager,
                            CFRunLoop::get_current().as_concrete_TypeRef(),
                            kCFRunLoopDefaultMode,
                        );
                        CFRelease(manager as CFTypeRef);
                        drop(Box::from_raw(ctx_ptr));
                        let _ = ready_tx.send(Err(anyhow!("IOHIDManagerOpen: not permitted (0x{ret:08x})")));
                        return;
                    }
                }
                let _ = ready_tx.send(Ok(()));
                CFRunLoop::run_current();
            }
        })
        .context("spawning HID listener thread")?;
    ready_rx.recv().context("HID listener thread exited early")??;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_counts_matched_devices() {
        let mut r = DeviceRegistry::default();
        r.matched(1, KeySource::BuiltIn);
        r.matched(2, KeySource::External);
        assert_eq!(r.counts(), (1, 1));
        r.removed(2);
        assert_eq!(r.counts(), (1, 0));
    }

    #[test]
    fn registry_lookup_of_unknown_device_does_not_register_it() {
        let r = DeviceRegistry::default();
        assert_eq!(r.source_of(7), None);
        assert_eq!(r.counts(), (0, 0));
    }

    #[test]
    fn open_outcome_classifies_iokit_results() {
        assert_eq!(open_outcome(0), OpenOutcome::Opened);
        assert_eq!(open_outcome(0xe00002e2u32 as i32), OpenOutcome::NotPermitted);
        // Any other error: some device failed to open, the rest may still work.
        assert_eq!(open_outcome(0xe00002c5u32 as i32), OpenOutcome::Partial);
    }
}
