/*
 * wt-hook.js — Frida 17.x hook for Max Android WebTransport signaling
 *
 * Captures:
 *   1. The WebTransport URL (with all query params) used for signaling
 *   2. All outbound signaling messages (plaintext, before compression)
 *   3. All inbound signaling messages (plaintext, after decompression)
 *
 * Usage:
 *   frida -U -n ru.ok.max -l wt-hook.js          # attach to running app
 *   frida -U -f ru.ok.max -l wt-hook.js          # spawn fresh
 *
 * Output format:
 *   [WT URL]   wss://videowebrtc.okcdn.ru:23432/wt?...
 *   [WT →srv]  {"command":"..."}
 *   [WT ←srv]  {"type":"notification",...}
 */

'use strict';

Java.perform(function () {

    // ── helpers ──────────────────────────────────────────────────────────────

    function jbytesToStr(jbytes) {
        if (jbytes === null) return '<null>';
        try {
            return Java.use('java.lang.String').$new(jbytes, 'UTF-8');
        } catch (e) { return '<bytes?>'; }
    }

    function sliceToStr(jbytes, off, len) {
        if (jbytes === null) return '<null>';
        try {
            var slice = Java.array('byte', Array.from({length: len}, function (_, i) {
                return jbytes[off + i];
            }));
            return Java.use('java.lang.String').$new(slice, 'UTF-8');
        } catch (e) { return '<slice?>'; }
    }

    // Hook every overload of a method, logging args and return value.
    function hookMethod(cls, methodName, tag, showReturn) {
        try {
            cls[methodName].overloads.forEach(function (ovl) {
                ovl.implementation = function () {
                    var args = Array.from(arguments);
                    var strs = args.map(function (a) {
                        if (a === null) return 'null';
                        try {
                            var cn = a.getClass().getName();
                            if (cn === '[B') return '"' + jbytesToStr(a) + '"';
                        } catch (_) {}
                        return String(a);
                    });
                    var ret = ovl.apply(this, args);
                    var line = tag + ' ' + methodName + '(' + strs.join(', ').substring(0, 400) + ')';
                    if (showReturn && ret !== undefined && ret !== null)
                        line += ' => ' + String(ret).substring(0, 200);
                    console.log(line);
                    return ret;
                };
            });
        } catch (e) {
            console.log('[!] hookMethod ' + methodName + ': ' + e);
        }
    }

    // Reflectively hook all declared methods of a class.
    function hookAllMethods(className, tag, opts) {
        opts = opts || {};
        try {
            var cls = Java.use(className);
            cls.class.getDeclaredMethods().forEach(function (m) {
                var name = m.getName();
                if (opts.skip && opts.skip.indexOf(name) >= 0) return;
                hookMethod(cls, name, tag, !!opts.showReturn);
            });
            console.log('[+] hooked ' + className);
        } catch (e) {
            console.log('[!] hookAllMethods ' + className + ': ' + e);
        }
    }

    // ── 1. WebTransportSocket constructor → URL ───────────────────────────────
    // The constructor receives the wt endpoint URL (and possibly other params).
    try {
        var WTSocket = Java.use(
            'ru.ok.android.externcalls.sdk.wt.internal.WebTransportSocket');

        WTSocket.$init.overloads.forEach(function (ovl) {
            ovl.implementation = function () {
                var args = Array.from(arguments);
                // Print all String arguments — the URL will be among them.
                args.forEach(function (a, i) {
                    if (a !== null) {
                        try {
                            if (a.getClass().getName() === 'java.lang.String') {
                                console.log('[WT URL] arg[' + i + '] = ' + a);
                            }
                        } catch (_) {}
                    }
                });
                return ovl.apply(this, args);
            };
        });
        console.log('[+] hooked WebTransportSocket.$init');
    } catch (e) {
        console.log('[!] WebTransportSocket.$init: ' + e);
    }

    // ── 2. WTSignaling — top-level send/receive (plaintext JSON) ─────────────
    // Hook every method; the interesting ones will carry JSON strings.
    hookAllMethods(
        'ru.ok.android.externcalls.sdk.wt.WTSignaling',
        '[WT sig]',
        { showReturn: true }
    );

    // ── 3. WebTransportSocket — raw frame send ────────────────────────────────
    // sendStreamData(WebTransportStream, Listener) — data lives on the stream;
    // also try any overloads that carry byte arrays directly.
    try {
        var WTSock2 = Java.use(
            'ru.ok.android.externcalls.sdk.wt.internal.WebTransportSocket');
        WTSock2.sendStreamData.overloads.forEach(function (ovl) {
            ovl.implementation = function () {
                var args = Array.from(arguments);
                // Log any byte-array arguments as strings.
                args.forEach(function (a, i) {
                    if (a !== null) {
                        try {
                            if (a.getClass().getName() === '[B')
                                console.log('[WT →srv raw] ' + jbytesToStr(a));
                        } catch (_) {}
                    }
                });
                return ovl.apply(this, args);
            };
        });
        console.log('[+] hooked WebTransportSocket.sendStreamData');
    } catch (e) {
        console.log('[!] sendStreamData: ' + e);
    }

    // ── 4. WebTransportSocket$Listener — inbound frames ──────────────────────
    // Implemented as anonymous classes; use Java.enumerateClassLoaders +
    // Java.choose to find all live implementations at call time instead.
    try {
        var ListenerCls = Java.use(
            'ru.ok.android.externcalls.sdk.wt.internal.WebTransportSocket$Listener');

        // Hook concrete subtype created inside openSession lambda.
        var openSession1 = Java.use(
            'ru.ok.android.externcalls.sdk.wt.internal.WebTransportSocket$openSession$1$1');
        openSession1.class.getDeclaredMethods().forEach(function (m) {
            var name = m.getName();
            try {
                openSession1[name].overloads.forEach(function (ovl) {
                    ovl.implementation = function () {
                        var args = Array.from(arguments);
                        var strs = args.map(function (a) {
                            if (a === null) return 'null';
                            try {
                                if (a.getClass().getName() === '[B')
                                    return '"' + jbytesToStr(a) + '"';
                            } catch (_) {}
                            return String(a).substring(0, 300);
                        });
                        console.log('[WT ←srv] ' + name + '(' + strs.join(', ') + ')');
                        return ovl.apply(this, args);
                    };
                });
            } catch (_) {}
        });
        console.log('[+] hooked WebTransportSocket$openSession$1$1');
    } catch (e) {
        console.log('[!] openSession listener: ' + e);
    }

    // ── 5. CompressorDecompressor — see data at both sides of compression ─────
    try {
        var CDC = Java.use(
            'ru.ok.android.externcalls.sdk.wt.internal.WebTransportCompressorDecompressor');
        CDC.class.getDeclaredMethods().forEach(function (m) {
            var name = m.getName();
            try {
                CDC[name].overloads.forEach(function (ovl) {
                    ovl.implementation = function () {
                        var args = Array.from(arguments);
                        var ret = ovl.apply(this, args);
                        // Log byte-array args (input to compress / output of decompress)
                        args.forEach(function (a) {
                            if (a !== null) {
                                try {
                                    if (a.getClass().getName() === '[B')
                                        console.log('[WT cdc ' + name + ' in]  '
                                            + jbytesToStr(a).substring(0, 400));
                                } catch (_) {}
                            }
                        });
                        if (ret !== null && ret !== undefined) {
                            try {
                                if (ret.getClass().getName() === '[B')
                                    console.log('[WT cdc ' + name + ' out] '
                                        + jbytesToStr(ret).substring(0, 400));
                            } catch (_) {}
                        }
                        return ret;
                    };
                });
            } catch (_) {}
        });
        console.log('[+] hooked WebTransportCompressorDecompressor');
    } catch (e) {
        console.log('[!] CompressorDecompressor: ' + e);
    }

    // ── 6. Kwik QuicConnection — catch the URL at the lowest QUIC level ───────
    // The exact class name in the kwik jar may differ; try common names.
    [
        'tech.kwik.core.impl.QuicClientConnectionImpl',
        'tech.kwik.core.QuicClientConnection',
        'tech.kwik.core.QuicConnection',
    ].forEach(function (cn) {
        try {
            var cls = Java.use(cn);
            cls.class.getDeclaredMethods().forEach(function (m) {
                var name = m.getName();
                if (!/connect|open|create|init|url|uri|host/i.test(name)) return;
                try {
                    cls[name].overloads.forEach(function (ovl) {
                        ovl.implementation = function () {
                            var args = Array.from(arguments);
                            console.log('[WT kwik ' + cn.split('.').pop() + '.' + name + '] '
                                + args.map(String).join(', ').substring(0, 300));
                            return ovl.apply(this, args);
                        };
                    });
                } catch (_) {}
            });
            console.log('[+] hooked ' + cn);
        } catch (_) {}
    });

    console.log('[wt-hook] all hooks installed');
});
