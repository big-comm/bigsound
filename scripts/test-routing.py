#!/usr/bin/env python3
"""Automatic output switching with a real session manager, in private D-Bus/PipeWire.

scripts/test-session.py links nodes by hand, so it cannot see routing policy.
This test starts WirePlumber and pipewire-pulse (hardware monitors disabled),
the daemon, an application playing audio, and fake output devices whose names
and priorities match what WirePlumber gives real hardware:

    laptop speakers / desktop jack   alsa_output.pci-...analog-stereo   1009
    Bluetooth speaker or headphones  bluez_output.<address>.1           1010
    USB headset or microphone jack   alsa_output.usb-...analog-stereo   1109

Two machines are simulated: a laptop that gains a Bluetooth speaker, and the
desktop where the v0.7.2 bug was reported (a USB microphone with a headphone
jack outranked the Bluetooth speaker, so BigSound played on the wrong device,
and choosing the speaker in the sound menu bypassed BigSound).

Every check requires the whole path to hold for longer than the daemon's 3 s
poll: the application plays into BigSound (or directly into the device when
processing is off), BigSound plays into exactly one device, and the profile
for that kind of device is active.

Requires pipewire, pipewire-pulse, wireplumber, pactl, pw-cli/pw-link/
pw-metadata/pw-dump/pw-cat, dbus-run-session, stdbuf. No physical device is
touched.
"""
import array
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import wave

# A private config name: PipeWire reads `<name>.conf.d` from every config
# directory, so with the stock name an installed BigSound package would add a
# second chain (with the installed plugins) to the one under test.
CHAIN_CONFIG = 'bigsound-test-chain.conf'

LAPTOP = 'alsa_output.pci-0000_00_1f.3.analog-stereo'
USB_HEADSET = 'alsa_output.usb-Kingston_HyperX_QuadCast_S-00.analog-stereo'
USB_MICROPHONE = 'alsa_output.usb-Generic_USB_Microphone-00.analog-stereo'
SPEAKER = 'bluez_output.00_1A_7D_DA_71_13.1'
HEADPHONES = 'bluez_output.00_1B_66_A1_B2_C3.1'
PRIORITY = {LAPTOP: 1009, SPEAKER: 1010, HEADPHONES: 1010, USB_HEADSET: 1109, USB_MICROPHONE: 1109}
# Profiles auto-applied for these names (crates/daemon/data/profiles). The USB
# names carry no port in this test, so they fall back to the BigSound profile.
PROFILE = {LAPTOP: 'Laptop Speaker', SPEAKER: 'Bluetooth', HEADPHONES: 'Bluetooth',
           USB_HEADSET: 'BigSound', USB_MICROPHONE: 'BigSound'}


def write_chain_config(config_home, target, root):
    base = next(p for p in (Path('/etc/pipewire/filter-chain.conf'), Path('/usr/share/pipewire/filter-chain.conf'))
                if p.is_file())
    pipewire = config_home / 'pipewire'
    (pipewire / (CHAIN_CONFIG + '.d')).mkdir(parents=True)
    (pipewire / CHAIN_CONFIG).write_text(base.read_text())
    text = (root / 'configs/pipewire/10-bigsound.conf.template').read_text()
    for name in ('bass', 'clarity', 'space', 'cross', 'loud'):
        text = text.replace('__BIG_' + name.upper() + '_SO_PATH__', str(target / f'libbig_{name}.so'))
    (pipewire / (CHAIN_CONFIG + '.d') / '99-bigsound.conf').write_text(text)


