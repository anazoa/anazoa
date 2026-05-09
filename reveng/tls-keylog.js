/*
 * tls-keylog.js — Frida 17.x TLS session-key extractor for Max Android
 *
 * Captures SSLKEYLOGFILE-format secrets from two BoringSSL instances:
 *
 *   1. Platform Conscrypt  (libjavacrypto.so)
 *      Covers: OkHttp HTTPS (REST API, fb.do), OkHttp WSS (OneMe WS),
 *              Kwik QUIC/WebTransport TLS (WebRTC signaling)
 *
 *   2. WebRTC native stack (libjingle_peerconnection_so.so)
 *      Covers: DTLS handshake secrets (decrypt media-path DTLS in Wireshark)
 *      Uses a fixed offset into the bundled stripped BoringSSL build.
 *
 * Both instances expose SSL_CTX_new / SSL_CTX_set_keylog_callback.
 * We hook SSL_CTX_new so every new context (including QUIC ones) gets the
 * callback; existing contexts created before attach are missed — spawn the
 * app instead of attaching to a running instance if you need them.
 *
 * Usage:
 *   frida -U -f ru.oneme.app -l tls-keylog.js    # spawn (captures all contexts)
 *   frida -U -n MAX          -l tls-keylog.js    # attach (misses early contexts)
 *
 * Extract keys from output:
 *   frida ... 2>&1 | grep '^\[KEYLOG\]' | sed 's/\[KEYLOG\] //' > keys.log
 *
 * Then in Wireshark: Edit → Preferences → Protocols → TLS → (Pre)-Master-Secret
 * log filename → point at keys.log
 *
 * Output format (one of):
 *   [KEYLOG] CLIENT_RANDOM <client-random-hex> <master-secret-hex>
 *   [KEYLOG] CLIENT_HANDSHAKE_TRAFFIC_SECRET <client-random-hex> <secret-hex>
 *   [KEYLOG] SERVER_HANDSHAKE_TRAFFIC_SECRET <client-random-hex> <secret-hex>
 *   [KEYLOG] CLIENT_TRAFFIC_SECRET_0 <client-random-hex> <secret-hex>
 *   [KEYLOG] SERVER_TRAFFIC_SECRET_0 <client-random-hex> <secret-hex>
 *   [KEYLOG] EXPORTER_SECRET <client-random-hex> <secret-hex>
 */

"use strict";

// ── helpers ──────────────────────────────────────────────────────────────────

function hexlify(ptr, len) {
    const bytes = ptr.readByteArray(len);
    const view = new Uint8Array(bytes);
    let s = "";
    for (let i = 0; i < view.length; i++) {
        s += ("0" + view[i].toString(16)).slice(-2);
    }
    return s;
}

// ── 1. Platform Conscrypt (exported symbols) ──────────────────────────────────
//
// Android < 12 : /system/lib64/libjavacrypto.so
// Android 12+  : /apex/com.android.conscrypt/lib64/libjavacrypto.so
//                (the APEX version shadows the system one once loaded)
//
// Both contain the same BoringSSL symbols; whichever is loaded first wins.
// We try both names since only one will be present in Process.enumerateModules().

function tryHookModule(modName) {
    const mod = Process.findModuleByName(modName);
    if (!mod) return false;

    const ctxNew = mod.findExportByName("SSL_CTX_new");
    const ctxSetKeylog = mod.findExportByName("SSL_CTX_set_keylog_callback");

    if (!ctxNew || !ctxSetKeylog) {
        console.log(
            "[tls-keylog] " +
                modName +
                ": SSL_CTX_new=" +
                ctxNew +
                " SSL_CTX_set_keylog_callback=" +
                ctxSetKeylog +
                " — skipping",
        );
        return false;
    }

    const setKeylog = new NativeFunction(ctxSetKeylog, "void", [
        "pointer",
        "pointer",
    ]);

    // NativeCallback must live as long as any SSL_CTX that uses it.
    // Storing on the global object keeps it alive.
    if (!globalThis._keylogCallbacks) globalThis._keylogCallbacks = [];
    const cb = new NativeCallback(
        function (ssl, linePtr) {
            const line = linePtr.readCString();
            if (line) console.log("[KEYLOG] " + line);
        },
        "void",
        ["pointer", "pointer"],
    );
    globalThis._keylogCallbacks.push(cb);

    Interceptor.attach(ctxNew, {
        onLeave: function (retval) {
            if (!retval.isNull()) {
                setKeylog(retval, cb);
            }
        },
    });

    console.log("[+] tls-keylog hooked " + modName);
    return true;
}

