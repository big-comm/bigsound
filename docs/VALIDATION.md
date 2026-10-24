# BigSound validation

Run from a clean checkout on Linux with Rust stable, GTK4, libadwaita >=1.4,
gettext and Python 3. The installation script is not a test: it changes the
user's services and audio routing. The validation scripts use offline DSP
processing or explicitly private D-Bus/PipeWire instances.

```bash
CARGO_TARGET_DIR=/tmp/bigsound-target ./scripts/validate.sh
BIGSOUND_TEST_SESSION=1 CARGO_TARGET_DIR=/tmp/bigsound-target ./scripts/validate.sh
cargo audit --deny warnings
python3 scripts/check-publication.py . --history
```

`cargo-audit` and ShellCheck are optional to the local convenience script;
their checks are mandatory before publication and run in CI. The session
integration test needs `pipewire`, `pw-cli`, `pw-link`, `pw-dump`, `pw-cat`
and `dbus-run-session`; GTK smoke testing additionally needs
`gtk4-broadwayd`. It starts no hardware monitors or session manager and does
not connect to the user's audio sockets.

The routing integration test (`scripts/test-routing.py`, also run by
`BIGSOUND_TEST_SESSION=1`) adds `wireplumber`, `pipewire-pulse`, `pactl`,
`pw-metadata` and `stdbuf`. It runs a private WirePlumber with hardware
monitors disabled, an application playing audio, and fake output devices
named and prioritised like real hardware: laptop speakers / desktop jack
(1009), Bluetooth (1010) and USB (1109). It simulates a laptop gaining a
Bluetooth speaker and the desktop where the v0.7.2 bug was reported, and
checks the whole path (application → BigSound → one device) and the profile
after every connection, disconnection, sound-menu choice and on/off switch.

## What each layer establishes

| Layer | Evidence | Limit |
|---|---|---|
| Unit tests | filters, delay, peak safety, invalid values, subprocess deadlines, persistence and control validation | deterministic inputs, not all numerical possibilities |
| Full DSP chain | all 15 profiles at 8, 16, 22.05, 44.1, 48, 96, 192 and 384 kHz; poison recovery, mono centring, ceiling | sample peaks, not reconstructed true peaks |
| Allocation counter | process, parameter update and reset allocate zero times after construction | covers the Rust chain, not PipeWire/GTK |
| LADSPA ABI | actual .so lifecycle, null controls/audio, invalid rates, in-place buffers and irregular block sizes | assumes host supplies valid non-null pointers and adequate buffer lengths |
| Stress/performance | complete chain with profile switches and resets, blocks of 64/256/1024 at four rates | offline throughput on one machine, not a hard real-time guarantee |
| Private session | real D-Bus CLI/daemon, live PipeWire controls, concurrent requests, profile persistence, real partial-profile rollback, filter-chain restart, captured processed audio | no physical ALSA/Bluetooth/HDMI device or WirePlumber policy |
| Private routing | real WirePlumber policy and pipewire-pulse on simulated laptop and desktop: fresh install, Bluetooth connect/disconnect/reconnect, sound-menu choices keep processing, quick switches, USB microphone ignored, fixed device, on/off, per-device profiles, state across daemon restarts | fake null sinks, not physical devices; the WirePlumber version is the distribution's |
| GTK | Broadway startup and private X11 2× captures with mode/profile synchronization; worker coalescence/order unit tests | no comprehensive accessibility, visual or prolonged interactive regression coverage |
| Security | RustSec and heuristic publication scan of candidates and reachable history | a clean scan does not prove absence of confidential context |

The stress duration is **simulated audio time per rate/block combination**.
For a longer run, use `BIGSOUND_STRESS_SECONDS=600`. The harness records
wall time, realtime factor, p99 and maximum block processing time. Compare
against the block deadline (`block_frames / sample_rate`) and repeat under
controlled CPU load when investigating underruns. Do not turn
machine-dependent performance numbers into brittle CI assertions.

## Audio boundaries and listening

DSP constructors accept finite rates in 8–768 kHz; the checked test matrix
ends at 384 kHz. LADSPA returns a null instance for unsupported rates.
Non-finite audio becomes silence; abnormally large finite input is bounded
to ±16 before stateful processing. Ordinary finite bypass audio is unchanged
inside that bound. Public DSP controls clamp to the supported range;
non-finite controls use defaults. D-Bus rejects invalid controls instead of
advertising values different from the DSP's clamped state.

