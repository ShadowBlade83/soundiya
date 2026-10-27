# Third-party notices

soundiya is released under the MIT License (see `LICENSE`). It builds on, and bundles, the
following MIT-licensed work. The license text is the one in `LICENSE`; the copyright notices
below apply to the parts named.

## spotiya

Copyright (c) 2026 klockky - https://github.com/klockky/spotiya

The voice layer (reading Alice's local events and cancelling her reply), the volume ducking and
volume guard, the native-player guard, the ALSA ABI workaround (`controls/alsa_compat.c`), the device
profiles and the cross-build scripts descend from spotiya.

## zapret and zapret-discord-youtube

Copyright (c) 2016-2026 bol-van - https://github.com/bol-van/zapret
Copyright (c) 2024-2026 Flowseal - https://github.com/Flowseal/zapret-discord-youtube

The strategy presets in `src/strategy.rs` reproduce the options of the HTTPS profile of every
`general*.bat` file of zapret-discord-youtube, using the desync techniques that zapret defines. The
files in `fakes/` (`stun.bin`, `stun2.bin`, `tls_clienthello_*.bin`) are copied unchanged from that
repository.

## Rust crates

The build depends on open-source crates (tokio, reqwest, symphonia, alsa, serde and others) under
their own licenses; see `Cargo.lock` for the exact versions.