["libjavacrypto.so", "libssl.so"].forEach(tryHookModule);

// ── 2. WebRTC native (libjingle_peerconnection_so.so) ─────────────────────────
//
// Bundled BoringSSL, fully stripped (no exported symbols).
//
// Binary analysis of the APK's arm64 libjingle_peerconnection_so.so identified
// ssl_log_secret at offset 0x8757f8. This function has the signature:
//
//   void ssl_log_secret(const SSL *ssl, const char *label,
//                       const uint8_t *secret, size_t secret_len);
//
// Struct layout (from disassembly of the function body):
//
//   SSL*  + 0x30  → SSL3_STATE*
//   SSL3_STATE* + 0x30 → client_random[32]
//
// The keylog_callback pointer lives at:
//   SSL*[0x68][0x240]   (checked at entry; function returns early if null)
//
// We hook ssl_log_secret and reconstruct the SSLKEYLOGFILE line ourselves,
// emitting it regardless of whether the app set a keylog callback.
//
// NOTE: This offset is specific to the BoringSSL build embedded in
// Max APK 26.12.0 / 26.13.0 (arm64). If the APK is updated and the binary
// changes, re-derive the offset with:
//
//   python3 reveng/find_ssl_log_secret.py reveng/libjingle-arm64.so
//
// The script scans for ADRP+ADD pairs referencing the SSLKEYLOGFILE label
// strings and traces the common call target.

(function hookJingleBoringSSL() {
    const MOD_NAME = "libjingle_peerconnection_so.so";

    // Offset of ssl_log_secret within the module — derived from binary analysis.
    // arm64, Max APK 26.12.0 / 26.13.0.
    const SSL_LOG_SECRET_OFFSET = 0x8757f8;

    // Struct offsets confirmed by disassembly of ssl_log_secret:
    //   ldr x23, [x19, #48]   → SSL*[0x30] = s3 ptr
    //   add x9, x23, #0x30    → s3 + 0x30 = client_random
    const S3_OFF = 0x30;
    const CLIENT_RANDOM_OFF_IN_S3 = 0x30;
    const CLIENT_RANDOM_LEN = 32;

    function doHook(mod) {
        const addr = mod.base.add(SSL_LOG_SECRET_OFFSET);
        Interceptor.attach(addr, {
            onEnter: function (args) {
                try {
                    const sslPtr = args[0];
                    const label = args[1].readCString();
                    const secretPtr = args[2];
                    const secretLen = args[3].toUInt32();

                    if (!label || secretLen === 0 || secretLen > 256) return;

                    const s3Ptr = sslPtr.add(S3_OFF).readPointer();
                    const crHex = hexlify(
                        s3Ptr.add(CLIENT_RANDOM_OFF_IN_S3),
                        CLIENT_RANDOM_LEN,
                    );
                    const secHex = hexlify(secretPtr, secretLen);

                    console.log(
                        "[KEYLOG] " + label + " " + crHex + " " + secHex,
                    );
                } catch (e) {
                    // Silently swallow read errors on malformed/early calls.
                }
            },
        });
        console.log(
            "[+] tls-keylog hooked " + MOD_NAME + " ssl_log_secret @ " + addr,
        );
    }

    const mod = Process.findModuleByName(MOD_NAME);
    if (mod) {
        doHook(mod);
    } else {
        // Module may be loaded lazily (split APK / on-demand).
        // Watch for the module being mapped via dlopen.
        Interceptor.attach(Module.getExportByName(null, "dlopen"), {
            onLeave: function (retval) {
                if (!retval.isNull()) {
                    const m = Process.findModuleByName(MOD_NAME);
                    if (m) doHook(m);
                }
            },
        });
        console.log(
            "[tls-keylog] " +
                MOD_NAME +
                " not yet loaded — will hook on dlopen",
        );
    }
})();

// ── 3. Any other bundled BoringSSL (future-proofing) ─────────────────────────

Process.enumerateModules().forEach(function (mod) {
    if (
        /libssl|libcrypto|boring/i.test(mod.name) &&
        mod.name !== "libjavacrypto.so" &&
        mod.name !== "libssl.so" &&
        mod.name !== "libjingle_peerconnection_so.so"
    ) {
        tryHookModule(mod.name);
    }
});

console.log(
    "[tls-keylog] hooks installed — use frida -f (spawn) for full coverage",
);
