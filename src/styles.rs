//! Built-in switch styles. Samples are embedded in the binary.

pub struct Style {
    pub name: &'static str,
    pub description: &'static str,
    pub press: &'static [&'static [u8]],
    pub release: &'static [&'static [u8]],
}

macro_rules! sounds {
    ($style:literal, $kind:literal, [$($n:literal),+]) => {
        &[$(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/sounds/", $style, "/", $kind, "-", $n, ".wav"))),+]
    };
}

static STYLES: &[Style] = &[
    Style {
        name: "clicky",
        description: "Bright click on every press (blue-style switches)",
        press: sounds!("clicky", "press", ["1", "2", "3"]),
        release: sounds!("clicky", "release", ["1", "2", "3"]),
    },
    Style {
        name: "tactile",
        description: "Soft bump, no click (brown-style switches)",
        press: sounds!("tactile", "press", ["1"]),
        release: sounds!("tactile", "release", ["1"]),
    },
    Style {
        name: "thocky",
        description: "Smooth and deep (linear red/black-style switches)",
        press: sounds!("thocky", "press", ["1", "2", "3"]),
        release: sounds!("thocky", "release", ["1", "2", "3"]),
    },
];

pub fn all() -> &'static [Style] {
    STYLES
}

pub fn find(name: &str) -> Option<&'static Style> {
    all().iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::sample::decode_wav;

    #[test]
    fn all_three_styles_present() {
        let names: Vec<_> = all().iter().map(|s| s.name).collect();
        assert_eq!(names, ["clicky", "tactile", "thocky"]);
    }

    #[test]
    fn every_style_has_press_and_release() {
        for s in all() {
            assert!(!s.press.is_empty() && !s.release.is_empty(), "{}", s.name);
        }
    }

    #[test]
    fn every_sample_decodes_and_is_short() {
        for s in all() {
            for bytes in s.press.iter().chain(s.release.iter()) {
                let pcm = decode_wav(bytes, 48000).unwrap();
                assert!((1..=12000).contains(&pcm.len()), "{}: {} samples", s.name, pcm.len());
            }
        }
    }

    #[test]
    fn find_unknown_is_none() {
        assert!(find("nope").is_none());
    }
}
