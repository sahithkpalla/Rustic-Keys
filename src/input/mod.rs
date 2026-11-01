//! Keyboard input: event types and device classification.

pub mod hid;
pub mod permission;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeySource {
    BuiltIn,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub source: KeySource,
    pub action: KeyAction,
    pub usage: u32,
}

/// HID properties of a keyboard device that matter for classification.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub built_in: Option<bool>,
    pub transport: Option<String>,
    pub product: Option<String>,
}

/// Decide whether a keyboard is the laptop's own keyboard.
///
/// The `Built-In` HID property is authoritative when present. Otherwise an
/// internal transport (SPI on Intel T2 Macs, FIFO on Apple Silicon) combined
/// with an "Internal" product name counts as built-in. Anything else,
/// including devices with no information, is treated as external so we err
/// towards silence.
pub fn classify(d: &DeviceInfo) -> KeySource {
    if d.built_in == Some(true) {
        return KeySource::BuiltIn;
    }
    let internal_transport = matches!(d.transport.as_deref(), Some("SPI" | "FIFO"));
    let internal_product = d.product.as_deref().is_some_and(|p| p.contains("Internal"));
    if d.built_in.is_none() && internal_transport && internal_product {
        KeySource::BuiltIn
    } else {
        KeySource::External
    }
}

/// HID keyboard page (0x07), real key usages only (excludes error/rollover codes).
pub fn is_key_usage(usage_page: u32, usage: u32) -> bool {
    usage_page == 0x07 && (0x04..=0xE7).contains(&usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_flag_wins() {
        assert_eq!(
            classify(&DeviceInfo { built_in: Some(true), transport: Some("USB".into()), product: None }),
            KeySource::BuiltIn
        );
    }

    #[test]
    fn internal_transport_is_builtin_when_flag_missing() {
        for t in ["SPI", "FIFO"] {
            assert_eq!(
                classify(&DeviceInfo {
                    built_in: None,
                    transport: Some(t.into()),
                    product: Some("Apple Internal Keyboard / Trackpad".into())
                }),
                KeySource::BuiltIn
            );
        }
    }

    #[test]
    fn usb_and_bluetooth_are_external() {
        for t in ["USB", "Bluetooth", "Bluetooth Low Energy"] {
            assert_eq!(
                classify(&DeviceInfo {
                    built_in: Some(false),
                    transport: Some(t.into()),
                    product: Some("Keychron K2".into())
                }),
                KeySource::External
            );
        }
    }

    #[test]
    fn unknown_device_is_external() {
        assert_eq!(classify(&DeviceInfo::default()), KeySource::External);
    }

    #[test]
    fn key_usage_range() {
        assert!(is_key_usage(0x07, 0x04)); // 'a'
        assert!(is_key_usage(0x07, 0xE7)); // right GUI
        assert!(!is_key_usage(0x07, 0x01)); // rollover error
        assert!(!is_key_usage(0x07, 0xFFFF_FFFF));
        assert!(!is_key_usage(0x0C, 0xE9)); // consumer page (volume)
    }
}
