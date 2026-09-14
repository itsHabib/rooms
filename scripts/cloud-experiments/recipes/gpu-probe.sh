#!/bin/bash
set -euo pipefail
mkdir -p "$HOME/lab"
cd "$HOME/lab"
curl -fL --retry 2 -o cloud-hypervisor https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v53.0/cloud-hypervisor-static
printf '%s\n' '448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc  cloud-hypervisor' | sha256sum -c -
chmod +x cloud-hypervisor
./cloud-hypervisor --version
sudo modprobe vfio-pci
sudo lspci -nnk -s 00:03.0 > gpu-pci.txt
sudo find /sys/kernel/iommu_groups -mindepth 1 > iommu-groups.txt
set +e
sudo timeout 20 ./cloud-hypervisor --kernel "/boot/vmlinuz-$(uname -r)" --cpus boot=1 --memory size=512M --cmdline 'console=ttyS0' --device path=/sys/bus/pci/devices/0000:00:03.0 > vfio-probe.stdout 2> vfio-probe.stderr
status=$?
set -e
printf '%s\n' "$status" > vfio-probe.exit
cat vfio-probe.stderr
printf 'probe_exit=%s\n' "$status"
