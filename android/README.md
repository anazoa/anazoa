# Android builds and signing

Run `scripts/build-android.sh` from the repository root for a debug APK, or add
`--skip-rust` to reuse the existing native libraries. Debug APKs use the local
Android debug key and are intended for development.

Tagged GitHub releases build a signed release APK. All releases must use the
same private signing key so Android can install them as updates. Keep a backup
of the keystore and its passwords; they cannot be recovered from an APK.

## Configure release signing

Create the signing key once, then reuse it for every release. For example:

```sh
umask 077
mkdir -p .scratch/android-signing
keytool -genkeypair -keystore .scratch/android-signing/anazoa-release.keystore \
  -storetype PKCS12 -alias anazoa -keyalg RSA -keysize 4096 \
  -sigalg SHA256withRSA -validity 10000 -dname "CN=Anazoa, O=Anazoa"
```

This command prompts for the keystore password. PKCS12 uses that password for
the private key too. Do not regenerate the key for later releases.

Set these GitHub Actions repository secrets:

| Secret | Value |
| --- | --- |
| `ANDROID_KEYSTORE_BASE64` | Base64 encoding of the complete keystore file |
| `ANDROID_KEYSTORE_PASSWORD` | Keystore password |
| `ANDROID_KEY_ALIAS` | `anazoa` for the example above |
| `ANDROID_KEY_PASSWORD` | Private key password, matching the PKCS12 store password |

On Linux, write the encoded keystore to a file for importing into GitHub:

```sh
base64 -w 0 .scratch/android-signing/anazoa-release.keystore \
  > .scratch/android-signing/keystore.base64
chmod 600 .scratch/android-signing/keystore.base64
```

For a local release build, export `ANDROID_KEYSTORE_PATH` (an absolute path to
the keystore), `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, and
`ANDROID_KEY_PASSWORD`, then run:

```sh
scripts/build-android.sh --release
# To reuse existing native libraries:
scripts/build-android.sh --skip-rust --release
```

The result is `android/app/build/outputs/apk/release/app-release.apk`. Release
builds fail when signing credentials are missing; CI never substitutes a debug
key for a release. Increase `versionCode` in `app/build.gradle.kts` for each new
release and update `versionName` alongside it.

## Moving from the old APKs

The published v0.3.2 and v0.3.3 APKs were signed with different temporary debug
keys. A new release key cannot update either APK in place. Save your config,
uninstall the old app, then install the first APK signed with the persistent
release key. Uninstalling normally deletes app data. Later releases signed
with that same key can update in place. Local debug builds also require a
reinstall when switching between debug and release signing identities.
