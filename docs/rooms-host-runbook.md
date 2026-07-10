# rooms-host runbook

Operating the Ubuntu-under-Hyper-V `rooms-host` VM that runs Firecracker: rebuild
it from scratch, reach it, provision it, and validate the pool end-to-end. This
is the operational companion to [`CONTRIBUTING.md`](../CONTRIBUTING.md) (which
covers the in-guest stack) and [`scripts/README.md`](../scripts/README.md).

The `rooms` binary runs **on this Ubuntu host, not on Windows**. Windows only
hosts the VM. Host layout: Windows -> Hyper-V -> Ubuntu `rooms-host` -> a
Firecracker microVM per room.

## 0. Reach the host

- User is `mh`; auth is **SSH key only** (`~/.ssh/id_rooms_host` on Windows;
  cloud-init bakes the pubkey in). There is **no password** — the Hyper-V
  `vmconnect` console cannot be logged into, by design.
- The IP is a Hyper-V Default-Switch DHCP lease in **`172.21.240.0/20`** and
  **drifts on every rebuild/reboot**. Find the current one from Windows without
  the console:
  ```powershell
  # the guest's MAC starts 00-15-5D (Hyper-V OUI); match it in the ARP table
  Get-NetNeighbor -InterfaceAlias 'vEthernet (Default Switch)' -AddressFamily IPv4 |
    Where-Object LinkLayerAddress -like '00-15-5D-*'
  ```
- If an agent drives the host over SSH, the session needs a permission rule
  pinning the current IP, e.g.
  `Bash(ssh -o BatchMode=yes -o ConnectTimeout=8 mh@<ip>:*)`.

## 1. Rebuild the VM from scratch (hands-free)

One elevated command builds the VM from an Ubuntu **cloud image** — no
interactive installer. See [`scripts/provision-hyperv-auto.ps1`](../scripts/provision-hyperv-auto.ps1).

Prereqs (once):
- Hyper-V enabled; WSL with a distro named `Ubuntu` (used to author the seed ISO).
- A base VHDX from the official cloud image:
  ```bash
  curl -LO https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img
  # verify against SHA256SUMS in that same directory, then (qemu-img via WSL apt install qemu-utils):
  qemu-img convert -f qcow2 -O vhdx -o subformat=dynamic ubuntu-24.04-server-cloudimg-amd64.img C:/Hyper-V/rooms-host/os.vhdx
  ```

Run (elevated PowerShell):
```powershell
powershell -ExecutionPolicy Bypass -File C:\Users\MichaelHabib\pers\rooms\scripts\provision-hyperv-auto.ps1 -Force
```
It copies the pristine base, builds a cloud-init `cidata` seed **ISO** in WSL,
attaches it as a **DVD**, creates a gen2 VM (nested virt, static memory), boots,
finds the guest IP via the host neighbor table, and prints `ssh mh@<ip>` once
sshd answers. `-Force` tears down any prior VM/disks first.

### Why the seed is an ISO-on-DVD, not a mounted disk

A locked-down corp host enforces **BitLocker To Go** (*deny write to removable
drives not BitLocker-protected*). The old approach mounted a small FAT volume and
wrote the seed files to it — `Format-Volume` (a management op) succeeds but the
file write is policy-blocked, and the volume is too small to BitLocker, so it
dead-ends both ways. Authoring the ISO in WSL and writing only to the fixed `C:`
drive sidesteps the policy entirely; cloud-init reads a `cidata` CD identically.

## 2. Provision the in-guest stack

SSH in as `mh`, then:
```bash
git clone https://github.com/itsHabib/rooms ~/dev/rooms   # canonical clone path
cd ~/dev/rooms
bash scripts/setup-rooms-host.sh                          # Firecracker, kernel, Rust, Node, /dev/kvm, work dirs (idempotent)
```
Build the binary and the **agent rootfs** (bakes the `rooms` guest user + key):
```bash
export PATH="$HOME/.cargo/bin:$PATH"                       # cargo isn't on the non-login SSH PATH
make release
bash scripts/build-rootfs-alpine.sh                        # current builder; build-rootfs.sh (noble) + bake-rootfs-ssh.sh are legacy/POC
sudo bash scripts/setup-tap.sh --host                      # installs the ROOMS_FWD chain (gone after every reboot)
```

## 3. Validate

```bash
rooms doctor                                               # run as mh, NOT sudo (sudo reads root's HOME -> empty state base)
export PATH="$HOME/.cargo/bin:$PATH"; sudo -E env "PATH=$PATH" make e2e   # boots 3 microVMs, asserts isolation + zero leaks
```
- `doctor` should be all-`ok` except an acceptable `anthropic_api_key` WARN
  (unset key is a warning, not a failure — the base substrate needs no key).
- `make e2e` should report egress **Verified** and run the behavioral
  guest->guest cross-talk probe (requires the key-paired rootfs from step 2).

### N-room CLI smoke (the pool doing real work)
```bash
for i in 1 2 3; do rooms run --command "echo room-$i-$(hostname)" & done; wait
rooms ls        # must be clean afterwards — every slot freed
```

## Gotchas (each cost a debugging cycle — don't relearn them)

| Trap | Symptom | Fix |
| --- | --- | --- |
| `sudo rooms <verb>` | `rooms ls` says "no rooms" though rooms exist | sudo reads root's `HOME`; run rooms verbs as `mh` |
| cargo not found over SSH | `make: cargo: No such file or directory` | `export PATH="$HOME/.cargo/bin:$PATH"` (and `sudo -E env "PATH=$PATH"`) |
| `ROOMS_FWD` missing | doctor `rooms_fwd` FAIL after a reboot | `sudo bash scripts/setup-tap.sh --host` |
| guest login fails | e2e egress `ReachableNoAuth` | rootfs must bake the `rooms` user (not just root) + `~/.ssh/id_rooms` |
| VM IP not found | provision script's KVP wait times out | use the host neighbor table (see §0); KVP needs guest tools that install late |
| BitLocker prompt on provision | seed volume write "media is write protected" | seed is built as an ISO in WSL, never a mounted volume (see §1) |
| PS parse error on a valid script | "string is missing the terminator" in PS 5.1 | keep `.ps1` pure ASCII — 5.1 reads no-BOM files as CP1252 and mangles em-dashes |
