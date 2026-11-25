# BlackHole source

`BlackHole/` is BlackHole v0.7.1 (commit e2b22aaaba4e507a097131704bf96dabc004d9cf) from https://github.com/ExistentialAudio/BlackHole, unmodified, without the `.github`, `Images`, `Installer` and `Uninstaller` folders. It is GPL-3.0; the license is `BlackHole/LICENSE`.

`build.sh` builds it twice, as `MicDSP.driver` ("Francisco's Mic" and its hidden feed) and as `MicDSPSpeakers.driver` ("Francisco's Speakers" and its hidden loopback). Each build is configured through preprocessor definitions only and gets its own bundle ID and its own plug-in factory UUID, which `build.sh` rewrites in the built `Info.plist`, as BlackHole's own installer does. Built binaries stay on this machine and are never published.