The last stage is a stereo-linked **sample-peak** limiter. It does not
implement oversampling, lookahead or inter-sample true-peak measurement.
Bass/clarity nonlinearities can generate aliasing; downstream low-pass
filtering cannot reverse spectral folding that already occurred. Crossfeed
is stereo filtering and delay, without Atmos decoding or head tracking.

Before approving an audio release, listen at moderate levels with matched
loudness against bypass, using speakers and headphones. Check vocals,
percussion, bass-heavy material, hard-panned channels, mono and silence;
inspect distortion/aliasing when changing drive and frequency. Perform
Bluetooth/HDMI hotplug, suspend/resume and a user-session PipeWire restart
on the target distribution. These hardware/listening checks are separate
from automated validation and must be recorded honestly.

## Publication and reproducibility

Keep `Cargo.lock` committed and build with `--locked`; packaging uses
`--frozen` after a locked fetch. Packaging clears workspace release artifacts
while retaining dependency builds: normalized archive timestamps must not
allow stale Cargo fingerprints to reuse an earlier application binary.
The recipe remaps build/home paths so panic locations do not expose the
builder's machine identity even after stripping. Actions have read-only permissions, fixed
revisions and time limits. Review the diff, screenshots, SVG/PNG metadata
and author identity before committing or uploading. The publication checker also checks author/committer identities in history and
prints finding categories and locations, never the matched secret. It
checks the working tree and, with `--history`, all locally reachable blobs
and commit messages. It does not rewrite Git history or examine unrelated
repositories. No AI coauthor trailer is added by these workflows.

## Arch package recipe

`pkgbuild/PKGBUILD` follows the layout of the other `big-comm` packages. It
clones `big-comm/bigsound` (build-package points testing builds at the
`dev-*` branch) and versions the package by build date. To package the
committed state of the current branch locally, build a copy whose source
points at this checkout:

```bash
tmp=$(mktemp -d)
sed "s|^source=.*|source=(\"bigsound::git+file://$PWD#branch=$(git branch --show-current)\")|" \
    pkgbuild/PKGBUILD > "$tmp/PKGBUILD"
(cd "$tmp" && makepkg -f)
```

For a bounded CPU-load soak:

```bash
python3 scripts/stress-under-load.py target/release/bigsound-qa 600
```

This runs at most four load workers and reports peak DSP-process RSS; it does
not change system services, scheduler policy or global CPU settings.

## Native high-resolution GUI captures

Build the workspace first, then run:

```bash
cargo build --release --workspace --locked
python3 scripts/test-gui.py --target target/release --theme light
python3 scripts/test-gui.py --target target/release --theme dark
```

Requires Xvfb, xauth, xdotool, ImageMagick (`magick`), gettext, a Brazilian
Portuguese locale and the private-session tools above. `--xvfb PATH` selects
a test executable without installing it. `--capture DIRECTORY` selects the
output directory (default `validation-output/gui`, ignored by Git).

The harness starts its own authenticated X11 server without TCP, private
D-Bus and PipeWire instances, test profiles and XDG directories. It does not
attach to the user's display, audio server or physical devices. It captures
three actual GTK views per theme at native 2× scale, strips PNG metadata and checks
window/daemon survival, distinct views, profile synchronization and absence
of GTK critical errors. All child processes are stopped on exit.

This is a reproducible integration/capture smoke test, not a substitute for
screen-reader review, hardware testing or complete GUI fault injection.

## Legacy pw-cli profile boundary

Older `pw-cli` versions use a fixed 1024-byte POD buffer for `set-param`.
The daemon partitions profile updates into bounded batches without splitting
stereo aliases. The Rust regression test checks a conservative encoded-size
bound. Profile selection is committed only after all batches succeed.

The private-session test injects one failure in the second profile batch,
then confirms every live control returns to the previous state and the cached
profile remains unchanged. If restoration fails because the graph is gone,
the daemon retries only the previous confirmed cache. Multiple batches do
not imply a sample-synchronous, globally atomic profile change.
