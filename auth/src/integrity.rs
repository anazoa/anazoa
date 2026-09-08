//! START_AUTH `mode` integrity token.
//!
//! Reverse-engineered from `libmax.so`'s
//! `one.me.callssdk.CallsSdkInitializer.initializeSessionSeed(Context, byte[] seed, byte[] deviceId)`
//!
//! ```text
//! mode = H1 ++ H3 ++ H2                       (96 bytes)
//! Hn   = SHA256( Xn ++ be64(callsSeed) ++ deviceId_utf8 )
//! X1 = SHA256(APK signer cert DER)
//! X2 = SHA256(every lib/<abi>/*.so, decompressed, name-sorted)
//! X3 = SHA256(first 20 bytes of each classes*.dex, name-sorted)
//! ```

use sha2::{Digest, Sha256};

pub const MAX_APP_VERSION: &str = "26.30.1";
pub const MAX_BUILD_NUMBER: u32 = 6819;

pub const MAX_ARCH: &str = "arm64-v8a";

const H_SIG: [u8; 32] = [
    0x16, 0x84, 0x41, 0x40, 0x33, 0xeb, 0x26, 0x3e, 0x2c, 0x61, 0x5f, 0x8b, 0x7d, 0xf5, 0xed, 0x87,
    0x93, 0x85, 0x0a, 0x07, 0x65, 0x63, 0x04, 0x99, 0x7f, 0xbf, 0x07, 0xe9, 0xe2, 0x1e, 0x1e, 0x93,
];

const DEX_META: [u8; 32] = [
    0xf0, 0x47, 0x8d, 0x38, 0xa9, 0xd9, 0x17, 0x2a, 0x7b, 0xf0, 0x55, 0xf7, 0xa3, 0xbb, 0xf5, 0xa8,
    0x78, 0xea, 0x9e, 0x4d, 0xf8, 0x19, 0x6a, 0x1c, 0x9e, 0x78, 0x8a, 0xd3, 0xc3, 0x5e, 0x95, 0xb7,
];

const SO_META: [u8; 32] = [
    0x9a, 0x7d, 0x6c, 0x3f, 0x2d, 0x3d, 0x01, 0xca, 0x5e, 0x86, 0x92, 0xf2, 0x54, 0xbe, 0x92, 0x30,
    0x6d, 0x37, 0xfa, 0xdb, 0x07, 0x46, 0x92, 0x47, 0x9b, 0xdf, 0x96, 0x19, 0x43, 0x67, 0x5f, 0x7a,
];

/// Build the 96-byte `mode` token for a START_AUTH request.
///
/// `calls_seed` is the `callsSeed` value from the SESSION_INIT response;
/// `device_id` must be the exact string sent as `deviceId` in the hello.
pub fn mode(calls_seed: i64, device_id: &str) -> [u8; 96] {
    let seed = calls_seed.to_be_bytes();
    let did = device_id.as_bytes();
    let round = |prefix: &[u8; 32]| -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(prefix);
        h.update(seed);
        h.update(did);
        h.finalize().into()
    };

    let h1 = round(&H_SIG);
    let h2 = round(&SO_META);
    let h3 = round(&DEX_META);

    let mut out = [0u8; 96];
    out[..32].copy_from_slice(&h1);
    out[32..64].copy_from_slice(&h3);
    out[64..].copy_from_slice(&h2);
    out
}
