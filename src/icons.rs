//! Tree and list icons.
//!
//! Standard Unicode symbols are used by default: they are available in common
//! monospace fonts (JetBrains Mono, Menlo, …) and in the system fallbacks, so they
//! render in every terminal. Nerd Font glyphs (private-use code points, shown as
//! "missing character" boxes without a Nerd Font) are opt-in via
//! `AZ_STORAGE_ICONS=nerd` or `"nerd_font_icons": true` in config.json.
//!
//! Every icon must be exactly one terminal column wide (layout code relies on it).

use std::sync::OnceLock;

pub struct Icons {
    pub subs_root: &'static str,
    pub attached_root: &'static str,
    pub subscription: &'static str,
    pub account: &'static str,
    pub blobs: &'static str,
    pub queues: &'static str,
    pub container: &'static str,
    pub queue: &'static str,
    pub dir: &'static str,
    pub file: &'static str,
    pub loading: &'static str,
}

const UNICODE: Icons = Icons {
    subs_root: "◈",
    attached_root: "◇",
    subscription: "◆",
    account: "■",
    blobs: "⊞",
    queues: "≡",
    container: "□",
    queue: "≡",
    dir: "■",
    file: "▫",
    loading: "◌",
};

const NERD: Icons = Icons {
    subs_root: "\u{f0312}",
    attached_root: "\u{f0337}",
    subscription: "\u{f0126}",
    account: "\u{f02ca}",
    blobs: "\u{f01bc}",
    queues: "\u{f04b2}",
    container: "\u{f01a7}",
    queue: "\u{f0055}",
    dir: "\u{f024b}",
    file: "\u{f021a}",
    loading: "◌",
};

static USE_NERD: OnceLock<bool> = OnceLock::new();

/// Select the icon set once at startup (environment variable wins over config).
pub fn init(config_nerd: bool) {
    let nerd = match std::env::var("AZ_STORAGE_ICONS") {
        Ok(v) if v.eq_ignore_ascii_case("nerd") => true,
        Ok(v) if v.eq_ignore_ascii_case("unicode") => false,
        _ => config_nerd,
    };
    let _ = USE_NERD.set(nerd);
}

pub fn icons() -> &'static Icons {
    if *USE_NERD.get().unwrap_or(&false) { &NERD } else { &UNICODE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_icons_are_single_width_and_not_private_use() {
        let i = &UNICODE;
        for s in [
            i.subs_root, i.attached_root, i.subscription, i.account, i.blobs, i.queues,
            i.container, i.queue, i.dir, i.file, i.loading,
        ] {
            assert_eq!(s.chars().count(), 1, "{s}");
            let c = s.chars().next().unwrap() as u32;
            assert!(!(0xE000..=0xF8FF).contains(&c) && c < 0xF0000, "private-use glyph {s}");
        }
    }
}