class Machine:
    """One private audio session: PipeWire, WirePlumber, pulse, BigSound, an app."""

    def __init__(self, name, target, root, tmp):
        self.name, self.target, self.tmp = name, target, tmp
        self.children, self.logs = [], []
        runtime = tmp / 'runtime'
        runtime.mkdir(mode=0o700)
        self.runtime = runtime
        self.config = tmp / 'config'
        self.history = self.config / 'bigsound/output-history'
        self.env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime), PIPEWIRE_RUNTIME_DIR=str(runtime),
                        XDG_CONFIG_HOME=str(self.config), XDG_STATE_HOME=str(tmp / 'state'), LC_ALL='C',
                        DBUS_SYSTEM_BUS_ADDRESS='unix:path=' + str(runtime / 'unavailable-system-bus'))
        self.env.pop('PULSE_SERVER', None)
        dropin = self.config / 'wireplumber/wireplumber.conf.d'
        dropin.mkdir(parents=True)
        # WirePlumber 0.5 syntax; older versions ignore it, and CI runners have
        # no sound hardware anyway.
        (dropin / '90-no-hardware.conf').write_text(
            'wireplumber.profiles = { main = { monitor.alsa = disabled monitor.bluez = disabled '
            'monitor.bluez-midi = disabled monitor.alsa-midi = disabled monitor.v4l2 = disabled '
            'monitor.libcamera = disabled } }\n')
        write_chain_config(self.config, target, root)
        self.silence = tmp / 'silence.wav'
        with wave.open(str(self.silence), 'wb') as wav:
            wav.setnchannels(1)
            wav.setsampwidth(2)
            wav.setframerate(8000)
            wav.writeframes(array.array('h', bytes(8000 * 2 * 400)).tobytes())
        self.daemon = None

    # -- processes -----------------------------------------------------------
    def start(self, args, name):
        log = open(self.tmp / (name + '.log'), 'ab')
        self.logs.append(log)
        proc = subprocess.Popen(args, env=self.env, stdout=log, stderr=log)
        self.children.append(proc)
        return proc

    def run(self, *args, ok=True):
        result = subprocess.run(args, env=self.env, capture_output=True, text=True, timeout=15)
        if ok and result.returncode:
            raise AssertionError(f'{args} failed: {result.stderr[-1500:]}')
        return result.stdout

    def wait_for(self, predicate, what, seconds=15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            result = predicate()
            if result:
                return result
            time.sleep(.1)
        raise AssertionError('timed out: ' + what)

    def nodes(self):
        try:
            dump = json.loads(self.run('pw-dump', ok=False) or '[]')
        except json.JSONDecodeError:
            return {}
        return {o['info']['props'].get('node.name'): o['id'] for o in dump
                if o.get('type') == 'PipeWire:Interface:Node'}

    def boot(self, devices, before_daemon=None):
        self.start(['pipewire'], 'pipewire')
        self.wait_for(lambda: (self.runtime / 'pipewire-0').exists(), 'private PipeWire socket')
        self.start(['wireplumber'], 'wireplumber')
        self.start(['pipewire-pulse'], 'pipewire-pulse')
        self.wait_for(lambda: subprocess.run(['pactl', 'info'], env=self.env, capture_output=True).returncode == 0,
                      'pipewire-pulse')
        for device in devices:
            self.connect(device)
        self.start(['pipewire', '-c', CHAIN_CONFIG], 'chain')
        self.wait_for(lambda: 'BigSound.output' in self.nodes(), 'BigSound filter-chain')
        if before_daemon:
            before_daemon()
        self.start_daemon()
        # An application that follows the system output, like a browser.
        self.start(['pw-cat', '--playback', '--properties', '{ node.name = test-app }', str(self.silence)], 'app')
        self.wait_for(lambda: 'test-app' in self.nodes(), 'application stream')

    def start_daemon(self):
        self.daemon = self.start([str(self.target / 'bigsound-daemon')], 'daemon')
        self.wait_for(lambda: subprocess.run([str(self.target / 'bigsound'), 'list'], env=self.env,
                                             capture_output=True).returncode == 0, 'D-Bus daemon')

    def stop_daemon(self):
        self.daemon.terminate()
        self.daemon.wait(timeout=5)

    def close(self):
        for proc in reversed(self.children):
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
        for log in self.logs:
            log.close()

    # -- user actions --------------------------------------------------------
    def connect(self, device):
        self.run('pw-cli', 'create-node', 'adapter',
                 '{ factory.name = support.null-audio-sink node.name = %s node.description = %s '
                 'media.class = Audio/Sink audio.position = [ FL FR ] object.linger = true '
                 'priority.session = %d priority.driver = %d }' % (device, device, PRIORITY[device], PRIORITY[device]))
        self.wait_for(lambda: device in self.nodes(), device + ' appears')

    def disconnect(self, device):
        self.run('pw-cli', 'destroy', str(self.nodes()[device]))
        self.wait_for(lambda: device not in self.nodes(), device + ' disappears')

    def sound_menu(self, device):
        """What GNOME/KDE sound menus do (through pipewire-pulse)."""
        self.run('pactl', 'set-default-sink', device)

    def cli(self, *args):
        return self.run(str(self.target / 'bigsound'), *args)

    # -- observations --------------------------------------------------------
    def targets(self, source):
        found, current = set(), None
        for line in self.run('pw-link', '-l', ok=False).splitlines():
            if not line.startswith(' '):
                current = line.strip().rsplit(':', 1)[0]
            elif line.strip().startswith(f'|<- {source}:'):
                found.add(current)
        return found

    def state(self):
        return {
            'app': self.targets('test-app'),
            'bigsound': self.targets('BigSound.output'),
            'default': self.run('pactl', 'get-default-sink', ok=False).strip(),
            'profile': self.run(str(self.target / 'bigsound'), 'profile', 'active', ok=False).strip(),
        }

    def expect(self, device, why, processing=True, settle=3.5):
        if processing:
            wanted = {'app': {'BigSound'}, 'bigsound': {device}, 'default': 'BigSound', 'profile': PROFILE[device]}
        else:
            wanted = {'app': {device}, 'default': device}

        def matches(state):
            return all(state[key] == value for key, value in wanted.items())
        deadline = time.monotonic() + 25
        since = None
        while time.monotonic() < deadline:
            if matches(self.state()):
                since = since or time.monotonic()
                if time.monotonic() - since >= settle:
                    print(f'ok - {self.name}: {why}')
                    return
            else:
                since = None
            time.sleep(.1)
        raise AssertionError(f'{self.name}: {why}\n  expected {wanted}\n  got      {self.state()}')

    def dump_logs(self):
        print(self.run('pw-link', '-l', ok=False), file=sys.stderr)
        print(self.run('pw-metadata', '-n', 'default', ok=False), file=sys.stderr)
        for log in sorted(self.tmp.glob('*.log')):
            print(f'--- {self.name} {log.name}:\n' + log.read_text(errors='replace')[-3000:], file=sys.stderr)


def laptop(m):
    m.boot([LAPTOP])
    m.expect(LAPTOP, 'internal speakers with the Laptop Speaker profile, processing on by default')
    m.connect(SPEAKER)
    m.expect(SPEAKER, 'Bluetooth speaker turned on: it plays, with the Bluetooth profile')
    m.disconnect(SPEAKER)
    m.expect(LAPTOP, 'speaker turned off: back to the internal speakers')
    m.connect(SPEAKER)
    m.expect(SPEAKER, 'speaker on again: it plays again')
    m.sound_menu(LAPTOP)
    m.expect(LAPTOP, 'internal speakers chosen in the sound menu while the speaker stays connected')
    m.connect(HEADPHONES)
    m.expect(HEADPHONES, 'Bluetooth headphones connect: they take over')
    m.sound_menu(SPEAKER)
    m.expect(SPEAKER, 'speaker chosen in the sound menu: processing continues on it')


def desktop(m):
    # The reported machine: the user had chosen the speaker directly in the
    # sound menu (bypassing BigSound) before upgrading.
    m.boot([USB_HEADSET, SPEAKER], before_daemon=lambda: m.sound_menu(SPEAKER))
    m.expect(SPEAKER, 'after the upgrade, BigSound takes the system output back and plays on the speaker')
    m.sound_menu(USB_HEADSET)
    m.expect(USB_HEADSET, 'headset chosen in the sound menu: processing continues on it')
    m.sound_menu(SPEAKER)
    m.expect(SPEAKER, 'speaker chosen again, despite the headset\'s higher priority (v0.7.2 bug)')
    m.sound_menu(USB_HEADSET)
    time.sleep(.2)
    m.sound_menu(SPEAKER)
    m.expect(SPEAKER, 'a quick double switch ends on the last choice')
    m.connect(USB_MICROPHONE)
    m.expect(SPEAKER, 'plugging a USB microphone with a headphone jack does not steal the sound')
    m.disconnect(SPEAKER)
    m.expect(USB_HEADSET, 'speaker turned off: the previous choice takes over')
    m.connect(SPEAKER)
    m.expect(SPEAKER, 'speaker on again: BigSound returns to it')

    m.cli('output', 'set', USB_HEADSET)
    m.expect(USB_HEADSET, 'a device fixed in the app')
    m.connect(HEADPHONES)
    m.expect(USB_HEADSET, 'a fixed device is not replaced by Bluetooth headphones connecting')
    m.sound_menu(SPEAKER)
    m.expect(SPEAKER, 'an explicit sound-menu choice replaces the fixed device')
    assert m.cli('output', 'show').strip() == SPEAKER, m.cli('output', 'show')
    m.cli('output', 'auto')
    m.expect(SPEAKER, 'back to automatic')

    m.cli('disable')
    m.expect(SPEAKER, 'processing off: the app plays directly on the speaker', processing=False)
    m.sound_menu(USB_HEADSET)
    m.expect(USB_HEADSET, 'while off, the sound menu works as usual', processing=False)
    m.stop_daemon()
    m.start_daemon()
    m.expect(USB_HEADSET, 'off survives a daemon restart', processing=False)
    m.sound_menu('BigSound')
    m.expect(USB_HEADSET, 'choosing BigSound (DSP) in the sound menu turns processing on')
    assert 'processing: on' in m.cli('status'), m.cli('status')

    m.stop_daemon()
    m.history.write_text(SPEAKER + '\n' + USB_HEADSET + '\n')
    m.start_daemon()
    m.expect(SPEAKER, 'the saved device list is used after a daemon restart')


def main(target):
    root = Path(__file__).resolve().parents[1]
    for name, scenario in (('laptop', laptop), ('desktop', desktop)):
        with tempfile.TemporaryDirectory(prefix=f'bigsound-routing-{name}-') as temporary:
            machine = Machine(name, target, root, Path(temporary))
            try:
                scenario(machine)
                assert machine.daemon.poll() is None
            except Exception:
                machine.dump_logs()
                raise
            finally:
                machine.close()
    print('Routing: laptop and desktop; Bluetooth connect/disconnect, sound-menu choices with processing, '
          'USB microphone ignored, fixed device, on/off, profiles per device and daemon restarts OK')


if __name__ == '__main__':
    missing = [tool for tool in ('pipewire', 'pipewire-pulse', 'wireplumber', 'pactl', 'pw-cli', 'pw-cat',
                                 'pw-link', 'pw-metadata', 'pw-dump', 'dbus-run-session', 'stdbuf')
               if not shutil.which(tool)]
    if missing:
        raise SystemExit('missing tools: ' + ', '.join(missing))
    if '--private-bus' not in sys.argv:
        target = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/release').resolve())
        with tempfile.TemporaryDirectory(prefix='bigsound-bus-') as bus_tmp:
            bus_config = Path(bus_tmp) / 'session.conf'
            # No service directories: never activate host services (gvfs,
            # portals, keyrings) from this deliberately private bus.
            bus_config.write_text('<busconfig><type>session</type><listen>unix:tmpdir=' + bus_tmp + '</listen>'
                                  '<policy context="default"><allow send_destination="*"/>'
                                  '<allow receive_sender="*"/><allow own="*"/></policy></busconfig>')
            raise SystemExit(subprocess.call(['dbus-run-session', '--config-file', str(bus_config), '--',
                                              sys.executable, str(Path(__file__).resolve()), target,
                                              '--private-bus']))
    main(Path(sys.argv[1]))
