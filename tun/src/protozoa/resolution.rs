//! A single video-frame resolution type, replacing the ad-hoc mix of
//! `(u32, u32)` / `(u16, u16)` tuples, packed `u64`s and `"WxH"` strings that
//! used to represent the same thing across the tunnel, engine and media code.

/// A video frame resolution.
///
/// Stored as a `u16` pair — both the tunnel frame headers and the VP9
/// bitstream cap width/height at 16 bits — with `u32` accessors for the
/// libwebrtc and buffer-sizing call sites, a packed `u64` form for the atomics
/// that track the local/remote encoder resolution, and `Display` as `"WxH"`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Resolution {
    pub width: u16,
    pub height: u16,
}

impl Resolution {
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    pub const fn w32(self) -> u32 {
        self.width as u32
    }

    pub const fn h32(self) -> u32 {
        self.height as u32
    }

    /// Packed as `(w as u64) << 16 | h`, for storage in an `AtomicU64`.
    pub const fn pack(self) -> u64 {
        (self.width as u64) << 16 | self.height as u64
    }

    /// Inverse of [`pack`](Self::pack).
    pub const fn unpack(v: u64) -> Self {
        Self {
            width: (v >> 16) as u16,
            height: v as u16,
        }
    }

    /// Parse `"WxH"` and validate it against the available prebuilt keyframes.
    /// Returns `None` if the format is wrong or the resolution is not one of
    /// the presets. Delegates to [`super::vp9::parse_resolution`] so the
    /// (generated) keyframe table stays the single source of valid presets.
    pub fn parse_preset(s: &str) -> Option<Self> {
        let (w, h) = super::vp9::parse_resolution(s)?;
        Some(Self::new(w as u16, h as u16))
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}
